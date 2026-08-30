//! `thegn doctor bundle` — write a single redacted support archive.
//!
//! Contents: the extended `doctor --json`, the effective config with secret
//! values redacted, bounded tails of every log sink (compositor, daemon, stderr
//! capture, audit), the current-process WARN ring, and all retained crash
//! reports — plus a printed `MANIFEST`
//! of exactly what was included, so the user can see what they are about to
//! share before they share it.
//!
//! The archive is a hand-rolled ustar tar wrapped in gzip (`flate2`): no new
//! vendored crate, cross-compilable. Everything it writes is local; the verb is
//! catalog-gated (`doctor.bundle`, admin scope, operator surface — never MCP or
//! plugins), so an agent in a pane cannot exfiltrate a bundle through thegn's
//! own API.

#![allow(clippy::disallowed_macros)] // a CLI verb: println! is the user surface

use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::PathBuf;
use thegn_core::config::Config;

/// Default lines of each log sink to include.
const TAIL_LINES: usize = 500;

/// `thegn doctor bundle` arguments.
#[derive(clap::Args, Clone)]
pub struct BundleArgs {
    /// Write the archive here (default: `thegn-bundle-<ts>.tar.gz` in the cwd).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(cfg: &Config, args: BundleArgs) -> Result<()> {
    let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("thegn-bundle-{ts}.tar.gz")));

    let mut tar = TarBuilder::new();
    let mut manifest = String::from("thegn debug bundle\n===================\n");
    let add = |tar: &mut TarBuilder, manifest: &mut String, name: &str, data: Vec<u8>| {
        manifest.push_str(&format!("  {:<28} {} bytes\n", name, data.len()));
        tar.append(name, &data);
    };

    // 1. doctor.json (extended, with the identification block).
    let doctor = serde_json::to_vec_pretty(&crate::cmd::doctor::doctor_json(cfg))
        .unwrap_or_else(|_| b"{}".to_vec());
    add(&mut tar, &mut manifest, "doctor.json", doctor);

    // 2. config.redacted.toml — the effective config with secret values masked.
    let redacted_config = redacted_config_toml(cfg);
    add(
        &mut tar,
        &mut manifest,
        "config.redacted.toml",
        redacted_config.into_bytes(),
    );

    // 3. bounded, redacted tails of every log sink.
    for sink in crate::cmd::doctor::log_sinks(cfg) {
        if sink.size.is_none() {
            continue; // absent — skip
        }
        let tail = redacted_tail(&sink.path, TAIL_LINES);
        add(
            &mut tar,
            &mut manifest,
            &format!("logs/{}", sink.name),
            tail.into_bytes(),
        );
    }

    // 4. The WARN ring from this bundle process. This is not a live snapshot
    // of another host or daemon process.
    let ring = current_process_ring_log(&thegn_core::diagnostics::ring_snapshot());
    add(
        &mut tar,
        &mut manifest,
        "diagnostics/ring.log",
        ring.into_bytes(),
    );

    // 5. all retained crash reports. Re-redact historical files in case they
    // predate the serialization boundary or were written by another source.
    for report in thegn_core::diagnostics::list_reports() {
        append_retained_report(&mut tar, &mut manifest, &report, &add);
    }

    // 6. the manifest itself (also printed below).
    manifest.push_str(&format!("\nwritten: {}\n", out.display()));
    tar.append("MANIFEST", manifest.as_bytes());

    // gzip the tar and write it out.
    let gz = gzip(&tar.finish()).context("gzip bundle")?;
    std::fs::write(&out, gz).with_context(|| format!("write {}", out.display()))?;

    // Print the manifest so the user sees exactly what they are about to share.
    print!("{manifest}");
    println!("Wrote {}", out.display());
    Ok(())
}

/// The effective config serialized to TOML with secret scalar values replaced by
/// the redaction placeholder (keys matched by the log/diagnostics sensitive-key
/// predicate — the sibling of the MCP config redactor).
fn redacted_config_toml(cfg: &Config) -> String {
    match toml::Value::try_from(cfg) {
        Ok(mut v) => {
            redact_toml(&mut v);
            toml::to_string_pretty(&v)
                .unwrap_or_else(|_| "# (config serialization failed)\n".into())
        }
        Err(e) => format!("# (config serialization failed: {e})\n"),
    }
}

