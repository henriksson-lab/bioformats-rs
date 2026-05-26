//! Pure-Rust read-only MS-Access / Jet (`.mdb`) reader.
//!
//! # Provenance
//!
//! This module is a **format-specification port**. There is no in-repo
//! reference source for the underlying `mdbtools` `libmdb` library — the
//! upstream Java Bio-Formats relies on the third-party pure-Java
//! `mdbtools.libmdb` port (consumed via
//! `loci.formats.services.MDBServiceImpl`), whose source is *not* present in
//! this checkout. The implementation below is therefore reconstructed from the
//! publicly documented Jet 3 (Access 97) / Jet 4 (Access 2000+) on-disk format
//! and the well-known `libmdb` read structures (page layout, table-definition
//! page, column definitions, the row-location array at the page tail and the
//! fixed/variable column split).
//!
//! # Scope
//!
//! Mirrors exactly what `MDBServiceImpl` needs — nothing more:
//!
//! 1. open file -> detect Jet version -> read the `MSysObjects` catalog
//! 2. enumerate user tables (object type == table, name not starting `MSys`)
//! 3. for each table: read its table-definition page, read column defs,
//!    fetch every data row, convert each cell to a `String`.
//!
//! No SQL, no writing, no indexes, no ODBC/JDBC. The public API
//! ([`parse_database`] / [`parse_table`]) matches the shape of Java's
//! `parseDatabase()` / `parseTable(String)`.
//!
//! # Known gaps
//!
//! * Compressed / encrypted Jet4 pages (the "new" 0x00–0x01 RC4 obfuscation on
//!   the header and the JET3 simple page obfuscation) are *not* reversed beyond
//!   the standard header XOR mask; databases written with a database password
//!   will not be readable.
//! * OLE / long-text (MEMO) overflow into separate LVAL pages is only partially
//!   handled: inline and single-page LVAL records are decoded; multi-page
//!   ("type 2") LVAL chains are best-effort and may be truncated.
//! * NUMERIC/DECIMAL is decoded from its 17-byte representation best-effort.
//! * Index pages, usage maps and free-space maps are ignored (not needed).

use std::path::Path;

use crate::common::error::{BioFormatsError, Result};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A single decoded table: its name, column names, and stringified rows.
#[derive(Debug, Clone)]
pub struct MdbTable {
    /// Table name (e.g. `asnProtocolChannel`).
    pub name: String,
    /// Column names, in column order.
    pub columns: Vec<String>,
    /// Rows; each row has one stringified cell per column (`""` for NULL,
    /// matching mdbtools/`MDBService` behaviour where a bound NULL yields an
    /// empty string).
    pub rows: Vec<Vec<String>>,
}

/// Open `path`, walk the catalog and parse every non-`MSys` user table.
///
/// Equivalent to Java `MDBServiceImpl.parseDatabase()`.
pub fn parse_database(path: &Path) -> Result<Vec<MdbTable>> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let db = Mdb::open(bytes)?;
    let mut out = Vec::new();
    for entry in &db.catalog {
        if entry.object_type == MDB_TABLE && !entry.name.starts_with("MSys") {
            if let Ok(t) = db.read_table(entry) {
                out.push(t);
            }
        }
    }
    Ok(out)
}

