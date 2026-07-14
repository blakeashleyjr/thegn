//! help — the built-in documentation model.
//!
//! Substrate-free core of thegn's in-app help system: pages are markdown
//! sources (embedded by the host via `include_str!`) with a strict
//! frontmatter header binding each page to the UI (focus contexts, action
//! ids, TOC placement). This module owns the pure, testable half — parsing
//! (frontmatter + a small markdown subset), the validated registry/TOC,
//! full-text search, and the generated config-reference page. The host owns
//! rendering (AST → styled lines), the overlay/panel UI, and the ratchet
//! test that keeps every action documented.

pub mod config_ref;
pub mod frontmatter;
pub mod markdown;
pub mod registry;
pub mod search;

pub use frontmatter::{FrontmatterError, PageMeta};
pub use markdown::{Block, Inline, LinkTarget, ListItem};
pub use registry::{HelpPage, HelpRegistry, TocNode, ValidationError};
pub use search::{SearchHit, Snippet, search};
