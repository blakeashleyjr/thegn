//! Rendering for templated session briefs.

use crate::agent_task::{TaskVars, bound_text, render_prompt};

const MAX_VAR_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBrief {
    pub text: String,
    pub rendered_bytes: usize,
    pub truncated: bool,
}

/// Parse one `key=value` binding, preserving every `=` after the first one in
/// the value.
pub fn parse_var(binding: &str) -> Result<(String, String), String> {
    let (key, value) = binding
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_string())?;
    if key.is_empty() {
        return Err("variable name is empty".to_string());
    }
    let mut chars = key.chars();
    let first = chars.next().expect("checked non-empty key");
    if !first.is_ascii_lowercase() {
        return Err(format!("invalid variable name `{key}`"));
    }
    if chars.any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')) {
        return Err(format!("invalid variable name `{key}`"));
    }
    Ok((key.to_string(), value.to_string()))
}

/// Render a brief using the supplied bindings.
///
/// An empty binding list deliberately leaves the template untouched: callers
/// use that mode for prose that may contain arbitrary literal braces. Once a
/// binding is present, the shared task renderer handles `{{`/`}}`, unknown
/// placeholders, and the aggregate output cap.
pub fn render_brief(template: &str, vars: &[(String, String)]) -> Result<RenderedBrief, String> {
    if vars.is_empty() {
        let text = non_empty(template.to_string())?;
        return Ok(RenderedBrief {
            rendered_bytes: text.len(),
            text,
            truncated: false,
        });
    }

    let mut task_vars = TaskVars::new();
    for (key, value) in vars {
        if task_vars.get(key).is_some() {
            return Err(format!("duplicate variable `{key}`"));
        }
        task_vars = task_vars.set(key, bound_text(value, MAX_VAR_BYTES));
    }
    let rendered = render_prompt(template, &task_vars).map_err(|error| error.to_string())?;
    let rendered_bytes = rendered.len();
    let truncated = rendered_bytes > crate::agent_task::MAX_RENDERED_PROMPT_BYTES;
    let text = non_empty(bound_text(
        &rendered,
        crate::agent_task::MAX_RENDERED_PROMPT_BYTES,
    ))?;
    Ok(RenderedBrief {
        text,
        rendered_bytes,
        truncated,
    })
}

fn non_empty(text: String) -> Result<String, String> {
    if text.trim().is_empty() {
        Err("rendered brief is empty".to_string())
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_var, render_brief};

    #[test]
    fn no_vars_keeps_braces_verbatim() {
        assert_eq!(
            render_brief("Rust {code}", &[]).unwrap().text,
            "Rust {code}"
        );
    }

    #[test]
    fn substitutes_vars_and_preserves_equals_in_values() {
        let vars = vec![("name".into(), "a=b".into())];
        assert_eq!(
            render_brief("hello {name}", &vars).unwrap().text,
            "hello a=b"
        );
    }

    #[test]
    fn double_braces_escape_literal_braces() {
        let vars = vec![("name".into(), "world".into())];
        assert_eq!(
            render_brief("{{name}} {name}", &vars).unwrap().text,
            "{name} world"
        );
    }

    #[test]
    fn unknown_var_lists_bound_keys() {
        let vars = vec![
            ("name".into(), "world".into()),
            ("other".into(), "x".into()),
        ];
        let error = render_brief("{missing}", &vars).unwrap_err();
        assert!(error.contains("{missing}"), "{error}");
        assert!(error.contains("{name}"), "{error}");
        assert!(error.contains("{other}"), "{error}");
    }

    #[test]
    fn duplicate_var_is_rejected() {
        let vars = vec![("name".into(), "one".into()), ("name".into(), "two".into())];
        assert!(
            render_brief("{name}", &vars)
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn parse_var_rejects_missing_equals_empty_and_bad_keys() {
        assert!(parse_var("name").is_err());
        assert!(parse_var("=value").is_err());
        assert!(parse_var("Name=value").is_err());
        assert!(parse_var("name-value").is_err());
        assert!(parse_var("name_2=value").is_ok());
    }

    #[test]
    fn values_are_bounded_before_rendering() {
        let vars = vec![("value".into(), "x".repeat(32 * 1024))];
        let rendered = render_brief("{value}", &vars).unwrap();
        assert!(rendered.text.len() <= 16 * 1024);
        assert!(rendered.text.contains("omitted"));
    }

    #[test]
    fn rendered_brief_has_the_aggregate_prompt_cap() {
        let vars = vec![("value".into(), "x".repeat(16 * 1024))];
        let rendered = render_brief("{value}{value}{value}{value}{value}", &vars).unwrap();
        assert!(rendered.truncated);
        assert_eq!(rendered.rendered_bytes, 5 * 16 * 1024);
        assert!(rendered.text.len() <= crate::agent_task::MAX_RENDERED_PROMPT_BYTES);
        assert!(rendered.text.contains("omitted"));
    }

    #[test]
    fn malformed_template_is_rejected() {
        let vars = vec![("value".into(), "x".into())];
        assert!(
            render_brief("{value", &vars)
                .unwrap_err()
                .contains("unterminated")
        );
        assert!(
            render_brief("{}", &vars)
                .unwrap_err()
                .contains("empty placeholder")
        );
    }

    #[test]
    fn empty_result_is_rejected() {
        assert!(render_brief(" ", &[]).unwrap_err().contains("empty"));
        let vars = vec![("value".into(), "".into())];
        assert!(
            render_brief("{value}", &vars)
                .unwrap_err()
                .contains("empty")
        );
    }
}
