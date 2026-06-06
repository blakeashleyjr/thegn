//! `superzej panel-snapshot --session <s> --tab <t>` — the panel's fast first
//! paint. In ONE process (no git, no gh) it resolves the focused tab's worktree
//! and returns whatever is already cached — `pr_cache` + `diff_cache` — as a
//! single JSON document, so the panel renders immediately. The panel then kicks
//! off the live fetch in the background to hydrate.
//!
//! Side effect: it records the focused worktree to the focus file, which is how
//! the `watch` daemon learns what to filesystem-watch (no extra round-trip).

use crate::commands::{resolve, watch};
use crate::db::{self, Db};
use anyhow::Result;
use serde_json::{Map, Value, json};

pub fn run(session: Option<String>, tab: Option<String>) -> Result<()> {
    let session = session.unwrap_or_else(db::session);
    let worktree = tab
        .as_deref()
        .and_then(|t| resolve::resolve_tab_worktree(&session, t));

    let mut obj = Map::new();
    if let Some(wt) = &worktree {
        obj.insert("worktree".into(), json!(wt));
        // Tell the watch daemon which worktree now has focus.
        watch::write_focus(&session, wt);
        if let Ok(db) = Db::open() {
            if let Ok(Some((pr_json, _))) = db.get_pr_cache(wt) {
                if let Ok(v) = serde_json::from_str::<Value>(&pr_json) {
                    obj.insert("pr".into(), v);
                }
            }
            if let Ok(Some((files, _))) = db.get_diff_cache(wt) {
                obj.insert("files".into(), json!(files));
            }
        }
    }
    println!("{}", Value::Object(obj));
    Ok(())
}
