//! `DocsRouter` — a read-only, Context7-style MCP surface that lets an external
//! coding agent *understand thegn*: search and read the in-app help corpus, see
//! the generated keybindings / config-reference pages, inspect the user's
//! current (secret-redacted) config, and trace how a config key resolves.
//!
//! Read-only and DB-free by design, so it is safe to hand to any agent. It
//! speaks the JSON-RPC envelope in [`super::protocol`], and the host drives it
//! over stdio (`thegn mcp serve`).
//!
//! Purity: everything the router needs is injected by the host — the built
//! [`HelpRegistry`], the already-serialized-and-redacted config `Value`, the
//! config JSON schema, extra long-form docs, a fuzzy `ranker`, and an `explain`
//! closure (so config-file I/O stays in the host, not core). That keeps the
//! router unit-testable against fixtures.

use crate::help::registry::HelpRegistry;
use crate::help::search::{self, Ranker};
use crate::mcp::protocol::{JsonRpcRequest, JsonRpcResponse};
use serde_json::{Value, json};

/// A long-form document served as an MCP resource (README, CLI grammar, …).
/// `id` is the `thegn://doc/<id>` slug; `body` is markdown.
pub struct DocResource {
    pub id: String,
    pub title: String,
    pub body: String,
}

/// Resolve `explain_config`: given a dotted key and an optional repo path,
/// render the layer-resolution trace. Injected by the host so the config-file
/// read (in `config_resolve::explain`) stays out of core.
pub type ExplainFn<'a> = dyn Fn(&str, Option<&str>) -> String + 'a;

/// Read-only docs/help/config MCP router. Borrows the registry + ranker for the
/// life of one `thegn mcp serve` process; owns the config/schema/doc payloads.
pub struct DocsRouter<'a> {
    reg: &'a HelpRegistry,
    /// Resolved config as JSON, **already redacted** by the host.
    config: Value,
    /// The config's JSON schema (schemars) — valid keys, defaults, enums.
    schema: Value,
    /// Extra long-form docs exposed as `thegn://doc/<id>` resources.
    docs: Vec<DocResource>,
    ranker: &'a Ranker,
    explain: Box<ExplainFn<'a>>,
}

impl<'a> DocsRouter<'a> {
    pub fn new(
        reg: &'a HelpRegistry,
        config: Value,
        schema: Value,
        docs: Vec<DocResource>,
        ranker: &'a Ranker,
        explain: impl Fn(&str, Option<&str>) -> String + 'a,
    ) -> Self {
        Self {
            reg,
            config,
            schema,
            docs,
            ranker,
            explain: Box::new(explain),
        }
    }

