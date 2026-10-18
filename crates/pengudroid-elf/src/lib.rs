//! Minimal ELF reader: what does a shared library *import* and *export*?
//!
//! PenguDroid needs this to answer a concrete question for the native-library
//! milestone: given an Android `.so`, which Bionic symbols does it require, and
//! how many of them does the translation shim already provide? The answer
//! determines whether the library can be `dlopen`ed at all.
//!
//! Only what is needed is parsed: the section header table, `.dynsym` and its
//! linked string table. ELF32 and ELF64, little-endian (the shapes that
//! matter for armv7/arm64/x86-64 Android). No external dependencies and no
//! `unsafe`.
//!
//! This deliberately does **not** resolve `DT_NEEDED`, relocations, or symbol
//! versions — imports are reported by name, which is what coverage analysis
//! needs.

#![forbid(unsafe_code)]

use std::fmt;

/// ELF class (word size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Elf32,
    Elf64,
}

/// The dynamic symbols of one ELF object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfInfo {
    pub class: Class,
    /// `e_machine` (e.g. 0x28 = AArch64, 0x03 = x86, 0xB7 = AArch64...).
    pub machine: u16,
    /// Symbols referenced but not defined here (`SHN_UNDEF`).
    pub imports: Vec<String>,
    /// Symbols defined here and exported (non-`SHN_UNDEF`, non-local).
    pub exports: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfError(pub String);

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ElfError {}

type Result<T> = std::result::Result<T, ElfError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ElfError(msg.into()))
}

