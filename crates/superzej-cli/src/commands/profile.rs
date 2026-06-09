//! `superzej profile` — view and manage performance profiles.
//!
//! This command displays the current profiling data collected via the
//! `SUPERZEJ_PROFILE` environment variable or the profiler's macros.

use crate::profiler::Profiler;
use anyhow::Result;

/// View performance profiles.
pub fn show(format: &str) -> Result<()> {
    if !Profiler::is_enabled() {
        crate::outln!(
            "\x1b[38;2;{}m⚠\x1b[0m Profiling is not enabled.\n\
            \n\
            Enable with: SUPERZEJ_PROFILE=1 superzej <command>\n\
            ",
            crate::theme::AMBER
        );
        return Ok(());
    }

    let report = Profiler::report(format);
    crate::out!("{report}");
    Ok(())
}

/// Clear profile data.
pub fn clear() -> Result<()> {
    Profiler::reset();
    crate::outln!(
        "\x1b[38;2;{}m✓\x1b[0m Profile data cleared",
        crate::theme::GREEN
    );
    Ok(())
}

/// Dump profile as JSON.
pub fn json() -> Result<()> {
    if !Profiler::is_enabled() {
        anyhow::bail!("profiling is not enabled");
    }
    let report = Profiler::report("json");
    println!("{report}");
    Ok(())
}
