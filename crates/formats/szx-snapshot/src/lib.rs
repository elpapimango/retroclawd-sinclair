//! Parser/writer for the ZX Spectrum `.szx` (ZX-State) snapshot file
//! format.
//!
//! Format reference: `libspectrum`'s own real implementation
//! (`github.com/fuse-emulator/libspectrum`, `szx.c`), shallow-cloned and
//! read directly this session -- every byte offset, block-header shape,
//! machine-id numbering, and bit layout below came from that source, not
//! a doc summary (an earlier planning pass had one, and it disagreed with
//! itself about whether the block header was 4 or 8 bytes -- reading the
//! real code settled it: 8, a 4-byte ASCII ID plus a 4-byte little-endian
//! length). See `CREDITS.md` for the exact citation.
//!
//! `.szx` is a chunked (RIFF-like) format: an 8-byte file header
//! (`"ZXST"` + major version + minor version + machine id + flags byte),
//! then a stream of 8-byte-header blocks, each safely skippable by its
//! own length if this crate doesn't recognize the block id -- same
//! "must-skip-not-error" design `zx_tape::parse_pzx` already uses for its
//! own unknown block types (real `.szx` files always carry at least a
//! `CRTR` creator-info block this crate has no reason to read).
//!
//! Deliberately format-only, same split as `z80-snapshot`/`sna-snapshot`:
//! no dependency on `machine` or `zx_spectrum`, just the shared `z80::Cpu`
//! register type plus this crate's own `PagingState`/`Paging2A3State`
//! (same field shapes as `z80_snapshot`'s own, duplicated rather than
//! imported -- format crates stay siblings, not chained, matching how
//! `sna-snapshot` already duplicates rather than depends on
//! `z80-snapshot`). Applying a parsed `Snapshot` to a running machine is
//! `zx_spectrum`'s own job (see `ZxSpectrum::load_snapshot`).
//!
//! Only the blocks needed for a real load/save are implemented: `Z80R`
//! (registers), `SPCR` (border + paging ports), `RAMP` (one per 16K RAM
//! page, optionally zlib-compressed), and `AY\0\0` (the single AY chip's
//! register state -- Turbosound's second chip has no block of its own in
//! the real format and isn't modeled here). Everything else (creator
//! info, tape/disk/peripheral blocks, ...) is skipped by size on read and
//! never written.

use std::fmt;
use std::io::Read;

use z80::Cpu;

const SIGNATURE: &[u8; 4] = b"ZXST";
const HEADER_LEN: usize = 8;
const CHUNK_HEADER_LEN: usize = 8;

/// The version this crate writes: 1.1, the first that carries the
/// `chFlags`/halted-flag byte this crate actually populates on `Z80R`
/// (1.4 added `wMemPtr`, which this crate doesn't model, so there's no
/// reason to claim a higher version than what's genuinely written).
const WRITE_VERSION_MAJOR: u8 = 1;
const WRITE_VERSION_MINOR: u8 = 1;

/// One decompressed memory block -- same shape as `z80_snapshot::
/// MemoryBlock`/`sna_snapshot::MemoryBlock` (see this module's own doc
/// comment for why it's a separate, duplicated type rather than a shared
/// import).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryBlock {
    /// 48K memory: a fixed absolute address, no paging involved. `.szx`
    /// still labels these with 128K-style page numbers (5/2/0 -- the bank
    /// that would occupy this address at a 128K machine's *default*
    /// paging state), converted to/from a real address at this crate's
    /// own boundary (`page_address`/`address_page`), same pattern
    /// `z80_snapshot` already uses for its own (different) page numbers.
    Absolute { address: u16, data: Vec<u8> },
    /// 128K-family memory: one of the 8 real RAM banks (0-7) -- `.szx`'s
    /// own page number *is* the bank index directly here, no offset
    /// (verified against `libspectrum`'s real `write_ram_pages`, not
    /// guessed).
    RamBank { bank: u8, data: Vec<u8> },
}

/// Real ZX Spectrum 128K paging state (last `OUT` to port `0x7ffd`) --
/// same field shape as `z80_snapshot::PagingState`, duplicated per this
/// module's own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagingState {
    pub rom_bank: u8,
    pub ram_bank: u8,
    pub shadow_screen: bool,
    pub locked: bool,
}

