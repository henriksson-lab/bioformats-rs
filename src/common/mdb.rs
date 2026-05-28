//! MS-Access / Jet (`.mdb`, `.accdb`) table reader.
//!
//! Bio-Formats' Java code accesses Access databases through `MDBServiceImpl`.
//! This module provides the same small service shape for Rust callers, backed
//! by the translated `mdbtools-rs` crate rather than the older in-tree partial
//! Jet parser.

use std::path::Path;

use crate::common::error::{BioFormatsError, Result};

/// A single decoded table: its name, column names, and stringified rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdbTable {
    /// Table name.
    pub name: String,
    /// Column names, in column order.
    pub columns: Vec<String>,
    /// Rows; each row has one stringified cell per column (`""` for NULL).
    pub rows: Vec<Vec<String>>,
}

/// Open `path`, walk the catalog and parse every non-system user table.
///
/// Equivalent to Java `MDBServiceImpl.parseDatabase()`.
pub fn parse_database(path: &Path) -> Result<Vec<MdbTable>> {
    let mut db = open_db(path)?;
    let table_names = user_table_names(&mut db)?;
    let mut tables = Vec::with_capacity(table_names.len());
    for name in table_names {
        if let Some(table) = read_named_table(&mut db, &name) {
            tables.push(table);
        }
    }
    Ok(tables)
}

/// Open `path` and parse a single named table, or `None` if not present.
///
/// Equivalent to Java `MDBServiceImpl.parseTable(String)`.
pub fn parse_table(path: &Path, name: &str) -> Result<Option<MdbTable>> {
    let mut db = open_db(path)?;
    Ok(read_named_table(&mut db, name))
}

fn open_db(path: &Path) -> Result<mdbtools::safe::Db> {
    mdbtools::safe::Db::open(path).map_err(|e| {
        BioFormatsError::UnsupportedFormat(format!(
            "MDB database could not be opened by mdbtools-rs: {} ({e})",
            path.display()
        ))
    })
}

fn user_table_names(db: &mut mdbtools::safe::Db) -> Result<Vec<String>> {
    let catalog = db.read_catalog(mdbtools::MDB_TABLE).ok_or_else(|| {
        BioFormatsError::InvalidData("MDB catalog could not be read by mdbtools-rs".into())
    })?;
    Ok(catalog
        .iter()
        .filter(|entry| entry.is_user_table())
        .map(|entry| entry.name().to_string())
        .collect())
}

fn read_named_table(db: &mut mdbtools::safe::Db, name: &str) -> Option<MdbTable> {
    let mut table = db.read_table(name)?;
    let columns: Vec<String> = table.columns().map(|c| c.name().to_string()).collect();
    let rows: Vec<Vec<String>> = table.rows().collect();
    Some(MdbTable {
        name: table.name().to_string(),
        columns,
        rows,
    })
}
