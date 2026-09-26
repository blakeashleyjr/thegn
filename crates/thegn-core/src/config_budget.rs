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
pub const MAX_CONTEXT_BYTES: usize = 16 * 1024;
pub const MAX_ENV_ENTRIES: usize = 1_024;
pub const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
pub const MAX_ENV_BYTES: usize = 1024 * 1024;
pub const MAX_DIAGNOSTICS: usize = 256;
pub const MAX_DIAGNOSTIC_BYTES: usize = 200;
pub const MAX_WORK: usize = 262_144;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerKind {
    Array,
    InlineTable,
}

#[derive(Debug, Clone, Copy)]
struct Container {
    kind: ContainerKind,
    members: usize,
    value_started: bool,
    saved_key_depth: usize,
    saved_key_has_token: bool,
    saved_in_key: bool,
    key_prefix_depth: usize,
}

fn note_array_value(containers: &mut [Container], nodes: &mut usize) -> Result<(), BudgetError> {
    let Some(container) = containers.last_mut() else {
        return Ok(());
    };
    if container.kind != ContainerKind::Array || container.value_started {
        return Ok(());
    }
    container.value_started = true;
    container.members = container
        .members
        .checked_add(1)
        .ok_or(BudgetError::Members)?;
    if container.members > MAX_MEMBERS {
        return Err(BudgetError::Members);
    }
    *nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
    if *nodes > MAX_NODES {
        return Err(BudgetError::Nodes);
    }
    Ok(())
}

/// Count physical source bytes independently from the TOML token walk.  The
/// token walk is allowed to skip a three-byte multiline-string delimiter, but
/// a delimiter is still part of the physical line and must consume line
/// budget.  Keeping this pass separate also makes the line limit independent
/// of parser-state recovery on malformed input.
fn check_physical_bounds(bytes: &[u8]) -> Result<(), BudgetError> {
    let mut line_bytes = 0usize;
    let mut lines = 1usize;
    for &byte in bytes {
        if byte == b'\n' {
            lines = lines.checked_add(1).ok_or(BudgetError::Lines)?;
            if lines > MAX_LINES {
                return Err(BudgetError::Lines);
            }
            line_bytes = 0;
        } else {
            line_bytes = line_bytes.checked_add(1).ok_or(BudgetError::LineBytes)?;
            if line_bytes > MAX_LINE_BYTES {
                return Err(BudgetError::LineBytes);
            }
        }
    }
    Ok(())
}

