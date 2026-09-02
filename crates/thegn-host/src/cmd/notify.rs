//! `thegn notify` — plugin/script API for the notification tray.
//!
//! These subcommands write to the shared SQLite DB, which the running
//! thegn host picks up on its next 2s hydration tick.

#![allow(clippy::disallowed_macros)]

use clap::Subcommand;
use thegn_core::store::NotificationStore;

#[derive(Subcommand, Clone)]
pub enum Action {
    /// Push a new notification into the tray.
    Push {
        /// Notification kind — one of the built-in kinds (listed in
        /// `--help`, each with its default priority) or any custom string.
        #[arg(long, default_value = "agent_done", long_help = kind_help())]
        kind: String,
        /// Opaque source reference (issue id, PR ref, worktree path, etc.).
        #[arg(long, default_value = "")]
        source: String,
        /// Human-readable message shown in the inbox.
        message: String,
        /// Worktree path this notification is most relevant to.
        #[arg(long, default_value = "")]
        worktree: String,
    },
    /// List current notifications (default: unread only).
    List {
        /// Include already-read notifications.
        #[arg(long, short = 'a')]
        all: bool,
        /// Output as JSON (one object per line).
        #[arg(long)]
        json: bool,
        /// Max number of notifications to return.
        #[arg(long, default_value = "50")]
        limit: usize,
    },
    /// Mark a notification as read.
    Read {
        /// Notification id (from `notify list`).
        id: i64,
    },
    /// Mark all notifications as read.
    ReadAll,
    /// Delete a notification.
    Dismiss {
        /// Notification id (from `notify list`).
        id: i64,
    },
    /// Delete all notifications.
    DismissAll,
}

pub fn run(action: Action) -> anyhow::Result<()> {
    let db = thegn_core::db::Db::open()?;
    match action {
        Action::Push {
            kind,
            source,
            message,
            worktree,
        } => {
            let id = crate::automation_events::emit(&db, &kind, &source, &message, &worktree)?;
            println!("{id}");
        }
        Action::List { all, json, limit } => {
            let notifs = db.get_all_notifications(limit)?;
            let filtered: Vec<_> = notifs.iter().filter(|n| all || !n.read).collect();
            if json {
                for n in filtered {
                    println!("{}", serde_json::to_string(n)?);
                }
            } else {
                for n in filtered {
                    let read_tag = if n.read { "[read] " } else { "" };
                    println!(
                        "{:>6}  {:<16}  {read_tag}{}  ({})",
                        n.id, n.source_ref, n.message, n.worktree_path,
                    );
                }
            }
        }
        Action::Read { id } => {
            db.mark_notification_read(id)?;
        }
        Action::ReadAll => {
            db.mark_all_notifications_read()?;
        }
        Action::Dismiss { id } => {
            db.delete_notification(id)?;
        }
        Action::DismissAll => {
            // Delete all notifications (no confirmation — non-interactive context).
            let all = db.get_all_notifications(usize::MAX)?;
            for n in all {
                db.delete_notification(n.id)?;
            }
        }
    }
    Ok(())
}

/// The `--kind` long help, generated from `NotificationKind::ALL` so the CLI
/// can never list a stale subset of the kinds (the single source is the enum;
/// `config.toml.example`'s prose is pinned to it by a core test).
fn kind_help() -> String {
    use thegn_core::notification::NotificationKind;
    let mut out = String::from(
        "Notification kind. Built-in kinds (default priority in brackets) — any \
         other string is accepted as a custom kind:\n",
    );
    for k in NotificationKind::ALL {
        out.push_str(&format!(
            "  {:<22} [{}]\n",
            k.as_str(),
            k.default_priority().as_str()
        ));
    }
    out
}