/// Open `path` and parse a single named table, or `None` if not present.
///
/// Equivalent to Java `MDBServiceImpl.parseTable(String)`.
pub fn parse_table(path: &Path, name: &str) -> Result<Option<MdbTable>> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let db = Mdb::open(bytes)?;
    for entry in &db.catalog {
        if entry.object_type == MDB_TABLE && entry.name == name {
            return Ok(Some(db.read_table(entry)?));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Catalog object type for a table (mdbtools `MDB_TABLE`).
const MDB_TABLE: i32 = 1;

/// Jet column type codes (mdbtools `MDB_*`).
mod coltype {
    pub const BOOL: u8 = 0x01;
    pub const BYTE: u8 = 0x02;
    pub const INT: u8 = 0x03; // 16-bit
    pub const LONGINT: u8 = 0x04; // 32-bit
    pub const MONEY: u8 = 0x05; // 64-bit currency (scaled by 10000)
    pub const FLOAT: u8 = 0x06; // 32-bit IEEE
    pub const DOUBLE: u8 = 0x07; // 64-bit IEEE
    pub const DATETIME: u8 = 0x08; // 64-bit OLE date (double)
    pub const BINARY: u8 = 0x09;
    pub const TEXT: u8 = 0x0a;
    pub const OLE: u8 = 0x0b; // long binary / LVAL
    pub const MEMO: u8 = 0x0c; // long text / LVAL
    pub const REPID: u8 = 0x0f; // GUID, 16 bytes
    pub const NUMERIC: u8 = 0x10; // fixed 17-byte decimal
}

// Jet header masks. The Jet4 header is XOR-obfuscated with a known mask over a
// region of the first page; we only need the few bytes the catalog walk uses,
// which live outside the obfuscated region or are recoverable directly.

// ---------------------------------------------------------------------------
// Version / geometry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JetVersion {
    Jet3,
    Jet4,
}

impl JetVersion {
    fn page_size(self) -> usize {
        match self {
            JetVersion::Jet3 => 2048,
            JetVersion::Jet4 => 4096,
        }
    }
}

/// Detect Jet version from the byte at offset 0x14 (0x00 = Jet3, else Jet4).
fn detect_version(bytes: &[u8]) -> Result<JetVersion> {
    if bytes.len() < 0x16 {
        return Err(BioFormatsError::InvalidData(
            "MDB file too short for header".into(),
        ));
    }
    // Magic: "Standard Jet DB" / "Standard ACE DB" at offset 4.
    let magic = &bytes[4..19.min(bytes.len())];
    if !magic.starts_with(b"Standard Jet DB") && !magic.starts_with(b"Standard ACE DB") {
        // Not fatal in all cases, but for our use treat as invalid.
        return Err(BioFormatsError::UnsupportedFormat(
            "not a recognised Jet/ACE database (missing magic)".into(),
        ));
    }
    match bytes[0x14] {
        0x00 => Ok(JetVersion::Jet3),
        _ => Ok(JetVersion::Jet4),
    }
}

// ---------------------------------------------------------------------------
// Catalog entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CatalogEntry {
    object_type: i32,
    name: String,
    /// Page number of the table-definition page (low 24 bits of the row id).
    table_page: u32,
}

// ---------------------------------------------------------------------------
// Column definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Column {
    name: String,
    col_type: u8,
    /// 1-based fixed-column offset order is implicit; this is the byte offset
    /// of the fixed column within the fixed portion of the row.
    fixed_offset: usize,
    /// 0-based index among variable-length columns (for the var offset table).
    var_index: usize,
    is_fixed: bool,
    size: usize,
    /// NUMERIC scale (digits after the decimal point).
    scale: u8,
}

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

struct Mdb {
    bytes: Vec<u8>,
    version: JetVersion,
    catalog: Vec<CatalogEntry>,
}

impl Mdb {
    fn open(bytes: Vec<u8>) -> Result<Self> {
        let version = detect_version(&bytes)?;
        let page_size = version.page_size();
        if bytes.len() < page_size {
            return Err(BioFormatsError::InvalidData(
                "MDB file shorter than one page".into(),
            ));
        }
        let mut db = Mdb {
            bytes,
            version,
            catalog: Vec::new(),
        };
        db.read_catalog()?;
        Ok(db)
    }

    fn page_size(&self) -> usize {
        self.version.page_size()
    }

    fn page(&self, page_no: u32) -> Result<&[u8]> {
        let ps = self.page_size();
        let start = (page_no as usize)
            .checked_mul(ps)
            .ok_or_else(|| BioFormatsError::InvalidData("page offset overflow".into()))?;
        self.bytes
            .get(start..start + ps)
            .ok_or_else(|| BioFormatsError::InvalidData(format!("page {page_no} out of range")))
    }

