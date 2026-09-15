//! Bounded, provider-aware tracker identities.
//!
//! These checks are deliberately syntax checks.  Account selection and
//! generation/authority binding belong to THE-324; this module only prevents
//! an untrusted value from becoming a path segment, a CLI option, or an
//! opaque plugin control value.

const BUILTIN_SEGMENT_MAX: usize = 128;
const BUILTIN_ID_MAX: usize = 256;
const PLUGIN_NATIVE_MAX: usize = 384;
const CONTROL_ENVELOPE_MAX: usize = 512;
const GITHUB_COMPONENT_MAX: usize = 100;

pub(crate) fn complete_identity(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("control issue identity must not be empty".into());
    }
    if value.len() > CONTROL_ENVELOPE_MAX {
        return Err(format!(
            "control issue identity exceeds {CONTROL_ENVELOPE_MAX} bytes"
        ));
    }
    if value.chars().any(|c| c.is_control() || c == '\0') {
        return Err("control issue identity contains a control character".into());
    }
    Ok(())
}

fn bounded(value: &str, max: usize, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.len() > max {
        return Err(format!("{label} exceeds {max} bytes"));
    }
    if value
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\0')
    {
        return Err(format!(
            "{label} contains whitespace or a control character"
        ));
    }
    Ok(())
}

/// Validate one built-in provider path/option segment.
pub(crate) fn builtin_segment<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    bounded(value, BUILTIN_SEGMENT_MAX, label)?;
    if value == "."
        || value == ".."
        || value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%' | '&' | '='))
    {
        return Err(format!("{label} contains a reserved path/query character"));
    }
    if value.starts_with('-') {
        return Err(format!("{label} must not look like a CLI option"));
    }
    Ok(value)
}

/// Validate a plugin's registered namespace. Plugin business keys are opaque
/// and may contain `:`, but the namespace itself is a routing delimiter and
/// must be one flat, non-delimiter segment.
pub(crate) fn plugin_namespace(value: &str) -> Result<&str, String> {
    builtin_segment(value, "plugin namespace")?;
    if value.contains(':') {
        return Err("plugin namespace must not contain ':'".into());
    }
    Ok(value)
}

pub(crate) fn builtin_identity(value: &str) -> Result<&str, String> {
    bounded(value, BUILTIN_ID_MAX, "tracker identity")?;
    if value == "."
        || value == ".."
        || value.starts_with('-')
        || value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%' | '&' | '='))
    {
        return Err("tracker identity contains a reserved path/query character".into());
    }
    Ok(value)
}

/// Plugin business keys are opaque UTF-8.  They are bounded and cannot carry
/// controls, while provider-specific path restrictions remain the plugin's
/// responsibility.  The control transport encodes the complete envelope once.
pub(crate) fn plugin_key(value: &str) -> Result<&str, String> {
    if value.is_empty() {
        return Err("plugin issue key must not be empty".into());
    }
    if value.len() > PLUGIN_NATIVE_MAX {
        return Err(format!(
            "plugin issue key exceeds {PLUGIN_NATIVE_MAX} bytes"
        ));
    }
    if value.chars().any(|c| c.is_control() || c == '\0') {
        return Err("plugin issue key contains a control character".into());
    }
    Ok(value)
}

pub(crate) fn github_number(number: &str) -> Result<&str, String> {
    bounded(number, 20, "GitHub issue number")?;
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err("GitHub issue number must contain only ASCII digits".into());
    }
    let parsed = number
        .parse::<u64>()
        .map_err(|_| "GitHub issue number exceeds u64".to_string())?;
    if parsed == 0 || (number.len() > 1 && number.starts_with('0')) {
        return Err("GitHub issue number must be a canonical positive decimal".into());
    }
    Ok(number)
}

pub(crate) fn github_repo(repo: &str) -> Result<&str, String> {
    bounded(repo, BUILTIN_ID_MAX, "GitHub repository")?;
    if repo.starts_with('-') {
        return Err("GitHub repository must not look like a CLI option".into());
    }
    let parts: Vec<&str> = repo.split('/').collect();
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => (None, *owner, *name),
        [host, owner, name] => (Some(*host), *owner, *name),
        _ => return Err("GitHub repository must be owner/repo or HOST/owner/repo".into()),
    };
    if let Some(host) = host {
        github_host_for_url(host)?;
    }
    github_component(owner, "GitHub owner")?;
    github_component(name, "GitHub repository name")?;
    Ok(repo)
}

