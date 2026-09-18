use std::io::Read;
use std::path::Path;

// Magic constants from DepotDownloader's manifest format
const MAGIC_METADATA: u32 = 0x1F4812BE;
const MAGIC_EOF: u32 = 0x32C415AB;

// Protobuf field numbers in the metadata section
const FIELD_DEPOT_ID: i32 = 1;
const FIELD_GID_MANIFEST: i32 = 2;
const FIELD_FILENAMES_ENCRYPTED: i32 = 4;
const FIELD_SIZE_ON_DISK: i32 = 5;

/// Parsed metadata from a Steam depot manifest file.
#[derive(Debug, Clone)]
pub struct ManifestInfo {
    pub depot_id: u64,
    pub gid_manifest: u64,
    pub filenames_encrypted: bool,
    pub size_on_disk: u64,
}

/// Try to read and parse a .manifest file, returning its metadata.
pub fn try_read_manifest(path: &Path) -> Option<ManifestInfo> {
    let mut file = std::fs::File::open(path).ok()?;

    // Check if it's a ZIP-wrapped manifest (PK header)
    let mut magic_buf = [0u8; 2];
    file.read_exact(&mut magic_buf).ok()?;
    if magic_buf[0] == b'P' && magic_buf[1] == b'K' {
        // ZIP-wrapped: extract and parse metadata section
        return try_read_zipped_manifest(path);
    }

    // Reset and scan for metadata section
    drop(file);
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 8];

    loop {
        if file.read_exact(&mut header).is_err() {
            break;
        }
        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

        if magic == MAGIC_EOF {
            break;
        }

        if len as usize > 10 * 1024 * 1024 {
            // Sanity check: metadata shouldn't be > 10MB
            break;
        }

        if magic != MAGIC_METADATA {
            // Skip this section
            let mut buf = vec![0u8; len as usize];
            if file.read_exact(&mut buf).is_err() {
                break;
            }
            continue;
        }

        // Found metadata section
        let mut meta = vec![0u8; len as usize];
        if file.read_exact(&mut meta).is_err() {
            break;
        }
        return parse_metadata(&meta);
    }

    None
}

/// Try to parse manifest metadata from an in-memory byte slice.
/// This is used to validate downloaded manifest bytes without writing to disk first.
pub fn try_read_manifest_bytes(data: &[u8]) -> Option<ManifestInfo> {
    if data.len() < 16 {
        return None;
    }

    // Check for ZIP header
    if data[0] == b'P' && data[1] == b'K' {
        // ZIP-wrapped: try to parse as zip in-memory
        let cursor = std::io::Cursor::new(data);
        if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
            for i in 0..archive.len() {
                if let Ok(mut entry) = archive.by_index(i) {
                    let name = entry.name().to_string();
                    if name.contains("metadata") || i == 0 {
                        let mut buf = Vec::new();
                        if entry.read_to_end(&mut buf).is_ok() {
                            return parse_metadata(&buf);
                        }
                    }
                }
            }
        }
        return None;
    }

    // Scan for metadata section in raw bytes
    let mut offset = 0;
    while offset + 8 <= data.len() {
        let magic = u32::from_le_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]);
        let len = u32::from_le_bytes([
            data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
        ]) as usize;

        offset += 8;

        if magic == MAGIC_EOF {
            break;
        }

        if len > 10 * 1024 * 1024 {
            break;
        }

        if offset + len > data.len() {
            break;
        }

        if magic == MAGIC_METADATA {
            return parse_metadata(&data[offset..offset + len]);
        }

        offset += len;
    }

    None
}

/// Try to read a ZIP-wrapped manifest.
fn try_read_zipped_manifest(path: &Path) -> Option<ManifestInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).ok()?;
        let name = entry.name().to_string();

        // Look for metadata file or the first file
        if name.contains("metadata") || i == 0 {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).ok()?;
            return parse_metadata(&buf);
        }
    }

    None
}

/// Parse the protobuf-like metadata section.
fn parse_metadata(data: &[u8]) -> Option<ManifestInfo> {
    let mut depot_id: u64 = 0;
    let mut gid_manifest: u64 = 0;
    let mut filenames_encrypted = false;
    let mut size_on_disk: u64 = 0;

    let mut offset = 0;

    while offset < data.len() {
        let (field, wire_type, new_offset) = read_tag(data, offset)?;
        offset = new_offset;

        if wire_type == 0 {
            // Varint
            let (value, new_offset) = read_varint(data, offset)?;
            offset = new_offset;

            match field {
                FIELD_DEPOT_ID => depot_id = value,
                FIELD_GID_MANIFEST => gid_manifest = value,
                FIELD_FILENAMES_ENCRYPTED => filenames_encrypted = value != 0,
                FIELD_SIZE_ON_DISK => size_on_disk = value,
                _ => {}
            }
        } else {
            // Skip unknown wire types
            offset = skip_field(data, offset, wire_type)?;
        }
    }

    Some(ManifestInfo {
        depot_id,
        gid_manifest,
        filenames_encrypted,
        size_on_disk,
    })
}

/// Read a protobuf tag (field number + wire type) from the data.
fn read_tag(data: &[u8], offset: usize) -> Option<(i32, i32, usize)> {
    let (value, new_offset) = read_varint(data, offset)?;
    let field = (value >> 3) as i32;
    let wire_type = (value & 0x7) as i32;
    Some((field, wire_type, new_offset))
}

/// Read a protobuf varint from the data.
fn read_varint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut pos = offset;

    loop {
        if pos >= data.len() {
            return None;
        }
        let byte = data[pos];
        result |= ((byte & 0x7F) as u64) << shift;
        pos += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }

    Some((result, pos))
}

/// Skip a protobuf field based on its wire type.
fn skip_field(data: &[u8], offset: usize, wire_type: i32) -> Option<usize> {
    match wire_type {
        0 => {
            // Varint - skip it
            let (_, new_offset) = read_varint(data, offset)?;
            Some(new_offset)
        }
        1 => {
            // 64-bit
            Some(offset + 8)
        }
        2 => {
            // Length-delimited
            let (len, new_offset) = read_varint(data, offset)?;
            Some(new_offset + len as usize)
        }
        5 => {
            // 32-bit
            Some(offset + 4)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_varint() {
        // Single byte: 0x00 = 0
        assert_eq!(read_varint(&[0x00], 0), Some((0, 1)));
        // Single byte: 0x01 = 1
        assert_eq!(read_varint(&[0x01], 0), Some((1, 1)));
        // Multi-byte: 0x80 0x01 = 128
        assert_eq!(read_varint(&[0x80, 0x01], 0), Some((128, 2)));
    }
}