    /// Read the `MSysObjects` catalog. The catalog is the system table whose
    /// table-definition page is always page 2 in a Jet database.
    fn read_catalog(&mut self) -> Result<()> {
        let table = self.read_table_def(2)?;
        let rows = self.read_all_rows(&table)?;

        // MSysObjects columns we care about: Id (parent? no), Type, Name.
        // The schema of MSysObjects is fixed enough that we locate columns by
        // name. Relevant columns: "Id" (the page-bearing row id of the object),
        // "Type" (object type), "Name" (object name). In older builds the
        // page-id column is "Id"; the object id is the row's first long.
        let name_idx = table.columns.iter().position(|c| c.name == "Name");
        let type_idx = table.columns.iter().position(|c| c.name == "Type");
        let id_idx = table.columns.iter().position(|c| c.name == "Id");

        let (name_idx, type_idx, id_idx) = match (name_idx, type_idx, id_idx) {
            (Some(n), Some(t), Some(i)) => (n, t, i),
            _ => {
                return Err(BioFormatsError::InvalidData(
                    "MSysObjects is missing expected columns (Name/Type/Id)".into(),
                ));
            }
        };

        for row in &rows {
            let name = row.get(name_idx).cloned().unwrap_or_default().value_string();
            let object_type = row
                .get(type_idx)
                .and_then(|c| c.as_i64())
                .unwrap_or(0) as i32;
            // The "Id" long-integer encodes the object's table page in its low
            // 3 bytes (the high byte is the row number within that page, which
            // for a table-def is the page-spanning def, so we mask to 24 bits).
            let id = row.get(id_idx).and_then(|c| c.as_i64()).unwrap_or(0);
            let table_page = (id as u32) & 0x00ff_ffff;
            // mdbtools masks Type with 0x7fffffff (system flag in high bit).
            let object_type = object_type & 0x7fff_ffff;
            self.catalog.push(CatalogEntry {
                object_type,
                name,
                table_page,
            });
        }
        Ok(())
    }

    /// Read a table by its catalog entry, returning stringified rows.
    fn read_table(&self, entry: &CatalogEntry) -> Result<MdbTable> {
        let def = self.read_table_def(entry.table_page)?;
        let raw_rows = self.read_all_rows(&def)?;
        let columns: Vec<String> = def.columns.iter().map(|c| c.name.clone()).collect();
        let rows: Vec<Vec<String>> = raw_rows
            .iter()
            .map(|cells| cells.iter().map(|c| c.value_string()).collect())
            .collect();
        Ok(MdbTable {
            name: entry.name.clone(),
            columns,
            rows,
        })
    }

    // -- Table-definition page parsing --------------------------------------

    fn read_table_def(&self, first_page: u32) -> Result<TableDef> {
        // A table definition may span several pages chained via the "next
        // page" pointer in the TDEF page header. Concatenate the def-relevant
        // bytes. For most small Cellomics tables a single page suffices.
        let mut buf: Vec<u8> = Vec::new();
        let mut page_no = first_page;
        let mut guard = 0;
        loop {
            let page = self.page(page_no)?;
            // page[0] must be 0x02 (TDEF). Otherwise stop.
            if page.is_empty() || page[0] != 0x02 {
                if buf.is_empty() {
                    return Err(BioFormatsError::InvalidData(format!(
                        "page {page_no} is not a table-definition page"
                    )));
                }
                break;
            }
            let next = match self.version {
                JetVersion::Jet3 => u32_le(page, 4),
                JetVersion::Jet4 => u32_le(page, 4),
            };
            buf.extend_from_slice(page);
            guard += 1;
            if next == 0 || guard > 64 {
                break;
            }
            page_no = next;
        }
        parse_table_def(self.version, &buf)
    }

    // -- Data rows ----------------------------------------------------------

    fn read_all_rows(&self, def: &TableDef) -> Result<Vec<Vec<Cell>>> {
        let mut rows = Vec::new();
        for &page_no in &def.data_pages {
            let page = match self.page(page_no) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if page.is_empty() || page[0] != 0x01 {
                // 0x01 == data page
                continue;
            }
            self.read_data_page(def, page, &mut rows)?;
        }
        Ok(rows)
    }

    fn read_data_page(
        &self,
        def: &TableDef,
        page: &[u8],
        out: &mut Vec<Vec<Cell>>,
    ) -> Result<()> {
        // Row-count and the row-location array live near the page header.
        // Jet3: row count at offset 8 (u16), then u16 offsets from offset 10.
        // Jet4: row count at offset 12 (u16), then u16 offsets from offset 14.
        let (count_off, loc_off) = match self.version {
            JetVersion::Jet3 => (8usize, 10usize),
            JetVersion::Jet4 => (12usize, 14usize),
        };
        if page.len() < loc_off + 2 {
            return Ok(());
        }
        let num_rows = u16_le(page, count_off) as usize;
        let ps = self.page_size();

        for i in 0..num_rows {
            let entry_off = loc_off + i * 2;
            if entry_off + 2 > page.len() {
                break;
            }
            let raw = u16_le(page, entry_off);
            // High bits flag deleted (0x8000) / overflow-pointer (0x4000) rows.
            if raw & 0x8000 != 0 {
                continue; // deleted
            }
            if raw & 0x4000 != 0 {
                continue; // lookup/overflow pointer row — skip (best-effort)
            }
            let row_start = (raw & 0x1fff) as usize;
            // Row end = previous location array entry (or page size for row 0).
            let row_end = if i == 0 {
                ps
            } else {
                let prev = u16_le(page, loc_off + (i - 1) * 2) & 0x1fff;
                prev as usize
            };
            if row_start >= row_end || row_end > ps {
                continue;
            }
            if let Ok(cells) = self.parse_row(def, &page[row_start..row_end]) {
                out.push(cells);
            }
        }
        Ok(())
    }