/// Real ZX Spectrum +2A/+3 paging state (`0x7ffd` and `0x1ffd` combined)
/// -- same field shape as `z80_snapshot::Paging2A3State`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paging2A3State {
    pub ram_bank: u8,
    pub shadow_screen: bool,
    pub rom_bit_lo: bool,
    pub locked: bool,
    pub special_mode: bool,
    pub bits_1_2: u8,
    pub disk_motor: bool,
}

/// The single AY-3-8910's register state, from an optional `AY\0\0`
/// block. Turbosound's second chip has no block of its own in the real
/// format, so there's nothing to carry it even if this crate wanted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AyState {
    pub current_register: u8,
    pub registers: [u8; 16],
}

/// A fully parsed `.szx` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cpu: Cpu,
    pub border_color: u8,
    pub memory: Vec<MemoryBlock>,
    /// `Some` for a 128K-memory-model snapshot -- `None` for +2A/+3 too
    /// (see `paging_2a3` instead), same split `z80_snapshot` already
    /// uses and for the same reason (matches `zx_spectrum::bus`'s own
    /// `Banked128`/`Banked2A3` split).
    pub paging: Option<PagingState>,
    pub paging_2a3: Option<Paging2A3State>,
    pub ay: Option<AyState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    Spectrum48,
    Spectrum128,
    Spectrum2A3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Fewer than the fixed 8-byte file header.
    TooShort,
    WrongSignature,
    /// A real `.szx` machine id this crate has no memory model for (SAM
    /// Coupe, Pentagon, Scorpion, Timex TC2048/2068/TS2068, ...) --
    /// real, different hardware, not a guess-wrong bug.
    UnsupportedMachine(u8),
    /// Fewer than 8 bytes left for a chunk header.
    TruncatedChunkHeader,
    /// A chunk's own declared length runs past the end of the file.
    TruncatedChunk,
    /// `Z80R`'s length wasn't the fixed 37 bytes the real format always
    /// uses.
    WrongZ80RegsLength(u32),
    /// `SPCR`'s length wasn't the fixed 8 bytes the real format always
    /// uses.
    WrongSpecRegsLength(u32),
    /// `AY\0\0`'s length wasn't the fixed 18 bytes the real format always
    /// uses.
    WrongAyLength(u32),
    /// A `RAMP` chunk was too short to even carry its own flags+page
    /// header (3 bytes).
    RamPageTooShort,
    /// A `RAMP` chunk's own page number is a real bank id this crate
    /// doesn't have a machine-memory-shape mapping for.
    UnknownRamPage(u8),
    /// A `RAMP` chunk claimed zlib compression but the stream didn't
    /// decode cleanly.
    InflateFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => write!(f, "file is shorter than the 8-byte .szx header"),
            Error::WrongSignature => write!(f, "missing 'ZXST' signature"),
            Error::UnsupportedMachine(id) => write!(
                f,
                "unsupported .szx machine id {id} (16K, 48K, 128K, +2, +2A, +3, and +3E are supported; SAM Coupe/Pentagon/Scorpion/Timex clones are not)"
            ),
            Error::TruncatedChunkHeader => write!(f, "not enough data for a chunk header"),
            Error::TruncatedChunk => write!(f, "chunk length goes beyond end of file"),
            Error::WrongZ80RegsLength(len) => {
                write!(f, "Z80R chunk has length {len}, expected 37")
            }
            Error::WrongSpecRegsLength(len) => {
                write!(f, "SPCR chunk has length {len}, expected 8")
            }
            Error::WrongAyLength(len) => write!(f, "AY chunk has length {len}, expected 18"),
            Error::RamPageTooShort => write!(f, "RAMP chunk shorter than its own 3-byte header"),
            Error::UnknownRamPage(page) => write!(f, "RAMP chunk names unknown page {page}"),
            Error::InflateFailed => write!(f, "RAMP chunk's zlib-compressed data is corrupt"),
        }
    }
}

impl std::error::Error for Error {}

