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
/// `.gnu.version_r` (version requirements, `DT_VERNEED`).
const SHT_GNU_VERNEED: u32 = 0x6FFF_FFFE;
/// `VER_FLG_WEAK`: a version requirement glibc is allowed to ignore.
const VER_FLG_WEAK: u16 = 0x2;
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

/// Android's packed-relocation section types and dynamic tags.
const SHT_ANDROID_REL: u32 = 0x6000_0001;
const SHT_ANDROID_RELA: u32 = 0x6000_0002;
const SHT_DYNAMIC: u32 = 6;
const SHT_RELA: u32 = 4;
const SHT_REL: u32 = 9;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_REL: i64 = 17;
const DT_RELSZ: i64 = 18;
const DT_ANDROID_REL: i64 = 0x6000_000f;
const DT_ANDROID_RELSZ: i64 = 0x6000_0010;
const DT_ANDROID_RELA: i64 = 0x6000_0011;
const DT_ANDROID_RELASZ: i64 = 0x6000_0012;
const DT_RELR: i64 = 0x24;
const DT_RELRSZ: i64 = 0x23;
const DT_RELRENT: i64 = 0x25;
const DT_ANDROID_RELR: i64 = 0x6fff_e000;
const DT_ANDROID_RELRSZ: i64 = 0x6fff_e001;
const DT_ANDROID_RELRENT: i64 = 0x6fff_e003;
const SHT_RELR: u32 = 0x13;
const SHT_ANDROID_RELR: u32 = 0x6fff_ff00;
const PT_LOAD: u32 = 1;

/// `RELOCATION_GROUPED_BY_INFO_FLAG` — every relocation in the group shares one
/// `r_info`.
const GROUPED_BY_INFO: i64 = 1;
/// `RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG` — offsets advance by a fixed delta.
const GROUPED_BY_OFFSET_DELTA: i64 = 2;
/// `RELOCATION_GROUPED_BY_ADDEND_FLAG` — the group shares one addend delta.
const GROUPED_BY_ADDEND: i64 = 4;
/// `RELOCATION_GROUP_HAS_ADDEND_FLAG` — the group's relocations carry addends.
const HAS_ADDEND: i64 = 8;

/// A cursor over a signed LEB128 stream.
struct SlebReader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl SlebReader<'_> {
    fn read(&mut self) -> Result<i64> {
        let mut result: i64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = *self
                .d
                .get(self.pos)
                .ok_or_else(|| ElfError("truncated sleb128".into()))?;
            self.pos += 1;
            result |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                // Sign-extend from the final byte's bit 6.
                if shift < 64 && byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
            if shift >= 64 {
                return err("sleb128 value too long");
            }
        }
    }
}

/// One decoded relocation: `(r_offset, r_info, r_addend)`.
type Reloc = (u64, u64, i64);

/// Decode an Android `APS2` packed-relocation stream.
///
/// The format (see bionic's `linker_reloc_iterators.h`) is a header of
/// `num_relocs` and an initial `r_offset`, then a sequence of *groups*. Each
/// group declares a size and a flag word; depending on the flags the group's
/// `r_info`, offset delta and addend delta are either shared by the whole group
/// or read per relocation. Offsets, `r_info` and addends all accumulate across
/// groups, which is what makes the encoding compact.
///
/// `with_addend` selects the `Elf64_Rela` shape (addends present) over the
/// `Elf32_Rel` one (no addends).
fn decode_aps2(data: &[u8], with_addend: bool) -> Result<Vec<Reloc>> {
    if data.len() < 4 || &data[0..4] != b"APS2" {
        return err("not an APS2 packed relocation stream");
    }
    let mut r = SlebReader { d: data, pos: 4 };
    let num_relocs = r.read()? as usize;
    let mut reloc_offset = r.read()? as u64;
    let mut reloc_info: u64 = 0;
    let mut reloc_addend: i64 = 0;

    let mut out = Vec::with_capacity(num_relocs);
    let mut idx = 0usize;
    while idx < num_relocs {
        let group_size = r.read()? as usize;
        let group_flags = r.read()?;

        let mut group_offset_delta: i64 = 0;
        if group_flags & GROUPED_BY_OFFSET_DELTA != 0 {
            group_offset_delta = r.read()?;
        }
        if group_flags & GROUPED_BY_INFO != 0 {
            reloc_info = r.read()? as u64;
        }

        let grouped_by_addend = group_flags & GROUPED_BY_ADDEND != 0;
        let has_addend = with_addend && group_flags & HAS_ADDEND != 0;
        if has_addend {
            if grouped_by_addend {
                reloc_addend = reloc_addend.wrapping_add(r.read()?);
            }
        } else {
            reloc_addend = 0;
        }

        for _ in 0..group_size {
            if group_flags & GROUPED_BY_OFFSET_DELTA != 0 {
                reloc_offset = reloc_offset.wrapping_add(group_offset_delta as u64);
            } else {
                reloc_offset = reloc_offset.wrapping_add(r.read()? as u64);
            }
            if group_flags & GROUPED_BY_INFO == 0 {
                reloc_info = r.read()? as u64;
            }
            if has_addend && !grouped_by_addend {
                reloc_addend = reloc_addend.wrapping_add(r.read()?);
            }
            out.push((reloc_offset, reloc_info, reloc_addend));
        }
        idx += group_size;
    }
    Ok(out)
}

