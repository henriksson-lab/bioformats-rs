use crate::error::{BioFormatsError, Result};
use std::io::{Read, Seek, SeekFrom};

/// Read exactly `n` bytes at a given file offset.
pub fn read_bytes_at<R: Read + Seek>(r: &mut R, offset: u64, n: usize) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read a null-terminated ASCII string, up to `max_len` bytes.
pub fn read_cstring(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Peek at the first N bytes of a file without consuming a reader.
pub fn peek_header(path: &std::path::Path, n: usize) -> Result<Vec<u8>> {
    use std::fs::File;
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; n];
    let read = f.read(&mut buf).map_err(BioFormatsError::Io)?;
    buf.truncate(read);
    Ok(buf)
}
