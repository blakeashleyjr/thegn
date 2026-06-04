//! `superzej recent [N]` — recently opened repos (most recent first).

use crate::db::Db;
use anyhow::Result;

pub fn run(count: Option<i64>) -> Result<()> {
    let db = Db::open()?;
    for path in db.recent_repos(count.unwrap_or(20))? {
        println!("{path}");
    }
    Ok(())
}