/// Recursively replace secret scalar values in a `toml::Value` tree.
fn redact_toml(v: &mut toml::Value) {
    match v {
        toml::Value::Table(t) => {
            for (k, val) in t.iter_mut() {
                if thegn_core::log_redact::is_sensitive_key(k)
                    && !matches!(val, toml::Value::Table(_) | toml::Value::Array(_))
                {
                    *val = toml::Value::String(thegn_core::log_redact::REDACTED.to_string());
                } else {
                    redact_toml(val);
                }
            }
        }
        toml::Value::Array(a) => a.iter_mut().for_each(redact_toml),
        _ => {}
    }
}

/// Read the last `n` lines of a log file, running each through the text redactor
/// as a belt-and-braces pass (lines are already redacted at emit time). Missing
/// file ⇒ empty.
fn redacted_tail(path: &std::path::Path, n: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .map(|line| thegn_core::log_redact::redact_text_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the WARN ring captured by this bundle process. A bundle command is a
/// separate process from a host or daemon, so this must never imply that it is
/// a live cross-process snapshot.
fn current_process_ring_log(lines: &[String]) -> String {
    let mut out = String::from(
        "thegn current-process WARN ring\n================================\nThis is the ring from the current bundle process; it does not include a separate host or daemon process.\n",
    );
    if lines.is_empty() {
        out.push_str("(none)\n");
    } else {
        for line in lines {
            out.push_str(&thegn_core::log_redact::redact_text_line(line));
            out.push('\n');
        }
    }
    out
}

/// Sanitize a retained crash report before copying it into a bundle. Invalid
/// UTF-8 is replaced rather than copied verbatim: crash reports are text and
/// no report bytes should bypass the final redaction boundary.
fn redacted_report_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_inclusive('\n')
        .map(thegn_core::log_redact::redact_text_line)
        .collect()
}

/// Read a retained report only while it is still a regular file. The metadata
/// check closes the symlink inclusion boundary at the bundle read site too,
/// covering a report replaced between enumeration and reading.
fn read_retained_report(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "retained report is not a regular file",
        ));
    }
    std::fs::read(path)
}

fn append_retained_report(
    tar: &mut TarBuilder,
    manifest: &mut String,
    report: &std::path::Path,
    add: &impl Fn(&mut TarBuilder, &mut String, &str, Vec<u8>),
) {
    let name = report
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "report.txt".into());
    match read_retained_report(report) {
        Ok(body) => {
            let body = redacted_report_text(&body);
            add(tar, manifest, &format!("crash/{name}"), body.into_bytes());
        }
        Err(e) => manifest.push_str(&format!("  crash/{name:<28} omitted: {e}\n")),
    }
}

/// gzip `data` with flate2 (pure-Rust miniz_oxide backend).
fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data)?;
    enc.finish()
}

/// A minimal ustar tar writer — enough for regular files with short names.
struct TarBuilder {
    out: Vec<u8>,
}

impl TarBuilder {
    fn new() -> Self {
        TarBuilder { out: Vec::new() }
    }