pub(crate) fn github_host_for_url(value: &str) -> Result<&str, String> {
    bounded(value, GITHUB_COMPONENT_MAX, "GitHub host")?;
    let bytes = value.as_bytes();
    if !bytes.first().is_some_and(|b| b.is_ascii_alphanumeric())
        || !bytes.last().is_some_and(|b| b.is_ascii_alphanumeric())
        || value.contains("..")
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'.' | b'-'))
    {
        return Err("GitHub host must use bounded ASCII hostname grammar".into());
    }
    Ok(value)
}

fn github_component<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    bounded(value, GITHUB_COMPONENT_MAX, label)?;
    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        || matches!(value, "." | "..")
    {
        return Err(format!("{label} must use bounded ASCII repository grammar"));
    }
    Ok(value)
}

pub(crate) fn jira_key(key: &str) -> Result<&str, String> {
    builtin_segment(key, "Jira issue key")?;
    if key.len() <= 20 && key.bytes().all(|b| b.is_ascii_digit()) && key != "0" {
        return Ok(key);
    }
    let Some((project, number)) = key.split_once('-') else {
        return Err("Jira issue key must be PROJECT-number".into());
    };
    if jira_project(project).is_err()
        || number.is_empty()
        || number.len() > 20
        || !number.bytes().all(|b| b.is_ascii_digit())
        || number == "0"
    {
        return Err("Jira issue key must match PROJECT-[1-9][0-9]{0,19}".into());
    }
    Ok(key)
}

pub(crate) fn jira_project(project: &str) -> Result<&str, String> {
    builtin_segment(project, "Jira project key")?;
    let mut bytes = project.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        || !bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("Jira project key must match [A-Z][A-Z0-9_]*".into());
    }
    Ok(project)
}

pub(crate) fn kaneo_id<'a>(id: &'a str, label: &str) -> Result<&'a str, String> {
    // Kaneo's server model uses CUIDs today, while existing fixtures and
    // installations also expose UUIDs and short slugs.  Keep it opaque.
    builtin_segment(id, label)
}

/// Percent encode a complete control identity as one path segment.  Callers
/// must not split or decode this value before the server's single path decode.
pub(crate) fn encode_control_segment(value: &str) -> Result<String, String> {
    complete_identity(value)?;
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_reject_path_and_option_injection() {
        assert!(builtin_segment("owner/repo", "x").is_err());
        assert!(builtin_segment("--repo", "x").is_err());
        assert!(builtin_segment("a&b", "x").is_err());
        assert!(builtin_segment("..", "x").is_err());
        assert!(builtin_segment("white space", "x").is_err());
        assert!(github_number("42").is_ok());
        assert!(github_number("0").is_err());
        assert!(github_number("00").is_err());
        assert!(github_number("18446744073709551616").is_err());
    }

    #[test]
    fn builtin_grammar_keeps_ascii_repository_and_jira_rules_bounded() {
        assert!(github_repo("owner/repo-name_1.2").is_ok());
        assert!(github_repo("ghe.example/owner/repo").is_ok());
        assert!(github_repo("owner/.github").is_ok());
        assert!(github_repo("owner/repo name").is_err());
        assert!(github_repo("../repo").is_err());
        assert!(github_repo("--help/repo").is_err());
        assert!(github_repo("owner/-repository").is_ok());
        assert!(github_host_for_url("ghe.example").is_ok());
        assert!(github_host_for_url(".ghe.example").is_err());
        assert!(jira_project("PROJ_2").is_ok());
        assert!(jira_project("2PROJ").is_err());
    }

    #[test]
    fn plugin_keys_keep_opaque_utf8_but_reject_controls() {
        assert!(plugin_key("客户/任务#7").is_ok());
        assert!(plugin_key("opaque key\tless").is_err());
        assert!(plugin_key("opaque key with spaces").is_ok());
        assert!(plugin_key("bad\nkey").is_err());
        assert!(encode_control_segment("plugin:demo/客户#7").is_ok());
    }

    #[test]
    fn complete_control_identity_bounds_the_whole_envelope() {
        assert!(complete_identity(&"x".repeat(CONTROL_ENVELOPE_MAX)).is_ok());
        assert!(complete_identity(&"x".repeat(CONTROL_ENVELOPE_MAX + 1)).is_err());
        assert!(complete_identity("plugin:demo:").is_ok());
        assert!(complete_identity("plugin:demo:\0").is_err());
    }

    #[test]
    fn native_jira_and_kaneo_compatibility_examples() {
        assert!(jira_key("PROJ-7").is_ok());
        assert!(jira_key("10001").is_ok());
        assert!(jira_key("P-0").is_err());
        for id in ["p1", "ws-1", "bare", "550e8400-e29b-41d4-a716-446655440000"] {
            assert!(kaneo_id(id, "task").is_ok());
        }
    }
}
