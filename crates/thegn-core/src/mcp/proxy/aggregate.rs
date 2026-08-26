//! Merge upstream `tools/list` replies into one namespaced, filtered table.
//!
//! For each *exposed* upstream, every tool that its `proxy.tools` filter admits
//! is re-advertised under `<upstream>__<tool>` carrying the upstream's own
//! schema, and a route back to `(upstream, original tool)` is recorded. Tools
//! the filter hides are neither advertised nor routable. The result is what
//! `tools/list` returns and what `tools/call` looks names up in — both derived
//! from the one pass here, so discovery and invocation can never disagree.

use super::filter::tool_exposed;
use super::namespaced;
use super::route::ToolRoute;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One upstream's contribution to the aggregate.
pub struct UpstreamTools<'a> {
    /// The `[mcp_servers.<name>]` key (the namespace prefix).
    pub name: &'a str,
    /// The upstream's `tools/list` result — either `{ "tools": [ … ] }` or a
    /// bare `[ … ]` array of tool entries.
    pub tools: &'a Value,
    /// The `proxy.tools` glob list (default-deny: empty ⇒ nothing exposed).
    pub exposure: &'a [String],
}

/// Per-upstream exposed/hidden breakdown, for `mcp list`, `status`, doctor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSummary {
    pub name: String,
    pub exposed: Vec<String>,
    pub hidden: Vec<String>,
    /// Namespaced names dropped because a prior upstream already claimed them
    /// (a pathological name collision — rare, but reported not silently lost).
    pub collisions: Vec<String>,
}

/// The merged, filtered, namespaced tool surface.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Aggregate {
    /// `tools/list` entries (namespaced names, upstream schemas).
    pub tools: Vec<Value>,
    /// Namespaced tool name → owning upstream + original tool name.
    pub routes: BTreeMap<String, ToolRoute>,
    /// Per-upstream policy breakdown.
    pub summary: Vec<UpstreamSummary>,
}

impl Aggregate {
    /// Resolve a `tools/call` name to its route, or `None` if the name is not
    /// an exposed/advertised tool (⇒ the caller answers an unknown-tool error;
    /// nothing is forwarded to any upstream).
    pub fn route(&self, name: &str) -> Option<&ToolRoute> {
        self.routes.get(name)
    }

    /// The advertised tool count.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

/// Extract the tool-entry array from a `tools/list` result (`{ "tools": [...] }`
/// or a bare array). An unexpected shape yields an empty slice — a malformed or
/// hostile upstream contributes nothing rather than corrupting the aggregate.
fn tool_entries(result: &Value) -> &[Value] {
    if let Some(arr) = result.get("tools").and_then(Value::as_array) {
        arr
    } else if let Some(arr) = result.as_array() {
        arr
    } else {
        &[]
    }
}

/// Aggregate every upstream's exposed tools into one table. Upstreams are
/// processed in the given order; on a namespaced-name collision the first
/// claimant wins and the later one is recorded in that upstream's summary.
pub fn aggregate(upstreams: &[UpstreamTools]) -> Aggregate {
    let mut agg = Aggregate::default();
    for up in upstreams {
        let mut exposed = Vec::new();
        let mut hidden = Vec::new();
        let mut collisions = Vec::new();
        for entry in tool_entries(up.tools) {
            let Some(tool) = entry.get("name").and_then(Value::as_str) else {
                continue; // a nameless tool entry can't be routed — drop it.
            };
            if !tool_exposed(up.exposure, tool) {
                hidden.push(tool.to_string());
                continue;
            }
            let ns = namespaced(up.name, tool);
            if agg.routes.contains_key(&ns) {
                collisions.push(ns);
                continue;
            }
            // Re-advertise with the namespaced name, keeping the upstream's
            // description + inputSchema verbatim.
            let mut advertised = entry.clone();
            if let Value::Object(map) = &mut advertised {
                map.insert("name".to_string(), json!(ns));
            }
            agg.routes.insert(
                ns,
                ToolRoute {
                    upstream: up.name.to_string(),
                    tool: tool.to_string(),
                },
            );
            agg.tools.push(advertised);
            exposed.push(tool.to_string());
        }
        agg.summary.push(UpstreamSummary {
            name: up.name.to_string(),
            exposed,
            hidden,
            collisions,
        });
    }
    agg
}

/// The proxy's `initialize` result: merged capabilities advertising `tools`
/// with `listChanged` (so a reconcile can push `notifications/tools/
/// list_changed` and connected agents refresh). The proxy fixes a stable
/// protocol version rather than negotiating per-upstream — upstream protocol
/// quirks stay behind the proxy.
pub fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": {
            "name": super::PROXY_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Value {
        json!({ "name": name, "description": format!("{name} tool"),
                "inputSchema": { "type": "object" } })
    }

