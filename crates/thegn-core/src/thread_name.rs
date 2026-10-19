//! Named OS threads for attribution in `/proc/<pid>/task/*/comm`.

use std::thread::{self, JoinHandle};

/// Spawn a thread with a stable name visible to OS-level thread attribution.
///
/// Linux keeps at most 15 bytes in the task `comm` field, so names are limited
/// to that length to keep the whole attribution key visible.
/// Panics if the OS refuses the thread; use `try_spawn_named` where that must degrade.
pub fn spawn_named<F, T>(name: &'static str, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    try_spawn_named(name, f)
        .unwrap_or_else(|error| panic!("failed to spawn thread {name}: {error}"))
}

/// Try to spawn a named thread, returning an error if the OS refuses it.
///
/// Use this where a refused thread must degrade instead of panicking.
pub fn try_spawn_named<F, T>(name: &'static str, f: F) -> std::io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    debug_assert!(name.starts_with("thegn-") && name.len() <= 15);
    thread::Builder::new().name(name.to_owned()).spawn(f)
}

#[cfg(test)]
mod tests {
    use super::{spawn_named, try_spawn_named};

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

    #[test]
    fn try_spawn_named_sets_the_os_visible_name_and_returns_the_closure_value() {
        let thread = try_spawn_named("thegn-try-name", || {
            (std::thread::current().name().map(str::to_owned), 42)
        })
        .unwrap();

        let (name, value) = thread.join().unwrap();
        assert_eq!(name.as_deref(), Some("thegn-try-name"));
        assert_eq!(value, 42);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn try_spawn_named_rejects_overlong_names_in_debug_builds() {
        let _thread = try_spawn_named("thegn-123456789a", || ());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn try_spawn_named_rejects_wrong_prefixes_in_debug_builds() {
        let _thread = try_spawn_named("invalid", || ());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn spawn_named_rejects_overlong_names_in_debug_builds() {
        let _thread = spawn_named("thegn-123456789a", || ());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn spawn_named_rejects_wrong_prefixes_in_debug_builds() {
        let _thread = spawn_named("invalid", || ());
    }
}