/// Rewrite Android's packed relocations into the standard `RELA`/`REL` form
/// glibc's dynamic linker understands.
///
/// Android compresses `.rela.dyn` with the `APS2` scheme and tags it
/// `DT_ANDROID_RELA`, a tag glibc does not recognise — so a modern Android `.so`
/// loads with *no* relocations applied and crashes in its first constructor.
///
/// The unpacked table is larger than the packed one, so it cannot be written in
/// place. Instead it is appended at the end of the file together with a **new
/// `PT_LOAD`** that maps it, the program-header table is moved to the end (with
/// one extra entry) and `DT_ANDROID_RELA`/`DT_ANDROID_RELASZ` are retagged as
/// `DT_RELA`/`DT_RELASZ`. A new segment is used rather than extending the last
/// one because that segment usually carries `.bss`, and growing its file size
/// would overwrite the zero-filled tail.
///
/// Returns `None` when the object has no packed relocations (the common case for
/// host binaries and older Android objects), so callers can avoid a copy.
pub fn unpack_android_relocations(d: &[u8]) -> Result<Option<Vec<u8>>> {
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

    let (phoff, phentsize, phnum, shoff, shentsize, shnum) = if is64 {
        (
            rd_u64(d, 0x20)? as usize,
            rd_u16(d, 0x36)? as usize,
            rd_u16(d, 0x38)? as usize,
            rd_u64(d, 0x28)? as usize,
            rd_u16(d, 0x3A)? as usize,
            rd_u16(d, 0x3C)? as usize,
        )
    } else {
        (
            rd_u32(d, 0x1C)? as usize,
            rd_u16(d, 0x2A)? as usize,
            rd_u16(d, 0x2C)? as usize,
            rd_u32(d, 0x20)? as usize,
            rd_u16(d, 0x2E)? as usize,
            rd_u16(d, 0x30)? as usize,
        )
    };
    if shoff == 0 || shnum == 0 || shentsize == 0 {
        return Ok(None);
    }

    // Locate the packed-relocation section and the dynamic section.
    let mut packed: Option<(usize, usize, bool)> = None; // (offset, size, is_rela)
    let mut dynamic: Option<(usize, usize)> = None;
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let sh_type = rd_u32(d, base + 4)?;
        let (off, size) = if is64 {
            (rd_u64(d, base + 24)? as usize, rd_u64(d, base + 32)? as usize)
        } else {
            (rd_u32(d, base + 16)? as usize, rd_u32(d, base + 20)? as usize)
        };
        match sh_type {
            SHT_ANDROID_RELA => packed = Some((off, size, true)),
            SHT_ANDROID_REL => packed = Some((off, size, false)),
            SHT_DYNAMIC => dynamic = Some((off, size)),
            _ => {}
        }
    }
    // Without a dynamic section there is nothing to retag.
    let Some((dyn_off, dyn_size)) = dynamic else {
        return Ok(None);
    };

    // Decode the packed relocations, if this object has any.
    let relocs = match packed {
        Some((off, size, is_rela)) => {
            let bytes = d
                .get(off..off + size)
                .ok_or_else(|| ElfError("packed relocation section out of range".into()))?;
            Some((decode_aps2(bytes, is_rela)?, is_rela))
        }
        None => None,
    };

    let mut out = d.to_vec();
    let mut changed = false;

    // The unpacked table is larger than the packed one, so it cannot be written
    // in place: it is appended with a new `PT_LOAD` and the program-header table
    // is moved to the end. A new segment is used rather than extending the last
    // one because that segment usually carries `.bss`, and growing its file size
    // would overwrite the zero-filled tail.
    let mut rela_vaddr: u64 = 0;
    let mut rela_size: u64 = 0;
    let mut rela_off: usize = 0;
    let mut rela_ent: usize = 0;
    let mut rela_is_rela = false;
    if let Some((relocs, is_rela)) = &relocs {
        if !relocs.is_empty() {
            rela_is_rela = *is_rela;
            rela_ent = if is64 { 24 } else { 8 };
            rela_size = (relocs.len() * rela_ent) as u64;

            // Collect the loadable segments so the new one can be placed after
            // them.
            let mut max_vaddr_end: u64 = 0;
            for i in 0..phnum {
                let base = phoff + i * phentsize;
                if rd_u32(d, base)? != PT_LOAD {
                    continue;
                }
                let (p_vaddr, p_memsz) = if is64 {
                    (rd_u64(d, base + 16)?, rd_u64(d, base + 40)?)
                } else {
                    (rd_u32(d, base + 8)? as u64, rd_u32(d, base + 20)? as u64)
                };
                max_vaddr_end = max_vaddr_end.max(p_vaddr + p_memsz);
            }

            // Lay out the appended data: [new PHDR table][RELA array].
            while out.len() % 8 != 0 {
                out.push(0);
            }
            let new_phoff = out.len();
            let new_phnum = phnum + 1;
            let phdr_bytes = new_phnum * phentsize;
            out.resize(new_phoff + phdr_bytes, 0);
            while out.len() % 8 != 0 {
                out.push(0);
            }
            rela_off = out.len();
            out.resize(rela_off + rela_size as usize, 0);

            // The new segment maps the whole appended block. Its virtual address
            // is placed past every existing segment, and `p_vaddr - p_offset` is
            // kept a multiple of the page size, which the loader requires.
            let total = (rela_off + rela_size as usize - new_phoff) as u64;
            let p_offset = new_phoff as u64;
            let base = (max_vaddr_end + 0xFFF) & !0xFFF;
            let p_vaddr = base + (p_offset & 0xFFF);
            rela_vaddr = p_vaddr + (rela_off as u64 - p_offset);

            // Copy the original program headers, then append the new PT_LOAD.
            let orig_phdrs = d
                .get(phoff..phoff + phnum * phentsize)
                .ok_or_else(|| ElfError("program header table out of range".into()))?;
            out[new_phoff..new_phoff + phnum * phentsize].copy_from_slice(orig_phdrs);
            let new_ph = new_phoff + phnum * phentsize;
            if is64 {
                w_u32(&mut out, new_ph, PT_LOAD);
                w_u32(&mut out, new_ph + 4, 4); // PF_R
                w_u64(&mut out, new_ph + 8, p_offset);
                w_u64(&mut out, new_ph + 16, p_vaddr);
                w_u64(&mut out, new_ph + 24, p_vaddr);
                w_u64(&mut out, new_ph + 32, total);
                w_u64(&mut out, new_ph + 40, total);
                w_u64(&mut out, new_ph + 48, 0x1000);
            } else {
                w_u32(&mut out, new_ph, PT_LOAD);
                w_u32(&mut out, new_ph + 4, p_offset as u32);
                w_u32(&mut out, new_ph + 8, p_vaddr as u32);
                w_u32(&mut out, new_ph + 12, p_vaddr as u32);
                w_u32(&mut out, new_ph + 16, total as u32);
                w_u32(&mut out, new_ph + 20, total as u32);
                w_u32(&mut out, new_ph + 24, 4); // PF_R
                w_u32(&mut out, new_ph + 28, 0x1000);
            }

            // Write the unpacked relocations.
            for (i, &(r_offset, r_info, r_addend)) in relocs.iter().enumerate() {
                let p = rela_off + i * rela_ent;
                if is64 {
                    w_u64(&mut out, p, r_offset);
                    w_u64(&mut out, p + 8, r_info);
                    w_u64(&mut out, p + 16, r_addend as u64);
                } else {
                    w_u32(&mut out, p, r_offset as u32);
                    w_u32(&mut out, p + 4, r_info as u32);
                }
            }

            // Point the ELF header at the moved program-header table.
            if is64 {
                w_u64(&mut out, 0x20, new_phoff as u64);
                w_u16(&mut out, 0x38, new_phnum as u16);
            } else {
                w_u32(&mut out, 0x1C, new_phoff as u32);
                w_u16(&mut out, 0x2C, new_phnum as u16);
            }
            changed = true;
        }
    }

    // Retag Android's relocation dynamic entries as the standard ones glibc
    // understands. `DT_ANDROID_RELR` carries the standard RELR payload, so only
    // the tag needs changing.
    let dyn_ent = if is64 { 16 } else { 8 };
    let mut i = 0usize;
    while i + dyn_ent <= dyn_size {
        let p = dyn_off + i;
        let (tag, val) = if is64 {
            (rd_u64(d, p)? as i64, rd_u64(d, p + 8)?)
        } else {
            (rd_u32(d, p)? as i32 as i64, rd_u32(d, p + 4)? as u64)
        };
        let (new_tag, new_val) = match tag {
            DT_ANDROID_RELA => (DT_RELA, rela_vaddr),
            DT_ANDROID_RELASZ => (DT_RELASZ, rela_size),
            DT_ANDROID_REL => (DT_REL, rela_vaddr),
            DT_ANDROID_RELSZ => (DT_RELSZ, rela_size),
            DT_ANDROID_RELR => (DT_RELR, val),
            DT_ANDROID_RELRSZ => (DT_RELRSZ, val),
            DT_ANDROID_RELRENT => (DT_RELRENT, val),
            _ => (tag, val),
        };
        if new_tag != tag || new_val != val {
            if is64 {
                w_u64(&mut out, p, new_tag as u64);
                w_u64(&mut out, p + 8, new_val);
            } else {
                w_u32(&mut out, p, new_tag as u32);
                w_u32(&mut out, p + 4, new_val as u32);
            }
            changed = true;
        }
        i += dyn_ent;
    }

    // Keep the section headers honest so `readelf` shows the real tables.
    for i in 0..shnum {
        let base = shoff + i * shentsize;
        let sh_type = rd_u32(d, base + 4)?;
        match sh_type {
            SHT_ANDROID_RELA | SHT_ANDROID_REL if rela_size > 0 => {
                let new_type = if rela_is_rela { SHT_RELA } else { SHT_REL };
                w_u32(&mut out, base + 4, new_type);
                if is64 {
                    w_u64(&mut out, base + 24, rela_off as u64);
                    w_u64(&mut out, base + 32, rela_size);
                    w_u64(&mut out, base + 48, rela_vaddr);
                    w_u64(&mut out, base + 56, rela_ent as u64);
                } else {
                    w_u32(&mut out, base + 16, rela_off as u32);
                    w_u32(&mut out, base + 20, rela_size as u32);
                    w_u32(&mut out, base + 12, rela_vaddr as u32);
                    w_u32(&mut out, base + 36, rela_ent as u32);
                }
            }
            SHT_ANDROID_RELR => {
                w_u32(&mut out, base + 4, SHT_RELR);
            }
            _ => {}
        }
    }

    if changed {
        Ok(Some(out))
    } else {
        Ok(None)
    }
}