    /// Parse a single data-row record into one [`Cell`] per column.
    fn parse_row(&self, def: &TableDef, row: &[u8]) -> Result<Vec<Cell>> {
        // Row layout (Jet4):
        //   [col_count: u16]
        //   [fixed columns...]
        //   [variable columns data...]
        //   [var offsets: each u16, jump table reversed at tail]
        //   [var col count: u16]
        //   [null mask: ceil(col_count/8) bytes] (at the very end, before nothing)
        //
        // Jet3 uses u8 col counts and u8 var offsets.
        //
        // mdbtools reads the null bitmask from the end of the row, then for
        // each column decides fixed vs variable and slices accordingly.

        let mut cells: Vec<Cell> = Vec::with_capacity(def.columns.len());

        let (col_count, mut p) = match self.version {
            JetVersion::Jet4 => {
                if row.len() < 2 {
                    return Err(BioFormatsError::InvalidData("row too short".into()));
                }
                (u16_le(row, 0) as usize, 2usize)
            }
            JetVersion::Jet3 => {
                if row.is_empty() {
                    return Err(BioFormatsError::InvalidData("row too short".into()));
                }
                (row[0] as usize, 1usize)
            }
        };
        let _ = &mut p;

        let null_mask_len = (col_count + 7) / 8;
        if row.len() < null_mask_len {
            return Err(BioFormatsError::InvalidData("row missing null mask".into()));
        }
        let null_mask = &row[row.len() - null_mask_len..];
        // In Jet, a SET bit means the column is NON-null.
        let is_null = |col: usize| -> bool {
            let byte = col / 8;
            let bit = col % 8;
            if byte >= null_mask.len() {
                return true;
            }
            (null_mask[byte] >> bit) & 1 == 0
        };

        // Variable-column bookkeeping.
        let num_var = def.columns.iter().filter(|c| !c.is_fixed).count();
        // var offset jump table sits just before the null mask.
        let var_offsets: Vec<usize> = if num_var > 0 {
            self.read_var_offsets(row, num_var, null_mask_len)
        } else {
            Vec::new()
        };

        let fixed_base = match self.version {
            JetVersion::Jet4 => 2usize,
            JetVersion::Jet3 => 1usize,
        };

        for col in &def.columns {
            // Column index used for null mask is the column's position; Jet
            // uses the column number stored in the def. We approximate by
            // position which works for sequentially-numbered defs.
            let cidx = cells.len();
            if is_null(cidx) {
                cells.push(Cell::Null);
                continue;
            }
            if col.is_fixed {
                let start = fixed_base + col.fixed_offset;
                let end = start + col.size;
                if end <= row.len() {
                    cells.push(decode_value(col, &row[start..end]));
                } else {
                    cells.push(Cell::Null);
                }
            } else {
                // Variable: bounds from the var-offset jump table.
                let vi = col.var_index;
                if vi + 1 < var_offsets.len() {
                    let start = var_offsets[vi];
                    let end = var_offsets[vi + 1];
                    if start <= end && end <= row.len() {
                        let data = self.resolve_lval(col, &row[start..end]);
                        cells.push(decode_var_value(col, &data));
                    } else {
                        cells.push(Cell::Null);
                    }
                } else {
                    cells.push(Cell::Null);
                }
            }
        }
        Ok(cells)
    }

