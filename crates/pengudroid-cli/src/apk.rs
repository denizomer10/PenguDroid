//! APK reading and binary `AndroidManifest.xml` (AXML) parsing.
//!
//! An APK is a ZIP archive whose `AndroidManifest.xml` is *not* text: it is the
//! compiled binary XML format produced by `aapt`. Reading it requires (a) ZIP
//! central-directory parsing with DEFLATE support and (b) a chunked AXML
//! parser. Both are implemented here on top of `miniz_oxide` for inflate.
//!
//! This module is the only place that understands the on-disk container format;
//! the rest of the CLI works with [`ApkInfo`].

use miniz_oxide::inflate::decompress_to_vec;

/// A declared Android component (`activity`, `service`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Fully-qualified class name (already expanded against the package).
    pub name: String,
    /// `android:exported`, defaulting to the platform default when absent.
    pub exported: bool,
    /// True if this component handles the MAIN/LAUNCHER intent.
    pub is_launcher: bool,
    /// `android:theme` as a resource id, when the manifest names one.
    pub theme: Option<u32>,
}

/// Everything PenguDroid extracts from a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApkInfo {
    pub package: String,
    pub version_name: Option<String>,
    pub version_code: Option<u32>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    /// `android:label` when it is an inline string (not a resource reference).
    pub label: Option<String>,
    /// `android:name` of `<application>` (custom Application subclass).
    pub application_class: Option<String>,
    /// `android:theme` of `<application>` as a resource id.
    pub application_theme: Option<u32>,
    pub activities: Vec<Component>,
    pub services: Vec<Component>,
    pub receivers: Vec<Component>,
    pub providers: Vec<Component>,
    pub permissions: Vec<String>,
    pub uses_features: Vec<String>,
    /// The MAIN/LAUNCHER activity, if one is declared.
    pub main_activity: Option<String>,
}

impl ApkInfo {
    /// Expand a manifest component name against the package, mirroring
    /// Android's own rules: `.Main` → `pkg.Main`, `Main` → `pkg.Main`,
    /// `a.b.C` → unchanged.
    pub fn resolve(&self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix('.') {
            format!("{}.{}", self.package, rest)
        } else if !name.contains('.') {
            format!("{}.{}", self.package, name)
        } else {
            name.to_string()
        }
    }

    /// The component to launch, if any.
    pub fn main_activity_name(&self) -> Option<&str> {
        self.main_activity.as_deref()
    }
}

/// Read and parse an APK at `path`.
pub fn read_apk(path: &str) -> Result<ApkInfo, String> {
    let manifest = extract_manifest(path)?;
    parse_axml(&manifest)
}

/// Read and parse an APK that is already in memory.
pub fn read_apk_bytes(data: &[u8]) -> Result<ApkInfo, String> {
    let manifest = extract_named_bytes(data, "AndroidManifest.xml")?;
    parse_axml(&manifest)
}

// ---------------------------------------------------------------------------
// Little-endian read helpers (every access is bounds-checked; nothing panics).
// ---------------------------------------------------------------------------

