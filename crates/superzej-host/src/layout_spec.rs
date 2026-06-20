//! An **abstract** pane-layout description: a tree of splits/stacks whose leaves
//! carry an optional *program* (not a live `PaneId`). This is what `CenterTree`
//! can't be — `CenterTree` holds concrete pane ids, fine for resurrect but
//! useless for "save a layout to reuse later" (the panes don't exist yet).
//!
//! `LayoutSpec` backs named layout snapshots (item 115), layout import/export
//! (item 99), and worktree-template layouts (item 54). It serializes to JSON.
//!
//! Conversions are decoupled from `Panes` via callbacks so the logic is unit
//! testable without spawning real PTYs:
//! - [`LayoutSpec::from_tab`] takes a `program_of: Fn(PaneId) -> Option<String>`
//!   (return `None` for a default shell).
//! - [`LayoutSpec::apply`] takes a `spawn: FnMut(Option<&str>) -> Option<PaneId>`.

use serde::{Deserialize, Serialize};

use crate::center::{Branch, CenterTree, Dir, PaneId};

/// An abstract layout node. Mirrors [`CenterTree`] but leaves hold an optional
/// command instead of a pane id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutSpec {
    /// A single pane running `command` (`None` = the default worktree shell).
    Leaf { command: Option<String> },
    /// A weighted tiled split.
    Split {
        dir: Dir,
        children: Vec<LayoutBranch>,
    },
    /// A tabbed stack; `active` is the initially-visible member.
    Stack {
        panes: Vec<LayoutSpec>,
        active: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutBranch {
    pub weight: f32,
    pub child: LayoutSpec,
}

impl LayoutSpec {
    /// Capture the *shape* of a live `CenterTree`, recording each leaf's program
    /// (via `program_of`, which returns `None` for a plain shell so the snapshot
    /// re-opens a shell rather than re-exec'ing the login program by name).
    pub fn from_tab(tree: &CenterTree, program_of: &impl Fn(PaneId) -> Option<String>) -> Self {
        match tree {
            CenterTree::Leaf(id) => LayoutSpec::Leaf {
                command: program_of(*id),
            },
            CenterTree::Split { dir, children } => LayoutSpec::Split {
                dir: *dir,
                children: children
                    .iter()
                    .map(|b| LayoutBranch {
                        weight: b.weight,
                        child: LayoutSpec::from_tab(&b.child, program_of),
                    })
                    .collect(),
            },
            CenterTree::Stack { panes, active } => LayoutSpec::Stack {
                panes: panes
                    .iter()
                    .map(|id| LayoutSpec::Leaf {
                        command: program_of(*id),
                    })
                    .collect(),
                active: *active,
            },
        }
    }

    /// Build a live `CenterTree` by spawning a pane per leaf (`spawn` returns the
    /// new pane's id, or `None` if it couldn't be created — that leaf is then
    /// dropped). Returns the assembled tree and the focused pane (the first
    /// successfully-spawned leaf, top-left). Returns `None` if nothing spawned.
    pub fn apply(
        &self,
        spawn: &mut impl FnMut(Option<&str>) -> Option<PaneId>,
    ) -> Option<(CenterTree, PaneId)> {
        let mut first: Option<PaneId> = None;
        let tree = self.build(spawn, &mut first)?;
        first.map(|f| (tree, f))
    }

    fn build(
        &self,
        spawn: &mut impl FnMut(Option<&str>) -> Option<PaneId>,
        first: &mut Option<PaneId>,
    ) -> Option<CenterTree> {
        match self {
            LayoutSpec::Leaf { command } => {
                let id = spawn(command.as_deref())?;
                first.get_or_insert(id);
                Some(CenterTree::Leaf(id))
            }
            LayoutSpec::Split { dir, children } => {
                let built: Vec<Branch> = children
                    .iter()
                    .filter_map(|b| {
                        b.child.build(spawn, first).map(|child| Branch {
                            weight: if b.weight > 0.0 { b.weight } else { 1.0 },
                            child,
                        })
                    })
                    .collect();
                match built.len() {
                    0 => None,
                    // A split that collapsed to one survivor is just that child.
                    1 => Some(built.into_iter().next().unwrap().child),
                    _ => Some(CenterTree::Split {
                        dir: *dir,
                        children: built,
                    }),
                }
            }
            LayoutSpec::Stack { panes, active } => {
                let ids: Vec<PaneId> = panes
                    .iter()
                    .filter_map(|p| match p {
                        // Stacks only hold leaves; spawn each member's command.
                        LayoutSpec::Leaf { command } => {
                            let id = spawn(command.as_deref())?;
                            first.get_or_insert(id);
                            Some(id)
                        }
                        // A nested non-leaf in a stack is flattened to its first
                        // spawnable leaf (stacks are tab strips of single panes).
                        other => other.build(spawn, first).and_then(|t| match t {
                            CenterTree::Leaf(id) => Some(id),
                            _ => t.pane_ids().first().copied(),
                        }),
                    })
                    .collect();
                match ids.len() {
                    0 => None,
                    1 => Some(CenterTree::Leaf(ids[0])),
                    _ => Some(CenterTree::Stack {
                        active: (*active).min(ids.len() - 1),
                        panes: ids,
                    }),
                }
            }
        }
    }

    /// A shorthand layout: an even row-split running one pane per command (an
    /// empty command string → a plain shell). Backs the `commands = [...]`
    /// field of a worktree template (item 54).
    pub fn even_split(commands: &[String]) -> LayoutSpec {
        let children = commands
            .iter()
            .map(|c| LayoutBranch {
                weight: 1.0,
                child: LayoutSpec::Leaf {
                    command: (!c.trim().is_empty()).then(|| c.clone()),
                },
            })
            .collect();
        LayoutSpec::Split {
            dir: Dir::Row,
            children,
        }
    }

    /// Serialize to a pretty JSON string (for file export, item 99).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from a JSON string (DB column or imported file).
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_tab_records_programs_and_shells() {
        // feat split: [shell | nvim]; program_of maps shells to None.
        let tree = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 2.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let program_of = |id: PaneId| match id {
            2 => Some("nvim".to_string()),
            _ => None, // shell
        };
        let spec = LayoutSpec::from_tab(&tree, &program_of);
        match spec {
            LayoutSpec::Split { dir, children } => {
                assert_eq!(dir, Dir::Row);
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].child, LayoutSpec::Leaf { command: None });
                assert_eq!(children[1].weight, 2.0);
                assert_eq!(
                    children[1].child,
                    LayoutSpec::Leaf {
                        command: Some("nvim".into())
                    }
                );
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn apply_spawns_a_pane_per_leaf_and_picks_focus() {
        let spec = LayoutSpec::Split {
            dir: Dir::Row,
            children: vec![
                LayoutBranch {
                    weight: 1.0,
                    child: LayoutSpec::Leaf { command: None },
                },
                LayoutBranch {
                    weight: 1.0,
                    child: LayoutSpec::Leaf {
                        command: Some("nvim".into()),
                    },
                },
            ],
        };
        // Fake spawner hands out incrementing ids and records the commands.
        let mut next = 10u32;
        let mut launched: Vec<Option<String>> = Vec::new();
        let mut spawn = |cmd: Option<&str>| {
            launched.push(cmd.map(|s| s.to_string()));
            let id = next;
            next += 1;
            Some(id)
        };
        let (tree, focus) = spec.apply(&mut spawn).expect("layout applied");
        assert_eq!(focus, 10, "focus is the first (top-left) leaf");
        assert_eq!(tree.pane_ids(), vec![10, 11]);
        assert_eq!(launched, vec![None, Some("nvim".to_string())]);
    }

    #[test]
    fn json_round_trip() {
        let spec = LayoutSpec::Split {
            dir: Dir::Col,
            children: vec![
                LayoutBranch {
                    weight: 1.0,
                    child: LayoutSpec::Leaf {
                        command: Some("cargo watch -x test".into()),
                    },
                },
                LayoutBranch {
                    weight: 3.0,
                    child: LayoutSpec::Leaf { command: None },
                },
            ],
        };
        let json = spec.to_json().unwrap();
        assert_eq!(LayoutSpec::from_json(&json).unwrap(), spec);
    }

    #[test]
    fn even_split_maps_commands_and_blanks() {
        let spec = LayoutSpec::even_split(&["nvim".into(), "".into(), "cargo watch".into()]);
        match spec {
            LayoutSpec::Split { dir, children } => {
                assert_eq!(dir, Dir::Row);
                assert_eq!(children.len(), 3);
                assert_eq!(
                    children[0].child,
                    LayoutSpec::Leaf {
                        command: Some("nvim".into())
                    }
                );
                assert_eq!(children[1].child, LayoutSpec::Leaf { command: None });
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn apply_drops_failed_spawns_and_collapses_singletons() {
        let spec = LayoutSpec::Split {
            dir: Dir::Row,
            children: vec![
                LayoutBranch {
                    weight: 1.0,
                    child: LayoutSpec::Leaf { command: None },
                },
                LayoutBranch {
                    weight: 1.0,
                    child: LayoutSpec::Leaf {
                        command: Some("boom".into()),
                    },
                },
            ],
        };
        // Spawner fails for "boom" → that leaf is dropped → split collapses to
        // the single survivor (a bare Leaf).
        let mut spawn = |cmd: Option<&str>| {
            if cmd == Some("boom") {
                None
            } else {
                Some(5u32)
            }
        };
        let (tree, focus) = spec.apply(&mut spawn).unwrap();
        assert_eq!(focus, 5);
        assert_eq!(tree, CenterTree::Leaf(5));
    }
}