/// `.szx`'s own machine-id numbering (verified against `libspectrum`'s
/// real `szx_machine_type` enum, not guessed -- a doc summary this
/// project's planning pass saw earlier assumed 1=48K/2=128K/3=+2/4=+2A,
/// off by exactly one from the real values below since 16K occupies id
/// 0).
fn machine_model(id: u8) -> Result<Model, Error> {
    match id {
        0 | 1 => Ok(Model::Spectrum48), // 16K -- memory-shaped like 48K minus the unused RAM, loads fine as 48K
        2 => Ok(Model::Spectrum128),
        3 => Ok(Model::Spectrum128),     // +2: memory-identical to 128K
        4..=6 => Ok(Model::Spectrum2A3), // +2A / +3 / +3E
        15 => Ok(Model::Spectrum48),     // 48K NTSC
        16 => Ok(Model::Spectrum128),    // 128KE
        other => Err(Error::UnsupportedMachine(other)),
    }
}

/// The reverse of `machine_model` for `write` -- picks one canonical id
/// per model, same "pick a reasonable single write-side value, accept
/// several read-side" pattern `z80_snapshot::write` already uses for its
/// own +2A/+3 ambiguity.
fn model_machine_id(model: Model) -> u8 {
    match model {
        Model::Spectrum48 => 1,
        Model::Spectrum128 => 2,
        Model::Spectrum2A3 => 5, // "+3", the most generic of the three 2A3-shaped ids
    }
}

/// 48K RAM only -- `.szx`'s own page numbers for the 3 blocks a 48K
/// snapshot needs, verified against `libspectrum`'s real
/// `write_ram_pages` (unconditional page 5, then non-16K pages 2 and 0).
/// Different literal numbers from `z80_snapshot`'s own 48K page
/// convention (4/5/8) -- a different format, its own numbering.
fn page_address(page: u8) -> Option<u16> {
    match page {
        5 => Some(0x4000),
        2 => Some(0x8000),
        0 => Some(0xC000),
        _ => None,
    }
}

fn address_page(address: u16) -> Option<u8> {
    match address {
        0x4000 => Some(5),
        0x8000 => Some(2),
        0xC000 => Some(0),
        _ => None,
    }
}

/// Parses a `.szx` snapshot from raw file bytes.
pub fn parse(bytes: &[u8]) -> Result<Snapshot, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::TooShort);
    }
    if &bytes[0..4] != SIGNATURE {
        return Err(Error::WrongSignature);
    }
    let version = ((bytes[4] as u16) << 8) | bytes[5] as u16;
    let model = machine_model(bytes[6])?;
    // byte 7 (chFlags, ZXSTMF_ALTERNATETIMINGS) -- PAL/NTSC frame timing,
    // not modeled: this project's own `Region` is a `--region` CLI
    // choice, not carried by any snapshot format it already supports
    // either.

    let mut snapshot = Snapshot {
        cpu: Cpu::default(),
        border_color: 0,
        memory: Vec::new(),
        paging: matches!(model, Model::Spectrum128).then(|| PagingState {
            rom_bank: 0,
            ram_bank: 0,
            shadow_screen: false,
            locked: false,
        }),
        paging_2a3: matches!(model, Model::Spectrum2A3).then(|| Paging2A3State {
            ram_bank: 0,
            shadow_screen: false,
            rom_bit_lo: false,
            locked: false,
            special_mode: false,
            bits_1_2: 0,
            disk_motor: false,
        }),
        ay: None,
    };

    let mut pos = HEADER_LEN;
    while pos < bytes.len() {
        if bytes.len() - pos < CHUNK_HEADER_LEN {
            return Err(Error::TruncatedChunkHeader);
        }
        let id = &bytes[pos..pos + 4];
        let data_length = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += CHUNK_HEADER_LEN;
        if data_length > bytes.len() - pos {
            return Err(Error::TruncatedChunk);
        }
        let data = &bytes[pos..pos + data_length];

        match id {
            b"Z80R" => parse_z80r(&mut snapshot.cpu, data, version)?,
            b"SPCR" => parse_spcr(&mut snapshot, model, data, version)?,
            b"RAMP" => snapshot.memory.push(parse_ramp(model, data)?),
            b"AY\0\0" => snapshot.ay = Some(parse_ay(data)?),
            _ => {} // unrecognized block -- safely skipped by its own length
        }

        pos += data_length;
    }

    Ok(snapshot)
}