    /// Handle one JSON-RPC request and return the response value.
    pub fn handle(&self, req_raw: &Value) -> Value {
        let req: JsonRpcRequest = match serde_json::from_value(req_raw.clone()) {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_value(JsonRpcResponse::error(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                ))
                .unwrap();
            }
        };
        let id = req.id.clone().unwrap_or(Value::Null);
        let result = match req.method.as_str() {
            "initialize" => Ok(self.initialize()),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => self.tools_call(&req.params),
            "resources/list" => Ok(self.resources_list()),
            "resources/read" => self.resources_read(&req.params),
            _ => Err((-32601, "Method not found".to_string())),
        };
        match result {
            Ok(res) => serde_json::to_value(JsonRpcResponse::success(id, res)).unwrap(),
            Err((code, msg)) => {
                serde_json::to_value(JsonRpcResponse::error(id, code, &msg)).unwrap()
            }
        }
    }

    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {}, "resources": {} },
            "serverInfo": { "name": "thegn-docs", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    fn tools_list(&self) -> Value {
        json!({ "tools": [
            {
                "name": "search_docs",
                "description": "Full-text search thegn's help corpus (how to use thegn: worktrees, sidebar, merge queue, sandboxing, keybindings, …). Returns matching page ids to read with read_doc.",
                "inputSchema": { "type": "object",
                    "properties": { "query": { "type": "string", "description": "Search terms" } },
                    "required": ["query"] },
            },
            {
                "name": "read_doc",
                "description": "Read a help page's full markdown by id (from list_docs or search_docs). Includes the generated `keybindings` (effective keymap) and `config-reference` (every config key) pages.",
                "inputSchema": { "type": "object",
                    "properties": { "id": { "type": "string", "description": "Page id, e.g. 'getting-started', 'merge-queue', 'keybindings'" } },
                    "required": ["id"] },
            },
            {
                "name": "list_docs",
                "description": "List every help page (id + title) — the browse index for thegn's documentation.",
                "inputSchema": { "type": "object", "properties": {} },
            },
            {
                "name": "get_config",
                "description": "The user's current effective thegn config as JSON (secrets redacted). Pass a dotted `key` for one value, or omit for the whole tree.",
                "inputSchema": { "type": "object",
                    "properties": { "key": { "type": "string", "description": "Dotted key, e.g. 'sandbox.backend' (optional)" } } },
            },
            {
                "name": "explain_config",
                "description": "Explain how a config key resolves: its effective value and which layer (builtin/user/profile/workspace/runtime) set it, with the value at each layer.",
                "inputSchema": { "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Dotted key, e.g. 'sandbox.backend'" } },
                    "required": ["key"] },
            },
        ] })
    }

    fn tools_call(&self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(Value::Null);
        let str_arg = |k: &str| args.get(k).and_then(Value::as_str);
        match name {
            "search_docs" => {
                let query = str_arg("query").unwrap_or("");
                Ok(text_result(self.search_docs(query)))
            }
            "read_doc" => {
                let id = str_arg("id").unwrap_or("");
                match self.reg.page(id) {
                    Some(p) => Ok(text_result(p.body.clone())),
                    None => Err((-32602, format!("no such doc `{id}` (see list_docs)"))),
                }
            }
            "list_docs" => Ok(text_result(self.list_docs())),
            "get_config" => match str_arg("key") {
                Some(key) => match dotted_get(&self.config, key) {
                    Some(v) => Ok(text_result(pretty(v))),
                    None => Err((-32602, format!("no such config key `{key}`"))),
                },
                None => Ok(text_result(pretty(&self.config))),
            },
            "explain_config" => {
                let Some(key) = str_arg("key") else {
                    return Err((-32602, "missing `key`".to_string()));
                };
                Ok(text_result((self.explain)(key, str_arg("repo"))))
            }
            _ => Err((-32601, format!("Tool not found: {name}"))),
        }
    }

    /// Render search hits as `id — title` lines with the matched snippet's
    /// section, so the agent can pick a page to `read_doc`.
    fn search_docs(&self, query: &str) -> String {
        let hits = search::search(self.reg.pages(), query, self.ranker);
        if hits.is_empty() {
            return format!("no help pages match {query:?}");
        }
        let mut out = String::new();
        for h in hits.iter().take(20) {
            out.push_str(&format!("{} — {}", h.page, h.title));
            if let Some(s) = &h.snippet
                && let Some(sec) = &s.section
            {
                out.push_str(&format!("  [{sec}]"));
            }
            out.push('\n');
        }
        out
    }

    fn list_docs(&self) -> String {
        let mut out = String::new();
        for p in self.reg.pages() {
            let tag = if p.meta.generated { " (generated)" } else { "" };
            out.push_str(&format!("{} — {}{}\n", p.meta.id, p.meta.title, tag));
        }
        out
    }

    fn resources_list(&self) -> Value {
        let mut resources: Vec<Value> = self
            .reg
            .pages()
            .iter()
            .map(|p| {
                json!({
                    "uri": format!("thegn://help/{}", p.meta.id),
                    "name": p.meta.title,
                    "mimeType": "text/markdown",
                })
            })
            .collect();
        resources.push(json!({
            "uri": "thegn://config/current",
            "name": "Current config (redacted)",
            "mimeType": "application/json",
        }));
        resources.push(json!({
            "uri": "thegn://config/schema",
            "name": "Config JSON schema",
            "mimeType": "application/json",
        }));
        for d in &self.docs {
            resources.push(json!({
                "uri": format!("thegn://doc/{}", d.id),
                "name": d.title,
                "mimeType": "text/markdown",
            }));
        }
        json!({ "resources": resources })
    }

    fn resources_read(&self, params: &Value) -> Result<Value, (i32, String)> {
        let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
        let content = |mime: &str, text: String| {
            Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": text }] }))
        };
        if let Some(id) = uri.strip_prefix("thegn://help/") {
            match self.reg.page(id) {
                Some(p) => content("text/markdown", p.body.clone()),
                None => Err((-32602, format!("Resource not found: {uri}"))),
            }
        } else if uri == "thegn://config/current" {
            content("application/json", pretty(&self.config))
        } else if uri == "thegn://config/schema" {
            content("application/json", pretty(&self.schema))
        } else if let Some(id) = uri.strip_prefix("thegn://doc/") {
            match self.docs.iter().find(|d| d.id == id) {
                Some(d) => content("text/markdown", d.body.clone()),
                None => Err((-32602, format!("Resource not found: {uri}"))),
            }
        } else {
            Err((-32602, format!("Resource not found: {uri}")))
        }
    }
}

