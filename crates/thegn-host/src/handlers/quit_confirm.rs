//! The "Quit thegn?" confirmation. Quit is a detach (daemon-backed panes keep
//! running), so the prompt states how many panes survive and how many close.
//! Everything here is pure and I/O-free: the counts come from memory and the
//! run loop owns the actual quit bodies. Signals, the CLI and live upgrades
//! never reach this module.

use crate::keymap::Action;
use crate::menu::{self, MenuChoice, MenuOverlay};

/// The `MenuChoice::Confirm` tag the run loop's dispatch matches on.
pub(crate) const TAG: &str = "quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitKind {
    Detach,
    Kill,
}

/// Whether a user quit asks first. Pure.
pub(crate) fn should_prompt(confirm_quit: bool, prompt_open: bool) -> bool {
    confirm_quit && !prompt_open
}

/// Body text of the prompt for `kind`. Zero lines are omitted.
pub(crate) fn body_text(kind: QuitKind, kept: usize, closed: usize) -> String {
    match kind {
        QuitKind::Kill => format!(
            "{} pane(s) will be stopped, including those in the daemon.",
            kept + closed
        ),
        QuitKind::Detach => {
            let mut lines: Vec<String> = Vec::new();
            if kept > 0 {
                lines.push(format!(
                    "{kept} pane(s) keep running in the daemon; the next launch reattaches them."
                ));
            }
            if closed > 0 {
                lines.push(format!("{closed} pane(s) not in the daemon will close."));
            }
            if lines.is_empty() {
                "No panes are open.".to_string()
            } else {
                lines.join(" ")
            }
        }
    }
}

/// The prompt overlay. Detach defaults to confirm, Kill to cancel.
pub(crate) fn menu(kind: QuitKind, kept: usize, closed: usize) -> MenuOverlay {
    let body = body_text(kind, kept, closed);
    match kind {
        QuitKind::Detach => menu::confirm_menu("Quit thegn?", body, TAG, "detach".into(), false),
        QuitKind::Kill => {
            menu::confirm_menu_default_cancel("Quit and kill every pane?", body, TAG, "kill".into())
        }
    }
}

/// Which quit prompt `m` is, if it is one.
pub(crate) fn is_quit_prompt(m: &MenuOverlay) -> Option<QuitKind> {
    m.items().iter().find_map(|i| match &i.choice {
        MenuChoice::Confirm { tag, arg } if *tag == TAG => kind_from_arg(arg),
        _ => None,
    })
}

/// `"detach"` / `"kill"` from the confirm's arg.
pub(crate) fn kind_from_arg(arg: &str) -> Option<QuitKind> {
    match arg {
        "detach" => Some(QuitKind::Detach),
        "kill" => Some(QuitKind::Kill),
        _ => None,
    }
}

/// A second quit while the prompt is open confirms. Pure.
pub(crate) fn second_press(open: Option<QuitKind>, action: Option<&Action>) -> Option<QuitKind> {
    match (open?, action?) {
        (QuitKind::Detach, Action::Quit) => Some(QuitKind::Detach),
        (QuitKind::Kill, Action::QuitKill) => Some(QuitKind::Kill),
        _ => None,
    }
}

/// The loop's quit-key decision: `true` = run the quit body now; `false` =
/// the prompt was opened into `active_menu` (caller marks dirty and
/// continues). `counts` is only evaluated when a prompt opens.
pub(crate) fn request(
    confirm_quit: bool,
    active_menu: &mut Option<MenuOverlay>,
    kind: QuitKind,
    counts: impl FnOnce() -> (usize, usize),
) -> bool {
    let open = active_menu.as_ref().and_then(is_quit_prompt).is_some();
    if should_prompt(confirm_quit, open) {
        let (kept, closed) = counts();
        *active_menu = Some(menu(kind, kept, closed));
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_prompt_table() {
        assert!(should_prompt(true, false));
        assert!(!should_prompt(true, true));
        assert!(!should_prompt(false, false));
        assert!(!should_prompt(false, true));
    }

    #[test]
    fn body_text_cases() {
        let b = body_text(QuitKind::Detach, 2, 0);
        assert!(b.contains("2 pane(s) keep running in the daemon"));
        assert!(!b.contains("will close"));
        let b = body_text(QuitKind::Detach, 0, 3);
        assert!(b.contains("3 pane(s) not in the daemon will close."));
        assert!(!b.contains("keep running"));
        assert_eq!(body_text(QuitKind::Detach, 0, 0), "No panes are open.");
        assert_eq!(
            body_text(QuitKind::Kill, 1, 2),
            "3 pane(s) will be stopped, including those in the daemon."
        );
    }

    #[test]
    fn default_selection_and_kind() {
        let d = menu(QuitKind::Detach, 1, 1);
        assert_eq!(d.selected(), 0);
        assert_eq!(is_quit_prompt(&d), Some(QuitKind::Detach));
        let k = menu(QuitKind::Kill, 1, 1);
        assert_eq!(k.selected(), 1);
        assert_eq!(is_quit_prompt(&k), Some(QuitKind::Kill));
        assert_eq!(k.title, "Quit and kill every pane?");
    }

    #[test]
    fn second_press_table() {
        let d = Some(QuitKind::Detach);
        assert_eq!(second_press(d, Some(&Action::Quit)), Some(QuitKind::Detach));
        assert_eq!(
            second_press(Some(QuitKind::Kill), Some(&Action::QuitKill)),
            Some(QuitKind::Kill)
        );
        assert_eq!(second_press(d, Some(&Action::QuitKill)), None);
        assert_eq!(
            second_press(Some(QuitKind::Kill), Some(&Action::Quit)),
            None
        );
        assert_eq!(second_press(d, Some(&Action::Detach)), None);
        assert_eq!(second_press(d, None), None);
        assert_eq!(second_press(None, Some(&Action::Quit)), None);
    }

    #[test]
    fn request_prompts_or_runs() {
        let mut menu_slot = None;
        assert!(!request(true, &mut menu_slot, QuitKind::Detach, || (1, 0)));
        assert_eq!(
            menu_slot.as_ref().and_then(is_quit_prompt),
            Some(QuitKind::Detach)
        );
        let mut off = None;
        assert!(request(false, &mut off, QuitKind::Detach, || (1, 0)));
        assert!(off.is_none());
    }
}