/// Scan a UTF-8 TOML source after the caller has checked its encoding.
/// Strings and comments are skipped before structural tokens are counted, so
/// hostile-looking text inside a quoted value cannot consume structural budget.
pub fn scan(bytes: &[u8]) -> Result<(), BudgetError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(BudgetError::SourceBytes);
    }
    check_physical_bounds(bytes)?;

    let mut depth = 0usize;
    let mut tables = 0usize;
    let mut nodes = 0usize;
    let mut work = 0usize;
    let mut key_depth = 0usize;
    let mut key_has_token = false;
    let mut in_key = true;
    let mut statement_start = true;
    let mut table_header_closer = 0usize;
    let mut table_path_depth = 0usize;
    let mut scope_members = 0usize;
    let mut containers: Vec<Container> = Vec::new();
    let mut string: Option<(StringKind, usize)> = None;
    let mut escaped = false;
    let mut comment = false;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];

        if byte == b'\n' {
            if containers.is_empty() {
                key_depth = 0;
                key_has_token = false;
                in_key = true;
                statement_start = true;
                table_header_closer = 0;
            } else if containers
                .last()
                .is_some_and(|c| c.kind == ContainerKind::InlineTable)
            {
                key_depth = containers
                    .last()
                    .map_or(0, |container| container.key_prefix_depth);
                key_has_token = false;
                in_key = true;
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
                let mut width = if kind.multiline() { 3 } else { 1 };
                if kind.multiline() {
                    // TOML permits one or two literal quotes immediately
                    // before the three-quote terminator. Consume that complete
                    // run instead of treating its fourth quote as a new string.
                    while width < 5 && bytes.get(index + width) == Some(&byte) {
                        width += 1;
                    }
                    if string_bytes.saturating_add(width - 3) > MAX_STRING_BYTES {
                        return Err(BudgetError::StringBytes);
                    }
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
                if in_key && !key_has_token {
                    key_has_token = true;
                    key_depth = 1;
                } else if !in_key {
                    note_array_value(&mut containers, &mut nodes)?;
                }
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
            b'[' if statement_start && containers.is_empty() => {
                // `[[name]]` is one array-of-tables header, not two nested
                // containers.  A `[[` occurring after `=` is handled by the
                // value-array branch below, one opener at a time.
                let width = if bytes.get(index..index + 2) == Some(b"[[") {
                    2
                } else {
                    1
                };
                tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if tables > MAX_TABLES {
                    return Err(BudgetError::Tables);
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                key_depth = 0;
                table_path_depth = 0;
                key_has_token = false;
                in_key = true;
                table_header_closer = width;
                statement_start = false;
                index += width;
            }
            b'[' => {
                if !in_key {
                    note_array_value(&mut containers, &mut nodes)?;
                }
                tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if tables > MAX_TABLES {
                    return Err(BudgetError::Tables);
                }
                depth = depth.checked_add(1).ok_or(BudgetError::Depth)?;
                if table_path_depth + depth + key_depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                containers.push(Container {
                    kind: ContainerKind::Array,
                    members: 0,
                    value_started: false,
                    saved_key_depth: key_depth,
                    saved_key_has_token: key_has_token,
                    saved_in_key: in_key,
                    key_prefix_depth: key_depth,
                });
                statement_start = false;
                in_key = false;
                index += 1;
            }
            b'{' => {
                if !in_key {
                    note_array_value(&mut containers, &mut nodes)?;
                }
                tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if tables > MAX_TABLES {
                    return Err(BudgetError::Tables);
                }
                depth = depth.checked_add(1).ok_or(BudgetError::Depth)?;
                if table_path_depth + depth + key_depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                containers.push(Container {
                    kind: ContainerKind::InlineTable,
                    members: 0,
                    value_started: false,
                    saved_key_depth: key_depth,
                    saved_key_has_token: key_has_token,
                    saved_in_key: in_key,
                    key_prefix_depth: key_depth,
                });
                key_depth = containers
                    .last()
                    .map_or(0, |container| container.key_prefix_depth);
                key_has_token = false;
                in_key = true;
                statement_start = false;
                index += 1;
            }
            b']' if table_header_closer != 0 => {
                table_header_closer -= 1;
                if table_header_closer == 0 {
                    if key_depth > MAX_DEPTH {
                        return Err(BudgetError::Depth);
                    }
                    table_path_depth = key_depth;
                    if table_path_depth > MAX_DEPTH {
                        return Err(BudgetError::Depth);
                    }
                    scope_members = 0;
                    in_key = false;
                }
                index += 1;
            }
            b']' | b'}' => {
                let Some(container) = containers.pop() else {
                    // Let TOML parsing report an unmatched closer.  The
                    // scanner must not turn malformed syntax into a false
                    // budget failure.
                    index += 1;
                    continue;
                };
                depth = depth.checked_sub(1).ok_or(BudgetError::Depth)?;
                if byte == b']' && container.kind != ContainerKind::Array {
                    index += 1;
                    continue;
                }
                if byte == b'}' && container.kind != ContainerKind::InlineTable {
                    index += 1;
                    continue;
                }
                if container.kind == ContainerKind::InlineTable {
                    key_depth = container.saved_key_depth;
                    key_has_token = container.saved_key_has_token;
                    in_key = container.saved_in_key;
                }
                index += 1;
            }
            b'=' => {
                in_key = false;
                key_has_token = false;
                nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
                if nodes > MAX_NODES {
                    return Err(BudgetError::Nodes);
                }
                let members = if let Some(container) = containers.last_mut() {
                    if container.kind == ContainerKind::InlineTable {
                        &mut container.members
                    } else {
                        &mut scope_members
                    }
                } else {
                    &mut scope_members
                };
                *members = members.checked_add(1).ok_or(BudgetError::Members)?;
                if *members > MAX_MEMBERS {
                    return Err(BudgetError::Members);
                }
                index += 1;
            }
            b',' => {
                if let Some(container) = containers.last_mut()
                    && container.kind == ContainerKind::Array
                {
                    // A comma terminates a member; a trailing comma must not
                    // invent an additional value.  Members are charged when
                    // their first token is seen, so arrays with and without a
                    // trailing comma have identical accounting.
                    container.value_started = false;
                }
                if containers
                    .last()
                    .is_some_and(|c| c.kind == ContainerKind::InlineTable)
                {
                    key_depth = containers
                        .last()
                        .map_or(0, |container| container.key_prefix_depth);
                    key_has_token = false;
                    in_key = true;
                }
                index += 1;
            }
            b'.' if in_key => {
                if !key_has_token {
                    key_has_token = true;
                    key_depth = 1;
                }
                key_depth = key_depth.checked_add(1).ok_or(BudgetError::Depth)?;
                if table_path_depth + containers.len() + key_depth > MAX_DEPTH {
                    return Err(BudgetError::Depth);
                }
                index += 1;
            }
            b' ' | b'\t' | b'\r' => index += 1,
            _ => {
                if in_key && !byte.is_ascii_whitespace() {
                    key_has_token = true;
                    if key_depth == 0 {
                        key_depth = 1;
                    }
                    if table_path_depth + containers.len() + key_depth > MAX_DEPTH {
                        return Err(BudgetError::Depth);
                    }
                }
                if !in_key && !byte.is_ascii_whitespace() {
                    note_array_value(&mut containers, &mut nodes)?;
                }
                if statement_start && !byte.is_ascii_whitespace() {
                    statement_start = false;
                }
                index += 1;
            }
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
    let mut entry_bytes = entry_bytes;
    if entry_bytes.any(|bytes| bytes > max_entry_bytes) {
        return Err(BudgetError::EntryBytes);
    }
    Ok(())
}

/// Apply the decoded collection/string/depth limits immediately after TOML
/// parsing and before converting the tree to JSON or recursively merging it.
pub fn check_toml_value(value: &toml::Value) -> Result<(), BudgetError> {
    fn walk(
        value: &toml::Value,
        depth: usize,
        nodes: &mut usize,
        tables: &mut usize,
    ) -> Result<(), BudgetError> {
        *nodes = nodes.checked_add(1).ok_or(BudgetError::Nodes)?;
        if *nodes > MAX_NODES || depth > MAX_DEPTH {
            return Err(if depth > MAX_DEPTH {
                BudgetError::Depth
            } else {
                BudgetError::Nodes
            });
        }
        match value {
            toml::Value::String(value) => {
                if value.len() > MAX_STRING_BYTES {
                    return Err(BudgetError::StringBytes);
                }
            }
            toml::Value::Array(values) => {
                if values.len() > MAX_MEMBERS {
                    return Err(BudgetError::Members);
                }
                for value in values {
                    walk(value, depth + 1, nodes, tables)?;
                }
            }
            toml::Value::Table(values) => {
                *tables = tables.checked_add(1).ok_or(BudgetError::Tables)?;
                if *tables > MAX_TABLES || values.len() > MAX_MEMBERS {
                    return Err(if *tables > MAX_TABLES {
                        BudgetError::Tables
                    } else {
                        BudgetError::Members
                    });
                }
                for (key, value) in values {
                    if key.len() > MAX_STRING_BYTES {
                        return Err(BudgetError::StringBytes);
                    }
                    walk(value, depth + 1, nodes, tables)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut nodes = 0;
    let mut tables = 0;
    walk(value, 0, &mut nodes, &mut tables)
}
