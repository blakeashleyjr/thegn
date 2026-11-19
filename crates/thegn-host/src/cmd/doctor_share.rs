use thegn_core::outln;

pub(crate) fn json() -> serde_json::Value {
    match thegn_svc::vpn::pending_teardown_rows() {
        Ok((rows, errors)) => serde_json::json!({
            "pending": rows.into_iter().map(|row| serde_json::json!({
                "container": thegn_svc::lsp::sanitize_for_terminal(&row.container),
                "worktree": row.worktree.map(|worktree| {
                    thegn_svc::lsp::sanitize_for_terminal(&worktree)
                }),
                "ports": row.ports,
            })).collect::<Vec<_>>(),
            "errors": errors.into_iter().map(|error| {
                thegn_svc::lsp::sanitize_for_terminal(&error)
            }).collect::<Vec<_>>(),
        }),
        Err(error) => serde_json::json!({
            "error": thegn_svc::lsp::sanitize_for_terminal(&format!("{error:#}")),
        }),
    }
}

pub(crate) fn report() {
    match thegn_svc::vpn::pending_teardown_rows() {
        Ok((rows, errors)) => {
            for line in lines(&rows, &errors) {
                outln!("{line}");
            }
        }
        Err(error) => outln!(
            "  share teardowns  error: {}",
            thegn_svc::lsp::sanitize_for_terminal(&format!("{error:#}"))
        ),
    }
}

pub(crate) fn lines(rows: &[thegn_svc::vpn::PendingTeardownRow], errors: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    for row in rows {
        let target = row.worktree.as_deref().unwrap_or(&row.container);
        for port in &row.ports {
            lines.push(thegn_svc::lsp::sanitize_for_terminal(&format!(
                "  share teardown  {target}: tailscale serve port {port} still pending; retry with thegn share stop {port} --worktree {target}"
            )));
        }
    }
    for error in errors {
        lines.push(thegn_svc::lsp::sanitize_for_terminal(&format!(
            "  share teardown  error: {error}"
        )));
    }
    if lines.is_empty() {
        lines.push(thegn_svc::lsp::sanitize_for_terminal(
            "  share teardowns  none pending",
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_lines_say_none_pending() {
        assert_eq!(lines(&[], &[]), vec!["  share teardowns  none pending"]);
    }

    #[test]
    fn share_lines_name_port_and_retry_verb() {
        let rows = [thegn_svc::vpn::PendingTeardownRow {
            container: "c-vpn".into(),
            worktree: Some("/work/tree".into()),
            ports: vec![443],
        }];

        assert_eq!(
            lines(&rows, &[]),
            vec![
                "  share teardown  /work/tree: tailscale serve port 443 still pending; retry with thegn share stop 443 --worktree /work/tree"
            ]
        );
    }

    #[test]
    fn share_lines_sanitize_control_characters() {
        let rows = [thegn_svc::vpn::PendingTeardownRow {
            container: "c\u{1b}]0;evil\u{7}".into(),
            worktree: None,
            ports: vec![80],
        }];

        let output = lines(&rows, &[]).join("\n");
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{7}'));
        assert!(output.contains("c"));
    }
}
