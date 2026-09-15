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

pub(crate) fn builtin_identity<'a>(value: &'a str) -> Result<&'a str, String> {
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
pub(crate) fn plugin_key<'a>(value: &'a str) -> Result<&'a str, String> {
    bounded(value, PLUGIN_NATIVE_MAX, "plugin issue key").map(|_| value)
}

pub(crate) fn github_number<'a>(number: &'a str) -> Result<&'a str, String> {
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

pub(crate) fn github_repo<'a>(repo: &'a str) -> Result<&'a str, String> {
    bounded(repo, BUILTIN_ID_MAX, "GitHub repository")?;
    let mut parts = repo.split('/');
    let owner = parts.next().ok_or("GitHub repository needs owner/repo")?;
    let name = parts.next().ok_or("GitHub repository needs owner/repo")?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return Err("GitHub repository must be exactly owner/repo".into());
    }
    github_component(owner, "GitHub owner")?;
    github_component(name, "GitHub repository name")?;
    Ok(repo)
}

fn github_component<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    bounded(value, GITHUB_COMPONENT_MAX, label)?;
    let mut bytes = value.bytes();
    if !bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
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
    bounded(value, CONTROL_ENVELOPE_MAX, "control issue identity")?;
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
        assert!(github_repo("owner/repo name").is_err());
        assert!(github_repo("../repo").is_err());
        assert!(jira_project("PROJ_2").is_ok());
        assert!(jira_project("2PROJ").is_err());
    }

    #[test]
    fn plugin_keys_keep_opaque_utf8_but_reject_controls() {
        assert!(plugin_key("客户/任务#7").is_ok());
        assert!(plugin_key("bad\nkey").is_err());
        assert!(encode_control_segment("plugin:demo/客户#7").is_ok());
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
