//! Run with `cargo run -p thegn-job-spec --example schema -- job` (or `result`).

use std::io::{self, Write};

fn main() -> anyhow::Result<()> {
    let schema = match std::env::args().nth(1).as_deref() {
        Some("job") => thegn_job_spec::job_schema(),
        Some("result") => thegn_job_spec::result_schema(),
        _ => anyhow::bail!("expected job or result"),
    };
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, &schema)?;
    writeln!(out)?;
    Ok(())
}