fn le_u16(d: &[u8], o: usize) -> Result<u16, String> {
    let b = d.get(o..o + 2).ok_or_else(|| format!("truncated u16 at offset {o}"))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn le_u32(d: &[u8], o: usize) -> Result<u32, String> {
    let b = d.get(o..o + 4).ok_or_else(|| format!("truncated u32 at offset {o}"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

// ---------------------------------------------------------------------------
// ZIP container
// ---------------------------------------------------------------------------

/// One entry from the ZIP central directory.
struct ZipEntry {
    name: String,
    /// Compression method: 0 = stored, 8 = deflate.
    method: u16,
    compressed_size: usize,
    /// Offset of the matching local file header.
    local_offset: usize,
}

/// Locate the End Of Central Directory record. The record is at least 22 bytes
/// and is followed by an optional comment of up to 65535 bytes, so we scan the
/// last 65557 bytes backwards for its signature.
fn find_eocd(data: &[u8]) -> Result<usize, String> {
    if data.len() < 22 {
        return Err("file is too small to be a ZIP archive".into());
    }
    let window = data.len().min(65557);
    let start = data.len() - window;
    // i is the candidate EOCD start; the record must fit entirely.
    for i in (start..=data.len() - 22).rev() {
        if data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            return Ok(i);
        }
    }
    Err("ZIP end-of-central-directory record not found".into())
}

/// Parse the central directory into a list of entries.
fn read_central_directory(data: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let eocd = find_eocd(data)?;
    let total_entries = le_u16(data, eocd + 10)? as usize;
    let cd_offset = le_u32(data, eocd + 16)? as usize;

    let mut out = Vec::with_capacity(total_entries);
    let mut off = cd_offset;
    for n in 0..total_entries {
        if le_u32(data, off)? != 0x0201_4b50 {
            return Err(format!("bad central directory signature for entry {n}"));
        }
        let method = le_u16(data, off + 10)?;
        let compressed_size = le_u32(data, off + 20)? as usize;
        let name_len = le_u16(data, off + 28)? as usize;
        let extra_len = le_u16(data, off + 30)? as usize;
        let comment_len = le_u16(data, off + 32)? as usize;
        let local_offset = le_u32(data, off + 42)? as usize;

        let name_start = off + 46;
        let name_bytes = data
            .get(name_start..name_start + name_len)
            .ok_or_else(|| format!("truncated filename for entry {n}"))?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();

        out.push(ZipEntry { name, method, compressed_size, local_offset });
        off = name_start + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// Extract (and, for DEFLATE entries, inflate) a single entry's bytes.
fn extract_entry(data: &[u8], entry: &ZipEntry) -> Result<Vec<u8>, String> {
    let l = entry.local_offset;
    if le_u32(data, l)? != 0x0403_4b50 {
        return Err(format!("bad local header signature for `{}`", entry.name));
    }
    // The local header stores its own filename/extra lengths, which may differ
    // from the central directory's, so read them from here.
    let name_len = le_u16(data, l + 26)? as usize;
    let extra_len = le_u16(data, l + 28)? as usize;
    let data_start = l + 30 + name_len + extra_len;
    let data_end = data_start + entry.compressed_size;
    let compressed = data
        .get(data_start..data_end)
        .ok_or_else(|| format!("truncated data for `{}`", entry.name))?;

    match entry.method {
        0 => Ok(compressed.to_vec()),
        8 => decompress_to_vec(compressed)
            .map_err(|e| format!("inflate failed for `{}`: {e:?}", entry.name)),
        m => Err(format!("unsupported compression method {m} for `{}`", entry.name)),
    }
}

/// Extract the raw `AndroidManifest.xml` bytes from an APK.
pub fn extract_manifest(path: &str) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
    let entries = read_central_directory(&data)?;
    let entry = entries
        .iter()
        .find(|e| e.name == "AndroidManifest.xml")
        .ok_or_else(|| format!("`{path}` contains no AndroidManifest.xml"))?;
    extract_entry(&data, entry)
}

/// List the entry names in an APK's central directory.
pub fn apk_entries(path: &str) -> Result<Vec<String>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
    apk_entries_bytes(&data)
}

/// List the entry names of an in-memory ZIP/APK.
pub fn apk_entries_bytes(data: &[u8]) -> Result<Vec<String>, String> {
    let entries = read_central_directory(data)?;
    Ok(entries.into_iter().map(|e| e.name).collect())
}

/// A native library shipped inside an APK (an entry under `lib/<abi>/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLib {
    /// Android ABI directory, e.g. `x86_64`, `arm64-v8a`.
    pub abi: String,
    /// File name, e.g. `libfoo.so`.
    pub file: String,
    /// Full archive entry name, e.g. `lib/x86_64/libfoo.so`.
    pub entry: String,
}

impl NativeLib {
    /// The base name without the `lib` prefix and `.so` suffix, useful for
    /// comparing against a `DT_NEEDED` entry (e.g. `libfoo.so` -> `foo`).
    #[allow(dead_code)] // used by dependency matching on the next milestone
    pub fn base(&self) -> &str {
        let stem = self.file.strip_suffix(".so").unwrap_or(&self.file);
        stem.strip_prefix("lib").unwrap_or(stem)
    }
}

/// List the native libraries in an APK across every ABI it ships.
pub fn native_libs(path: &str) -> Result<Vec<NativeLib>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
    native_libs_bytes(&data)
}

/// List the native libraries of an in-memory APK.
pub fn native_libs_bytes(data: &[u8]) -> Result<Vec<NativeLib>, String> {
    let mut out = Vec::new();
    for entry in apk_entries_bytes(data)? {
        // Shape: lib/<abi>/<file>.so
        let parts: Vec<&str> = entry.split('/').collect();
        if parts.len() == 3 && parts[0] == "lib" && parts[2].ends_with(".so") {
            out.push(NativeLib {
                abi: parts[1].to_string(),
                file: parts[2].to_string(),
                entry,
            });
        }
    }
    out.sort_by(|a, b| (&a.abi, &a.file).cmp(&(&b.abi, &b.file)));
    Ok(out)
}

/// Extract a named entry from an APK (any file, not just the manifest).
pub fn extract_named(path: &str, name: &str) -> Result<Vec<u8>, String> {
    let data = std::fs::read(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
    extract_named_bytes(&data, name)
}

/// Extract a named entry from an in-memory ZIP/APK.
pub fn extract_named_bytes(data: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let entries = read_central_directory(data)?;
    let entry = entries
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("no entry `{name}`"))?;
    extract_entry(data, entry)
}

// ---------------------------------------------------------------------------
// Binary XML (AXML)
// ---------------------------------------------------------------------------

// Chunk types.
const RES_XML_TYPE: u16 = 0x0003;
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;

// Attribute value typed kinds.
const TYPE_STRING: u8 = 0x03;
/// `Res_value::TYPE_REFERENCE` — the value is a resource id.
const TYPE_REFERENCE: u8 = 0x01;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const TYPE_INT_BOOLEAN: u8 = 0x12;

const NO_INDEX: u32 = 0xFFFF_FFFF;

/// A decoded attribute of a start-element chunk.
struct Attr {
    /// Local attribute name (namespace deliberately ignored).
    name: String,
    /// `raw_value` resolved against the string pool, when it is a valid index.
    raw: Option<String>,
    typed_type: u8,
    typed_data: u32,
}

/// Parse the string pool chunk beginning at `chunk_start`.
fn parse_string_pool(data: &[u8], chunk_start: usize) -> Result<Vec<String>, String> {
    let count = le_u32(data, chunk_start + 8)? as usize;
    let flags = le_u32(data, chunk_start + 16)?;
    let strings_start = le_u32(data, chunk_start + 20)? as usize;
    let is_utf8 = flags & 0x100 != 0;

    let offsets_base = chunk_start + 28;
    let strings_base = chunk_start + strings_start;

    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = le_u32(data, offsets_base + i * 4)? as usize;
        let p = strings_base + off;
        let s = if is_utf8 {
            read_utf8_string(data, p)?
        } else {
            read_utf16_string(data, p)?
        };
        out.push(s);
    }
    Ok(out)
}

