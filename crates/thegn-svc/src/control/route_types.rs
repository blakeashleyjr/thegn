//! Request/response type per control capability: the data source for the
//! route reference's Request/Response columns (THE-192).

/// (capability id, request, response). A cell is a wire type name that
/// exists in docs/api/control-v1.json (`types` or `definitions`), an
/// inline JSON shape starting with `{` (ad-hoc replies, or a Rust type the
/// wire snapshot does not pin), or `—` (none). For query-string routes the
/// request cell names the query type.
pub static ROUTE_TYPES: &[(&str, &str, &str)] = &[
    ("me", "—", "{pairing_id, label, scopes}"),
    ("sessions.list", "—", "{sessions:[SessionInfo]}"),
    ("sessions.open", "OpenSpec", "SessionInfo"),
    ("sessions.fork", "ForkSpec", "SessionInfo"),
    (
        "sessions.snapshot",
        "—",
        "{session, seq, cols, rows, ansi_b64}",
    ),
    ("sessions.input", "InputBody", "{sent}"),
    ("sessions.resize", "ResizeBody", "{resized}"),
    ("sessions.wait", "WaitBody", "WaitOutcome"),
    ("sessions.split", "SplitBody", "SessionInfo"),
    ("sessions.record", "RecordSpec", "RecordStatus"),
    ("sessions.detach", "DetachBody", "{detached}"),
    ("sessions.attach", "AttachQuery", "{stream}"),
    ("sessions.kill", "—", "{killed}"),
    ("events.subscribe", "EventsQuery", "{stream}"),
    (
        "leases.list",
        "—",
        "{leases:[{lease_id, session, kind, client, expires_at}]}",
    ),
    ("worktrees.list", "—", "{worktrees:[WorktreeInfo]}"),
    ("worktrees.create", "WorktreeCreateReq", "WorktreeInfo"),
    ("folders.assign", "FolderAssignReq", "{ok}"),
    (
        "worktrees.sandbox_profile",
        "SandboxProfileQuery",
        "SandboxProfileReply",
    ),
    (
        "worktrees.sandbox_profile_set",
        "SandboxProfileSetReq",
        "SandboxProfileReply",
    ),
    ("worktrees.open", "OpenWorktreeBody", "{opened}"),
    ("skills.list", "—", "SkillsList"),
    ("editor.open", "EditorOpenRequest", "{queued}"),
    (
        "issues.list",
        "IssuesQuery",
        "{issues, complete, incomplete}",
    ),
    ("issues.get", "IssueRepoQuery", "{IssueDetail}"),
    ("issues.update", "{IssuePatch}", "{Issue}"),
    ("issues.comment", "CommentBody", "{commented}"),
    (
        "dispatches.list",
        "DispatchesQuery",
        "{dispatches, truncated, next_before}",
    ),
    ("dispatches.put", "DispatchPutReq", "{AgentDispatch}"),
    (
        "dispatches.set_status",
        "DispatchStatusBody",
        "{id, status}",
    ),
    (
        "dispatches.report",
        "{text, token}",
        "{id, bytes, pass_without_gate_evidence}",
    ),
    (
        "dispatches.note",
        "{text, token}",
        "{id, note_id, created_at_ms}",
    ),
    ("preview.fetch", "PreviewFetchRequest", "PreviewFetchReply"),
    ("git.status", "WorktreeQuery", "{files:[GitFileStatus]}"),
    ("git.stage", "StageBody", "{staged}"),
    ("git.commit", "CommitBody", "{commit}"),
    ("merge.list", "WorktreeQuery", "{queue}"),
    ("merge.add", "MergeBody", "{queued, message}"),
    ("merge.clear", "MergeBody", "{cleared}"),
    ("pr.status", "—", "{prs:[PrStatusRow]}"),
    ("ci.runs", "{worktree, limit}", "CiRunsReply"),
    ("ci.logs", "{worktree, run, job, tail_lines}", "CiLogsReply"),
    (
        "agent.sessions",
        "AgentSessionsQuery",
        "{sessions:[SessionRecord]}",
    ),
    ("notify.push", "PushedNote", "{id}"),
    ("automations.list", "—", "{rules:[AutomationRuleInfo]}"),
    (
        "automations.test",
        "AutomationTestRequest",
        "AutomationTestReply",
    ),
    ("tools.run", "ToolRunRequest", "SessionInfo"),
    (
        "mcp_proxy.status",
        "—",
        "{enabled, daemon_owned, upstreams}",
    ),
    ("mcp_proxy.reload", "—", "{actions, tools_changed}"),
    ("plugins.status", "—", "PluginsStatusReply"),
    (
        "calendar.events",
        "CalendarQuery",
        "{events, skipped_rows, incomplete}",
    ),
    ("calendar.clocks", "—", "{clocks}"),
    ("calendar.ingest", "CalendarIngestBody", "{stored}"),
    ("pairings.list", "—", "{pairings}"),
    (
        "pairings.issue",
        "IssueBody",
        "{pairing_id, code, scopes, expires_at}",
    ),
    ("pairings.revoke", "—", "{revoked}"),
    ("pairings.approve", "—", "{approved}"),
    ("daemon.shutdown", "—", "{shutdown}"),
];

