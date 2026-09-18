//! Bounded suppression of unchanged runtime configuration diagnostics.
//! Strict validation still returns every diagnostic on every invocation.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

const MAX_DIAGNOSTICS: usize = 256;
thread_local! { static SOURCE: Cell<u64> = const { Cell::new(0) }; }
thread_local! { static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) }; }
thread_local! { static SUPPLIED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) }; }
thread_local! { static QUIET: Cell<bool> = const { Cell::new(false) }; }
static RECENT: OnceLock<Mutex<Recent>> = OnceLock::new();
fn recent() -> &'static Mutex<Recent> {
    RECENT.get_or_init(|| Mutex::new(Recent::default()))
}

fn fingerprint(value: impl Hash) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// A scope includes source identity and content. Nested overlays restore their
/// parent on return; parallel hydration threads never exchange source context.
pub(crate) struct SourceGuard(u64);
impl Drop for SourceGuard {
    fn drop(&mut self) {
        SOURCE.set(self.0);
    }
}

pub(crate) fn source(identity: impl Hash, content: &str) -> SourceGuard {
    let identity = fingerprint((SOURCE.get(), identity));
    if let Ok(mut seen) = recent().lock() {
        seen.observe_source(identity, fingerprint(content));
    }
    SourceGuard(SOURCE.replace(identity))
}

#[derive(Default)]
struct Recent {
    diagnostics: VecDeque<(u64, u64)>,
    sources: VecDeque<(u64, u64)>,
}
impl Recent {
    fn observe_source(&mut self, identity: u64, content: u64) {
        if let Some((_, previous)) = self.sources.iter_mut().find(|(id, _)| *id == identity) {
            if *previous == content {
                return;
            }
            *previous = content;
            self.diagnostics.retain(|(source, _)| *source != identity);
        } else {
            if self.sources.len() == MAX_DIAGNOSTICS
                && let Some((expired, _)) = self.sources.pop_front()
            {
                self.diagnostics.retain(|(source, _)| *source != expired);
            }
            self.sources.push_back((identity, content));
        }
    }

    fn admit(&mut self, source: u64, diagnostic: impl Hash) -> bool {
        // Fixed-size fingerprints retain no configuration values or secrets.
        let key = (source, fingerprint(diagnostic));
        if self.diagnostics.contains(&key) {
            return false;
        }
        if self.diagnostics.len() == MAX_DIAGNOSTICS {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(key);
        true
    }
}

#[track_caller]
pub(crate) fn warn(message: &str) {
    let message = safe_warning(message);
    CAPTURE.with_borrow_mut(|captured| {
        if let Some(captured) = captured {
            if captured.len() < MAX_DIAGNOSTICS {
                captured.push(message.clone());
            }
        }
    });
    let emit = !QUIET.get()
        && recent()
            .lock()
            .map(|mut seen| seen.admit(SOURCE.get(), (std::panic::Location::caller(), &message)))
            .unwrap_or(true);
    if emit {
        crate::msg::warn(&format!("config: {message}"));
    }
}

fn safe_warning(message: &str) -> String {
    let redacted = crate::log_redact::redact_text_line(message);
    // Environment and CLI parsers historically included the supplied value in
    // "ignoring" warnings.  Those values are not needed for remediation and
    // may be credentials, so retain only the key and the typed failure class.
    if (redacted.contains("ignoring") || redacted.contains("override ignored"))
        && let Some((key, _)) = redacted.split_once(':')
    {
        return format!("{key}: invalid supplied value (ignored)");
    }
    redacted
}

/// Capture warnings emitted by one composition step while retaining normal
/// warning delivery.  Admission uses this only to turn malformed supplied
/// env/CLI values into typed errors; compatibility warnings remain warnings.
pub(crate) fn capture<T>(action: impl FnOnce() -> T) -> (T, Vec<String>) {
    struct Restore(Option<Vec<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.with(|captured| *captured.borrow_mut() = self.0.take());
        }
    }

    let restore = Restore(CAPTURE.with(|captured| captured.replace(Some(Vec::new()))));
    let result = action();
    let captured = CAPTURE
        .with(|captured| captured.replace(None))
        .unwrap_or_default();
    drop(restore);
    (result, captured)
}