    /// Append one regular file entry (`name`, `data`).
    fn append(&mut self, name: &str, data: &[u8]) {
        let mut header = [0u8; 512];
        let nb = name.as_bytes();
        let nlen = nb.len().min(100);
        header[..nlen].copy_from_slice(&nb[..nlen]);
        octal(&mut header[100..108], 0o644); // mode
        octal(&mut header[108..116], 0); // uid
        octal(&mut header[116..124], 0); // gid
        octal(&mut header[124..136], data.len() as u64); // size
        octal(&mut header[136..148], mtime()); // mtime
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        // Checksum: sum every header byte with the chksum field taken as spaces.
        for b in &mut header[148..156] {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        // 6 octal digits, a NUL, then a space (the traditional ustar form).
        let cs = format!("{sum:06o}");
        header[148..154].copy_from_slice(cs.as_bytes());
        header[154] = 0;
        header[155] = b' ';

        self.out.extend_from_slice(&header);
        self.out.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.out.extend(std::iter::repeat_n(0u8, pad));
    }

    /// Finalize: two zero blocks terminate the archive.
    fn finish(mut self) -> Vec<u8> {
        self.out.extend(std::iter::repeat_n(0u8, 1024));
        self.out
    }
}

/// Write `val` as a NUL-terminated octal string right-justified in `field`.
fn octal(field: &mut [u8], val: u64) {
    let digits = field.len() - 1;
    let s = format!("{val:0digits$o}");
    let bytes = s.as_bytes();
    // If it overflows (won't for our sizes), keep the low digits.
    let take = bytes.len().min(digits);
    field[..take].copy_from_slice(&bytes[bytes.len() - take..]);
    field[digits] = 0;
}

fn mtime() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_entry_has_valid_header_and_padding() {
        let mut t = TarBuilder::new();
        t.append("hello.txt", b"hi");
        let bytes = t.finish();
        // header(512) + data padded to 512 + 2 zero blocks(1024) = 2048.
        assert_eq!(bytes.len(), 512 + 512 + 1024);
        // Name at offset 0.
        assert!(bytes[..9] == *b"hello.txt");
        // ustar magic.
        assert_eq!(&bytes[257..262], b"ustar");
        // Checksum field parses as octal and matches a recomputation.
        let stored = std::str::from_utf8(&bytes[148..154]).unwrap();
        let stored = u32::from_str_radix(stored.trim(), 8).unwrap();
        let mut hdr = [0u8; 512];
        hdr.copy_from_slice(&bytes[..512]);
        for b in &mut hdr[148..156] {
            *b = b' ';
        }
        let sum: u32 = hdr.iter().map(|&b| b as u32).sum();
        assert_eq!(stored, sum);
    }

    #[test]
    fn gzip_roundtrips_via_magic() {
        let gz = gzip(b"payload").unwrap();
        // gzip magic bytes.
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
    }

    #[test]
    fn redact_toml_masks_secret_scalars() {
        let mut v: toml::Value =
            toml::from_str("token = \"sk-secret\"\n[nested]\napi_key = \"k\"\nname = \"ok\"\n")
                .unwrap();
        redact_toml(&mut v);
        let s = toml::to_string(&v).unwrap();
        assert!(s.contains("***redacted***"));
        assert!(!s.contains("sk-secret"));
        assert!(!s.contains("\"k\""));
        assert!(s.contains("ok")); // non-secret survives
    }

    #[test]
    fn current_process_ring_entry_is_explicit_for_empty_and_non_empty_rings() {
        let empty = current_process_ring_log(&[]);
        assert!(empty.contains("current-process WARN ring"));
        assert!(empty.contains("(none)"));

        let non_empty = current_process_ring_log(&["WARN --token ring-secret safe".into()]);
        assert!(non_empty.contains("current-process WARN ring"));
        assert!(non_empty.contains("--token ***redacted*** safe"));
        assert!(!non_empty.contains("ring-secret"));
    }

    #[test]
    fn historical_crash_report_is_redacted_before_bundle_copy() {
        let old = b"panic: --token old-secret\nTOKEN=older-secret safe\n";
        let sanitized = redacted_report_text(old);
        assert!(!sanitized.contains("old-secret"));
        assert!(!sanitized.contains("older-secret"));
        assert!(sanitized.contains("safe"));
    }

    #[test]
    fn retained_report_read_failure_is_available_to_manifest() {
        let path = std::env::temp_dir().join(format!(
            "tg-missing-report-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut tar = TarBuilder::new();
        let mut manifest = String::new();
        let add = |tar: &mut TarBuilder, manifest: &mut String, name: &str, data: Vec<u8>| {
            manifest.push_str(&format!("  {name} {} bytes\n", data.len()));
            tar.append(name, &data);
        };
        append_retained_report(&mut tar, &mut manifest, &path, &add);
        assert!(manifest.contains("crash/"));
        assert!(manifest.contains("omitted: "));
        assert_eq!(
            tar.finish().len(),
            1024,
            "an omitted report is not archived"
        );
    }
}