fn parse_z80r(cpu: &mut Cpu, data: &[u8], version: u16) -> Result<(), Error> {
    if data.len() != 37 {
        return Err(Error::WrongZ80RegsLength(data.len() as u32));
    }
    let f = data[0];
    let a = data[1];
    cpu.af = ((a as u16) << 8) | f as u16;
    cpu.bc = u16::from_le_bytes([data[2], data[3]]);
    cpu.de = u16::from_le_bytes([data[4], data[5]]);
    cpu.hl = u16::from_le_bytes([data[6], data[7]]);
    let f_ = data[8];
    let a_ = data[9];
    cpu.af_ = ((a_ as u16) << 8) | f_ as u16;
    cpu.bc_ = u16::from_le_bytes([data[10], data[11]]);
    cpu.de_ = u16::from_le_bytes([data[12], data[13]]);
    cpu.hl_ = u16::from_le_bytes([data[14], data[15]]);
    cpu.ix = u16::from_le_bytes([data[16], data[17]]);
    cpu.iy = u16::from_le_bytes([data[18], data[19]]);
    cpu.sp = u16::from_le_bytes([data[20], data[21]]);
    cpu.pc = u16::from_le_bytes([data[22], data[23]]);
    cpu.i = data[24];
    cpu.r = data[25];
    cpu.iff1 = data[26] != 0;
    cpu.iff2 = data[27] != 0;
    cpu.im = data[28];
    // data[29..33]: T-state-within-frame dword -- this crate has no bus/
    // frame concept to restore it into (format-only, see the module doc
    // comment), so it's read past but discarded, same "known
    // simplification" stance this project already documents elsewhere
    // for state a format carries that this codebase doesn't model.
    if version >= 0x0101 {
        // data[33]: chHoldIntReqCycles -- not modeled, skipped.
        let flags = data[34];
        cpu.halted = flags & ZXSTZF_HALTED != 0;
        // last-instruction-was-EI / last-instruction-set-F (the other two
        // flag bits) and, for version >= 0x0104, wMemPtr at data[35..37]
        // -- neither has a home in `z80::Cpu`, discarded.
    }
    // version < 0x0101: data[33..37] is a reserved dword, nothing to read.
    Ok(())
}

const ZXSTZF_HALTED: u8 = 0b010;

fn parse_spcr(
    snapshot: &mut Snapshot,
    model: Model,
    data: &[u8],
    version: u16,
) -> Result<(), Error> {
    if data.len() != 8 {
        return Err(Error::WrongSpecRegsLength(data.len() as u32));
    }
    let mut out_ula = data[0] & 0x07;
    let port_7ffd = data[1];
    let port_1ffd = data[2];
    if version >= 0x0101 {
        out_ula |= data[3] & 0xf8;
    }
    snapshot.border_color = out_ula & 0x07;

    // Bit meanings verified against the real 0x7ffd/0x1ffd hardware
    // ports (World of Spectrum FAQ / ZX Spectrum +3 technical manual,
    // see CREDITS.md) -- the same facts `z80_snapshot`'s own read side
    // already decodes its own (differently-positioned) paging bytes
    // with.
    match model {
        Model::Spectrum128 => {
            snapshot.paging = Some(PagingState {
                rom_bank: (port_7ffd >> 4) & 1,
                ram_bank: port_7ffd & 0b111,
                shadow_screen: port_7ffd & 0b0000_1000 != 0,
                locked: port_7ffd & 0b0010_0000 != 0,
            });
        }
        Model::Spectrum2A3 => {
            snapshot.paging_2a3 = Some(Paging2A3State {
                ram_bank: port_7ffd & 0b111,
                shadow_screen: port_7ffd & 0b0000_1000 != 0,
                rom_bit_lo: port_7ffd & 0b0001_0000 != 0,
                locked: port_7ffd & 0b0010_0000 != 0,
                special_mode: port_1ffd & 0b0000_0001 != 0,
                bits_1_2: (port_1ffd >> 1) & 0b11,
                disk_motor: port_1ffd & 0b0000_1000 != 0,
            });
        }
        Model::Spectrum48 => {}
    }

    Ok(())
}

const ZXSTRF_COMPRESSED: u16 = 1;