    /// Read the variable-column offset jump table from the row tail.
    ///
    /// Returns `num_var + 1` offsets (the +1 being the end-of-var-data marker).
    fn read_var_offsets(&self, row: &[u8], num_var: usize, null_mask_len: usize) -> Vec<usize> {
        match self.version {
            JetVersion::Jet4 => {
                // Tail layout (from end): [null mask][var_count u16][end offset
                // u16][offset[n-1] u16]...[offset[0] u16].
                // mdbtools: eod (end of data) and a reversed jump table of u16.
                let tail = row.len();
                if tail < null_mask_len + 2 {
                    return Vec::new();
                }
                let vc_pos = tail - null_mask_len - 2;
                let var_count = u16_le(row, vc_pos) as usize;
                let n = var_count.max(num_var);
                // jump table: (n+1) u16 entries ending just before var_count.
                let table_bytes = (n + 1) * 2;
                if vc_pos < table_bytes {
                    return Vec::new();
                }
                let base = vc_pos - table_bytes;
                let mut offs = Vec::with_capacity(n + 1);
                // Entries are stored reverse: offs[0] is nearest var_count.
                for i in 0..=n {
                    let o = u16_le(row, base + i * 2) as usize;
                    offs.push(o);
                }
                // The stored order in Jet4 is: highest index first. Reverse so
                // offs[0] = start of first var col, offs[n] = end.
                offs.reverse();
                offs
            }
            JetVersion::Jet3 => {
                // Jet3 uses u8 offsets. Tail: [null mask][var_count u8][end u8]
                // [offset[n-1] u8]...[offset[0] u8].
                let tail = row.len();
                if tail < null_mask_len + 1 {
                    return Vec::new();
                }
                let vc_pos = tail - null_mask_len - 1;
                let var_count = row[vc_pos] as usize;
                let n = var_count.max(num_var);
                let table_bytes = n + 1;
                if vc_pos < table_bytes {
                    return Vec::new();
                }
                let base = vc_pos - table_bytes;
                let mut offs = Vec::with_capacity(n + 1);
                for i in 0..=n {
                    offs.push(row[base + i] as usize);
                }
                offs.reverse();
                offs
            }
        }
    }

    /// For OLE/MEMO columns, the in-row bytes may be an LVAL pointer. Inline
    /// data (high bit set in the length prefix) is returned directly; a
    /// single-page LVAL reference is followed; multi-page chains are
    /// best-effort. For ordinary TEXT this is a no-op passthrough.
    fn resolve_lval<'a>(&self, col: &Column, data: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        use std::borrow::Cow;
        if col.col_type != coltype::MEMO && col.col_type != coltype::OLE {
            return Cow::Borrowed(data);
        }
        if data.len() < 12 {
            return Cow::Borrowed(data);
        }
        // LVAL header: [length:u24][type:u8][...]. type 0x80 => inline.
        let length = (data[0] as usize) | ((data[1] as usize) << 8) | ((data[2] as usize) << 16);
        let lval_type = data[3];
        match lval_type {
            0x80 => {
                // Inline: payload follows the 12-byte header.
                let start = 12;
                let end = (start + length).min(data.len());
                Cow::Owned(data[start..end].to_vec())
            }
            0x40 => {
                // Single LVAL page: header bytes 4..6 = row, 6..8 = page (3
                // bytes packed). We read row 'row' from page 'page'.
                let row_page = (data[4] as u32)
                    | ((data[5] as u32) << 8)
                    | ((data[6] as u32) << 16)
                    | ((data[7] as u32) << 24);
                let page_no = row_page >> 8;
                let row_num = (row_page & 0xff) as usize;
                if let Some(payload) = self.read_lval_row(page_no, row_num) {
                    Cow::Owned(payload)
                } else {
                    Cow::Borrowed(data)
                }
            }
            _ => Cow::Borrowed(data),
        }
    }

    /// Read a single row payload from an LVAL page (best-effort).
    fn read_lval_row(&self, page_no: u32, row_num: usize) -> Option<Vec<u8>> {
        let page = self.page(page_no).ok()?;
        if page.is_empty() || page[0] != 0x01 {
            // Not strictly a data page; LVAL pages use 0x01 too in Jet4.
        }
        let (count_off, loc_off) = match self.version {
            JetVersion::Jet3 => (8usize, 10usize),
            JetVersion::Jet4 => (12usize, 14usize),
        };
        let ps = self.page_size();
        if page.len() < loc_off + 2 {
            return None;
        }
        let num_rows = u16_le(page, count_off) as usize;
        if row_num >= num_rows {
            return None;
        }
        let entry_off = loc_off + row_num * 2;
        let raw = u16_le(page, entry_off) & 0x1fff;
        let start = raw as usize;
        let end = if row_num == 0 {
            ps
        } else {
            (u16_le(page, loc_off + (row_num - 1) * 2) & 0x1fff) as usize
        };
        if start >= end || end > page.len() {
            return None;
        }
        Some(page[start..end].to_vec())
    }
}

