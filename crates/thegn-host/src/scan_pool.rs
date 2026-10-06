//! Bounded, process-wide worker pool for the worktree glyph scans (THE-652).
//!
//! Glyph hydration used to `std::thread::scope`-spawn one native thread per
//! worktree to rescan, so thread count (and concurrent `git status` count)
//! grew with the number of expired rows and with overlapping hydrations, and
//! bypassed the runtime's `max_blocking_threads`. This pool caps the number of
//! scan threads across ALL hydration invocations.
//!
//! Shape, chosen to keep the 0%-idle contract:
//! * workers are spawned lazily on submit (up to `cap`) and EXIT when the queue
//!   drains, so an idle process owns zero scan threads and the pool adds no
//!   timer, poll or wake source;
//! * `urgent` jobs (the active worktree) go to the front of the queue; the rest
//!   are FIFO, so inactive rows always make eventual progress (urgent work is
//!   one job per hydration, never a stream that can starve the tail);
//! * every submitted job yields exactly one outcome: its value, or `None` when
//!   it panicked or no worker thread could be created. Callers map `None` to a
//!   degraded last-known-good row, never a fabricated clean one;
//! * no lock is held while a job runs or while the caller waits.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, mpsc};

type Task = Box<dyn FnOnce() + Send + 'static>;

/// A scan job returning `R`.
pub(crate) type ScanJob<R> = Box<dyn FnOnce() -> R + Send + 'static>;

struct State {
    queue: VecDeque<Task>,
    workers: usize,
    /// Workers currently executing a task (a subset of `workers`).
    busy: usize,
}

pub(crate) struct ScanPool {
    cap: usize,
    state: Mutex<State>,
}

impl ScanPool {
    pub(crate) fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap: cap.max(1),
            state: Mutex::new(State {
                queue: VecDeque::new(),
                workers: 0,
                busy: 0,
            }),
        })
    }

    /// The shared pool every hydration invocation submits to.
    pub(crate) fn global() -> &'static Arc<ScanPool> {
        static POOL: std::sync::OnceLock<Arc<ScanPool>> = std::sync::OnceLock::new();
        POOL.get_or_init(|| ScanPool::new(default_cap()))
    }

    /// Run every job on the pool and return one outcome per job, in input
    /// order. Blocks the caller (a hydration worker, never the event loop)
    /// until all outcomes are in.
    pub(crate) fn run<R: Send + 'static>(
        self: &Arc<Self>,
        jobs: Vec<(bool, ScanJob<R>)>,
    ) -> Vec<Option<R>> {
        let n = jobs.len();
        let (tx, rx) = mpsc::channel::<(usize, Option<R>)>();
        for (i, (urgent, job)) in jobs.into_iter().enumerate() {
            let tx = tx.clone();
            let task: Task = Box::new(move || {
                // A panicking job yields `None`; the caller degrades that row.
                let out = match catch_unwind(AssertUnwindSafe(job)) {
                    Ok(value) => Some(value),
                    Err(_) => None,
                };
                if tx.send((i, out)).is_err() {
                    // caller gone: nothing is waiting for this outcome.
                }
            });
            self.submit(task, urgent);
        }
        drop(tx);
        let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
        // Every task sends exactly once (even on panic); a refused one is
        // answered by `submit` itself. A dropped sender ends the loop.
        while let Ok((i, r)) = rx.recv() {
            out[i] = r;
        }
        out
    }

    fn submit(self: &Arc<Self>, task: Task, urgent: bool) {
        let spawn_worker = {
            let mut st = self.state.lock().unwrap();
            if urgent {
                st.queue.push_front(task);
            } else {
                st.queue.push_back(task);
            }
            // Spawn only while queued work exceeds the IDLE workers; busy
            // workers are not available to take it.
            if st.workers < self.cap && st.queue.len() > st.workers - st.busy {
                st.workers += 1;
                true
            } else {
                false
            }
        };
        if !spawn_worker {
            return;
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("thegn-glyph-scan".into())
            .spawn(move || me.worker_loop());
        if spawned.is_err() {
            // Bounded and visible: undo the reservation. If no worker exists
            // at all, nothing would ever drain the queue, so fail everything
            // queued with an explicit degraded outcome instead of hanging.
            tracing::warn!(
                target: "thegn::hydrate",
                "glyph scan worker thread could not be created"
            );
            let orphaned: Vec<Task> = {
                let mut st = self.state.lock().unwrap();
                st.workers -= 1;
                if st.workers == 0 {
                    st.queue.drain(..).collect()
                } else {
                    Vec::new()
                }
            };
            // Dropping a task drops its sender clone without sending, so the
            // caller's wait ends with `None` (degraded) for those indices.
            drop(orphaned);
        }
    }

    fn worker_loop(self: Arc<Self>) {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        loop {
            let task = {
                let mut st = self.state.lock().unwrap();
                match st.queue.pop_front() {
                    Some(t) => {
                        st.busy += 1;
                        t
                    }
                    None => {
                        st.workers -= 1;
                        return;
                    }
                }
            };
            task();
            self.state.lock().unwrap().busy -= 1;
        }
    }

    #[cfg(test)]
    fn workers(&self) -> usize {
        self.state.lock().unwrap().workers
    }
}

