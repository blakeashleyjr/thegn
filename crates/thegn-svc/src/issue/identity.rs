//! Bounded, provider-aware tracker identities.
//!
//! These checks are deliberately syntax checks.  Account selection and
//! generation/authority binding belong to THE-324; this module only prevents
//! an untrusted value from becoming a path segment, a CLI option, or an
//! opaque plugin control value.

const BUILTIN_SEGMENT_MAX: usize = 128;
const BUILTIN_ID_MAX: usize = 256;
const PLUGIN_KEY_MAX: usize = 512;

fn bounded(value: &str, max: usize, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.len() > max {
        return Err(format!("{label} exceeds {max} bytes"));
    }
    if value.chars().any(|c| c.is_control() || c == '\0') {
        return Err(format!("{label} contains a control character"));
    }
    Ok(())
}

/// Validate one built-in provider path/option segment.
pub(crate) fn builtin_segment(value: &str, label: &str) -> Result<&str, String> {
    bounded(value, BUILTIN_SEGMENT_MAX, label)?;
    if value
        .chars()
        .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%'))
    {
        return Err(format!("{label} contains a reserved path character"));
    }
    if value.starts_with('-') {
        return Err(format!("{label} must not look like a CLI option"));
    }
    Ok(value)
}

pub(crate) fn builtin_identity(value: &str) -> Result<&str, String> {
    bounded(value, BUILTIN_ID_MAX, "tracker identity")
}

/// Plugin business keys are opaque UTF-8.  They are bounded and cannot carry
/// controls, while provider-specific path restrictions remain the plugin's
/// responsibility.  The control transport encodes the complete envelope once.
pub(crate) fn plugin_key(value: &str) -> Result<&str, String> {
    bounded(value, PLUGIN_KEY_MAX, "plugin issue key")
}

pub(crate) fn github_number(number: &str) -> Result<&str, String> {
    builtin_segment(number, "GitHub issue number")?;
    if number.len() > 20 || !number.bytes().all(|b| b.is_ascii_digit()) || number == "0" {
        return Err("GitHub issue number must be a positive decimal integer".into());
    }
    Ok(number)
}

pub(crate) fn github_repo(repo: &str) -> Result<&str, String> {
    bounded(repo, BUILTIN_ID_MAX, "GitHub repository")?;
    let mut parts = repo.split('/');
    let owner = parts.next().ok_or("GitHub repository needs owner/repo")?;
    let name = parts.next().ok_or("GitHub repository needs owner/repo")?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return Err("GitHub repository must be exactly owner/repo".into());
    }
    builtin_segment(owner, "GitHub owner")?;
    builtin_segment(name, "GitHub repository name")?;
    Ok(repo)
}

pub(crate) fn jira_key(key: &str) -> Result<&str, String> {
    builtin_segment(key, "Jira issue key")?;
    if key.len() <= 20 && key.bytes().all(|b| b.is_ascii_digit()) && key != "0" {
        return Ok(key);
    }
    let Some((project, number)) = key.split_once('-') else {
        return Err("Jira issue key must be PROJECT-number".into());
    };
    if project.is_empty()
        || !project
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
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
    if !project
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("Jira project key must contain only uppercase ASCII, digits, or _".into());
    }
    Ok(project)
}

pub(crate) fn kaneo_id(id: &str, label: &str) -> Result<&str, String> {
    // Kaneo's server model uses CUIDs today, while existing fixtures and
    // installations also expose UUIDs and short slugs.  Keep it opaque.
    builtin_segment(id, label)
}

/// Percent encode a complete control identity as one path segment.  Callers
/// must not split or decode this value before the server's single path decode.
pub(crate) fn encode_control_segment(value: &str) -> Result<String, String> {
    bounded(value, PLUGIN_KEY_MAX, "control issue identity")?;
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
        assert!(github_number("42").is_ok());
        assert!(github_number("0").is_err());
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
