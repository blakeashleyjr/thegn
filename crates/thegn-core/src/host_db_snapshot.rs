//! Strict host capture on an already-authorized connection (THE-602).
//!
//! No opener, connection policy changes or migrations. Limits bound application
//! copies and ordinary cursor work, not SQLite page memory, filesystem latency
//! or arbitrary query execution time. The caller owns off-loop scheduling and
//! any connection busy deadline; lock timeouts are not execution deadlines.

use std::collections::BTreeSet;

use rusqlite::{Connection, Row, types::ValueRef};

use crate::host_definition_snapshot::{
    HostDefinitionReadError as Error, HostDefinitionsSnapshot, MAX_HOST_DEFINITION_BYTES,
    MAX_HOST_DEFINITIONS, MAX_HOST_NAME_BYTES, MAX_HOST_ROWS, MAX_HOST_SCHEMA_COLUMNS, add_bytes,
    validate_name,
};

pub(crate) fn read(conn: &Connection) -> Result<HostDefinitionsSnapshot, Error> {
    read_with_snapshot_hook(conn, || Ok(()))
}

fn sql_error(error: rusqlite::Error) -> Error {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            Error::Busy
        }
        _ => Error::Unavailable,
    }
}

fn text<'a>(row: &'a Row<'_>, index: usize, limit: usize) -> Result<&'a str, Error> {
    let ValueRef::Text(bytes) = row.get_ref(index).map_err(sql_error)? else {
        return Err(Error::InvalidColumnType);
    };
    if bytes.len() > limit {
        return Err(Error::Bounds);
    }
    std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)
}

fn schema(conn: &Connection) -> Result<i64, Error> {
    // This is deliberately INSIDE the same transaction as every later read.
    let version: i64 = conn
        .query_row("PRAGMA main.user_version", [], |row| row.get(0))
        .map_err(sql_error)?;
    if version != crate::db::SCHEMA_VERSION {
        return Err(Error::IncompatibleSchema {
            observed: version,
            supported: crate::db::SCHEMA_VERSION,
        });
    }
    // table_list distinguishes ordinary/virtual/view without copying arbitrary
    // sqlite_schema.sql. main qualification defeats temporary-name shadowing.
    let mut table_stmt = conn.prepare(
        "SELECT schema, name, type, ncol FROM pragma_table_list('hosts') WHERE schema = 'main' LIMIT 2"
    ).map_err(sql_error)?;
    let mut tables = table_stmt.query([]).map_err(sql_error)?;
    let Some(table) = tables.next().map_err(sql_error)? else {
        return Err(Error::InvalidSchema);
    };
    if text(table, 0, 16)? != "main"
        || text(table, 1, MAX_HOST_NAME_BYTES)? != "hosts"
        || text(table, 2, 16)? != "table"
    {
        return Err(Error::InvalidSchema);
    }
    let ncol: i64 = table.get(3).map_err(sql_error)?;
    if !(3..=MAX_HOST_SCHEMA_COLUMNS as i64).contains(&ncol)
        || tables.next().map_err(sql_error)?.is_some()
    {
        return Err(Error::InvalidSchema);
    }
    let mut columns_stmt = conn
        .prepare("SELECT name, type, pk, hidden FROM pragma_table_xinfo('hosts', 'main') LIMIT ?1")
        .map_err(sql_error)?;
    let mut columns = columns_stmt
        .query([MAX_HOST_SCHEMA_COLUMNS as i64 + 1])
        .map_err(sql_error)?;
    let mut names = BTreeSet::new();
    let mut required = [false; 3];
    while let Some(column) = columns.next().map_err(sql_error)? {
        if names.len() >= MAX_HOST_SCHEMA_COLUMNS {
            return Err(Error::InvalidSchema);
        }
        // Bound borrowed schema strings before copying; don't allocate a Vec
        // of attacker-controlled schema names or SQL before checking lengths.
        let name = text(column, 0, MAX_HOST_NAME_BYTES)?;
        let kind = text(column, 1, 64)?;
        let pk: i64 = column.get(2).map_err(sql_error)?;
        let hidden: i64 = column.get(3).map_err(sql_error)?;
        if name.is_empty() || hidden != 0 || !names.insert(name.to_owned()) {
            return Err(Error::InvalidSchema);
        }
        let required_index = match name {
            "host_id" => Some(0),
            "name" => Some(1),
            "config_json" => Some(2),
            _ => None,
        };
        if let Some(index) = required_index {
            if !kind.eq_ignore_ascii_case("TEXT") || pk != i64::from(index == 0) {
                return Err(Error::InvalidSchema);
            }
            required[index] = true;
        }
    }
    if required != [true; 3] || names.len() as i64 != ncol {
        return Err(Error::InvalidSchema);
    }
    Ok(version)
}