fn parse_ramp(model: Model, data: &[u8]) -> Result<MemoryBlock, Error> {
    if data.len() < 3 {
        return Err(Error::RamPageTooShort);
    }
    let flags = u16::from_le_bytes([data[0], data[1]]);
    let page = data[2];
    let raw = &data[3..];

    let contents: std::borrow::Cow<[u8]> = if flags & ZXSTRF_COMPRESSED != 0 {
        let mut out = Vec::with_capacity(0x4000);
        flate2::read::ZlibDecoder::new(raw)
            .read_to_end(&mut out)
            .map_err(|_| Error::InflateFailed)?;
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(raw)
    };

    match model {
        Model::Spectrum48 => {
            let address = page_address(page).ok_or(Error::UnknownRamPage(page))?;
            Ok(MemoryBlock::Absolute {
                address,
                data: contents.into_owned(),
            })
        }
        Model::Spectrum128 | Model::Spectrum2A3 => {
            if page > 7 {
                return Err(Error::UnknownRamPage(page));
            }
            Ok(MemoryBlock::RamBank {
                bank: page,
                data: contents.into_owned(),
            })
        }
    }
}

fn parse_ay(data: &[u8]) -> Result<AyState, Error> {
    if data.len() != 18 {
        return Err(Error::WrongAyLength(data.len() as u32));
    }
    // data[0]: flags (ZXSTAYF_FULLERBOX/ZXSTAYF_128AY) -- which non-AY
    // add-on is active, not AY register state itself; not modeled here.
    let current_register = data[1];
    let mut registers = [0u8; 16];
    registers.copy_from_slice(&data[2..18]);
    Ok(AyState {
        current_register,
        registers,
    })
}

/// Writes a `.szx` snapshot. Model is inferred from which of `paging`/
/// `paging_2a3` is set, same dispatch `z80_snapshot::write` already
/// uses. RAM pages are always written uncompressed (`ZXSTRF_COMPRESSED`
/// unset) -- `parse` above fully supports reading compressed pages (real
/// files from Fuse/ZEsarUX/Spectral default to compressing), but this
/// writer doesn't bother producing them: correctness doesn't depend on
/// it, and it keeps the writer simple.
pub fn write(snapshot: &Snapshot) -> Vec<u8> {
    let model = match (&snapshot.paging, &snapshot.paging_2a3) {
        (None, None) => Model::Spectrum48,
        (Some(_), None) => Model::Spectrum128,
        (None, Some(_)) => Model::Spectrum2A3,
        (Some(_), Some(_)) => unreachable!(
            "Snapshot never has both paging and paging_2a3 set -- see their own doc comments"
        ),
    };

    let mut out = Vec::new();
    out.extend_from_slice(SIGNATURE);
    out.push(WRITE_VERSION_MAJOR);
    out.push(WRITE_VERSION_MINOR);
    out.push(model_machine_id(model));
    out.push(0); // chFlags: ZXSTMF_ALTERNATETIMINGS unset -- see parse_spcr's own doc comment

    write_z80r(&mut out, &snapshot.cpu);
    write_spcr(&mut out, snapshot, model);
    for block in &snapshot.memory {
        write_ramp(&mut out, block);
    }
    if let Some(ay) = &snapshot.ay {
        write_ay(&mut out, ay);
    }

    out
}

fn write_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

fn write_z80r(out: &mut Vec<u8>, cpu: &Cpu) {
    let mut data = Vec::with_capacity(37);
    data.push(cpu.af as u8); // F
    data.push((cpu.af >> 8) as u8); // A
    data.extend_from_slice(&cpu.bc.to_le_bytes());
    data.extend_from_slice(&cpu.de.to_le_bytes());
    data.extend_from_slice(&cpu.hl.to_le_bytes());
    data.push(cpu.af_ as u8); // F'
    data.push((cpu.af_ >> 8) as u8); // A'
    data.extend_from_slice(&cpu.bc_.to_le_bytes());
    data.extend_from_slice(&cpu.de_.to_le_bytes());
    data.extend_from_slice(&cpu.hl_.to_le_bytes());
    data.extend_from_slice(&cpu.ix.to_le_bytes());
    data.extend_from_slice(&cpu.iy.to_le_bytes());
    data.extend_from_slice(&cpu.sp.to_le_bytes());
    data.extend_from_slice(&cpu.pc.to_le_bytes());
    data.push(cpu.i);
    data.push(cpu.r);
    data.push(cpu.iff1 as u8);
    data.push(cpu.iff2 as u8);
    data.push(cpu.im);
    data.extend_from_slice(&0u32.to_le_bytes()); // T-state count -- not modeled, see parse_z80r
    data.push(0); // chHoldIntReqCycles -- not modeled
    data.push(if cpu.halted { ZXSTZF_HALTED } else { 0 });
    // Version 1.1's tail (WRITE_VERSION_MINOR < 4, so this is "the hidden
    // register" + "the reserved byte", not wMemPtr) -- this crate models
    // neither, so both are zero-filled. The chunk is always exactly 37
    // bytes regardless of version (only how these last 2 bytes are
    // *interpreted* differs by version, not the length -- see
    // parse_z80r's own version branch).
    data.extend_from_slice(&[0, 0]);
    debug_assert_eq!(data.len(), 37);
    write_chunk(out, b"Z80R", &data);
}