/// Read a length-prefixed UTF-16LE string from the pool.
fn read_utf16_string(data: &[u8], off: usize) -> Result<String, String> {
    let len = le_u16(data, off)? as usize;
    let start = off + 2;
    let end = start + len * 2;
    let bytes = data.get(start..end).ok_or("truncated UTF-16 string in pool")?;
    let units = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    Ok(char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect())
}

/// Read a length-prefixed UTF-8 string from the pool. Lengths may be encoded in
/// one or two bytes when the high bit is set.
fn read_utf8_string(data: &[u8], off: usize) -> Result<String, String> {
    let mut p = off;
    let first = *data.get(p).ok_or("truncated UTF-8 length")?;
    p += 1;
    // The first length is the UTF-16 code-unit count; it is only skipped here.
    if first & 0x80 != 0 {
        // Two-byte length: consume the second byte.
        data.get(p).ok_or("truncated UTF-8 length")?;
        p += 1;
    }
    let b0 = *data.get(p).ok_or("truncated UTF-8 byte length")?;
    p += 1;
    let byte_len = if b0 & 0x80 != 0 {
        let b1 = *data.get(p).ok_or("truncated UTF-8 byte length")?;
        p += 1;
        (((b0 as usize & 0x7f) << 8) | b1 as usize) as usize
    } else {
        b0 as usize
    };
    let bytes = data.get(p..p + byte_len).ok_or("truncated UTF-8 string in pool")?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Decode a string-pool index, treating `NO_INDEX` as absent.
fn pool_get(pool: &[String], index: u32) -> Option<String> {
    if index == NO_INDEX {
        None
    } else {
        pool.get(index as usize).cloned()
    }
}

/// Read an attribute's value as a string, preferring `raw_value`.
fn attr_str(pool: &[String], attrs: &[Attr], want: &str) -> Option<String> {
    let a = attrs.iter().find(|a| a.name == want)?;
    if let Some(raw) = &a.raw {
        return Some(raw.clone());
    }
    if a.typed_type == TYPE_STRING {
        return pool_get(pool, a.typed_data);
    }
    None
}

/// Read an attribute's value as an integer, accepting either an int-typed value
/// or a string that parses as one.
fn attr_int(pool: &[String], attrs: &[Attr], want: &str) -> Option<u32> {
    let a = attrs.iter().find(|a| a.name == want)?;
    if a.typed_type == TYPE_INT_DEC || a.typed_type == TYPE_INT_HEX {
        return Some(a.typed_data);
    }
    let as_str = match &a.raw {
        Some(raw) => raw.clone(),
        None if a.typed_type == TYPE_STRING => pool_get(pool, a.typed_data).unwrap_or_default(),
        None => return None,
    };
    parse_sdk_number(&as_str)
}

/// Read an attribute's value as a resource reference (typed `0x01`).
///
/// `android:theme` is always a reference to a style in the APK's resources, so
/// the raw `typed_data` is the resource id the framework expects.
fn attr_ref(attrs: &[Attr], want: &str) -> Option<u32> {
    let a = attrs.iter().find(|a| a.name == want)?;
    if a.typed_type == TYPE_REFERENCE {
        Some(a.typed_data)
    } else {
        None
    }
}

/// Read an attribute's value as a boolean (typed `0x12`, or any non-zero int).
fn attr_bool(attrs: &[Attr], want: &str) -> Option<bool> {
    let a = attrs.iter().find(|a| a.name == want)?;
    if a.typed_type == TYPE_INT_BOOLEAN || a.typed_type == TYPE_INT_DEC || a.typed_type == TYPE_INT_HEX {
        Some(a.typed_data != 0)
    } else {
        None
    }
}

/// Extract a leading decimal number, tolerating a `v` prefix or a codename like
/// `"S"` (for which `None` is returned — a codename is not a numeric API level).
fn parse_sdk_number(s: &str) -> Option<u32> {
    let digits: String = s
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u32>().ok()
}

/// Decode the attributes of a start-element chunk into [`Attr`]s.
fn parse_attrs(data: &[u8], pool: &[String], chunk_start: usize) -> Result<Vec<Attr>, String> {
    let attr_start = le_u16(data, chunk_start + 24)? as usize;
    let attr_count = le_u16(data, chunk_start + 28)? as usize;
    // Attributes live after the 16-byte chunk header plus `attr_start` bytes.
    let base = chunk_start + 16 + attr_start;

    let mut out = Vec::with_capacity(attr_count);
    for i in 0..attr_count {
        let o = base + i * 20;
        let name_index = le_u32(data, o + 4)?;
        let raw_value = le_u32(data, o + 8)?;
        let typed_type = *data.get(o + 15).ok_or("truncated attribute record")?;
        let typed_data = le_u32(data, o + 16)?;

        let Some(name) = pool_get(pool, name_index) else { continue };
        let raw = pool_get(pool, raw_value);
        out.push(Attr { name, raw, typed_type, typed_data });
    }
    Ok(out)
}

/// Which kind of component an open element frame describes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompKind {
    Activity,
    Service,
    Receiver,
    Provider,
}

