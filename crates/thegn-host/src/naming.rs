//! Small, pure naming helpers shared by the loop and the CLI. Kept out of the
//! ratchet-pinned `run.rs` so the god-file stays lean.

/// Slugify an issue's `number` + `title` into a branch-name tail, or honour an
/// explicit `hint` when one is given. Lowercases, collapses runs of
/// non-alphanumerics to single dashes, trims leading/trailing dashes, and caps
/// the result at 48 chars.
pub(crate) fn issue_branch_tail(number: &str, title: &str, hint: Option<&str>) -> String {
    if let Some(h) = hint.filter(|h| !h.trim().is_empty()) {
        return h.trim().to_string();
    }
    let raw = format!("{number}-{title}");
    let mut out = String::new();
    let mut prev_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').chars().take(48).collect()
}