/// Wrap `text` in the MCP `tools/call` content envelope.
fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Navigate a JSON object tree by a dotted key (`a.b.c`). Returns `None` if any
/// segment is missing or a non-object is traversed.
fn dotted_get<'v>(root: &'v Value, key: &str) -> Option<&'v Value> {
    let mut cur = root;
    for seg in key.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Config keys whose scalar values are secrets and must never be served.
/// Matched case-insensitively as a substring of the key name.
const SENSITIVE: &[&str] = &[
    "token",
    "api_key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "credential",
    "private_key",
];

fn is_sensitive(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|s| k.contains(s)) || k.ends_with("_key")
}

/// Mask secret scalar values in a resolved-config JSON tree in place, so the
/// docs endpoint can serve `get_config` / `thegn://config/current` without
/// leaking tokens or credentials. A scalar (string/number) directly under a
/// sensitive key becomes `"***redacted***"`; objects/arrays are always
/// recursed (so nested secrets are caught, and non-secret subtrees survive).
pub fn redact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if is_sensitive(k) && matches!(val, Value::String(_) | Value::Number(_)) {
                    *val = json!("***redacted***");
                } else {
                    redact(val);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::registry::HelpRegistry;

    /// Trivial substring ranker (mirrors search.rs's test ranker).
    fn ranker(needle: &str, haystacks: &[&str]) -> Vec<(usize, u16)> {
        let n = needle.to_ascii_lowercase();
        haystacks
            .iter()
            .enumerate()
            .filter(|(_, h)| h.to_ascii_lowercase().contains(&n))
            .map(|(i, _)| (i, 10))
            .collect()
    }

    fn registry() -> HelpRegistry {
        let index = "---\nid: index\ntitle: Welcome\n---\n# Tour\nthegn is a worktree IDE.\n";
        let mq =
            "---\nid: merge-queue\ntitle: Merge queue\n---\n# Draining\nrun the drain command\n";
        let (reg, errors) = HelpRegistry::build(&[index, mq], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        reg
    }

    fn router<'a>(reg: &'a HelpRegistry, config: Value) -> DocsRouter<'a> {
        DocsRouter::new(
            reg,
            config,
            json!({ "title": "Config" }),
            vec![DocResource {
                id: "cli".to_string(),
                title: "CLI".to_string(),
                body: "# CLI\ngrammar".to_string(),
            }],
            &ranker,
            |key, repo| format!("{key} = <value> (from user){}", repo.unwrap_or("")),
        )
    }

    fn call(r: &DocsRouter, name: &str, args: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": args } });
        r.handle(&req)
    }

    fn call_text(r: &DocsRouter, name: &str, args: Value) -> String {
        let resp = call(r, name, args);
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn initialize_reports_docs_server() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let resp = r.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }));
        assert_eq!(resp["result"]["serverInfo"]["name"], "thegn-docs");
    }

    #[test]
    fn tools_list_advertises_the_read_only_set() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let resp = r.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "search_docs",
                "read_doc",
                "list_docs",
                "get_config",
                "explain_config"
            ]
        );
    }

    #[test]
    fn search_docs_finds_a_page() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let out = call_text(&r, "search_docs", json!({ "query": "merge" }));
        assert!(out.contains("merge-queue — Merge queue"), "{out}");
    }

    #[test]
    fn search_docs_body_hit_reports_section() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let out = call_text(&r, "search_docs", json!({ "query": "drain command" }));
        assert!(out.contains("merge-queue"), "{out}");
        assert!(out.contains("[Draining]"), "section annotated: {out}");
    }

    #[test]
    fn read_doc_returns_body_and_errors_on_miss() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let out = call_text(&r, "read_doc", json!({ "id": "index" }));
        assert!(out.contains("thegn is a worktree IDE."), "{out}");
        let miss = call(&r, "read_doc", json!({ "id": "nope" }));
        assert_eq!(miss["error"]["code"], -32602);
    }

    #[test]
    fn list_docs_lists_every_page() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let out = call_text(&r, "list_docs", json!({}));
        assert!(out.contains("index — Welcome"), "{out}");
        assert!(out.contains("merge-queue — Merge queue"), "{out}");
    }

    #[test]
    fn get_config_whole_and_dotted() {
        let reg = registry();
        let cfg = json!({ "sandbox": { "backend": "podman" } });
        let r = router(&reg, cfg);
        let whole = call_text(&r, "get_config", json!({}));
        assert!(whole.contains("podman"), "{whole}");
        let one = call_text(&r, "get_config", json!({ "key": "sandbox.backend" }));
        assert_eq!(one.trim(), "\"podman\"");
        let miss = call(&r, "get_config", json!({ "key": "sandbox.nope" }));
        assert_eq!(miss["error"]["code"], -32602);
    }

    #[test]
    fn explain_config_uses_injected_closure() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let out = call_text(&r, "explain_config", json!({ "key": "sandbox.backend" }));
        assert!(
            out.contains("sandbox.backend = <value> (from user)"),
            "{out}"
        );
    }

    #[test]
    fn unknown_method_and_tool_error() {
        let reg = registry();
        let r = router(&reg, json!({}));
        let m = r.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "nope" }));
        assert_eq!(m["error"]["code"], -32601);
        let t = call(&r, "no_such_tool", json!({}));
        assert_eq!(t["error"]["code"], -32601);
    }

    #[test]
    fn resources_round_trip() {
        let reg = registry();
        let r = router(&reg, json!({ "a": 1 }));
        let list = r.handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }));
        let uris: Vec<String> = list["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|res| res["uri"].as_str().unwrap().to_string())
            .collect();
        assert!(uris.contains(&"thegn://help/index".to_string()));
        assert!(uris.contains(&"thegn://config/current".to_string()));
        assert!(uris.contains(&"thegn://config/schema".to_string()));
        assert!(uris.contains(&"thegn://doc/cli".to_string()));

        let read = |uri: &str| {
            r.handle(
                &json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/read",
                "params": { "uri": uri } }),
            )
        };
        let help = read("thegn://help/index");
        assert!(
            help["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("worktree IDE")
        );
        let doc = read("thegn://doc/cli");
        assert!(
            doc["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("grammar")
        );
        let bad = read("thegn://help/missing");
        assert_eq!(bad["error"]["code"], -32602);
    }

    #[test]
    fn redact_masks_secrets_and_keeps_the_rest() {
        let mut v = json!({
            "github_token": "ghp_realsecret",
            "sandbox": { "backend": "podman" },
            "accounts": [ { "name": "work", "api_key": "sk-123" } ],
            "monitor_key": "F5",
            "keybinds": { "quit": "ctrl-q" },
        });
        redact(&mut v);
        assert_eq!(v["github_token"], "***redacted***");
        assert_eq!(v["accounts"][0]["api_key"], "***redacted***");
        assert_eq!(v["monitor_key"], "***redacted***"); // ends_with _key
        // Non-secrets survive, including the name alongside a redacted key.
        assert_eq!(v["sandbox"]["backend"], "podman");
        assert_eq!(v["accounts"][0]["name"], "work");
        assert_eq!(v["keybinds"]["quit"], "ctrl-q");
    }
}

/// Host capabilities the MCP server exposes as tools, by catalog id. The docs
/// tools (`search_docs`, `read_doc`, …) are not catalog items — they read the
/// embedded help corpus, not a running instance. State tools land in the
/// client-API phase; until then every `Surface::Mcp` row is excused in
/// `SURFACE_GAPS`, and this table is the thing that must grow to retire
/// those excuses.
pub const MCP_STATE_CAPS: &[&str] = &[];

#[cfg(test)]
mod catalog_tests {
    use crate::capability::{Surface, coverage_problems};

    #[test]
    fn mcp_tools_cover_catalog() {
        let problems = coverage_problems(Surface::Mcp, super::MCP_STATE_CAPS);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