/// Aggregate scan-thread cap: enough to overlap I/O-bound `git status` reads,
/// small enough not to compete with the render loop or the build-slot budget.
fn default_cap() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(2)
        .clamp(2, 6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Workers exit just after sending their last result, so poll briefly.
    fn drained(pool: &ScanPool) -> bool {
        for _ in 0..200 {
            if pool.workers() == 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    fn jobs(
        n: usize,
        cur: &Arc<AtomicUsize>,
        peak: &Arc<AtomicUsize>,
    ) -> Vec<(bool, Box<dyn FnOnce() -> usize + Send + 'static>)> {
        (0..n)
            .map(|i| {
                let (cur, peak) = (Arc::clone(cur), Arc::clone(peak));
                let f: Box<dyn FnOnce() -> usize + Send + 'static> = Box::new(move || {
                    let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    cur.fetch_sub(1, Ordering::SeqCst);
                    i
                });
                (i == 0, f)
            })
            .collect()
    }

    #[test]
    fn concurrency_never_exceeds_cap_for_1_8_32_and_100() {
        for n in [1usize, 8, 32, 100] {
            let pool = ScanPool::new(3);
            let (cur, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let out = pool.run(jobs(n, &cur, &peak));
            assert_eq!(out.len(), n);
            assert!(out.iter().enumerate().all(|(i, r)| *r == Some(i)));
            assert!(peak.load(Ordering::SeqCst) <= 3, "n={n}");
            assert!(drained(&pool), "workers exit when drained (n={n})");
        }
    }

    #[test]
    fn overlapping_invocations_share_the_aggregate_cap() {
        let pool = ScanPool::new(3);
        let (cur, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let hs: Vec<_> = (0..4)
            .map(|_| {
                let (pool, cur, peak) = (Arc::clone(&pool), cur.clone(), peak.clone());
                std::thread::spawn(move || pool.run(jobs(20, &cur, &peak)))
            })
            .collect();
        for h in hs {
            let out = h.join().unwrap();
            assert!(out.iter().all(|r| r.is_some()));
        }
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn new_job_gets_a_worker_while_another_is_blocked() {
        let pool = ScanPool::new(2);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let g = Arc::clone(&gate);
        let p2 = Arc::clone(&pool);
        let blocker = std::thread::spawn(move || {
            let job: ScanJob<()> = Box::new(move || {
                let (m, c) = &*g;
                let mut open = m.lock().unwrap();
                while !*open {
                    open = c.wait(open).unwrap();
                }
            });
            p2.run(vec![(false, job)])
        });
        // Wait until the blocker is actually running on a worker.
        while pool.state.lock().unwrap().busy == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Must complete on a second worker even though the first is blocked.
        let job: ScanJob<u8> = Box::new(|| 7);
        assert_eq!(pool.run(vec![(true, job)]), vec![Some(7)]);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(blocker.join().unwrap(), vec![Some(())]);
    }

    #[test]
    fn panicking_job_yields_none_and_others_complete() {
        let pool = ScanPool::new(2);
        let mut js: Vec<(bool, Box<dyn FnOnce() -> u8 + Send>)> = Vec::new();
        js.push((false, Box::new(|| 1)));
        js.push((false, Box::new(|| panic!("boom"))));
        js.push((false, Box::new(|| 3)));
        let out = pool.run(js);
        assert_eq!(out, vec![Some(1), None, Some(3)]);
        assert!(drained(&pool));
    }

    #[test]
    fn urgent_job_runs_before_queued_background_jobs() {
        let pool = ScanPool::new(1);
        let order = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut js: Vec<(bool, Box<dyn FnOnce() + Send>)> = Vec::new();
        {
            let g = Arc::clone(&gate);
            js.push((
                false,
                Box::new(move || {
                    let (m, c) = &*g;
                    let mut open = m.lock().unwrap();
                    while !*open {
                        open = c.wait(open).unwrap();
                    }
                }),
            ));
        }
        for (i, urgent) in [(1u8, false), (2, false), (3, true)] {
            let o = Arc::clone(&order);
            js.push((urgent, Box::new(move || o.lock().unwrap().push(i))));
        }
        let g = Arc::clone(&gate);
        let opener = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            *g.0.lock().unwrap() = true;
            g.1.notify_all();
        });
        let out = pool.run(js);
        opener.join().unwrap();
        assert!(out.iter().all(|r| r.is_some()));
        // Urgent was submitted last but ahead of every queued background job;
        // the background rows still all ran (eventual progress).
        assert_eq!(*order.lock().unwrap(), vec![3, 1, 2]);
    }
}
