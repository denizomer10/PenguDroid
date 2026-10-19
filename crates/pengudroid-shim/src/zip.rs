//! Minimal ZIP reader for `AAssetManager`.
//!
//! An APK is a ZIP archive; `AAssetManager_open` reads entries straight out of
//! it. This module implements just enough of the format for that: the central
//! directory, stored and DEFLATE entries, and entry enumeration. It is
//! deliberately independent of the CLI's richer APK parser so the shim stays
//! self-contained (it is `LD_PRELOAD`ed into arbitrary guest processes).

use std::collections::BTreeMap;

use miniz_oxide::inflate::decompress_to_vec;

/// One central-directory entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub method: u16,
    pub compressed_size: usize,
    pub local_offset: usize,
}

/// A parsed ZIP archive (the file is read once, on first use).
pub struct Archive {
    data: Vec<u8>,
    entries: BTreeMap<String, Entry>,
}

impl Archive {
    /// Read and index the archive at `path`.
    pub fn open(path: &str) -> Result<Archive, String> {
        let data = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let entries = read_central_directory(&data)?;
        Ok(Archive { data, entries })
    }

    /// Names directly under `prefix` (one path component deep), as
    /// `AAssetDir_getNextFileName` expects.
    pub fn list_dir(&self, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        let prefix = if prefix.is_empty() {
            String::new()
        } else if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{prefix}/")
        };
        for name in self.entries.keys() {
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            if rest.is_empty() {
                continue;
            }
            let first = rest.split('/').next().unwrap_or(rest);
            if !out.iter().any(|n| n == first) {
                out.push(first.to_string());
            }
        }
        out.sort();
        out
    }

    /// Extract an entry's bytes (inflating DEFLATE entries).
    pub fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| format!("no entry `{name}`"))?;
        let l = entry.local_offset;
        if le_u32(&self.data, l)? != 0x0403_4b50 {
            return Err(format!("bad local header for `{name}`"));
        }
        let name_len = le_u16(&self.data, l + 26)? as usize;
        let extra_len = le_u16(&self.data, l + 28)? as usize;
        let start = l + 30 + name_len + extra_len;
        let end = start
            .checked_add(entry.compressed_size)
            .ok_or_else(|| format!("entry `{name}` overflows"))?;
        let compressed = self
            .data
            .get(start..end)
            .ok_or_else(|| format!("truncated data for `{name}`"))?;
        match entry.method {
            0 => Ok(compressed.to_vec()),
            8 => decompress_to_vec(compressed)
                .map_err(|e| format!("inflate `{name}`: {e:?}")),
            m => Err(format!("unsupported compression method {m} for `{name}`")),
        }
    }
}

fn le_u16(d: &[u8], o: usize) -> Result<u16, String> {
    let b = d.get(o..o + 2).ok_or_else(|| format!("truncated u16 at {o}"))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn le_u32(d: &[u8], o: usize) -> Result<u32, String> {
    let b = d.get(o..o + 4).ok_or_else(|| format!("truncated u32 at {o}"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_central_directory(data: &[u8]) -> Result<BTreeMap<String, Entry>, String> {
    if data.len() < 22 {
        return Err("file is too small to be a ZIP archive".into());
    }
    // The EOCD is within the last 22 + 65535 bytes.
    let window = data.len().min(65557);
    let start = data.len() - window;
    let mut eocd = None;
    for i in (start..=data.len() - 22).rev() {
        if data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd.ok_or("ZIP end-of-central-directory record not found")?;
    let total = le_u16(data, eocd + 10)? as usize;
    let cd_offset = le_u32(data, eocd + 16)? as usize;

    let mut out = BTreeMap::new();
    let mut off = cd_offset;
    for n in 0..total {
        if le_u32(data, off)? != 0x0201_4b50 {
            return Err(format!("bad central directory signature for entry {n}"));
        }
        let method = le_u16(data, off + 10)?;
        let compressed_size = le_u32(data, off + 20)? as usize;
        let uncompressed_size = le_u32(data, off + 24)? as usize;
        let name_len = le_u16(data, off + 28)? as usize;
        let extra_len = le_u16(data, off + 30)? as usize;
        let comment_len = le_u16(data, off + 32)? as usize;
        let local_offset = le_u32(data, off + 42)? as usize;
        let name_start = off + 46;
        let name_bytes = data
            .get(name_start..name_start + name_len)
            .ok_or_else(|| format!("truncated filename for entry {n}"))?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        let _ = uncompressed_size;
        out.insert(name, Entry { method, compressed_size, local_offset });
        off = name_start + name_len + extra_len + comment_len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny ZIP in memory (stored entries only) so the reader can be
    /// tested without an APK on disk.
    fn tiny_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len();
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc (unchecked)
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(offset as u32).to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len();
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&(cd_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn reads_stored_entries_and_lists_directories() {
        let zip = tiny_zip(&[
            ("assets/a.txt", b"alpha"),
            ("assets/sub/b.bin", b"beta"),
            ("lib/x86_64/libmain.so", b"elf"),
        ]);
        let dir = std::env::temp_dir().join(format!("pd-zip-{}.zip", std::process::id()));
        std::fs::write(&dir, &zip).unwrap();
        let archive = Archive::open(dir.to_str().unwrap()).unwrap();
        assert_eq!(archive.read("assets/a.txt").unwrap(), b"alpha");
        assert_eq!(archive.read("assets/sub/b.bin").unwrap(), b"beta");
        assert!(archive.read("assets/missing").is_err());
        assert_eq!(archive.list_dir("assets"), vec!["a.txt", "sub"]);
        assert_eq!(archive.list_dir("assets/sub"), vec!["b.bin"]);
        let _ = std::fs::remove_file(dir);
    }
}