fn rd_u16(d: &[u8], o: usize) -> Result<u16> {
    let b = d.get(o..o + 2).ok_or_else(|| ElfError("truncated u16".into()))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn rd_u32(d: &[u8], o: usize) -> Result<u32> {
    let b = d.get(o..o + 4).ok_or_else(|| ElfError("truncated u32".into()))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn rd_u64(d: &[u8], o: usize) -> Result<u64> {
    let b = d.get(o..o + 8).ok_or_else(|| ElfError("truncated u64".into()))?;
    Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

/// Read a NUL-terminated string from a string table.
fn strz(d: &[u8], o: usize) -> Result<String> {
    if o >= d.len() {
        return err("string offset out of range");
    }
    let end = d[o..].iter().position(|&b| b == 0).map(|p| o + p).unwrap_or(d.len());
    Ok(String::from_utf8_lossy(&d[o..end]).into_owned())
}

/// A section header, normalised across classes.
struct Section {
    sh_type: u32,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

const SHT_DYNSYM: u32 = 11;
/// `.gnu.version` (symbol version indices).
const SHT_GNU_VERSYM: u32 = 0x6FFF_FFFF;
const SHN_UNDEF: u16 = 0;
/// `st_info` binding values we consider "exported".
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;

/// Parse an in-memory ELF image.
pub fn parse(d: &[u8]) -> Result<ElfInfo> {
    if d.len() < 16 || &d[0..4] != b"\x7fELF" {
        return err("not an ELF file");
    }
    let class = match d[4] {
        1 => Class::Elf32,
        2 => Class::Elf64,
        other => return err(format!("unknown ELF class {other}")),
    };
    if d[5] != 1 {
        return err("only little-endian ELF is supported");
    }

    let (machine, shoff, shentsize, shnum) = match class {
        Class::Elf64 => (
            rd_u16(d, 0x12)?,
            rd_u64(d, 0x28)? as usize,
            rd_u16(d, 0x3A)? as usize,
            rd_u16(d, 0x3C)? as usize,
        ),
        Class::Elf32 => (
            rd_u16(d, 0x12)?,
            rd_u32(d, 0x20)? as usize,
            rd_u16(d, 0x2E)? as usize,
            rd_u16(d, 0x30)? as usize,
        ),
    };

    if shoff == 0 || shnum == 0 || shentsize == 0 {
        return err("ELF has no section header table");
    }

    let mut sections = Vec::with_capacity(shnum);
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let s = match class {
            Class::Elf64 => Section {
                sh_type: rd_u32(d, base + 4)?,
                offset: rd_u64(d, base + 24)?,
                size: rd_u64(d, base + 32)?,
                link: rd_u32(d, base + 40)?,
                entsize: rd_u64(d, base + 56)?,
            },
            Class::Elf32 => Section {
                sh_type: rd_u32(d, base + 4)?,
                offset: rd_u32(d, base + 16)? as u64,
                size: rd_u32(d, base + 20)? as u64,
                link: rd_u32(d, base + 24)?,
                entsize: rd_u32(d, base + 36)? as u64,
            },
        };
        sections.push(s);
    }

    let Some(dynsym) = sections.iter().find(|s| s.sh_type == SHT_DYNSYM) else {
        return err("no .dynsym section (stripped or not a dynamic object)");
    };
    let strtab_idx = dynsym.link as usize;
    let strtab = sections.get(strtab_idx).ok_or_else(|| ElfError("bad .dynsym sh_link".into()))?;

    let sym_size = match class {
        Class::Elf64 => 24,
        Class::Elf32 => 16,
    };
    let entsize = if dynsym.entsize == 0 { sym_size as u64 } else { dynsym.entsize };
    let count = (dynsym.size / entsize) as usize;
    let str_base = strtab.offset as usize;

    let mut imports = Vec::new();
    let mut exports = Vec::new();
    for i in 0..count {
        let sym = dynsym.offset as usize + i * entsize as usize;
        let (name_off, info, shndx) = match class {
            Class::Elf64 => (rd_u32(d, sym)?, d.get(sym + 4).copied().unwrap_or(0), rd_u16(d, sym + 6)?),
            Class::Elf32 => (rd_u32(d, sym)?, d.get(sym + 12).copied().unwrap_or(0), rd_u16(d, sym + 14)?),
        };
        if name_off == 0 {
            continue; // index 0 / anonymous
        }
        let name = strz(d, str_base + name_off as usize)?;
        if name.is_empty() {
            continue;
        }
        if shndx == SHN_UNDEF {
            imports.push(name);
        } else {
            let bind = info >> 4;
            if bind == STB_GLOBAL || bind == STB_WEAK {
                exports.push(name);
            }
        }
    }

    imports.sort();
    imports.dedup();
    exports.sort();
    exports.dedup();

    Ok(ElfInfo { class, machine, imports, exports })
}

/// Parse an ELF file from disk.
pub fn read_file(path: &str) -> Result<ElfInfo> {
    let data = std::fs::read(path).map_err(|e| ElfError(format!("cannot read {path}: {e}")))?;
    parse(&data)
}

impl Class {
    pub fn bits(self) -> u32 {
        match self {
            Class::Elf32 => 32,
            Class::Elf64 => 64,
        }
    }
}

/// Human-readable machine name for common `e_machine` values.
pub fn machine_name(m: u16) -> &'static str {
    match m {
        0x02 => "sparc",
        0x03 => "x86",
        0x08 => "mips",
        0x14 => "powerpc",
        0x28 => "arm",
        0x3E => "x86-64",
        0xB7 => "aarch64",
        0xF3 => "risc-v",
        _ => "unknown",
    }
}

/// Remove Android's coarse symbol versioning so glibc's dynamic linker will
/// resolve a Bionic object's symbols.
///
/// Android's linker versions every libc symbol against a **single version node
/// per library** — Bionic's `libc.so` exports one version named `LIBC` (and
/// `LIBM`, `LD`) — whereas glibc exports granular `GLIBC_2.x` versions. Two
/// things then go wrong:
///
/// 1. the object carries a `VERNEED` for `libc.so`/`LIBC`, which a glibc
///    `libc.so` does not provide;
/// 2. its symbol references demand `free@LIBC`, `__cxa_finalize@LIBC`, ... and
///    glibc's `free@GLIBC_2.2.5` does not match a `LIBC` request.
///
/// This fixes (2): every non-zero `.gnu.version` entry is set to `1`
/// (*global*), so symbol references become **unversioned** and resolve normally
/// against glibc — or, for Bionic symbols, against the shim.
///
/// Fix (1) is not done here: the `VERNEED` stays, and is satisfied by providing
/// a `libc.so` that *does* carry a `LIBC` version node. `pengudroid libpath`
/// points the Bionic sonames at the shim, which exports `LIBC`/`LIBM`/`LD` via a
/// linker version script. Rewriting the dynamic section instead (retagging
/// `DT_VERNEED`) was tried and is unreliable: glibc still walks the `vn_aux`
/// chain, and an unrecognised tag can be misread as `DT_FILTER`/`DT_AUXILIARY`.
///
/// Returns a modified copy; the input is not touched.
pub fn strip_android_versions(d: &[u8]) -> Result<Vec<u8>> {
    if d.len() < 64 || &d[0..4] != b"\x7fELF" {
        return err("not an ELF file");
    }
    let is64 = match d[4] {
        1 => false,
        2 => true,
        other => return err(format!("unknown ELF class {other}")),
    };
    if d[5] != 1 {
        return err("only little-endian ELF is supported");
    }
    let (shoff, shentsize, shnum) = if is64 {
        (
            rd_u64(d, 0x28)? as usize,
            rd_u16(d, 0x3A)? as usize,
            rd_u16(d, 0x3C)? as usize,
        )
    } else {
        (
            rd_u32(d, 0x20)? as usize,
            rd_u16(d, 0x2E)? as usize,
            rd_u16(d, 0x30)? as usize,
        )
    };
    if shoff == 0 || shnum == 0 || shentsize == 0 {
        return err("ELF has no section header table");
    }

    // Find the symbol-version array.
    let mut versym: Option<(usize, usize)> = None;
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let sh_type = rd_u32(d, base + 4)?;
        let (off, size) = if is64 {
            (rd_u64(d, base + 24)? as usize, rd_u64(d, base + 32)? as usize)
        } else {
            (rd_u32(d, base + 16)? as usize, rd_u32(d, base + 20)? as usize)
        };
        if sh_type == SHT_GNU_VERSYM {
            versym = Some((off, size));
        }
    }

    let mut out = d.to_vec();

    // Drop per-symbol version selection: every non-zero entry becomes *global*.
    if let Some((off, size)) = versym {
        for k in 0..(size / 2) {
            let p = off + k * 2;
            if p + 2 <= out.len() && (out[p] != 0 || out[p + 1] != 0) {
                out[p] = 1; // *global*
                out[p + 1] = 0;
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_elf() {
        assert!(parse(&[0, 1, 2, 3]).is_err());
        assert!(parse(b"not an elf at all").is_err());
    }

    #[test]
    fn parses_the_running_test_binary() {
        // The test binary is a real dynamically-linked ELF: it must have
        // undefined dynamic symbols (libc imports) and parse cleanly.
        let exe = std::env::current_exe().expect("current_exe");
        let info = read_file(exe.to_str().unwrap()).expect("parse test binary");
        assert_eq!(info.class, Class::Elf64);
        assert!(!info.imports.is_empty(), "a linked ELF should import symbols");
        // Symbols must be clean, printable names.
        for s in info.imports.iter().chain(info.exports.iter()) {
            assert!(!s.contains('\0'));
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn reports_a_known_libc_import() {
        // Every Rust test binary imports something from libc/libgcc.
        let exe = std::env::current_exe().unwrap();
        let info = read_file(exe.to_str().unwrap()).unwrap();
        let looks_libc = info.imports.iter().any(|s| {
            s.starts_with("__libc") || s == "malloc" || s == "free" || s.starts_with("pthread_")
                || s.starts_with("mem")
        });
        assert!(
            looks_libc,
            "expected libc imports, got: {:?}",
            &info.imports[..info.imports.len().min(20)]
        );
    }

    #[test]
    fn strip_preserves_symbols_on_a_normal_binary() {
        // A host binary has no Android versions, so stripping must be a no-op
        // from the symbol table's point of view and must round-trip cleanly.
        let exe = std::env::current_exe().unwrap();
        let data = std::fs::read(&exe).unwrap();
        let before = parse(&data).unwrap();
        let stripped = strip_android_versions(&data).unwrap();
        assert_eq!(stripped.len(), data.len());
        let after = parse(&stripped).unwrap();
        assert_eq!(before.imports, after.imports);
        assert_eq!(before.exports, after.exports);
    }

    #[test]
    fn strip_rejects_non_elf() {
        assert!(strip_android_versions(&[0, 1, 2, 3]).is_err());
    }
}
