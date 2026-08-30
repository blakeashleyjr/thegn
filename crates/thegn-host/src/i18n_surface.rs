//! Typed localization adapter for the bounded statusbar and command-palette surface.
//!
//! This is the host's only bridge to the embedded core catalog. Draw sites pass
//! typed concepts and data here; every returned message performs exactly one
//! catalog lookup. Canonical action labels remain in [`crate::keymap::ActionSpec`]
//! for generated English help, and are used only when an id is provably absent
//! from the shipped action registry.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::keymap::Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteText {
    Title,
    Badge,
    FilterPlaceholder,
    Move,
    Run,
    Dismiss,
    NewTerminal,
    NewFolder,
}

impl PaletteText {
    const fn key(self) -> &'static str {
        match self {
            Self::Title => "palette-title",
            Self::Badge => "palette-badge",
            Self::FilterPlaceholder => "palette-filter-placeholder",
            Self::Move => "palette-footer-move",
            Self::Run => "palette-footer-run",
            Self::Dismiss => "palette-footer-dismiss",
            Self::NewTerminal => "palette-new-terminal",
            Self::NewFolder => "palette-new-folder",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusText {
    Offline,
    Zoom,
    Maximized,
    Locked,
    Sync,
    NewFolderPrompt,
    NewWorkspacePrompt,
}

impl StatusText {
    const fn key(self) -> &'static str {
        match self {
            Self::Offline => "statusbar-offline",
            Self::Zoom => "statusbar-zoom",
            Self::Maximized => "statusbar-maximized",
            Self::Locked => "statusbar-locked",
            Self::Sync => "statusbar-sync",
            Self::NewFolderPrompt => "statusbar-new-folder-prompt",
            Self::NewWorkspacePrompt => "statusbar-new-workspace-prompt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModeStyle {
    Full,
    Compact,
    Status,
}

pub(crate) fn palette(message: PaletteText) -> String {
    thegn_core::t!(message.key())
}

pub(crate) fn status(message: StatusText) -> String {
    thegn_core::t!(message.key())
}

pub(crate) fn mode(mode: Mode, style: ModeStyle) -> String {
    let key = match (mode, style) {
        (Mode::Normal, ModeStyle::Full) => "statusbar-mode-normal-full",
        (Mode::VimNormal, ModeStyle::Full) => "statusbar-mode-vim-normal-full",
        (Mode::VimInsert, ModeStyle::Full) => "statusbar-mode-vim-insert-full",
        (Mode::Emacs, ModeStyle::Full) => "statusbar-mode-emacs-full",
        (Mode::Normal, ModeStyle::Compact) => "statusbar-mode-normal-compact",
        (Mode::VimNormal, ModeStyle::Compact) => "statusbar-mode-vim-normal-compact",
        (Mode::VimInsert, ModeStyle::Compact) => "statusbar-mode-vim-insert-compact",
        (Mode::Emacs, ModeStyle::Compact) => "statusbar-mode-emacs-compact",
        (Mode::Normal, ModeStyle::Status) => "statusbar-mode-normal-status",
        (Mode::VimNormal, ModeStyle::Status) => "statusbar-mode-vim-normal-status",
        (Mode::VimInsert, ModeStyle::Status) => "statusbar-mode-vim-insert-status",
        (Mode::Emacs, ModeStyle::Status) => "statusbar-mode-emacs-status",
    };
    thegn_core::t!(key)
}

pub(crate) fn loc(count: &str) -> String {
    let mut args = HashMap::new();
    args.insert(Cow::Borrowed("count"), count.into());
    thegn_core::i18n::lookup_with_args("statusbar-loc", &args)
}

pub(crate) fn disk_free(percent: u8) -> String {
    let mut args = HashMap::new();
    args.insert(Cow::Borrowed("percent"), i64::from(percent).into());
    thegn_core::i18n::lookup_with_args("statusbar-disk-free", &args)
}

pub(crate) fn palette_matches(count: usize) -> String {
    let locale = thegn_core::i18n::active_lang().language.as_str();
    let key = match thegn_core::i18n_format::plural_category(locale, count as i64) {
        thegn_core::i18n_format::PluralCategory::One => "palette-matches-one",
        thegn_core::i18n_format::PluralCategory::Other => "palette-matches-other",
    };
    let mut args = HashMap::new();
    args.insert(Cow::Borrowed("count"), (count as i64).into());
    thegn_core::i18n::lookup_with_args(key, &args)
}

pub(crate) fn move_to_folder(folder: &str) -> String {
    let mut args = HashMap::new();
    args.insert(Cow::Borrowed("folder"), folder.into());
    thegn_core::i18n::lookup_with_args("palette-move-to-folder", &args)
}

/// Localize a shipped action id. An unknown id has no trustworthy catalog key,
/// so retain its caller-provided canonical label and emit one diagnostic.
pub(crate) fn action_label(id: &str, canonical: &str) -> String {
    let Some(spec) = crate::keymap::action_specs()
        .iter()
        .find(|spec| spec.id == id)
    else {
        thegn_core::msg::warn(&format!(
            "i18n: unregistered action id '{id}', using canonical label"
        ));
        return canonical.to_string();
    };
    thegn_core::t!(spec.message_key)
}

/// Read an argument-free proof-locale value for geometry tests without
/// mutating the process-global startup locale.
#[cfg(test)]
pub(crate) fn test_catalog_value(locale: &str, key: &str) -> String {
    let source = thegn_core::i18n_parity::SHIPPED_LOCALES
        .iter()
        .find(|source| source.locale == locale)
        .expect("shipped test locale")
        .source;
    source
        .lines()
        .find_map(|line| {
            line.strip_prefix(key)
                .and_then(|value| value.strip_prefix(" = "))
        })
        .expect("catalog key")
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn lookup(locale: &str, key: &str) -> String {
        test_catalog_value(locale, key)
    }

    #[test]
    fn japanese_catalog_observes_translated_action_and_status_labels() {
        let action = crate::keymap::action_specs()
            .iter()
            .find(|spec| spec.id == "new-worktree")
            .expect("registered action");
        assert_eq!(lookup("ja-JP", action.message_key), "新しいワークツリー");
        assert_eq!(lookup("ja-JP", StatusText::Offline.key()), "オフライン");
        assert_eq!(lookup("ja-JP", PaletteText::Title.key()), "ジャンプ");
    }

    #[test]
    fn palette_match_count_selects_singular_and_plural_keys() {
        assert_eq!(palette_matches(1), "1 match");
        assert_eq!(palette_matches(2), "2 matches");
    }

    #[test]
    fn every_action_has_a_unique_stable_catalog_key_in_each_locale() {
        let mut seen = BTreeSet::new();
        for spec in crate::keymap::action_specs() {
            assert_eq!(spec.message_key, format!("action-{}", spec.id));
            assert!(
                seen.insert(spec.message_key),
                "duplicate {}",
                spec.message_key
            );
            for source in thegn_core::i18n_parity::SHIPPED_LOCALES {
                assert!(
                    source
                        .source
                        .lines()
                        .any(|line| line.starts_with(&format!("{} =", spec.message_key))),
                    "{} is absent from {}",
                    spec.message_key,
                    source.locale
                );
            }
        }
    }

    #[test]
    fn unknown_actions_keep_the_canonical_user_facing_label() {
        assert_eq!(
            action_label("plugin-owned-action", "Plugin supplied label"),
            "Plugin supplied label"
        );
    }
}