fn w_u16(d: &mut [u8], o: usize, v: u16) {
    d[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn w_u32(d: &mut [u8], o: usize, v: u32) {
    d[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn w_u64(d: &mut [u8], o: usize, v: u64) {
    d[o..o + 8].copy_from_slice(&v.to_le_bytes());
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
/// Fix (1) is handled two ways. For the Bionic sonames (`libc.so`, `libm.so`,
/// `libdl.so`) the `VERNEED` is satisfied by pointing those sonames at the shim,
/// which exports `LIBC`/`LIBM`/`LD` via a linker version script. For **every
/// other** version node — Android-only ones such as `ZLIB_NDK`,
/// `LIBUNWINDSTACK`, `LIBBACKTRACE`, ... — there is no host library that carries
/// them. glibc's `_dl_check_map_versions` rejects a missing, non-weak version
/// requirement, so this function marks every `Vernaux` entry `VER_FLG_WEAK`:
/// glibc then skips the lookup and resolves the (already unversioned) symbol by
/// name. Rewriting the dynamic section (retagging `DT_VERNEED`) was tried and is
/// unreliable: glibc still walks the `vn_aux` chain, and an unrecognised tag can
/// be misread as `DT_FILTER`/`DT_AUXILIARY`.
///
/// Returns a modified copy; the input is not touched.
pub fn strip_android_versions(d: &[u8]) -> Result<Vec<u8>> {
    // Packed relocations are expanded first: the unpacker rewrites the dynamic
    // section and appends a segment, and the version fixups below must operate
    // on the result.
    let unpacked = unpack_android_relocations(d)?;
    let d: &[u8] = unpacked.as_deref().unwrap_or(d);
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

    // Find the symbol-version array and the version-requirement table.
    let mut versym: Option<(usize, usize)> = None;
    let mut verneed: Option<(usize, usize)> = None;
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
        } else if sh_type == SHT_GNU_VERNEED {
            verneed = Some((off, size));
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

    // Mark every version requirement weak, so a node no host library carries
    // (Android's `ZLIB_NDK`, `LIBUNWINDSTACK`, ...) is not a load error.
    if let Some((off, size)) = verneed {
        mark_verneed_weak(&mut out, off, size);
    }

    Ok(out)
}

/// Walk the `.gnu.version_r` (`Verneed`/`Vernaux`) chain and set `VER_FLG_WEAK`
/// on every auxiliary entry.
///
/// `Elf{32,64}_Verneed` and `Elf{32,64}_Vernaux` share the same field offsets in
/// both classes, so one walker covers ELF32 and ELF64. Offsets are bounds-checked
/// against `out`; a malformed table simply stops the walk.
fn mark_verneed_weak(out: &mut [u8], off: usize, size: usize) {
    let end = off.saturating_add(size).min(out.len());
    let mut vn = off;
    // A `Verneed` is 16 bytes; guard against a zero/looping `vn_next`.
    while vn + 16 <= end {
        let vn_cnt = u16::from_le_bytes([out[vn + 2], out[vn + 3]]) as usize;
        let vn_aux = u32::from_le_bytes([out[vn + 8], out[vn + 9], out[vn + 10], out[vn + 11]]) as usize;
        let vn_next = u32::from_le_bytes([out[vn + 12], out[vn + 13], out[vn + 14], out[vn + 15]]) as usize;

        // Each `Vernaux` is 16 bytes; `vna_flags` is a u16 at +4.
        let mut va = vn + vn_aux;
        for _ in 0..vn_cnt {
            if va + 16 > end {
                break;
            }
            let flags = u16::from_le_bytes([out[va + 4], out[va + 5]]);
            let flags = flags | VER_FLG_WEAK;
            out[va + 4..va + 6].copy_from_slice(&flags.to_le_bytes());
            let vna_next = u32::from_le_bytes([out[va + 12], out[va + 13], out[va + 14], out[va + 15]]) as usize;
            if vna_next == 0 {
                break;
            }
            va += vna_next;
        }

        if vn_next == 0 {
            break;
        }
        vn += vn_next;
    }
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

    /// Encode a value as signed LEB128 (test helper mirroring the decoder).
    fn sleb(mut v: i64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            let done = (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0);
            out.push(if done { byte } else { byte | 0x80 });
            if done {
                break;
            }
        }
    }

    #[test]
    fn decodes_a_grouped_aps2_stream() {
        // Two relocations sharing one r_info and a fixed offset delta.
        let mut s = b"APS2".to_vec();
        sleb(2, &mut s); // num_relocs
        sleb(0, &mut s); // initial r_offset
        sleb(2, &mut s); // group_size
        sleb(GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO, &mut s);
        sleb(0x10, &mut s); // group_r_offset_delta
        sleb(0x100, &mut s); // r_info
        let relocs = decode_aps2(&s, true).unwrap();
        assert_eq!(relocs, vec![(0x10, 0x100, 0), (0x20, 0x100, 0)]);
    }

    #[test]
    fn decodes_aps2_with_per_reloc_addends() {
        let mut s = b"APS2".to_vec();
        sleb(1, &mut s); // num_relocs
        sleb(0x1000, &mut s); // initial r_offset
        sleb(1, &mut s); // group_size
        sleb(HAS_ADDEND, &mut s); // flags: addends present, not grouped
        sleb(0x8, &mut s); // r_offset delta
        sleb(0x200, &mut s); // r_info
        sleb(0x10, &mut s); // r_addend delta
        let relocs = decode_aps2(&s, true).unwrap();
        assert_eq!(relocs, vec![(0x1008, 0x200, 0x10)]);
    }

    #[test]
    fn decode_rejects_a_bad_magic() {
        assert!(decode_aps2(b"XXXX\x00", true).is_err());
    }

    #[test]
    fn unpack_is_a_noop_without_packed_relocations() {
        // A host binary carries no `APS2` section, so nothing is rewritten.
        let exe = std::env::current_exe().unwrap();
        let data = std::fs::read(&exe).unwrap();
        assert!(unpack_android_relocations(&data).unwrap().is_none());
    }

    #[test]
    fn unpack_rejects_non_elf() {
        assert!(unpack_android_relocations(&[0, 1, 2, 3]).is_err());
    }
}