/// The `(request, response)` cells for a capability, if it has a row.
pub fn route_types_for(cap: &str) -> Option<(&'static str, &'static str)> {
    ROUTE_TYPES
        .iter()
        .find(|(c, _, _)| *c == cap)
        .map(|(_, req, resp)| (*req, *resp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::routes::API_CALLS;
    use std::collections::BTreeSet;

    fn wire_type_names() -> BTreeSet<String> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/control-v1.json");
        let text = std::fs::read_to_string(&path).expect("read control-v1.json");
        let doc: serde_json::Value = serde_json::from_str(&text).expect("parse control-v1.json");
        let mut names = BTreeSet::new();
        for key in ["types", "definitions"] {
            if let Some(map) = doc.get(key).and_then(|v| v.as_object()) {
                names.extend(map.keys().cloned());
            }
        }
        names
    }

    #[test]
    fn route_types_cover_every_api_call_exactly_once() {
        let mut have: Vec<&str> = ROUTE_TYPES.iter().map(|(c, _, _)| *c).collect();
        have.sort_unstable();
        let mut want: Vec<&str> = API_CALLS.iter().map(|(c, _, _)| *c).collect();
        want.sort_unstable();
        let missing: Vec<&&str> = want.iter().filter(|c| !have.contains(c)).collect();
        let extra: Vec<&&str> = have.iter().filter(|c| !want.contains(c)).collect();
        assert_eq!(
            have, want,
            "ROUTE_TYPES must have exactly one row per API_CALLS capability; missing {missing:?}, extra/duplicate {extra:?}"
        );
    }

    #[test]
    fn route_type_names_resolve_in_the_wire_snapshot() {
        let names = wire_type_names();
        for (cap, req, resp) in ROUTE_TYPES {
            for cell in [req, resp] {
                if *cell == "—" || cell.starts_with('{') {
                    continue;
                }
                assert!(
                    names.contains(*cell),
                    "{cap}: `{cell}` is not a type in docs/api/control-v1.json"
                );
            }
        }
    }

    #[test]
    fn route_type_inline_shapes_are_balanced() {
        for (cap, req, resp) in ROUTE_TYPES {
            for cell in [req, resp] {
                if !cell.starts_with('{') {
                    continue;
                }
                let count = |c: char| cell.chars().filter(|x| *x == c).count();
                assert_eq!(count('{'), count('}'), "{cap}: unbalanced braces in {cell}");
                assert_eq!(
                    count('['),
                    count(']'),
                    "{cap}: unbalanced brackets in {cell}"
                );
                assert!(
                    !cell.contains('`') && !cell.contains('|'),
                    "{cap}: {cell} would break a Markdown table cell"
                );
            }
        }
    }

    #[test]
    fn route_types_for_looks_up_by_capability() {
        assert!(matches!(
            route_types_for("git.commit"),
            Some(("CommitBody", _))
        ));
        assert_eq!(route_types_for("nope.nope"), None);
    }
}