/// An open element while walking the chunk stream.
#[derive(Default)]
struct Frame {
    /// Resolved component name, set for component frames.
    resolved_name: Option<String>,
    /// Index into the matching `ApkInfo` component vector.
    comp_index: Option<usize>,
    comp_kind: Option<CompKind>,
    /// `android:exported` as written, if present.
    explicit_exported: Option<bool>,
    /// Whether the component contains an `<intent-filter>`.
    has_intent_filter: bool,
    /// This frame is an `<intent-filter>`.
    is_intent: bool,
    intent_main: bool,
    intent_launcher: bool,
}

/// Parse binary Android XML (as found in `AndroidManifest.xml`).
pub fn parse_axml(data: &[u8]) -> Result<ApkInfo, String> {
    if le_u16(data, 0)? != RES_XML_TYPE {
        return Err("not binary XML: missing RES_XML_TYPE header".into());
    }
    let header_size = le_u16(data, 2)? as usize;

    let mut info = ApkInfo::default();
    let mut pool: Vec<String> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();

    // Walk the chunk stream, starting just past the 8-byte file header.
    let mut off = header_size;
    while off + 8 <= data.len() {
        let ctype = le_u16(data, off)?;
        let csize = le_u32(data, off + 4)? as usize;
        if csize < 8 {
            return Err(format!("invalid chunk size {csize} at offset {off}"));
        }

        match ctype {
            RES_STRING_POOL_TYPE => {
                pool = parse_string_pool(data, off)?;
            }
            RES_XML_START_ELEMENT_TYPE => {
                let name = pool_get(&pool, le_u32(data, off + 20)?).unwrap_or_default();
                let attrs = parse_attrs(data, &pool, off)?;

                let mut frame = Frame::default();
                match name.as_str() {
                    "manifest" => {
                        info.package = attr_str(&pool, &attrs, "package").unwrap_or_default();
                        info.version_name = attr_str(&pool, &attrs, "versionName");
                        info.version_code = attr_int(&pool, &attrs, "versionCode");
                    }
                    "uses-sdk" => {
                        info.min_sdk = attr_int(&pool, &attrs, "minSdkVersion");
                        info.target_sdk = attr_int(&pool, &attrs, "targetSdkVersion");
                    }
                    "uses-permission" | "uses-permission-sdk-23" => {
                        if let Some(n) = attr_str(&pool, &attrs, "name") {
                            if !info.permissions.contains(&n) {
                                info.permissions.push(n);
                            }
                        }
                    }
                    "uses-feature" => {
                        if let Some(n) = attr_str(&pool, &attrs, "name") {
                            if !info.uses_features.contains(&n) {
                                info.uses_features.push(n);
                            }
                        }
                    }
                    "application" => {
                        // A label that is a resource reference is not resolvable
                        // here, so only inline strings are kept.
                        info.label = attr_str(&pool, &attrs, "label");
                        info.application_theme = attr_ref(&attrs, "theme");
                        if let Some(n) = attr_str(&pool, &attrs, "name") {
                            info.application_class = Some(info.resolve(&n));
                        }
                    }
                    "activity" | "service" | "receiver" | "provider" => {
                        let kind = match name.as_str() {
                            "activity" => CompKind::Activity,
                            "service" => CompKind::Service,
                            "receiver" => CompKind::Receiver,
                            _ => CompKind::Provider,
                        };
                        let raw = attr_str(&pool, &attrs, "name")
                            .ok_or_else(|| format!("<{name}> is missing android:name"))?;
                        let resolved = info.resolve(&raw);
                        let explicit = attr_bool(&attrs, "exported");
                        let component = Component {
                            name: resolved.clone(),
                            exported: explicit.unwrap_or(false),
                            is_launcher: false,
                            theme: attr_ref(&attrs, "theme"),
                        };
                        let idx = match kind {
                            CompKind::Activity => {
                                info.activities.push(component);
                                info.activities.len() - 1
                            }
                            CompKind::Service => {
                                info.services.push(component);
                                info.services.len() - 1
                            }
                            CompKind::Receiver => {
                                info.receivers.push(component);
                                info.receivers.len() - 1
                            }
                            CompKind::Provider => {
                                info.providers.push(component);
                                info.providers.len() - 1
                            }
                        };
                        frame.resolved_name = Some(resolved);
                        frame.comp_index = Some(idx);
                        frame.comp_kind = Some(kind);
                        frame.explicit_exported = explicit;
                    }
                    "intent-filter" => {
                        frame.is_intent = true;
                    }
                    "action" => {
                        if let Some(top) = stack.last_mut() {
                            if attr_str(&pool, &attrs, "name").as_deref()
                                == Some("android.intent.action.MAIN")
                            {
                                top.intent_main = true;
                            }
                        }
                    }
                    "category" => {
                        if let Some(top) = stack.last_mut() {
                            if attr_str(&pool, &attrs, "name").as_deref()
                                == Some("android.intent.category.LAUNCHER")
                            {
                                top.intent_launcher = true;
                            }
                        }
                    }
                    _ => {}
                }
                stack.push(frame);
            }
            RES_XML_END_ELEMENT_TYPE => {
                let Some(frame) = stack.pop() else { continue };

                // Closing an intent-filter marks its enclosing component.
                if frame.is_intent {
                    if let Some(parent) = stack.last_mut() {
                        if parent.comp_index.is_some() {
                            parent.has_intent_filter = true;
                            if frame.intent_main && frame.intent_launcher {
                                let idx = parent.comp_index.unwrap();
                                match parent.comp_kind {
                                    Some(CompKind::Activity) => {
                                        info.activities[idx].is_launcher = true
                                    }
                                    Some(CompKind::Service) => {
                                        info.services[idx].is_launcher = true
                                    }
                                    Some(CompKind::Receiver) => {
                                        info.receivers[idx].is_launcher = true
                                    }
                                    Some(CompKind::Provider) => {
                                        info.providers[idx].is_launcher = true
                                    }
                                    None => {}
                                }
                                if info.main_activity.is_none() {
                                    info.main_activity = parent.resolved_name.clone();
                                }
                            }
                        }
                    }
                }

                // Finalise a component's default `exported` once it has closed
                // and we know whether it declared an intent-filter. Services
                // default to false; the others to true when filtered.
                if let (Some(idx), Some(kind)) = (frame.comp_index, frame.comp_kind) {
                    if frame.explicit_exported.is_none() && kind != CompKind::Service {
                        let exported = frame.has_intent_filter;
                        match kind {
                            CompKind::Activity => info.activities[idx].exported = exported,
                            CompKind::Receiver => info.receivers[idx].exported = exported,
                            CompKind::Provider => info.providers[idx].exported = exported,
                            CompKind::Service => {}
                        }
                    }
                }
            }
            // Resource map, namespaces, etc. carry no information we need.
            _ => {}
        }

        off += csize;
    }

    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rules() {
        let info = ApkInfo { package: "com.example.app".into(), ..Default::default() };
        assert_eq!(info.resolve(".Main"), "com.example.app.Main");
        assert_eq!(info.resolve("Main"), "com.example.app.Main");
        assert_eq!(info.resolve("a.b.C"), "a.b.C");
    }

    // -- AXML builder --------------------------------------------------------

    /// An attribute value for the test builder.
    enum A {
        Str(&'static str),
        Int(u32),
        Bool(bool),
    }

    /// A compact AXML document builder (UTF-16LE string pool).
    struct Axml {
        strings: Vec<String>,
        body: Vec<u8>,
    }

    impl Axml {
        fn new() -> Self {
            Axml { strings: Vec::new(), body: Vec::new() }
        }

        fn intern(&mut self, s: &str) -> u32 {
            if let Some(i) = self.strings.iter().position(|x| x == s) {
                return i as u32;
            }
            self.strings.push(s.to_string());
            (self.strings.len() - 1) as u32
        }

        fn start(&mut self, name: &str, attrs: &[(&str, A)]) {
            let name_idx = self.intern(name);
            let size = 36 + 20 * attrs.len();
            let mut c = Vec::new();
            c.extend(RES_XML_START_ELEMENT_TYPE.to_le_bytes());
            c.extend(16u16.to_le_bytes());
            c.extend((size as u32).to_le_bytes());
            c.extend(0u32.to_le_bytes()); // line
            c.extend(NO_INDEX.to_le_bytes()); // comment
            c.extend(NO_INDEX.to_le_bytes()); // namespace
            c.extend(name_idx.to_le_bytes());
            c.extend(20u16.to_le_bytes()); // attribute_start
            c.extend(20u16.to_le_bytes()); // attribute_size
            c.extend((attrs.len() as u16).to_le_bytes());
            c.extend(0u16.to_le_bytes()); // id index
            c.extend(0u16.to_le_bytes()); // class index
            c.extend(0u16.to_le_bytes()); // style index
            for (an, av) in attrs {
                let an_idx = self.intern(an);
                c.extend(NO_INDEX.to_le_bytes()); // attribute namespace
                c.extend(an_idx.to_le_bytes());
                match av {
                    A::Str(v) => {
                        let vi = self.intern(v);
                        c.extend(vi.to_le_bytes()); // raw_value
                        c.extend(8u16.to_le_bytes()); // typed_size
                        c.push(0); // res0
                        c.push(TYPE_STRING);
                        c.extend(vi.to_le_bytes());
                    }
                    A::Int(v) => {
                        c.extend(NO_INDEX.to_le_bytes());
                        c.extend(8u16.to_le_bytes());
                        c.push(0);
                        c.push(TYPE_INT_DEC);
                        c.extend(v.to_le_bytes());
                    }
                    A::Bool(v) => {
                        c.extend(NO_INDEX.to_le_bytes());
                        c.extend(8u16.to_le_bytes());
                        c.push(0);
                        c.push(TYPE_INT_BOOLEAN);
                        c.extend((if *v { 1u32 } else { 0u32 }).to_le_bytes());
                    }
                }
            }
            self.body.extend(c);
        }

        fn end(&mut self, name: &str) {
            let name_idx = self.intern(name);
            let mut c = Vec::new();
            c.extend(RES_XML_END_ELEMENT_TYPE.to_le_bytes());
            c.extend(16u16.to_le_bytes());
            c.extend(24u32.to_le_bytes());
            c.extend(0u32.to_le_bytes()); // line
            c.extend(NO_INDEX.to_le_bytes()); // comment
            c.extend(NO_INDEX.to_le_bytes()); // namespace
            c.extend(name_idx.to_le_bytes());
            self.body.extend(c);
        }

        fn finish(self) -> Vec<u8> {
            let pool = build_string_pool(&self.strings);
            let mut body = pool;
            body.extend(self.body);
            let total = 8 + body.len();
            let mut out = Vec::new();
            out.extend(RES_XML_TYPE.to_le_bytes());
            out.extend(8u16.to_le_bytes());
            out.extend((total as u32).to_le_bytes());
            out.extend(body);
            out
        }
    }

    /// Build a UTF-16LE string pool chunk.
    fn build_string_pool(strings: &[String]) -> Vec<u8> {
        let mut blob = Vec::new();
        let mut offsets = Vec::with_capacity(strings.len());
        for s in strings {
            offsets.push(blob.len() as u32);
            let units: Vec<u16> = s.encode_utf16().collect();
            blob.extend((units.len() as u16).to_le_bytes());
            for u in &units {
                blob.extend(u.to_le_bytes());
            }
            blob.extend(0u16.to_le_bytes());
        }

        let count = strings.len();
        let strings_start = 28 + count * 4;
        let size = strings_start + blob.len();

        let mut c = Vec::new();
        c.extend(RES_STRING_POOL_TYPE.to_le_bytes());
        c.extend(28u16.to_le_bytes());
        c.extend((size as u32).to_le_bytes());
        c.extend((count as u32).to_le_bytes()); // string_count
        c.extend(0u32.to_le_bytes()); // style_count
        c.extend(0u32.to_le_bytes()); // flags (UTF-16LE)
        c.extend((strings_start as u32).to_le_bytes());
        c.extend(0u32.to_le_bytes()); // styles_start
        for o in offsets {
            c.extend(o.to_le_bytes());
        }
        c.extend(blob);
        c
    }

    /// Build a minimal single-entry (stored) ZIP archive.
    fn make_zip(name: &str, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let name_bytes = name.as_bytes();
        let crc: u32 = 0; // never validated by the reader

        // Local file header.
        let local_offset = out.len() as u32;
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(0u16.to_le_bytes()); // flags
        out.extend(0u16.to_le_bytes()); // method = stored
        out.extend(0u16.to_le_bytes()); // time
        out.extend(0u16.to_le_bytes()); // date
        out.extend(crc.to_le_bytes());
        out.extend((content.len() as u32).to_le_bytes()); // compressed size
        out.extend((content.len() as u32).to_le_bytes()); // uncompressed size
        out.extend((name_bytes.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra length
        out.extend(name_bytes);
        out.extend(content);

        // Central directory.
        let cd_offset = out.len() as u32;
        out.extend(0x0201_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version made by
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(0u16.to_le_bytes()); // flags
        out.extend(0u16.to_le_bytes()); // method
        out.extend(0u16.to_le_bytes()); // time
        out.extend(0u16.to_le_bytes()); // date
        out.extend(crc.to_le_bytes());
        out.extend((content.len() as u32).to_le_bytes());
        out.extend((content.len() as u32).to_le_bytes());
        out.extend((name_bytes.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes()); // extra length
        out.extend(0u16.to_le_bytes()); // comment length
        out.extend(0u16.to_le_bytes()); // disk number start
        out.extend(0u16.to_le_bytes()); // internal attributes
        out.extend(0u32.to_le_bytes()); // external attributes
        out.extend(local_offset.to_le_bytes());
        out.extend(name_bytes);

        let cd_size = out.len() as u32 - cd_offset;

        // End of central directory.
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // disk number
        out.extend(0u16.to_le_bytes()); // disk with central directory
        out.extend(1u16.to_le_bytes()); // entries on this disk
        out.extend(1u16.to_le_bytes()); // total entries
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes()); // comment length
        out
    }

    /// A small hand-built manifest document used by several tests.
    fn demo_manifest_axml() -> Vec<u8> {
        let mut b = Axml::new();
        b.start(
            "manifest",
            &[
                ("package", A::Str("com.example.demo")),
                ("versionName", A::Str("1.0")),
                ("versionCode", A::Int(1)),
            ],
        );
        b.start("uses-sdk", &[("minSdkVersion", A::Int(21)), ("targetSdkVersion", A::Int(36))]);
        b.end("uses-sdk");
        b.start("uses-permission", &[("name", A::Str("android.permission.INTERNET"))]);
        b.end("uses-permission");
        b.start("application", &[("label", A::Str("Demo"))]);
        b.start("activity", &[("name", A::Str(".MainActivity"))]);
        b.start("intent-filter", &[]);
        b.start("action", &[("name", A::Str("android.intent.action.MAIN"))]);
        b.end("action");
        b.start("category", &[("name", A::Str("android.intent.category.LAUNCHER"))]);
        b.end("category");
        b.end("intent-filter");
        b.end("activity");
        b.end("application");
        b.end("manifest");
        b.finish()
    }

    #[test]
    fn parses_demo_manifest() {
        let axml = demo_manifest_axml();
        let info = parse_axml(&axml).expect("parse_axml");

        assert_eq!(info.package, "com.example.demo");
        assert_eq!(info.version_name.as_deref(), Some("1.0"));
        assert_eq!(info.version_code, Some(1));
        assert_eq!(info.min_sdk, Some(21));
        assert_eq!(info.target_sdk, Some(36));
        assert_eq!(info.label.as_deref(), Some("Demo"));
        assert_eq!(info.main_activity.as_deref(), Some("com.example.demo.MainActivity"));
        assert!(info.permissions.iter().any(|p| p == "android.permission.INTERNET"));

        let activity = info
            .activities
            .iter()
            .find(|a| a.name == "com.example.demo.MainActivity")
            .expect("launcher activity present");
        assert!(activity.is_launcher);
        // Activities with an intent-filter default to exported.
        assert!(activity.exported);
    }

    #[test]
    fn extracts_manifest_from_zip() {
        let axml = demo_manifest_axml();
        let zip = make_zip("AndroidManifest.xml", &axml);

        let path = std::env::temp_dir().join(format!("pengudroid_apk_test_{}.apk", std::process::id()));
        let path_str = path.to_str().expect("temp path is UTF-8");
        std::fs::write(&path, &zip).expect("write temp apk");

        let extracted = extract_manifest(path_str).expect("extract_manifest");
        assert_eq!(extracted, axml);

        let entries = apk_entries(path_str).expect("apk_entries");
        assert_eq!(entries, vec!["AndroidManifest.xml".to_string()]);

        // And the full round-trip through read_apk.
        let info = read_apk(path_str).expect("read_apk");
        assert_eq!(info.package, "com.example.demo");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn explicit_exported_and_service_default() {
        let mut b = Axml::new();
        b.start("manifest", &[("package", A::Str("com.example.svc"))]);
        b.start("service", &[("name", A::Str(".Svc")), ("exported", A::Bool(true))]);
        b.end("service");
        b.start("receiver", &[("name", A::Str(".Rec"))]);
        b.end("receiver");
        b.end("manifest");
        let info = parse_axml(&b.finish()).expect("parse_axml");

        // Explicit exported is honoured, even for services.
        assert_eq!(info.services.len(), 1);
        assert!(info.services[0].exported);
        // A receiver with no intent-filter defaults to not exported.
        assert_eq!(info.receivers.len(), 1);
        assert!(!info.receivers[0].exported);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_axml(&[0, 1, 2, 3]).is_err());
        assert!(extract_manifest("/nonexistent/pengudroid.apk").is_err());
    }

    #[test]
    fn lists_and_extracts_native_libs() {
        let zip = make_zip("lib/x86_64/libdemo.so", b"\x7fELF-not-really");
        let path = std::env::temp_dir().join("pengudroid-native-libs-test.apk");
        std::fs::write(&path, &zip).unwrap();
        let p = path.to_str().unwrap();

        let libs = native_libs(p).unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].abi, "x86_64");
        assert_eq!(libs[0].file, "libdemo.so");
        assert_eq!(libs[0].entry, "lib/x86_64/libdemo.so");
        assert_eq!(libs[0].base(), "demo");

        let bytes = extract_named(p, "lib/x86_64/libdemo.so").unwrap();
        assert_eq!(bytes, b"\x7fELF-not-really");
        assert!(extract_named(p, "lib/x86_64/missing.so").is_err());

        let _ = std::fs::remove_file(&path);
    }
}