fn write_spcr(out: &mut Vec<u8>, snapshot: &Snapshot, model: Model) {
    let (port_7ffd, port_1ffd) = match model {
        Model::Spectrum48 => (0, 0),
        Model::Spectrum128 => {
            let p = snapshot
                .paging
                .expect("write_spcr called with Model::Spectrum128 but paging is None");
            let port_7ffd = (p.ram_bank & 0b111)
                | if p.shadow_screen { 0b0000_1000 } else { 0 }
                | ((p.rom_bank & 1) << 4)
                | if p.locked { 0b0010_0000 } else { 0 };
            (port_7ffd, 0)
        }
        Model::Spectrum2A3 => {
            let p = snapshot
                .paging_2a3
                .expect("write_spcr called with Model::Spectrum2A3 but paging_2a3 is None");
            let port_7ffd = (p.ram_bank & 0b111)
                | if p.shadow_screen { 0b0000_1000 } else { 0 }
                | if p.rom_bit_lo { 0b0001_0000 } else { 0 }
                | if p.locked { 0b0010_0000 } else { 0 };
            let port_1ffd = (p.special_mode as u8)
                | ((p.bits_1_2 & 0b11) << 1)
                | if p.disk_motor { 0b0000_1000 } else { 0 };
            (port_7ffd, port_1ffd)
        }
    };

    let mut data = Vec::with_capacity(8);
    data.push(snapshot.border_color & 0x07);
    data.push(port_7ffd);
    data.push(port_1ffd);
    data.push(0); // chFe: further ULA output bits -- not modeled
    data.extend_from_slice(&[0; 4]); // reserved
    write_chunk(out, b"SPCR", &data);
}

fn write_ramp(out: &mut Vec<u8>, block: &MemoryBlock) {
    let (page, data) = match block {
        MemoryBlock::Absolute { address, data } => (
            address_page(*address).expect("MemoryBlock::Absolute at a non-48K-page address"),
            data,
        ),
        MemoryBlock::RamBank { bank, data } => (*bank, data),
    };

    let mut chunk_data = Vec::with_capacity(3 + data.len());
    chunk_data.extend_from_slice(&0u16.to_le_bytes()); // flags: uncompressed
    chunk_data.push(page);
    chunk_data.extend_from_slice(data);
    write_chunk(out, b"RAMP", &chunk_data);
}

