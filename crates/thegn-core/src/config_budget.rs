//! Bounded, allocation-free admission checks for captured TOML sources.
//!
//! This is deliberately a byte scanner rather than a TOML parser wrapper.  A
//! caller can reject an oversized or structurally hostile source before TOML
//! normalization, parsing, or cloning allocates a decoded tree.

use std::fmt;

pub const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_COMBINED_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const MAX_LINES: usize = 65_536;
pub const MAX_DEPTH: usize = 64;
pub const MAX_TABLES: usize = 4_096;
pub const MAX_MEMBERS: usize = 16_384;
pub const MAX_NODES: usize = 65_536;
pub const MAX_STRING_BYTES: usize = 64 * 1024;
pub const MAX_NORMALIZED_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CLI_ENTRIES: usize = 1_024;
pub const MAX_CLI_ENTRY_BYTES: usize = 16 * 1024;
pub const MAX_CLI_BYTES: usize = 1024 * 1024;
pub const MAX_ENV_ENTRIES: usize = 1_024;
pub const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_ENV_BYTES: usize = 1024 * 1024;
pub const MAX_DIAGNOSTICS: usize = 256;
pub const MAX_DIAGNOSTIC_BYTES: usize = 200;
pub const MAX_WORK: usize = 131_072;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    SourceBytes,
    CombinedSourceBytes,
    LineBytes,
    Lines,
    Depth,
    Tables,
    Members,
    Nodes,
    StringBytes,
    Work,
    Entries,
    EntryBytes,
    AggregateBytes,
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::SourceBytes => "source exceeds the byte limit",
            Self::CombinedSourceBytes => "base and profile sources exceed the combined byte limit",
            Self::LineBytes => "source contains a line exceeding the byte limit",
            Self::Lines => "source exceeds the physical line limit",
            Self::Depth => "source exceeds the structural depth limit",
            Self::Tables => "source exceeds the table/container limit",
            Self::Members => "source exceeds the members-per-container limit",
            Self::Nodes => "source exceeds the decoded node limit",
            Self::StringBytes => "source contains a string exceeding the byte limit",
            Self::Work => "source exceeds the deterministic scanner work limit",
            Self::Entries => "layer exceeds the entry limit",
            Self::EntryBytes => "layer contains an entry exceeding the byte limit",
            Self::AggregateBytes => "layer exceeds the aggregate byte limit",
        };
        f.write_str(message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StringKind {
    Basic,
    Literal,
    MultilineBasic,
    MultilineLiteral,
}

impl StringKind {
    fn multiline(self) -> bool {
        matches!(self, Self::MultilineBasic | Self::MultilineLiteral)
    }

    fn escaped(self) -> bool {
        matches!(self, Self::Basic | Self::MultilineBasic)
    }
}

#[derive(Debug, Clone, Copy)]
struct Container {
    members: usize,
}

/// Scan a UTF-8 TOML source after the caller has checked its encoding.
/// Strings and comments are skipped before structural tokens are counted, so
/// hostile-looking text inside a quoted value cannot consume structural budget.
pub fn scan(bytes: &[u8]) -> Result<(), BudgetError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(BudgetError::SourceBytes);
    }

    let mut line_bytes = 0usize;
    let mut lines = 1usize;
    let mut depth = 0usize;
    let mut tables = 0usize;
    let mut nodes = 0usize;
    let mut work = 0usize;
    let mut key_depth = 0usize;
    let mut has_equals = false;
    let mut containers = Vec::new();
    let mut string: Option<(StringKind, usize)> = None;
    let mut escaped = false;
    let mut comment = false;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];

        if byte == b'\n' {
            line_bytes = 0;
            lines = lines.checked_add(1).ok_or(BudgetError::Lines)?;
            if lines > MAX_LINES {
                return Err(BudgetError::Lines);
            }
            if containers.is_empty() {
                key_depth = 0;
                has_equals = false;
            }
        } else {
            line_bytes = line_bytes.checked_add(1).ok_or(BudgetError::LineBytes)?;
            if line_bytes > MAX_LINE_BYTES {
                return Err(BudgetError::LineBytes);
            }
        }

        if let Some((kind, string_bytes)) = string {
            work = work.checked_add(1).ok_or(BudgetError::Work)?;
            if work > MAX_WORK {
                return Err(BudgetError::Work);
            }
            if kind.escaped() && escaped {
                escaped = false;
                let next_len = string_bytes
                    .checked_add(1)
                    .ok_or(BudgetError::StringBytes)?;
                if next_len > MAX_STRING_BYTES {
                    return Err(BudgetError::StringBytes);
                }
                string = Some((kind, next_len));
                index += 1;
                continue;
            }
            if kind.escaped() && byte == b'\\' {
                escaped = true;
                let next_len = string_bytes
                    .checked_add(1)
                    .ok_or(BudgetError::StringBytes)?;
                if next_len > MAX_STRING_BYTES {
                    return Err(BudgetError::StringBytes);
                }
                string = Some((kind, next_len));
                index += 1;
                continue;
            }

            let closing = match kind {
                StringKind::Basic => byte == b'"',
                StringKind::Literal => byte == b'\'',
                StringKind::MultilineBasic => bytes.get(index..index + 3) == Some(b"\"\"\""),
                StringKind::MultilineLiteral => bytes.get(index..index + 3) == Some(b"'''"),
            };
            if closing {
                let width = if kind.multiline() { 3 } else { 1 };
                if kind.multiline() && bytes.get(index..index + 3).is_none() {
                    return Err(BudgetError::StringBytes);
                }
                string = None;
                escaped = false;
                index += width;
                continue;
            }
            let next_len = string_bytes
                .checked_add(1)
                .ok_or(BudgetError::StringBytes)?;
            if next_len > MAX_STRING_BYTES {
                return Err(BudgetError::StringBytes);
            }
            string = Some((kind, next_len));
            index += 1;
            continue;
        }

        if comment {
            if byte == b'\n' {
                comment = false;
            }
            index += 1;
            continue;
        }

        if !byte.is_ascii_whitespace() {
            work = work.checked_add(1).ok_or(BudgetError::Work)?;
            if work > MAX_WORK {
                return Err(BudgetError::Work);
            }
        }

        match byte {
            b'#' => comment = true,
            b'"' | b'\'' => {
                let (kind, width) = if bytes.get(index..index + 3) == Some(b"\"\"\"") {
                    (StringKind::MultilineBasic, 3)
                } else if bytes.get(index..index + 3) == Some(b"'''") {
                    (StringKind::MultilineLiteral, 3)
                } else if byte == b'"' {
                    (StringKind::Basic, 1)
                } else {
                    (StringKind::Literal, 1)
                };
                string = Some((kind, 0));
                escaped = false;
                index += width;
                continue;
            }
            b'[' => {
                let width = if bytes.get(index..index + 2) == Some(b"[[") {
                    2
                } else {
                    1
                };
                tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if tables > MAX_TABLES {
                    return Err(BudgetError::Tables);
                }
                depth = depth.checked_add(1 + key_depth).ok_or(BudgetError::Depth)?;
                if depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                containers.push(Container { members: 0 });
                key_depth = 0;
                has_equals = false;
                index += width;
            }
            b'{' => {
                tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if tables > MAX_TABLES {
                    return Err(BudgetError::Tables);
                }
                depth = depth.checked_add(1).ok_or(BudgetError::Depth)?;
                if depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                containers.push(Container { members: 0 });
                index += 1;
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1 + key_depth);
                containers.pop();
                key_depth = 0;
                has_equals = false;
                index += 1;
            }
            b'=' => {
                has_equals = true;
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                if let Some(container) = containers.last_mut() {
                    container.members = container
                        .members
                        .checked_add(1)
                        .ok_or(BudgetError::Members)?;
                    if container.members > MAX_MEMBERS {
                        return Err(BudgetError::Members);
                    }
                }
                index += 1;
            }
            b',' => {
                if let Some(container) = containers.last_mut() {
                    container.members = container
                        .members
                        .checked_add(1)
                        .ok_or(BudgetError::Members)?;
                    if container.members > MAX_MEMBERS {
                        return Err(BudgetError::Members);
                    }
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                index += 1;
            }
            b'.' if !has_equals => {
                key_depth = key_depth.checked_add(1).ok_or(BudgetError::Depth)?;
                if key_depth + depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }

    if string.is_some() {
        return Err(BudgetError::StringBytes);
    }
    Ok(())
}

pub fn check_entries(
    entries: usize,
    aggregate_bytes: usize,
    entry_bytes: impl Iterator<Item = usize>,
    max_entries: usize,
    max_entry_bytes: usize,
    max_aggregate_bytes: usize,
) -> Result<(), BudgetError> {
    if entries > max_entries {
        return Err(BudgetError::Entries);
    }
    if aggregate_bytes > max_aggregate_bytes {
        return Err(BudgetError::AggregateBytes);
    }
    if entry_bytes.any(|bytes| bytes > max_entry_bytes) {
        return Err(BudgetError::EntryBytes);
    }
    Ok(())
}