fn read_with_snapshot_hook(
    conn: &Connection,
    after_schema: impl FnOnce() -> Result<(), Error>,
) -> Result<HostDefinitionsSnapshot, Error> {
    if !conn.is_autocommit() {
        return Err(Error::TransactionActive);
    }
    let transaction = conn.unchecked_transaction().map_err(sql_error)?;
    let version = schema(&transaction)?;
    after_schema()?;
    let mut copied = Vec::new();
    let mut names = BTreeSet::new();
    let mut bytes = 0;
    {
        // No WHERE or ORDER BY: sparse NULL rows cannot cause an unbounded scan
        // or sort before LIMIT. The extra row makes exhaustion observable.
        let mut statement = transaction.prepare(
            "SELECT config_json IS NULL, typeof(name), typeof(config_json),
                    length(CAST(name AS BLOB)), length(CAST(config_json AS BLOB)),
                    CASE WHEN typeof(name) = 'text' AND length(CAST(name AS BLOB)) <= ?1 THEN name END,
                    CASE WHEN typeof(config_json) = 'text' AND length(CAST(config_json AS BLOB)) <= ?2 THEN config_json END
             FROM main.hosts LIMIT ?3"
        ).map_err(sql_error)?;
        let mut rows = statement
            .query(rusqlite::params![
                MAX_HOST_NAME_BYTES as i64,
                MAX_HOST_DEFINITION_BYTES as i64,
                MAX_HOST_ROWS as i64 + 1,
            ])
            .map_err(sql_error)?;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(sql_error)? {
            count += 1;
            if count > MAX_HOST_ROWS {
                return Err(Error::TooManyRows);
            }
            let no_definition: bool = row.get(0).map_err(sql_error)?;
            if no_definition {
                continue;
            }
            if copied.len() >= MAX_HOST_DEFINITIONS {
                return Err(Error::TooManyDefinitions);
            }
            if text(row, 1, 16)? != "text" || text(row, 2, 16)? != "text" {
                return Err(Error::InvalidColumnType);
            }
            let name_len: i64 = row.get(3).map_err(sql_error)?;
            let json_len: i64 = row.get(4).map_err(sql_error)?;
            let name_len = usize::try_from(name_len).map_err(|_| Error::Bounds)?;
            let json_len = usize::try_from(json_len).map_err(|_| Error::Bounds)?;
            bytes = add_bytes(bytes, name_len, json_len)?;
            let name = text(row, 5, MAX_HOST_NAME_BYTES)?;
            let json = text(row, 6, MAX_HOST_DEFINITION_BYTES)?;
            if name.len() != name_len || json.len() != json_len {
                return Err(Error::Bounds);
            }
            validate_name(name)?;
            if !names.insert(name.to_owned()) {
                return Err(Error::DuplicateName);
            }
            copied.push((name.to_owned(), json.to_owned()));
        }
    }
    transaction.commit().map_err(sql_error)?;
    // Decode captured bytes after closing the read transaction, not while
    // holding a database read lock. Later writers cannot change these bytes.
    HostDefinitionsSnapshot::from_raw(version, copied)
}

#[cfg(test)]
#[path = "host_db_snapshot_tests.rs"]
mod tests;