fn write_ay(out: &mut Vec<u8>, ay: &AyState) {
    let mut data = Vec::with_capacity(18);
    data.push(0); // flags: no Fuller box / Melodik add-on
    data.push(ay.current_register);
    data.extend_from_slice(&ay.registers);
    write_chunk(out, b"AY\0\0", &data);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_header(machine_id: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SIGNATURE);
        bytes.push(1); // major
        bytes.push(4); // minor
        bytes.push(machine_id);
        bytes.push(0); // flags
        bytes
    }

    #[test]
    fn parse_rejects_a_file_without_the_signature() {
        assert_eq!(parse(b"NOPE0000"), Err(Error::WrongSignature));
    }

    #[test]
    fn parse_rejects_a_file_shorter_than_the_header() {
        assert_eq!(parse(b"ZX"), Err(Error::TooShort));
    }

    #[test]
    fn parse_rejects_an_unsupported_machine_id() {
        // 9 = SZX_MACHINE_TC2068, a real id this crate doesn't model.
        assert_eq!(parse(&minimal_header(9)), Err(Error::UnsupportedMachine(9)));
    }

    #[test]
    fn parse_a_bare_header_with_no_chunks_is_a_default_48k_snapshot() {
        let snapshot = parse(&minimal_header(1)).unwrap();
        assert_eq!(snapshot.paging, None);
        assert_eq!(snapshot.paging_2a3, None);
        assert!(snapshot.memory.is_empty());
    }

    #[test]
    fn parse_skips_an_unrecognized_chunk_by_its_own_length() {
        let mut bytes = minimal_header(1);
        bytes.extend_from_slice(b"CRTR");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAA; 4]);
        let snapshot = parse(&bytes).unwrap();
        assert!(snapshot.memory.is_empty());
    }

    #[test]
    fn parse_rejects_a_chunk_whose_length_overruns_the_file() {
        let mut bytes = minimal_header(1);
        bytes.extend_from_slice(b"Z80R");
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        assert_eq!(parse(&bytes), Err(Error::TruncatedChunk));
    }

    fn z80r_bytes(pc: u16, halted: bool) -> Vec<u8> {
        let mut data = vec![0u8; 37];
        data[22..24].copy_from_slice(&pc.to_le_bytes());
        data[34] = if halted { ZXSTZF_HALTED } else { 0 };
        data
    }

    #[test]
    fn z80r_chunk_restores_registers_and_halted_flag() {
        let mut bytes = minimal_header(1);
        bytes.extend_from_slice(b"Z80R");
        let data = z80r_bytes(0x8000, true);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        assert_eq!(snapshot.cpu.pc, 0x8000);
        assert!(snapshot.cpu.halted);
    }

    #[test]
    fn z80r_chunk_rejects_the_wrong_length() {
        let mut bytes = minimal_header(1);
        bytes.extend_from_slice(b"Z80R");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 10]);
        assert_eq!(parse(&bytes), Err(Error::WrongZ80RegsLength(10)));
    }

    #[test]
    fn spcr_chunk_decodes_128k_paging_and_border() {
        let mut bytes = minimal_header(2); // 128K
        bytes.extend_from_slice(b"SPCR");
        let data = [
            0b0000_0101, // border = 5
            0b0011_1010, // port_7ffd: ram_bank=2, shadow=1, rom_bank=1, locked=1
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        assert_eq!(snapshot.border_color, 5);
        assert_eq!(
            snapshot.paging,
            Some(PagingState {
                rom_bank: 1,
                ram_bank: 2,
                shadow_screen: true,
                locked: true,
            })
        );
    }

    #[test]
    fn spcr_chunk_decodes_2a3_paging_from_both_ports() {
        let mut bytes = minimal_header(5); // +3
        bytes.extend_from_slice(b"SPCR");
        let data = [
            0,
            0b0000_0011, // port_7ffd: ram_bank=3
            0b0000_1101, // port_1ffd: special_mode=1, bits_1_2=(0b10), disk_motor=1
            0,
            0,
            0,
            0,
            0,
        ];
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        assert_eq!(
            snapshot.paging_2a3,
            Some(Paging2A3State {
                ram_bank: 3,
                shadow_screen: false,
                rom_bit_lo: false,
                locked: false,
                special_mode: true,
                bits_1_2: 0b10,
                disk_motor: true,
            })
        );
    }

    #[test]
    fn ramp_chunk_uncompressed_48k_page_maps_to_its_real_address() {
        let mut bytes = minimal_header(1); // 48K
        bytes.extend_from_slice(b"RAMP");
        let mut data = vec![0u8, 0u8]; // flags: uncompressed
        data.push(5); // page 5 -> 0x4000
        data.extend_from_slice(&[0x77; 0x4000]);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        assert_eq!(snapshot.memory.len(), 1);
        match &snapshot.memory[0] {
            MemoryBlock::Absolute { address, data } => {
                assert_eq!(*address, 0x4000);
                assert_eq!(data.len(), 0x4000);
                assert!(data.iter().all(|&b| b == 0x77));
            }
            other => panic!("expected Absolute, got {other:?}"),
        }
    }

    #[test]
    fn ramp_chunk_128k_page_is_a_direct_bank_index() {
        let mut bytes = minimal_header(2); // 128K
        bytes.extend_from_slice(b"RAMP");
        let mut data = vec![0u8, 0u8];
        data.push(6); // bank 6, no offset
        data.extend_from_slice(&[0x22; 0x4000]);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        match &snapshot.memory[0] {
            MemoryBlock::RamBank { bank, data } => {
                assert_eq!(*bank, 6);
                assert!(data.iter().all(|&b| b == 0x22));
            }
            other => panic!("expected RamBank, got {other:?}"),
        }
    }

    #[test]
    fn ramp_chunk_decompresses_a_real_zlib_stream() {
        use std::io::Write;
        let raw = vec![0x99u8; 0x4000];
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut bytes = minimal_header(1);
        bytes.extend_from_slice(b"RAMP");
        let mut data = vec![1u8, 0u8]; // flags: ZXSTRF_COMPRESSED
        data.push(0); // page 0 -> 0xC000
        data.extend_from_slice(&compressed);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        match &snapshot.memory[0] {
            MemoryBlock::Absolute { address, data } => {
                assert_eq!(*address, 0xC000);
                assert_eq!(data.len(), 0x4000);
                assert!(data.iter().all(|&b| b == 0x99));
            }
            other => panic!("expected Absolute, got {other:?}"),
        }
    }

    #[test]
    fn ay_chunk_restores_registers_and_current_selection() {
        let mut bytes = minimal_header(2);
        bytes.extend_from_slice(b"AY\0\0");
        let mut data = vec![0u8]; // flags
        data.push(6); // current_register
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let snapshot = parse(&bytes).unwrap();
        let ay = snapshot.ay.unwrap();
        assert_eq!(ay.current_register, 6);
        assert_eq!(
            ay.registers,
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn write_then_parse_round_trips_a_48k_snapshot() {
        let mut cpu = Cpu::new();
        cpu.pc = 0x1234;
        cpu.sp = 0x5678;
        cpu.af = 0xABCD;
        cpu.halted = true;

        let snapshot = Snapshot {
            cpu,
            border_color: 3,
            memory: vec![
                MemoryBlock::Absolute {
                    address: 0x4000,
                    data: vec![0x11; 0x4000],
                },
                MemoryBlock::Absolute {
                    address: 0x8000,
                    data: vec![0x22; 0x4000],
                },
                MemoryBlock::Absolute {
                    address: 0xC000,
                    data: vec![0x33; 0x4000],
                },
            ],
            paging: None,
            paging_2a3: None,
            ay: None,
        };

        let bytes = write(&snapshot);
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped.cpu.pc, 0x1234);
        assert_eq!(round_tripped.cpu.sp, 0x5678);
        assert_eq!(round_tripped.cpu.af, 0xABCD);
        assert!(round_tripped.cpu.halted);
        assert_eq!(round_tripped.border_color, 3);
        assert_eq!(round_tripped.memory, snapshot.memory);
        assert_eq!(round_tripped.paging, None);
        assert_eq!(round_tripped.paging_2a3, None);
    }

    #[test]
    fn write_then_parse_round_trips_a_128k_snapshot_with_paging_and_ay() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 1,
            memory: (0u8..8)
                .map(|bank| MemoryBlock::RamBank {
                    bank,
                    data: vec![bank; 0x4000],
                })
                .collect(),
            paging: Some(PagingState {
                rom_bank: 1,
                ram_bank: 4,
                shadow_screen: true,
                locked: false,
            }),
            paging_2a3: None,
            ay: Some(AyState {
                current_register: 8,
                registers: [9; 16],
            }),
        };

        let bytes = write(&snapshot);
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped.paging, snapshot.paging);
        assert_eq!(round_tripped.memory, snapshot.memory);
        assert_eq!(round_tripped.ay, snapshot.ay);
    }

    #[test]
    fn write_then_parse_round_trips_a_2a3_snapshot() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 0,
            memory: (0u8..8)
                .map(|bank| MemoryBlock::RamBank {
                    bank,
                    data: vec![0; 0x4000],
                })
                .collect(),
            paging: None,
            paging_2a3: Some(Paging2A3State {
                ram_bank: 2,
                shadow_screen: false,
                rom_bit_lo: true,
                locked: false,
                special_mode: true,
                bits_1_2: 0b11,
                disk_motor: false,
            }),
            ay: None,
        };

        let bytes = write(&snapshot);
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped.paging_2a3, snapshot.paging_2a3);
    }
}