/// Record a parser-classified supplied-value failure without encoding the
/// security decision in warning prose.  The key and kind are bounded and do
/// not retain the supplied value.
pub(crate) fn supplied_error(key: &str, kind: &str) {
    SUPPLIED.with_borrow_mut(|errors| {
        if let Some(errors) = errors
            && errors.len() < MAX_DIAGNOSTICS
        {
            let mut item = String::with_capacity(key.len() + kind.len() + 1);
            item.push_str(key);
            item.push(':');
            item.push_str(kind);
            errors.push(item);
        }
    });
}

/// Capture environment/CLI parsing without emitting a runtime warning.  The
/// typed supplied-value list is separate from redacted human diagnostics.
pub(crate) fn capture_quiet<T>(action: impl FnOnce() -> T) -> (T, Vec<String>, Vec<String>) {
    struct Restore(Option<Vec<String>>, Option<Vec<String>>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.with(|captured| *captured.borrow_mut() = self.0.take());
            SUPPLIED.with(|errors| *errors.borrow_mut() = self.1.take());
            QUIET.set(self.2);
        }
    }

    let previous = QUIET.replace(true);
    let restore = Restore(
        CAPTURE.with(|captured| captured.replace(Some(Vec::new()))),
        SUPPLIED.with(|errors| errors.replace(Some(Vec::new()))),
        previous,
    );
    let result = action();
    let warnings = CAPTURE
        .with(|captured| captured.replace(None))
        .unwrap_or_default();
    let supplied = SUPPLIED
        .with(|errors| errors.replace(None))
        .unwrap_or_default();
    drop(restore);
    (result, warnings, supplied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_and_source_determine_deduplication() {
        let mut seen = Recent::default();
        let source = fingerprint(("/one/config.toml", "legacy = true"));
        assert!(seen.admit(source, "legacy spelling"));
        assert!(!seen.admit(source, "legacy spelling"));
        assert!(seen.admit(source, "unknown key: typo"));
        assert!(seen.admit(source, "unknown key: changed_typo"));
        assert!(seen.admit(
            fingerprint(("/two/config.toml", "legacy = true")),
            "legacy spelling"
        ));
        assert!(seen.admit(
            fingerprint(("/one/config.toml", "legacy = false")),
            "legacy spelling"
        ));
    }

    #[test]
    fn fixing_then_reintroducing_a_warning_emits_again() {
        let mut seen = Recent::default();
        seen.observe_source(1, 100);
        assert!(seen.admit(1, "legacy spelling"));
        seen.observe_source(1, 100);
        assert!(!seen.admit(1, "legacy spelling"));
        seen.observe_source(1, 200); // fixed config emits no warning
        seen.observe_source(1, 100); // reintroduced legacy config
        assert!(seen.admit(1, "legacy spelling"));
        for id in 2..MAX_DIAGNOSTICS as u64 + 10 {
            seen.observe_source(id, 0);
        }
        assert_eq!(seen.sources.len(), MAX_DIAGNOSTICS);
    }

    #[test]
    fn memory_is_bounded_and_evicted_diagnostics_can_recur() {
        let mut seen = Recent::default();
        for i in 0..MAX_DIAGNOSTICS + 10 {
            assert!(seen.admit(i as u64, "warning"));
        }
        assert_eq!(seen.diagnostics.len(), MAX_DIAGNOSTICS);
        assert!(seen.admit(0, "warning"));
        assert!(!seen.admit(0, "warning"));
    }

    #[test]
    fn source_context_is_nested_and_thread_local() {
        let original = SOURCE.get();
        let parent = source("config.toml", "base");
        let base = SOURCE.get();
        {
            let _overlay = source("profile.toml", "overlay");
            assert_ne!(SOURCE.get(), base);
        }
        assert_eq!(SOURCE.get(), base);
        assert_eq!(std::thread::spawn(|| SOURCE.get()).join().unwrap(), 0);
        drop(parent);
        assert_eq!(SOURCE.get(), original);
    }

    #[test]
    fn invalid_supplied_values_are_redacted_before_capture_or_emission() {
        let warning = safe_warning(
            "THEGN_ISSUES_LINEAR_API_KEY: not a valid value (\"supersecret\"); ignoring",
        );
        assert_eq!(
            warning,
            "THEGN_ISSUES_LINEAR_API_KEY: invalid supplied value (ignored)"
        );
        assert!(!warning.contains("supersecret"));
    }
}
