//! Named OS threads for attribution in `/proc/<pid>/task/*/comm`.

use std::thread::{self, JoinHandle};

/// Spawn a thread with a stable name visible to OS-level thread attribution.
///
/// Linux keeps at most 15 bytes in the task `comm` field, so names are limited
/// to that length to keep the whole attribution key visible.
pub fn spawn_named<F, T>(name: &'static str, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    debug_assert!(name.starts_with("thegn-") && name.len() <= 15);
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .unwrap_or_else(|error| panic!("failed to spawn thread {name}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::spawn_named;

    #[test]
    fn spawn_named_sets_the_os_visible_name() {
        let thread = spawn_named("thegn-test-name", || {
            std::thread::current().name().map(str::to_owned)
        });

        assert_eq!(thread.join().unwrap().as_deref(), Some("thegn-test-name"));
    }

    #[test]
    fn spawn_named_returns_the_closure_value() {
        assert_eq!(spawn_named("thegn-test-val", || 42).join().unwrap(), 42);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn spawn_named_rejects_invalid_names_in_debug_builds() {
        let _thread = spawn_named("not-thegn-thread", || ());
    }
}