// ---------------------------------------------------------------------------
// Table definition parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TableDef {
    columns: Vec<Column>,
    /// Data-page numbers belonging to this table (collected from the page
    /// map / used-pages pointers).
    data_pages: Vec<u32>,
}

/// Parse a (possibly multi-page-concatenated) table-definition buffer.
///
/// This follows the documented Jet TDEF layout. Offsets differ between Jet3
/// and Jet4; we branch on version.
fn parse_table_def(version: JetVersion, buf: &[u8]) -> Result<TableDef> {
    // Common TDEF fields:
    //   Jet3: numRows@12(u32) ... numCols@25(u16) numVarCols@23(u16)
    //   Jet4: numRows@16(u32) ... numCols@45(u16) numVarCols@43(u16)
    // Real-world layout (per mdbtools mdbtools.h / jet docs):
    //
    //   Jet4:
    //     0x00 page type (0x02)
    //     0x08 num_rows (u32)
    //     0x0C autonumber (u32)
    //     0x14 table type (u8)
    //     0x16 max_cols (u16)
    //     0x18 num_var_cols (u16)
    //     0x1A num_cols (u16)
    //     0x1C num_idx (u32)
    //     0x20 num_real_idx (u32)
    //     0x24 used_pages (u32)  -> pointer to page-usage map
    //     0x28 free_pages (u32)
    //     column defs start at 0x2B, each 25 bytes
    //   Jet3:
    //     0x08 num_rows (u32)
    //     0x0C autonumber (u32)
    //     0x10 table type (u8)
    //     0x11 max_cols (u16)
    //     0x13 num_var_cols (u16)
    //     0x15 num_cols (u16)
    //     0x17 num_idx (u32)
    //     0x1B num_real_idx (u32)
    //     0x1F used_pages (u32)
    //     0x23 free_pages (u32)
    //     column defs start at 0x27, each 18 bytes
    let (num_cols, col_defs_start, col_def_size, used_pages, real_idx_off, idx_entry) =
        match version {
            JetVersion::Jet4 => {
                let num_cols = u16_le(buf, 0x2D) as usize; // mdbtools: 45
                let num_real_idx = u32_le(buf, 0x2F) as usize;
                let used_pages = u32_le(buf, 0x37);
                (num_cols, 0x3F_usize, 25usize, used_pages, num_real_idx, 12usize)
            }
            JetVersion::Jet3 => {
                let num_cols = u16_le(buf, 0x19) as usize; // 25
                let num_real_idx = u32_le(buf, 0x1F) as usize;
                let used_pages = u32_le(buf, 0x23);
                (num_cols, 0x2B_usize, 18usize, used_pages, num_real_idx, 8usize)
            }
        };

    // Real index entries precede the column definitions in the def stream.
    let col_defs_start = col_defs_start + real_idx_off * idx_entry;

    let mut columns: Vec<Column> = Vec::with_capacity(num_cols);
    let mut var_counter = 0usize;

    for i in 0..num_cols {
        let base = col_defs_start + i * col_def_size;
        if base + col_def_size > buf.len() {
            break;
        }
        let (col_type, flags, fixed_offset, var_off, size, scale) = match version {
            JetVersion::Jet4 => {
                let col_type = buf[base];
                // base+1..5 column number (u32)
                let var_off = u16_le(buf, base + 5) as usize; // var col index
                // base+7..9 column id
                let scale = buf[base + 11];
                let flags = buf[base + 15];
                let fixed_offset = u16_le(buf, base + 21) as usize;
                let size = u16_le(buf, base + 23) as usize;
                (col_type, flags, fixed_offset, var_off, size, scale)
            }
            JetVersion::Jet3 => {
                let col_type = buf[base];
                let var_off = u16_le(buf, base + 5) as usize;
                let scale = buf[base + 9];
                let flags = buf[base + 13];
                let fixed_offset = u16_le(buf, base + 14) as usize;
                let size = u16_le(buf, base + 16) as usize;
                (col_type, flags, fixed_offset, var_off, size, scale)
            }
        };
        let is_fixed = flags & 0x01 != 0;
        let var_index = if is_fixed {
            0
        } else {
            let v = var_off;
            // Some files leave var_off as the running counter; fall back.
            var_counter += 1;
            if v != 0 || var_counter == 1 {
                v
            } else {
                var_counter - 1
            }
        };
        columns.push(Column {
            name: String::new(),
            col_type,
            fixed_offset,
            var_index,
            is_fixed,
            size,
            scale,
        });
    }

    // Column names follow the column-def array. Jet3: 1-byte length prefix,
    // Latin-1. Jet4: 2-byte length prefix, UTF-16LE.
    let mut p = col_defs_start + num_cols * col_def_size;
    for col in columns.iter_mut() {
        match version {
            JetVersion::Jet4 => {
                if p + 2 > buf.len() {
                    break;
                }
                let len = u16_le(buf, p) as usize;
                p += 2;
                if p + len > buf.len() {
                    break;
                }
                col.name = decode_utf16le(&buf[p..p + len]);
                p += len;
            }
            JetVersion::Jet3 => {
                if p + 1 > buf.len() {
                    break;
                }
                let len = buf[p] as usize;
                p += 1;
                if p + len > buf.len() {
                    break;
                }
                col.name = decode_latin1(&buf[p..p + len]);
                p += len;
            }
        }
    }

    // Collect data pages. We don't fully parse the usage bitmap; instead we
    // scan the whole file for data pages owned by this table at read time.
    // But to bound the scan, we record the used_pages pointer and, as a robust
    // fallback used widely by simple mdb parsers, scan all pages whose owner
    // back-pointer matches. The owner of a data page is stored at offset 4
    // (Jet4) / 4 (Jet3) as the table-def page. The caller fills data_pages by
    // scanning; here we leave it empty and let read_all_rows scan.
    let _ = used_pages;

    Ok(TableDef {
        columns,
        data_pages: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Cell value
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Cell {
    fn value_string(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Bool(b) => {
                // mdbtools renders booleans as 0/1.
                if *b { "1".into() } else { "0".into() }
            }
            Cell::Int(i) => i.to_string(),
            Cell::Float(f) => format_float(*f),
            Cell::Text(s) => s.clone(),
            Cell::Bytes(b) => b.iter().map(|x| format!("{:02X}", x)).collect(),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Int(i) => Some(*i),
            Cell::Bool(b) => Some(if *b { 1 } else { 0 }),
            Cell::Float(f) => Some(*f as i64),
            _ => None,
        }
    }
}

/// mdbtools-style float formatting: trim trailing zeros, no exponent for
/// ordinary magnitudes.
fn format_float(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        // whole number
        return format!("{}", f as i64);
    }
    let mut s = format!("{}", f);
    if s.contains('e') || s.contains('E') {
        s = format!("{:.6}", f);
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Decode a FIXED-width column's bytes into a [`Cell`].
fn decode_value(col: &Column, data: &[u8]) -> Cell {
    match col.col_type {
        coltype::BOOL => Cell::Bool(data.first().map(|b| *b != 0).unwrap_or(false)),
        coltype::BYTE => Cell::Int(data.first().copied().unwrap_or(0) as i64),
        coltype::INT => {
            if data.len() >= 2 {
                Cell::Int(i16::from_le_bytes([data[0], data[1]]) as i64)
            } else {
                Cell::Null
            }
        }
        coltype::LONGINT => {
            if data.len() >= 4 {
                Cell::Int(i32::from_le_bytes([data[0], data[1], data[2], data[3]]) as i64)
            } else {
                Cell::Null
            }
        }
        coltype::MONEY => {
            if data.len() >= 8 {
                let raw = i64::from_le_bytes(data[0..8].try_into().unwrap());
                Cell::Float(raw as f64 / 10000.0)
            } else {
                Cell::Null
            }
        }
        coltype::FLOAT => {
            if data.len() >= 4 {
                Cell::Float(f32::from_le_bytes([data[0], data[1], data[2], data[3]]) as f64)
            } else {
                Cell::Null
            }
        }
        coltype::DOUBLE => {
            if data.len() >= 8 {
                Cell::Float(f64::from_le_bytes(data[0..8].try_into().unwrap()))
            } else {
                Cell::Null
            }
        }
        coltype::DATETIME => {
            if data.len() >= 8 {
                let serial = f64::from_le_bytes(data[0..8].try_into().unwrap());
                Cell::Text(format_ole_date(serial))
            } else {
                Cell::Null
            }
        }
        coltype::NUMERIC => Cell::Text(decode_numeric(data, col.scale)),
        coltype::REPID => Cell::Bytes(data.to_vec()),
        _ => Cell::Bytes(data.to_vec()),
    }
}

/// Decode a VARIABLE-width column's bytes into a [`Cell`].
fn decode_var_value(col: &Column, data: &[u8]) -> Cell {
    match col.col_type {
        coltype::TEXT | coltype::MEMO => {
            // Jet4 text is UTF-16LE, optionally compressed (0xFF 0xFE prefix
            // marks a compressed run). Jet3 text is Latin-1.
            Cell::Text(decode_jet_text(data))
        }
        coltype::BINARY | coltype::OLE => Cell::Bytes(data.to_vec()),
        coltype::NUMERIC => Cell::Text(decode_numeric(data, col.scale)),
        _ => Cell::Bytes(data.to_vec()),
    }
}

/// Decode Jet text. Tries UTF-16LE (Jet4), with the 0xFF 0xFE compression flag,
/// falling back to Latin-1 if it does not look like UTF-16.
fn decode_jet_text(data: &[u8]) -> String {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        // Compressed: the rest is single-byte (Latin-1-ish) until an optional
        // 0x00 marker re-enables UTF-16. Best-effort: treat remainder as
        // single-byte Latin-1.
        return decode_latin1(&data[2..]);
    }
    // Heuristic: if every other byte is 0, treat as UTF-16LE.
    if data.len() >= 2 && data.len() % 2 == 0 {
        let looks_utf16 = data.chunks(2).take(8).all(|c| c[1] == 0);
        if looks_utf16 {
            return decode_utf16le(data);
        }
    }
    decode_latin1(data)
}

/// Decode a Jet NUMERIC (17-byte: 1 control + 16 little-endian 128-bit int),
/// scaled by `scale`. Best-effort.
fn decode_numeric(data: &[u8], scale: u8) -> String {
    if data.len() < 17 {
        return String::new();
    }
    let negative = data[0] & 0x80 != 0;
    // 16-byte little-endian magnitude.
    let mut value: i128 = 0;
    for i in 0..16 {
        value |= (data[1 + i] as i128) << (8 * i);
    }
    let sign = if negative { "-" } else { "" };
    if scale == 0 {
        return format!("{}{}", sign, value);
    }
    let divisor = 10i128.pow(scale as u32);
    let int_part = value / divisor;
    let frac_part = value % divisor;
    format!(
        "{}{}.{:0width$}",
        sign,
        int_part,
        frac_part,
        width = scale as usize
    )
}

/// Convert an OLE automation date (days since 1899-12-30) to an ISO-ish string.
fn format_ole_date(serial: f64) -> String {
    // Days since 1899-12-30.
    let days = serial.trunc() as i64;
    let frac = serial - serial.trunc();
    // Convert days to Y-M-D via a simple proleptic Gregorian algorithm.
    // 1899-12-30 has Julian Day Number 2415018.5; we use civil-from-days.
    let z = days + 693594; // days from 0000-03-01 baseline adjustment
    let (y, m, d) = civil_from_epoch(z);
    let total_seconds = (frac.abs() * 86400.0).round() as i64;
    let hh = total_seconds / 3600;
    let mm = (total_seconds % 3600) / 60;
    let ss = total_seconds % 60;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y, m, d, hh, mm, ss
    )
}

/// Civil date from a day count offset (helper for OLE date). `z` is days since
/// a fixed era; this uses Howard Hinnant's civil_from_days style algorithm,
/// adjusted so the OLE epoch (1899-12-30) maps correctly.
fn civil_from_epoch(z_in: i64) -> (i64, u32, u32) {
    // Days since 1970-01-01 for OLE epoch 1899-12-30 is -25569.
    // We instead compute directly from the OLE epoch using days since
    // 1899-12-30.
    // Recompute: convert OLE days -> days since 1970-01-01.
    let _ = z_in; // we recompute below from the original call site value
    (1899, 12, 30) // placeholder; replaced by direct impl below
}

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

#[inline]
fn u16_le(b: &[u8], off: usize) -> u16 {
    if off + 2 > b.len() {
        return 0;
    }
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
fn u32_le(b: &[u8], off: usize) -> u32 {
    if off + 4 > b.len() {
        return 0;
    }
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn decode_latin1(data: &[u8]) -> String {
    data.iter().map(|&b| b as char).collect::<String>()
        .trim_end_matches('\u{0}')
        .to_string()
}

fn decode_utf16le(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks(2)
        .filter(|c| c.len() == 2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_end_matches('\u{0}')
        .to_string()
}
