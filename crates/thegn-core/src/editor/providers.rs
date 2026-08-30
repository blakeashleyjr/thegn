//! Logical editor-provider registry.

use super::Editor;
use crate::config::{EditorOpenIn, config_enum, config_warn};
use crate::seam::ProbeReport;

config_enum! {
    /// `[editor] provider` — a logical IDE/editor implementation. `auto` keeps
    /// the custom command/tool/environment ladder.
    pub enum EditorProvider: "editor provider" {
        Auto = "auto",
        Vscode = "vscode",
        Cursor = "cursor",
        Zed = "zed",
        Jetbrains = "jetbrains",
        NvimRemote = "nvim_remote",
        Emacs = "emacs",
    } default = Auto;
}

/// Construct an explicitly selected logical provider. `auto` deliberately
/// returns `None`; the caller then follows the compatibility ladder without a
/// PATH scan.
pub fn provider(kind: EditorProvider, open_in: EditorOpenIn) -> Option<Box<dyn Editor>> {
    match kind {
        EditorProvider::Auto => None,
        EditorProvider::Vscode => Some(Box::new(super::vscode::Vscode::new(open_in))),
        EditorProvider::Cursor => Some(Box::new(super::cursor::Cursor::new(open_in))),
        EditorProvider::Zed => Some(Box::new(super::zed::Zed::new(open_in))),
        EditorProvider::Jetbrains => Some(Box::new(super::jetbrains::Jetbrains::new(open_in))),
        EditorProvider::NvimRemote => Some(Box::new(super::nvim_remote::NvimRemote::new(open_in))),
        EditorProvider::Emacs => Some(Box::new(super::emacs::Emacs::new(open_in))),
    }
}

/// Cheap, PATH-only reports for every registered logical provider. Doctor can
/// enumerate these without teaching its registry vendor argv spellings.
pub fn probes(open_in: EditorOpenIn) -> Vec<ProbeReport> {
    [
        EditorProvider::Vscode,
        EditorProvider::Cursor,
        EditorProvider::Zed,
        EditorProvider::Jetbrains,
        EditorProvider::NvimRemote,
        EditorProvider::Emacs,
    ]
    .into_iter()
    .filter_map(|kind| provider(kind, open_in))
    .map(|editor| editor.probe())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{EditorError, EditorOperation, EditorTarget, Placement};
    use crate::seam::{ErrorClass, Kind, SeamError};

    fn file() -> EditorTarget {
        EditorTarget::file("/work/tree", "src/main.rs", Some(42), Some(7)).unwrap()
    }

    fn project() -> EditorTarget {
        EditorTarget::project("/work/tree").unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn every_kind_is_registered_and_probed() {
        for kind in EditorProvider::ALL {
            assert_eq!(
                provider(*kind, EditorOpenIn::Auto).is_some(),
                *kind != EditorProvider::Auto
            );
        }
        let reports = probes(EditorOpenIn::Auto);
        assert_eq!(reports.len(), 6);
        assert!(reports.iter().all(|report| report.seam == "editor"));
    }

    #[test]
    fn vscode_argv_caps_and_placement() {
        let editor = provider(EditorProvider::Vscode, EditorOpenIn::Auto).unwrap();
        assert_eq!(
            editor.caps(),
            crate::editor::EditorCaps {
                open_file: true,
                open_directory: true,
                line: true,
                column: true,
                external: true,
            }
        );
        let launch = editor.open_target(&file()).unwrap();
        assert_eq!(
            launch.argv,
            strings(&["code", "-g", "/work/tree/src/main.rs:42:7"])
        );
        assert_eq!(launch.placement, Placement::External);
        assert_eq!(launch.provider, "vscode");
        assert_eq!(launch.operation, EditorOperation::OpenFile);
        assert_eq!(
            editor.open_target(&project()).unwrap().argv,
            strings(&["code", "/work/tree"])
        );
    }

    #[test]
    fn cursor_argv_caps_and_placement_override() {
        let editor = provider(EditorProvider::Cursor, EditorOpenIn::Pane).unwrap();
        assert!(editor.caps().open_file && editor.caps().open_directory && editor.caps().column);
        assert!(!editor.caps().external);
        let launch = editor.open_target(&file()).unwrap();
        assert_eq!(
            launch.argv,
            strings(&["cursor", "-g", "/work/tree/src/main.rs:42:7"])
        );
        assert_eq!(launch.placement, Placement::Pane);
        assert_eq!(
            editor.open_target(&project()).unwrap().argv,
            strings(&["cursor", "/work/tree"])
        );
    }

    #[test]
    fn zed_argv_caps_and_placement() {
        let editor = provider(EditorProvider::Zed, EditorOpenIn::Auto).unwrap();
        assert!(editor.caps().open_directory && editor.caps().column && editor.caps().external);
        assert_eq!(
            editor.open_target(&file()).unwrap().argv,
            strings(&["zed", "/work/tree/src/main.rs:42:7"])
        );
        assert_eq!(
            editor.open_target(&project()).unwrap().argv,
            strings(&["zed", "/work/tree"])
        );
    }

    #[test]
    fn jetbrains_rejects_columns_and_plans_lines() {
        let editor = provider(EditorProvider::Jetbrains, EditorOpenIn::Auto).unwrap();
        assert!(editor.caps().open_file && editor.caps().open_directory && editor.caps().line);
        assert!(!editor.caps().column);
        assert_eq!(
            editor.open_target(&file()).unwrap_err(),
            EditorError::Unsupported("column")
        );
        let line = EditorTarget::file("/work/tree", "src/main.rs", Some(42), None).unwrap();
        assert_eq!(
            editor.open_target(&line).unwrap().argv,
            strings(&["idea", "--line", "42", "/work/tree/src/main.rs"])
        );
        assert_eq!(
            editor.open_target(&project()).unwrap().argv,
            strings(&["idea", "/work/tree"])
        );
    }

    #[test]
    fn nvim_remote_rejects_project_and_columns() {
        let editor = provider(EditorProvider::NvimRemote, EditorOpenIn::Auto).unwrap();
        assert!(editor.caps().open_file && editor.caps().line && editor.caps().external);
        assert!(!editor.caps().open_directory && !editor.caps().column);
        assert_eq!(
            editor.open_target(&file()).unwrap_err(),
            EditorError::Unsupported("column")
        );
        let line = EditorTarget::file("/work/tree", "src/main.rs", Some(42), None).unwrap();
        assert_eq!(
            editor.open_target(&line).unwrap().argv,
            strings(&["nvr", "--remote-silent", "+42", "/work/tree/src/main.rs"])
        );
        let error = editor.open_target(&project()).unwrap_err();
        assert_eq!(error, EditorError::Unsupported("open_directory"));
        assert_eq!(error.class(), ErrorClass::Unsupported);
    }

    #[test]
    fn emacs_argv_caps_and_placement() {
        let editor = provider(EditorProvider::Emacs, EditorOpenIn::Auto).unwrap();
        assert!(editor.caps().open_file && editor.caps().open_directory && editor.caps().column);
        assert_eq!(
            editor.open_target(&file()).unwrap().argv,
            strings(&["emacsclient", "-n", "+42:7", "/work/tree/src/main.rs"])
        );
        assert_eq!(
            editor.open_target(&project()).unwrap().argv,
            strings(&["emacsclient", "-n", "/work/tree"])
        );
    }
}
