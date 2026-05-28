use std::fmt;
use std::fs;
use std::path::Path;

pub type Result<T> = std::result::Result<T, MetakitError>;

#[derive(Debug)]
pub enum MetakitError {
    Io(std::io::Error),
    InvalidFormat(String),
    UnexpectedEof { needed: usize, position: usize },
    InvalidUtf8(std::str::Utf8Error),
}

impl fmt::Display for MetakitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::InvalidFormat(msg) => write!(f, "{msg}"),
            Self::UnexpectedEof { needed, position } => {
                write!(f, "unexpected EOF reading {needed} bytes at {position}")
            }
            Self::InvalidUtf8(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for MetakitError {}

impl From<std::io::Error> for MetakitError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<std::str::Utf8Error> for MetakitError {
    fn from(err: std::str::Utf8Error) -> Self {
        Self::InvalidUtf8(err)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    name: String,
    type_string: String,
}

impl Column {
    pub fn new(definition: &str) -> Result<Self> {
        let Some(separator) = definition.find(':') else {
            return Err(MetakitError::InvalidFormat(format!(
                "invalid column definition: {definition}"
            )));
        };

        Ok(Self {
            name: definition[..separator].to_owned(),
            type_string: definition[separator + 1..].to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn type_string(&self) -> &str {
        &self.type_string
    }

    pub fn column_type(&self) -> ColumnType {
        match self.type_string.as_bytes().first().copied() {
            Some(b'S') => ColumnType::String,
            Some(b'I') => ColumnType::Integer,
            Some(b'F') => ColumnType::Float,
            Some(b'D') => ColumnType::Double,
            Some(b'B') => ColumnType::Bytes,
            Some(b'L') => ColumnType::Long,
            _ => ColumnType::Unknown,
        }
    }

    fn is_fixed_map(&self) -> bool {
        !matches!(self.type_string.as_bytes().first(), Some(b'S' | b'B'))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    String,
    Integer,
    Float,
    Double,
    Bytes,
    Long,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Integer(i32),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
    Long(i64),
}

#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    columns: Vec<Column>,
    row_count: usize,
    data: Vec<Vec<Option<Value>>>,
}

impl Table {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column_names(&self) -> Vec<&str> {
        self.columns.iter().map(Column::name).collect()
    }

    pub fn column_types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(Column::column_type).collect()
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn row(&self, row_index: usize) -> Option<Vec<Option<Value>>> {
        if row_index >= self.row_count {
            return None;
        }

        Some(
            self.data
                .iter()
                .map(|column| column.get(row_index).cloned().flatten())
                .collect(),
        )
    }

    pub fn rows(&self) -> Vec<Vec<Option<Value>>> {
        (0..self.row_count)
            .filter_map(|row_index| self.row(row_index))
            .collect()
    }

    pub fn column_data(&self) -> &[Vec<Option<Value>>] {
        &self.data
    }
}

#[derive(Debug, Clone)]
pub struct MetakitReader {
    tables: Vec<Table>,
    little_endian: bool,
}

impl MetakitReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut stream = ByteReader::new(bytes);
        let magic = stream.read_string(2)?;
        let little_endian = match magic.as_str() {
            "JL" => true,
            "LJ" => false,
            _ => {
                return Err(MetakitError::InvalidFormat(format!(
                    "Invalid magic string; got {magic}"
                )))
            }
        };
        stream.set_little_endian(little_endian);

        if stream.read_u8()? != 26 {
            return Err(MetakitError::InvalidFormat(
                "'valid' flag was set to 'false'".to_owned(),
            ));
        }

        let header_type = stream.read_u8()?;
        if header_type != 0 {
            return Err(MetakitError::InvalidFormat(format!(
                "Header type {header_type} is not valid."
            )));
        }

        let footer_pointer = stream.read_i32_be()? as i64 - 16;
        stream.seek(usize::try_from(footer_pointer).map_err(|_| {
            MetakitError::InvalidFormat(format!("negative footer pointer: {footer_pointer}"))
        })?)?;

        stream.skip(4)?;
        let _header_location = stream.read_i32_be()?;
        stream.skip(4)?;
        let toc_location = stream.read_i32_be()?;

        stream.seek(usize::try_from(toc_location).map_err(|_| {
            MetakitError::InvalidFormat(format!("negative TOC pointer: {toc_location}"))
        })?)?;

        let tables = read_toc(&mut stream, little_endian)?;

        Ok(Self {
            tables,
            little_endian,
        })
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(Table::name).collect()
    }

    pub fn table(&self, table_index: usize) -> Option<&Table> {
        self.tables.get(table_index)
    }

    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    pub fn table_by_name(&self, table_name: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.name == table_name)
    }

    pub fn column_names(&self, table_index: usize) -> Option<Vec<&str>> {
        self.table(table_index).map(Table::column_names)
    }

    pub fn column_types(&self, table_index: usize) -> Option<Vec<ColumnType>> {
        self.table(table_index).map(Table::column_types)
    }

    pub fn row_count(&self, table_index: usize) -> Option<usize> {
        self.table(table_index).map(Table::row_count)
    }

    pub fn row_count_by_table(&self, table_name: &str) -> Option<usize> {
        self.table_by_name(table_name).map(Table::row_count)
    }

    pub fn table_data(&self, table_index: usize) -> Option<Vec<Vec<Option<Value>>>> {
        self.table(table_index).map(Table::rows)
    }

    pub fn row_data(&self, row_index: usize, table_index: usize) -> Option<Vec<Option<Value>>> {
        self.table(table_index)
            .and_then(|table| table.row(row_index))
    }

    pub fn little_endian(&self) -> bool {
        self.little_endian
    }
}

fn read_toc(stream: &mut ByteReader<'_>, little_endian: bool) -> Result<Vec<Table>> {
    let _toc_marker = read_bp_int(stream)?;
    let structure_definition = read_p_string(stream)?;

    let table_defs: Vec<&str> = structure_definition.split("],").collect();
    let mut parsed_tables = Vec::with_capacity(table_defs.len());

    for table_def in table_defs {
        let open_bracket = table_def.find('[').ok_or_else(|| {
            MetakitError::InvalidFormat(format!("invalid table definition: {table_def}"))
        })?;
        let name = table_def[..open_bracket].to_owned();
        let column_list = &table_def[open_bracket + 1..];
        let has_subviews = column_list.contains('[');

        let column_start = column_list.find('[').map_or(0, |index| index + 1);
        let columns = column_list[column_start..]
            .split(',')
            .filter(|column| !column.is_empty())
            .map(Column::new)
            .collect::<Result<Vec<_>>>()?;

        parsed_tables.push((name, columns, has_subviews));
    }

    let _row_count_marker = read_bp_int(stream)?;
    let mut tables = Vec::with_capacity(parsed_tables.len());

    for (name, columns, has_subviews) in parsed_tables {
        let _table_marker = read_bp_int(stream)?;
        let pointer = read_bp_int(stream)?;
        let fp = stream.position();
        let data_pointer = usize::try_from(pointer + 1).map_err(|_| {
            MetakitError::InvalidFormat(format!("negative data pointer: {}", pointer + 1))
        })?;
        stream.seek(data_pointer)?;

        let mut row_count = usize::try_from(read_bp_int(stream)?)
            .map_err(|_| MetakitError::InvalidFormat("negative row count".to_owned()))?;
        let data = if has_subviews {
            read_subview_table(stream, &columns, &mut row_count, little_endian)?
        } else if row_count > 0 {
            let mut data = Vec::with_capacity(columns.len());
            for column in &columns {
                data.push(read_column_map(column, stream, row_count, little_endian)?);
            }
            data
        } else {
            vec![Vec::new(); columns.len()]
        };

        tables.push(Table {
            name,
            columns,
            row_count,
            data,
        });
        stream.seek(fp)?;
    }

    Ok(tables)
}

fn read_subview_table(
    stream: &mut ByteReader<'_>,
    columns: &[Column],
    row_count: &mut usize,
    little_endian: bool,
) -> Result<Vec<Vec<Option<Value>>>> {
    let subview_count = *row_count;
    *row_count = 0;
    let mut subview_table = vec![vec![Vec::new(); columns.len()]; subview_count];

    for (subview, subview_columns) in subview_table.iter_mut().enumerate() {
        if subview == 0 {
            let _size = read_bp_int(stream)?;
            let subview_pointer = read_bp_int(stream)?;
            stream.seek(usize::try_from(subview_pointer).map_err(|_| {
                MetakitError::InvalidFormat(format!("negative subview pointer: {subview_pointer}"))
            })?)?;
        }

        let _marker = read_bp_int(stream)?;
        let count = usize::try_from(read_bp_int(stream)?)
            .map_err(|_| MetakitError::InvalidFormat("negative subview row count".to_owned()))?;

        if count > 1 {
            *row_count += count;
            for (column_index, column) in columns.iter().enumerate() {
                subview_columns[column_index] =
                    read_column_map(column, stream, count, little_endian)?;
            }
        }
    }

    let mut data = vec![Vec::with_capacity(*row_count); columns.len()];
    for subview in subview_table {
        if subview.first().is_some_and(|column| !column.is_empty()) {
            for (column_index, column) in subview.into_iter().enumerate() {
                data[column_index].extend(column);
            }
        }
    }
    Ok(data)
}

fn read_column_map(
    column: &Column,
    stream: &mut ByteReader<'_>,
    row_count: usize,
    little_endian: bool,
) -> Result<Vec<Option<Value>>> {
    stream.set_little_endian(little_endian);
    if column.is_fixed_map() {
        let ivec_size = read_bp_int(stream)?;
        if ivec_size > 0 {
            let ivec_pointer = read_bp_int(stream)?;
            let fp = stream.position();
            stream.seek(usize::try_from(ivec_pointer).map_err(|_| {
                MetakitError::InvalidFormat(format!("negative vector pointer: {ivec_pointer}"))
            })?)?;

            let mut values = Vec::with_capacity(row_count);
            for index in 0..row_count {
                values.push(read_element(
                    column,
                    usize::try_from(ivec_size).unwrap_or(0),
                    index,
                    row_count,
                    stream,
                    little_endian,
                )?);
            }
            stream.seek(fp)?;
            Ok(values)
        } else {
            Ok(vec![None; row_count])
        }
    } else {
        let ivec_size = read_bp_int(stream)?;
        let ivec_pointer = read_bp_int(stream)?;
        let map_ivec_size = read_bp_int(stream)?;
        let map_ivec_pointer = read_bp_int(stream)?;
        let catalog_ivec_size = read_bp_int(stream)?;
        if catalog_ivec_size > 0 {
            let _catalog_ivec_pointer = read_bp_int(stream)?;
        }

        let fp = stream.position();
        stream.seek(usize::try_from(map_ivec_pointer).map_err(|_| {
            MetakitError::InvalidFormat(format!("negative map pointer: {map_ivec_pointer}"))
        })?)?;

        let map_ivec_size = usize::try_from(map_ivec_size)
            .map_err(|_| MetakitError::InvalidFormat("negative map vector size".to_owned()))?;
        let bits = (map_ivec_size * 8) / row_count;
        let mut byte_counts = Vec::with_capacity(row_count);
        for index in 0..row_count {
            byte_counts.push(read_bits(stream, map_ivec_size, bits, index)?);
        }

        stream.seek(usize::try_from(ivec_pointer).map_err(|_| {
            MetakitError::InvalidFormat(format!("negative vector pointer: {ivec_pointer}"))
        })?)?;

        let mut values = Vec::with_capacity(row_count);
        for byte_count in byte_counts {
            let bytes = stream.read_bytes(byte_count as usize)?;
            match column.column_type() {
                ColumnType::Bytes => values.push(Some(Value::Bytes(bytes.to_vec()))),
                ColumnType::String => {
                    values.push(Some(Value::String(std::str::from_utf8(bytes)?.to_owned())))
                }
                _ => values.push(None),
            }
        }

        stream.seek(fp)?;
        let _ = ivec_size;
        Ok(values)
    }
}

fn read_element(
    column: &Column,
    vector_size: usize,
    index: usize,
    row_count: usize,
    stream: &mut ByteReader<'_>,
    little_endian: bool,
) -> Result<Option<Value>> {
    match column.column_type() {
        ColumnType::Float => Ok(Some(Value::Float(stream.read_f32(little_endian)?))),
        ColumnType::Double => Ok(Some(Value::Double(stream.read_f64(little_endian)?))),
        ColumnType::Long => Ok(Some(Value::Long(stream.read_i64(little_endian)?))),
        ColumnType::Integer => {
            let bits = (vector_size * 8) / row_count;
            let value = match bits {
                1 | 2 | 4 => read_bits(stream, vector_size, bits, index)?,
                8 => i32::from(stream.read_u8()?),
                16 => i32::from(stream.read_i16(little_endian)?),
                32 => stream.read_i32(little_endian)?,
                _ => stream.read_i32(little_endian)?,
            };
            Ok(Some(Value::Integer(value)))
        }
        _ => Ok(None),
    }
}

pub fn read_p_string(stream: &mut ByteReader<'_>) -> Result<String> {
    let string_length = read_bp_int(stream)?;
    if string_length < 0 {
        return Err(MetakitError::InvalidFormat(format!(
            "negative string length: {string_length}"
        )));
    }
    stream.read_string(string_length as usize)
}

pub fn read_bp_int(stream: &mut ByteReader<'_>) -> Result<i32> {
    let sign_byte = stream.read_u8()?;
    let negative = sign_byte == 0;
    let data_byte = if negative {
        stream.read_u8()?
    } else {
        sign_byte
    };
    let mut stop_byte = data_byte;
    let mut data_bytes = Vec::new();

    while (stop_byte & 0x80) == 0 {
        if data_bytes.len() >= 4 {
            return Err(MetakitError::InvalidFormat(
                "overlong byte-packed integer".to_owned(),
            ));
        }
        data_bytes.push(stop_byte);
        stop_byte = stream.read_u8()?;
    }

    let mut value = 0i32;
    for (index, byte) in data_bytes.iter().enumerate() {
        let shift = (data_bytes.len() - index) * 7;
        value |= i32::from(*byte) << shift;
    }
    value |= i32::from(stop_byte & 0x7f);

    if negative {
        value = !value;
    }

    Ok(value)
}

fn read_bits(
    stream: &mut ByteReader<'_>,
    _n_bytes: usize,
    bits: usize,
    index: usize,
) -> Result<i32> {
    if bits == 8 {
        return Ok(i32::from(stream.read_u8()?));
    } else if bits == 16 {
        return Ok(i32::from(stream.read_u16(stream.little_endian)?));
    } else if bits >= 32 {
        return stream.read_i32(stream.little_endian);
    }

    let fp = stream.position();
    stream.skip((index * bits) / 8)?;
    let byte = stream.read_u8()?;
    let mask = (1i32 << bits) - 1;
    let bit_index = index % (8 / bits);
    let mut value = i32::from(byte) & (mask << (bit_index * bits));
    value >>= (8 - (bit_index * bits)) % 8;
    stream.seek(fp)?;
    Ok(value)
}

#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    data: &'a [u8],
    position: usize,
    little_endian: bool,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            little_endian: false,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn set_little_endian(&mut self, little_endian: bool) {
        self.little_endian = little_endian;
    }

    pub fn seek(&mut self, position: usize) -> Result<()> {
        if position > self.data.len() {
            return Err(MetakitError::UnexpectedEof {
                needed: 0,
                position,
            });
        }
        self.position = position;
        Ok(())
    }

    pub fn skip(&mut self, bytes: usize) -> Result<()> {
        let position = self
            .position
            .checked_add(bytes)
            .ok_or_else(|| MetakitError::InvalidFormat("seek offset overflow".to_owned()))?;
        self.seek(position)
    }

    pub fn read_bytes(&mut self, bytes: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(bytes)
            .ok_or_else(|| MetakitError::InvalidFormat("read offset overflow".to_owned()))?;
        if end > self.data.len() {
            return Err(MetakitError::UnexpectedEof {
                needed: bytes,
                position: self.position,
            });
        }
        let value = &self.data[self.position..end];
        self.position = end;
        Ok(value)
    }

    pub fn read_string(&mut self, bytes: usize) -> Result<String> {
        Ok(std::str::from_utf8(self.read_bytes(bytes)?)?.to_owned())
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_bytes(1)?[0])
    }

    pub fn read_u16(&mut self, little_endian: bool) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(if little_endian {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        })
    }

    pub fn read_i16(&mut self, little_endian: bool) -> Result<i16> {
        Ok(self.read_u16(little_endian)? as i16)
    }

    pub fn read_i32_be(&mut self) -> Result<i32> {
        self.read_i32(false)
    }

    pub fn read_i32(&mut self, little_endian: bool) -> Result<i32> {
        let bytes = self.read_bytes(4)?;
        Ok(if little_endian {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    }

    pub fn read_i64(&mut self, little_endian: bool) -> Result<i64> {
        let bytes = self.read_bytes(8)?;
        Ok(if little_endian {
            i64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])
        } else {
            i64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])
        })
    }

    pub fn read_f32(&mut self, little_endian: bool) -> Result<f32> {
        Ok(f32::from_bits(self.read_i32(little_endian)? as u32))
    }

    pub fn read_f64(&mut self, little_endian: bool) -> Result<f64> {
        Ok(f64::from_bits(self.read_i64(little_endian)? as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp(value: i32) -> Vec<u8> {
        assert!(value >= 0);
        let mut groups = Vec::new();
        let mut remaining = value as u32;
        groups.push((remaining & 0x7f) as u8);
        remaining >>= 7;
        while remaining > 0 {
            groups.push((remaining & 0x7f) as u8);
            remaining >>= 7;
        }
        groups.reverse();
        let last = groups.len() - 1;
        groups[last] |= 0x80;
        groups
    }

    fn put_i32_be(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn fixture() -> Vec<u8> {
        let mut bytes = vec![0; 160];
        bytes[0..2].copy_from_slice(b"LJ");
        bytes[2] = 26;
        bytes[3] = 0;
        put_i32_be(&mut bytes, 4, 32);

        put_i32_be(&mut bytes, 20, 0);
        put_i32_be(&mut bytes, 28, 40);

        let structure = b"People[name:S,age:I]";
        let mut toc = Vec::new();
        toc.extend(bp(0));
        toc.extend(bp(structure.len() as i32));
        toc.extend(structure);
        toc.extend(bp(0));
        toc.extend(bp(0));
        toc.extend(bp(79));
        bytes[40..40 + toc.len()].copy_from_slice(&toc);

        let mut table = Vec::new();
        table.extend(bp(2));
        table.extend(bp(6));
        table.extend(bp(120));
        table.extend(bp(1));
        table.extend(bp(130));
        table.extend(bp(0));
        table.extend(bp(8));
        table.extend(bp(140));
        bytes[80..80 + table.len()].copy_from_slice(&table);

        bytes[120..126].copy_from_slice(b"AnnBob");
        bytes[130] = 0x33;
        put_i32_be(&mut bytes, 140, 7);
        put_i32_be(&mut bytes, 144, 11);
        bytes
    }

    #[test]
    fn column_definition_matches_java_behavior() {
        let column = Column::new("fieldID:I").unwrap();
        assert_eq!(column.name(), "fieldID");
        assert_eq!(column.type_string(), "I");
        assert_eq!(column.column_type(), ColumnType::Integer);
    }

    #[test]
    fn byte_packed_integer_matches_java_behavior() {
        let mut stream = ByteReader::new(&[0x81]);
        assert_eq!(read_bp_int(&mut stream).unwrap(), 1);

        let mut stream = ByteReader::new(&[0x01, 0x80]);
        assert_eq!(read_bp_int(&mut stream).unwrap(), 128);

        let mut stream = ByteReader::new(&[0x00, 0x81]);
        assert_eq!(read_bp_int(&mut stream).unwrap(), !1);
    }

    #[test]
    fn byte_packed_integer_rejects_overlong_encoding() {
        let mut stream = ByteReader::new(&[0x01, 0x01, 0x01, 0x01, 0x01, 0x80]);
        let err = read_bp_int(&mut stream).unwrap_err();
        assert!(err.to_string().contains("overlong"));
    }

    #[test]
    fn byte_reader_skip_rejects_offset_overflow() {
        let mut stream = ByteReader::new(&[]);
        stream.position = usize::MAX;
        let err = stream.skip(1).unwrap_err();
        assert!(err.to_string().contains("seek offset overflow"));
    }

    #[test]
    fn reads_table_names_columns_and_rows() {
        let reader = MetakitReader::from_bytes(&fixture()).unwrap();
        assert_eq!(reader.table_count(), 1);
        assert_eq!(reader.table_names(), vec!["People"]);

        let table = reader.table_by_name("People").unwrap();
        assert_eq!(table.column_names(), vec!["name", "age"]);
        assert_eq!(
            table.column_types(),
            vec![ColumnType::String, ColumnType::Integer]
        );
        assert_eq!(table.row_count(), 2);

        let rows = table.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Some(Value::String("Ann".to_owned())));
        assert_eq!(rows[0][1], Some(Value::Integer(7)));
        assert_eq!(rows[1][0], Some(Value::String("Bob".to_owned())));
        assert_eq!(rows[1][1], Some(Value::Integer(11)));
    }

    #[test]
    fn invalid_magic_is_rejected_like_java_reader() {
        let mut bytes = fixture();
        bytes[0..2].copy_from_slice(b"NO");
        let err = MetakitReader::from_bytes(&bytes).unwrap_err();
        assert!(err.to_string().contains("Invalid magic string"));
    }
}