    fn up<'a>(name: &'a str, tools: &'a Value, exposure: &'a [String]) -> UpstreamTools<'a> {
        UpstreamTools {
            name,
            tools,
            exposure,
        }
    }

    fn pats(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn two_upstreams_same_tool_namespaced_and_routed() {
        let a_tools = json!({ "tools": [tool("search")] });
        let b_tools = json!({ "tools": [tool("search")] });
        let star = pats(&["*"]);
        let agg = aggregate(&[up("a", &a_tools, &star), up("b", &b_tools, &star)]);

        let names: Vec<&str> = agg
            .tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["a__search", "b__search"]);
        // Each carries its own schema.
        assert_eq!(agg.tools[0]["description"], "search tool");
        // Routing resolves to the owning upstream + original name.
        assert_eq!(
            agg.route("a__search").unwrap(),
            &ToolRoute {
                upstream: "a".into(),
                tool: "search".into()
            }
        );
        assert_eq!(agg.route("b__search").unwrap().upstream, "b");
    }

    #[test]
    fn default_deny_undeclared_upstream_contributes_nothing() {
        let tools = json!({ "tools": [tool("search"), tool("delete")] });
        let agg = aggregate(&[up("git", &tools, &[])]); // no exposure
        assert!(agg.tools.is_empty());
        assert!(agg.routes.is_empty());
        assert_eq!(agg.summary[0].exposed, Vec::<String>::new());
        assert_eq!(agg.summary[0].hidden, ["search", "delete"]);
    }

    #[test]
    fn filtered_tool_is_absent_and_unroutable() {
        let tools = json!({ "tools": [tool("read_page"), tool("delete_page")] });
        let agg = aggregate(&[up("wiki", &tools, &pats(&["read_*"]))]);
        let names: Vec<&str> = agg
            .tools
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["wiki__read_page"]);
        // The hidden tool is not routable even by its exact namespaced name.
        assert!(agg.route("wiki__delete_page").is_none());
        assert_eq!(agg.summary[0].exposed, ["read_page"]);
        assert_eq!(agg.summary[0].hidden, ["delete_page"]);
    }

    #[test]
    fn accepts_bare_array_tools_list() {
        let tools = json!([tool("a"), tool("b")]);
        let agg = aggregate(&[up("x", &tools, &pats(&["*"]))]);
        assert_eq!(agg.tool_count(), 2);
    }

    #[test]
    fn malformed_tools_list_yields_nothing() {
        let tools = json!({ "not_tools": 3 });
        let agg = aggregate(&[up("x", &tools, &pats(&["*"]))]);
        assert!(agg.tools.is_empty());
        // A nameless entry is dropped too.
        let tools = json!({ "tools": [{ "description": "no name" }, tool("ok")] });
        let agg = aggregate(&[up("x", &tools, &pats(&["*"]))]);
        assert_eq!(agg.tool_count(), 1);
        assert_eq!(agg.tools[0]["name"], "x__ok");
    }

    #[test]
    fn namespaced_collision_keeps_first_and_reports() {
        // Pathological: upstream "a" tool "b__c" and upstream "a__b" tool "c"
        // both namespace to "a__b__c".
        let a = json!({ "tools": [tool("b__c")] });
        let ab = json!({ "tools": [tool("c")] });
        let star = pats(&["*"]);
        let agg = aggregate(&[up("a", &a, &star), up("a__b", &ab, &star)]);
        assert_eq!(agg.tool_count(), 1, "first claimant wins");
        assert_eq!(agg.tools[0]["name"], "a__b__c");
        assert_eq!(agg.route("a__b__c").unwrap().upstream, "a");
        // The loser is recorded, not silently lost.
        assert_eq!(agg.summary[1].collisions, ["a__b__c"]);
    }

    #[test]
    fn initialize_advertises_tools_list_changed() {
        let init = initialize_result();
        assert_eq!(init["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(init["serverInfo"]["name"], super::super::PROXY_SERVER_NAME);
    }

    #[test]
    fn empty_upstreams_empty_aggregate() {
        let agg = aggregate(&[]);
        assert!(agg.tools.is_empty());
        assert!(agg.summary.is_empty());
        assert!(agg.route("x__y").is_none());
    }
}
