//! Parser for the ZX Spectrum `.z80` snapshot file format (versions 1-3).
//!
//! Format reference: the World of Spectrum FAQ's "Z80 file format"
//! specification, <https://worldofspectrum.org/faq/reference/z80format.htm>
//! -- every byte offset, the compression scheme, the version-detection rule,
//! the hardware-mode table (including the fact that its numbering *differs*
//! between v2 and v3 -- mode 3 is "128k" in v2 but "48k + M.G.T." in v3, a
//! real trap this parser must get right per-version, not guess) and the
//! page-number-to-address/bank table below were taken from that document,
//! not guessed or reconstructed from memory. See `CREDITS.md` at the repo
//! root.
//!
//! Deliberately load-only for now (no `write`/save side yet -- planned as a
//! follow-up; the `Snapshot` type below is shaped so a future `save` can
//! reuse it symmetrically rather than needing its own representation).
//!
//! Deliberately format-only: this crate has no concept of a "machine" and
//! doesn't touch memory-mapped I/O, a `Bus`, or anything from the `machine`
//! crate. It hands back a plain `z80::Cpu` (a real register set, not a
//! Spectrum-specific one -- any future Z80-based machine could reuse this
//! crate for its own `.z80`-compatible snapshots) plus raw memory blocks;
//! writing those into a running machine's bus/border-color/paging state is
//! that machine crate's own job (see `zx_spectrum::ZxSpectrum::
//! load_snapshot`).

use std::fmt;

use z80::Cpu;

/// One decompressed memory block, in whichever shape its source hardware
/// mode implies -- see each variant's own doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryBlock {
    /// 48K memory: lands at a fixed absolute address, always valid
    /// regardless of any paging state (there is none on real 48K hardware).
    Absolute { address: u16, data: Vec<u8> },
    /// 128K memory: one of the 8 real RAM banks (0-7). Deliberately NOT
    /// resolved to a CPU address here -- unlike 48K, which bank appears at
    /// `0xC000` on real 128K hardware depends on `Snapshot::paging`'s
    /// `ram_bank` (banks 5 and 2 are the only ones with a fixed address,
    /// `0x4000` and `0x8000`, on both models). Applying this is
    /// `zx_spectrum`'s job, since only it knows its own memory layout.
    RamBank { bank: u8, data: Vec<u8> },
}

/// Real ZX Spectrum 128K's paging state (the last value `OUT` to port
/// `0x7ffd`), preserved verbatim by the v2/v3 `.z80` header for 128K-family
/// hardware modes. `None` on `Snapshot` for anything 48K-memory-shaped
/// (including "48K + Interface 1"/"48K + M.G.T.", which use no paging at
/// all on real hardware) and for +2A/+3 (see `Paging2A3State` instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagingState {
    /// Which of the 2 ROM banks is paged into `0x0000`-`0x3fff`.
    pub rom_bank: u8,
    /// Which of the 8 RAM banks is paged into `0xC000`-`0xffff`.
    pub ram_bank: u8,
    /// `true` selects RAM bank 7 for the ULA's own video read instead of
    /// the normal bank 5 -- independent of whatever's paged at `0xC000`.
    pub shadow_screen: bool,
    /// Once set, real hardware ignores all further writes to `0x7ffd`
    /// until reset. Preserved here so a resumed snapshot doesn't
    /// accidentally allow paging a real machine would have locked.
    pub locked: bool,
}

/// Real ZX Spectrum +2A/+3 paging state: both `0x7ffd` and `0x1ffd`
/// combined, same field meanings as `zx_spectrum::bus::Paging2A3` (this
/// crate deliberately has no dependency on `zx_spectrum`, see the
/// crate-level doc comment, so it can't reuse that type directly -- but
/// both were verified against the same official ZX Spectrum +3 technical
/// manual, see CREDITS.md). `Some` on `Snapshot` only for a +2A/+3-family
/// hardware mode (byte 34 = 7, 8, or 13 -- see `classify_hardware_mode`);
/// `None` for anything else, including plain 128K (see `PagingState`
/// instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paging2A3State {
    /// `0x7ffd` bits 0-2: RAM bank at `0xC000`, only meaningful when
    /// `!special_mode`.
    pub ram_bank: u8,
    /// `0x7ffd` bit 3.
    pub shadow_screen: bool,
    /// `0x7ffd` bit 4: ROM select's low bit.
    pub rom_bit_lo: bool,
    /// `0x7ffd` bit 5: paging lock, same meaning as `PagingState::locked`.
    pub locked: bool,
    /// `0x1ffd` bit 0: `false` = normal mode, `true` = special all-RAM
    /// mode.
    pub special_mode: bool,
    /// `0x1ffd` bits 2 and 1 combined as `(bit2 << 1) | bit1`, same
    /// encoding as `zx_spectrum::bus::Paging2A3::bits_1_2`.
    pub bits_1_2: u8,
    /// `0x1ffd` bit 3: floppy disk motor.
    pub disk_motor: bool,
}

/// A fully parsed `.z80` snapshot: CPU register state plus the memory
/// contents it expects loaded before execution resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cpu: Cpu,
    /// Border color at save time: bits 1-3 of header byte 12, same 3-bit
    /// range (0-7) as the ULA's own border output.
    pub border_color: u8,
    /// One block per 16K page (v2/v3) or one single block covering however
    /// much of the 48K image was present (v1).
    pub memory: Vec<MemoryBlock>,
    /// `Some` for a 128K-memory-model snapshot -- see `PagingState`. `None`
    /// for +2A/+3 too (see `paging_2a3` instead) -- a separate field rather
    /// than folding both shapes into one enum, matching `zx_spectrum::bus`'s
    /// own choice to keep `Banked128`/`Banked2A3` distinct rather than
    /// generalizing one into the other.
    pub paging: Option<PagingState>,
    /// `Some` for a +2A/+3-memory-model snapshot -- see `Paging2A3State`.
    pub paging_2a3: Option<Paging2A3State>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than the fixed 30-byte v1 header requires.
    TooShort,
    /// The additional-header length field (byte 30-31, v2/v3 only) wasn't
    /// one of the three values the real format ever writes there (23 for
    /// v2, 54 or 55 for v3).
    UnknownAdditionalHeaderLength(u16),
    /// Hardware mode (byte 34, v2/v3 only) this crate doesn't model a
    /// memory layout for -- SamRam, Pentagon, Scorpion, Didaktik,
    /// TC2048/2068, TS2068. Real, different hardware, not a "guess wrong"
    /// bug -- see `classify_hardware_mode`.
    UnsupportedHardwareMode(u8),
    /// Hardware mode said +2A/+3 (byte 34 = 7, 8, or 13), but the additional
    /// header length (byte 30-31) wasn't 55 -- the only length that includes
    /// byte 86, the last-OUT-to-`0x1ffd` byte this model needs. A 23 or
    /// 54-byte header genuinely cannot carry that state, so this isn't
    /// recoverable by guessing a default.
    Missing2A3PagingByte,
    /// A v2/v3 memory block's header claimed more compressed bytes than
    /// remain in the file.
    TruncatedMemoryBlock,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => write!(f, "file is shorter than a .z80 v1 header (30 bytes)"),
            Error::UnknownAdditionalHeaderLength(len) => write!(
                f,
                "unrecognized v2/v3 additional header length {len} (expected 23, 54, or 55)"
            ),
            Error::UnsupportedHardwareMode(mode) => write!(
                f,
                "unsupported hardware mode {mode} (48K, 48K+Interface1, 128K, 128K+Interface1, Spectrum +2, and Spectrum +2A/+3 are supported; SamRam and clone hardware are not)"
            ),
            Error::Missing2A3PagingByte => write!(
                f,
                "+2A/+3 hardware mode requires a 55-byte v3 additional header (to carry the last OUT to 0x1ffd), but this file's header is shorter"
            ),
            Error::TruncatedMemoryBlock => write!(f, "memory block extends past end of file"),
        }
    }
}

impl std::error::Error for Error {}

const V1_HEADER_LEN: usize = 30;

/// Real ZX81/Spectrum-format quirk, straight from the spec: "Because of
/// compatibility, if byte 12 is 255, it has to be regarded as being 1" --
/// some pre-compression-era tools always wrote 255 here by mistake, before
/// bit 5 (the compression flag) was even defined, and such files are not
/// actually compressed.
const BYTE_12_LEGACY_VALUE: u8 = 255;
const BYTE_12_LEGACY_REPLACEMENT: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    Spectrum48,
    Spectrum128,
    Spectrum2A3,
}

/// Maps a v2/v3 hardware-mode byte to the memory model it implies --
/// per the spec's own table, this numbering is NOT the same between v2 and
/// v3 (e.g. mode 3 is "128k" in v2 but "48k + M.G.T." in v3), so `is_v3`
/// must come from the real version-detection rule (additional header
/// length), not be assumed. Modes 7/8/13 (+2A/+3, and a mistaken +3 encoding
/// some XZX-Pro versions wrote) only exist in the spec's *extended* v3
/// table -- real hardware/tools never write them for a v2 file.
fn classify_hardware_mode(is_v3: bool, mode: u8) -> Result<Model, Error> {
    use Model::*;
    let model = if is_v3 {
        match mode {
            0 | 1 => Some(Spectrum48),
            // 3 = "48k + M.G.T." -- a 48K machine with a disk interface
            // attached; memory-shaped identically to plain 48K, the disk
            // drive itself just isn't emulated.
            3 => Some(Spectrum48),
            // 4 = "128k", 5 = "128k + If.1", 6 = "128k + M.G.T.".
            4..=6 => Some(Spectrum128),
            // 7 = "Spectrum +3", 8 = a known mistake some XZX-Pro versions
            // made when they meant +3 -- both memory-identical to +2A/+3
            // (4 ROM banks, 0x7ffd + 0x1ffd), so both map here.
            7 | 8 => Some(Spectrum2A3),
            // 12 = "Spectrum +2": hardware-identical memory model to plain
            // 128K (same 8x16K RAM banks, same 0x7ffd paging, same 2 ROMs)
            // -- the +2 designation is about the case/keyboard/built-in
            // tape deck, none of which this project models anyway.
            12 => Some(Spectrum128),
            // 13 = "Spectrum +2A": memory-identical to +3.
            13 => Some(Spectrum2A3),
            _ => None,
        }
    } else {
        match mode {
            0 | 1 => Some(Spectrum48),
            // 3 = "128k", 4 = "128k + If.1" (v2's numbering, shifted by one
            // versus v3's).
            3 | 4 => Some(Spectrum128),
            12 => Some(Spectrum128),
            _ => None,
        }
    };
    model.ok_or(Error::UnsupportedHardwareMode(mode))
}

/// Parses a `.z80` snapshot from raw file bytes.
pub fn parse(bytes: &[u8]) -> Result<Snapshot, Error> {
    if bytes.len() < V1_HEADER_LEN {
        return Err(Error::TooShort);
    }

    let byte12 = match bytes[12] {
        BYTE_12_LEGACY_VALUE => BYTE_12_LEGACY_REPLACEMENT,
        b => b,
    };
    let border_color = (byte12 >> 1) & 0b111;
    let compressed_v1 = byte12 & 0b0010_0000 != 0;
    let r7 = (byte12 & 1) != 0;

    let mut cpu = Cpu::default();
    cpu.af = ((bytes[0] as u16) << 8) | bytes[1] as u16; // byte 0 = A (high), byte 1 = F (low)
    cpu.bc = read_u16(bytes, 2);
    cpu.hl = read_u16(bytes, 4);
    let header_pc = read_u16(bytes, 6);
    cpu.sp = read_u16(bytes, 8);
    cpu.i = bytes[10];
    cpu.r = (bytes[11] & 0x7F) | if r7 { 0x80 } else { 0 };
    cpu.de = read_u16(bytes, 13);
    cpu.bc_ = read_u16(bytes, 15);
    cpu.de_ = read_u16(bytes, 17);
    cpu.hl_ = read_u16(bytes, 19);
    cpu.af_ = ((bytes[21] as u16) << 8) | bytes[22] as u16;
    cpu.iy = read_u16(bytes, 23);
    cpu.ix = read_u16(bytes, 25);
    cpu.iff1 = bytes[27] != 0;
    cpu.iff2 = bytes[28] != 0;
    cpu.im = bytes[29] & 0b11;

    // PC==0 in the v1 field signals a v2/v3 file, whose real PC lives in
    // the additional header instead -- the version-detection rule the spec
    // itself uses.
    let (memory, paging, paging_2a3) = if header_pc != 0 {
        cpu.pc = header_pc;
        (
            parse_v1_memory(&bytes[V1_HEADER_LEN..], compressed_v1),
            None,
            None,
        )
    } else {
        let additional_len = read_u16(bytes, 30);
        if !matches!(additional_len, 23 | 54 | 55) {
            return Err(Error::UnknownAdditionalHeaderLength(additional_len));
        }
        cpu.pc = read_u16(bytes, 32);

        let is_v3 = additional_len != 23;
        let hardware_mode = bytes[34];
        let model = classify_hardware_mode(is_v3, hardware_mode)?;

        let memory_start = 32 + additional_len as usize;
        match model {
            Model::Spectrum48 => (parse_paged_memory_48(&bytes[memory_start..])?, None, None),
            Model::Spectrum128 => {
                // Byte 35 ("Port state (0x7ffd/0xf4/74ls259)") mirrors the
                // real port's bit layout exactly: bits 0-2 RAM bank, bit 3
                // shadow screen, bit 4 ROM bank, bit 5 paging-locked.
                let port_state = bytes[35];
                let paging = PagingState {
                    rom_bank: (port_state >> 4) & 1,
                    ram_bank: port_state & 0b111,
                    shadow_screen: port_state & 0b0000_1000 != 0,
                    locked: port_state & 0b0010_0000 != 0,
                };
                (
                    parse_paged_memory_128(&bytes[memory_start..])?,
                    Some(paging),
                    None,
                )
            }
            Model::Spectrum2A3 => {
                // Byte 86 ("Last OUT to port 0x1ffd") only exists when the
                // additional header is the full 55 bytes -- a 23 or 54-byte
                // header cannot carry this model's paging state at all.
                if additional_len != 55 {
                    return Err(Error::Missing2A3PagingByte);
                }
                // Byte 35 behaves as on 128K, except bit 4 is only the ROM
                // select's *low* bit (the high bit lives in byte 86 below).
                let port_7ffd = bytes[35];
                let port_1ffd = bytes[86];
                let paging = Paging2A3State {
                    ram_bank: port_7ffd & 0b111,
                    shadow_screen: port_7ffd & 0b0000_1000 != 0,
                    rom_bit_lo: port_7ffd & 0b0001_0000 != 0,
                    locked: port_7ffd & 0b0010_0000 != 0,
                    special_mode: port_1ffd & 0b0000_0001 != 0,
                    bits_1_2: (port_1ffd >> 1) & 0b11,
                    disk_motor: port_1ffd & 0b0000_1000 != 0,
                };
                (
                    parse_paged_memory_128(&bytes[memory_start..])?,
                    None,
                    Some(paging),
                )
            }
        }
    };

    Ok(Snapshot {
        cpu,
        border_color,
        memory,
        paging,
        paging_2a3,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

/// 48K RAM only -- the page numbers real `.z80` files use in "48 mode" for
/// anything else (128K banks, Multiface ROM, Interface 1 ROM, ...) aren't
/// addresses on 48K hardware at all, so they're skipped rather than
/// erroring the whole load over one extraneous block.
fn page_address(page: u8) -> Option<u16> {
    match page {
        4 => Some(0x8000),
        5 => Some(0xC000),
        8 => Some(0x4000),
        _ => None,
    }
}

/// 128K RAM banks: the spec's "In '128 mode" column maps page numbers
/// 3-10 to RAM banks 0-7 (`page - 3`); pages 0-2 and 11 are ROM images
/// (the 128K editor ROM, an Interface 1/Disciple/Plus D ROM, the reset
/// ROM, the Multiface ROM) -- this project always uses the ROM supplied
/// via `--rom`, never one embedded in a snapshot, so those are skipped the
/// same way 48K mode skips page 11.
fn page_bank(page: u8) -> Option<u8> {
    (3..=10).contains(&page).then(|| page - 3)
}

/// v1: everything after the 30-byte header is one stream, either a raw
/// 48K (49152-byte) dump or, if the compression flag was set, an
/// `ED ED xx yy`-compressed stream terminated by `00 ED ED 00`. v1 predates
/// the 128K hardware-mode byte entirely -- there is no v1 128K format.
fn parse_v1_memory(rest: &[u8], compressed: bool) -> Vec<MemoryBlock> {
    const V1_SIZE: usize = 0xC000; // 48K: 0x4000-0xFFFF
    const END_MARKER: [u8; 4] = [0x00, 0xED, 0xED, 0x00];

    let data = if compressed {
        decompress(rest, Some(&END_MARKER), V1_SIZE)
    } else {
        let mut data = rest.to_vec();
        data.truncate(V1_SIZE);
        data
    };

    vec![MemoryBlock::Absolute {
        address: 0x4000,
        data,
    }]
}

/// v2/v3: a sequence of `[length: u16 LE][page: u8][data...]` blocks with no
/// separator between them; `length == 0xFFFF` means the following 16384
/// bytes are stored uncompressed rather than being an actual byte count.
/// Shared between the 48K and 128K paths -- only what a page number *means*
/// differs (see `page_address`/`page_bank`), not how blocks are framed.
fn decode_page_blocks(mut rest: &[u8]) -> Result<Vec<(u8, Vec<u8>)>, Error> {
    const PAGE_SIZE: usize = 0x4000; // 16K

    let mut pages = Vec::new();
    while rest.len() >= 3 {
        let length = read_u16(rest, 0) as usize;
        let page = rest[2];
        rest = &rest[3..];

        let data = if length == 0xFFFF {
            if rest.len() < PAGE_SIZE {
                return Err(Error::TruncatedMemoryBlock);
            }
            let data = rest[..PAGE_SIZE].to_vec();
            rest = &rest[PAGE_SIZE..];
            data
        } else {
            if rest.len() < length {
                return Err(Error::TruncatedMemoryBlock);
            }
            let data = decompress(&rest[..length], None, PAGE_SIZE);
            rest = &rest[length..];
            data
        };
        pages.push((page, data));
    }
    Ok(pages)
}

fn parse_paged_memory_48(rest: &[u8]) -> Result<Vec<MemoryBlock>, Error> {
    Ok(decode_page_blocks(rest)?
        .into_iter()
        .filter_map(|(page, data)| {
            page_address(page).map(|address| MemoryBlock::Absolute { address, data })
        })
        .collect())
}

fn parse_paged_memory_128(rest: &[u8]) -> Result<Vec<MemoryBlock>, Error> {
    Ok(decode_page_blocks(rest)?
        .into_iter()
        .filter_map(|(page, data)| page_bank(page).map(|bank| MemoryBlock::RamBank { bank, data }))
        .collect())
}

/// Shared `ED ED xx yy` decompressor: a run of `xx` copies of byte `yy`.
/// Stops as soon as `max_output` bytes have been produced, or (if
/// `end_marker` is given, v1's unbounded stream only) that exact byte
/// sequence is encountered. A lone `0xED` not part of an `ED ED` pair is
/// copied through literally -- the spec's own compressor guarantees that
/// case never collides with a real compressed run.
fn decompress(input: &[u8], end_marker: Option<&[u8]>, max_output: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(max_output);
    let mut i = 0;
    while i < input.len() && out.len() < max_output {
        if let Some(marker) = end_marker
            && input[i..].starts_with(marker)
        {
            break;
        }
        if input[i] == 0xED && i + 1 < input.len() && input[i + 1] == 0xED {
            if i + 3 >= input.len() {
                break;
            }
            let count = input[i + 2];
            let value = input[i + 3];
            for _ in 0..count {
                if out.len() >= max_output {
                    break;
                }
                out.push(value);
            }
            i += 4;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// 48K: expected either one `MemoryBlock::Absolute { address: 0x4000,
    /// .. }` covering the full 49152-byte range, or the 3 separately-
    /// addressed 16K blocks (`0x4000`/`0x8000`/`0xC000`) v2/v3 files
    /// themselves use.
    Missing48KRam,
    /// 128K/+2A/+3: one of the 8 real RAM banks (0-7) wasn't present in
    /// `Snapshot::memory`, or wasn't exactly 16K.
    MissingRamBank(u8),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Missing48KRam => write!(
                f,
                "snapshot has no complete 48K memory image (neither one 0x4000 Absolute block nor the 3 addressed 16K blocks)"
            ),
            WriteError::MissingRamBank(bank) => {
                write!(f, "snapshot is missing RAM bank {bank} (or it isn't 16K)")
            }
        }
    }
}

impl std::error::Error for WriteError {}

/// Inverse of `page_address` -- which v2/v3 "48 mode" page number a 48K
/// address's own 16K window maps to.
fn address_page(address: u16) -> Option<u8> {
    match address {
        0x8000 => Some(4),
        0xC000 => Some(5),
        0x4000 => Some(8),
        _ => None,
    }
}

/// Always writes exactly 16384 bytes uncompressed (the `length == 0xFFFF`
/// sentinel `decode_page_blocks` already reads) -- `write` never emits
/// compressed pages at all. This is a real, spec-legal `.z80` file (the
/// sentinel exists specifically to allow uncompressed storage; compression
/// is optional per the format), just a larger one -- implementing the RLE
/// *compressor* isn't needed for this crate to gain a real, correct save
/// side, only the (already-implemented) decompressor is required to load
/// back what this writes, or any real file compression ever produced.
fn push_page_block(out: &mut Vec<u8>, page: u8, data: &[u8]) {
    debug_assert_eq!(data.len(), 0x4000);
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.push(page);
    out.extend_from_slice(data);
}

/// Resolves `snapshot.memory` into the 3 addressed 16K chunks (`0x4000`,
/// `0x8000`, `0xC000`) real 48K RAM needs, accepting either shape a
/// `Snapshot` might carry it in: one merged 49152-byte `Absolute` block
/// (this crate's own v1-parsed shape) or the 3 separately-addressed 16K
/// blocks (v2/v3's own shape, and what `write` itself always produces) --
/// so `write` round-trips a `parse`d snapshot regardless of which version
/// produced it.
fn resolve_48k_ram(memory: &[MemoryBlock]) -> Result<[&[u8]; 3], WriteError> {
    if let Some(MemoryBlock::Absolute { data, .. }) = memory.iter().find(
        |b| matches!(b, MemoryBlock::Absolute { address: 0x4000, data } if data.len() == 0xC000),
    ) {
        return Ok([
            &data[0x0000..0x4000],
            &data[0x4000..0x8000],
            &data[0x8000..0xC000],
        ]);
    }

    let find = |addr: u16| {
        memory.iter().find_map(|b| match b {
            MemoryBlock::Absolute { address, data } if *address == addr && data.len() == 0x4000 => {
                Some(data.as_slice())
            }
            _ => None,
        })
    };
    Ok([
        find(0x4000).ok_or(WriteError::Missing48KRam)?,
        find(0x8000).ok_or(WriteError::Missing48KRam)?,
        find(0xC000).ok_or(WriteError::Missing48KRam)?,
    ])
}

fn ram_bank(memory: &[MemoryBlock], bank: u8) -> Result<&[u8], WriteError> {
    memory
        .iter()
        .find_map(|b| match b {
            MemoryBlock::RamBank { bank: b, data } if *b == bank && data.len() == 0x4000 => {
                Some(data.as_slice())
            }
            _ => None,
        })
        .ok_or(WriteError::MissingRamBank(bank))
}

fn write_header_and_registers(snapshot: &Snapshot, hardware_mode: u8, port_7ffd: u8) -> Vec<u8> {
    let cpu = &snapshot.cpu;
    let mut out = Vec::with_capacity(87);

    out.push((cpu.af >> 8) as u8); // A
    out.push(cpu.af as u8); // F
    out.extend_from_slice(&cpu.bc.to_le_bytes());
    out.extend_from_slice(&cpu.hl.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // PC: 0 marks a v2/v3 file
    out.extend_from_slice(&cpu.sp.to_le_bytes());
    out.push(cpu.i);
    out.push(cpu.r & 0x7F);
    let r7 = (cpu.r >> 7) & 1;
    out.push(r7 | ((snapshot.border_color & 0b111) << 1)); // byte 12: compression bit (5) always 0
    out.extend_from_slice(&cpu.de.to_le_bytes());
    out.extend_from_slice(&cpu.bc_.to_le_bytes());
    out.extend_from_slice(&cpu.de_.to_le_bytes());
    out.extend_from_slice(&cpu.hl_.to_le_bytes());
    out.push((cpu.af_ >> 8) as u8); // A'
    out.push(cpu.af_ as u8); // F'
    out.extend_from_slice(&cpu.iy.to_le_bytes());
    out.extend_from_slice(&cpu.ix.to_le_bytes());
    out.push(cpu.iff1 as u8);
    out.push(cpu.iff2 as u8);
    out.push(cpu.im & 0b11);

    out.extend_from_slice(&55u16.to_le_bytes()); // additional header length -- always the full v3 shape
    out.extend_from_slice(&cpu.pc.to_le_bytes()); // real PC
    out.push(hardware_mode);
    out.push(port_7ffd);
    out.resize(86, 0); // bytes 36-85: unmodeled v3 fields (AY state, MGT/Disciple ROM paging, T-state counters, ...), zero-filled
    out.push(0); // byte 86: last OUT to 0x1ffd -- overwritten by write_2a3 when it applies

    out
}

/// Serializes a `Snapshot` back to `.z80` bytes, always as a v3 file (55-
/// byte additional header, uncompressed pages -- see `push_page_block`'s
/// doc comment) regardless of which real hardware mode it represents.
/// Symmetric with `parse`: a round trip through `write` then `parse`
/// reproduces the same `Snapshot`, whichever of the 3 models it's for.
/// Deliberately never writes a ROM page (0/1/2/11) -- this project always
/// uses the ROM supplied via `--rom`, never one embedded in a snapshot,
/// same reasoning as `page_bank`'s own doc comment; a real external reader
/// loading this file simply uses its own ROM instead, the normal case for
/// any same-model `.z80` file anyway.
pub fn write(snapshot: &Snapshot) -> Result<Vec<u8>, WriteError> {
    match (&snapshot.paging, &snapshot.paging_2a3) {
        (None, None) => write_48k(snapshot),
        (Some(paging), None) => write_128k(snapshot, *paging),
        (None, Some(paging_2a3)) => write_2a3(snapshot, *paging_2a3),
        (Some(_), Some(_)) => unreachable!(
            "Snapshot never has both paging and paging_2a3 set -- see their own doc comments"
        ),
    }
}

fn write_48k(snapshot: &Snapshot) -> Result<Vec<u8>, WriteError> {
    let ram = resolve_48k_ram(&snapshot.memory)?;
    let mut out = write_header_and_registers(snapshot, 0, 0);
    for (address, data) in [0x4000u16, 0x8000, 0xC000].into_iter().zip(ram) {
        push_page_block(&mut out, address_page(address).unwrap(), data);
    }
    Ok(out)
}

fn write_128k(snapshot: &Snapshot, paging: PagingState) -> Result<Vec<u8>, WriteError> {
    let port_7ffd = (paging.ram_bank & 0b111)
        | if paging.shadow_screen { 0b0000_1000 } else { 0 }
        | ((paging.rom_bank & 1) << 4)
        | if paging.locked { 0b0010_0000 } else { 0 };
    let mut out = write_header_and_registers(snapshot, 4, port_7ffd);
    for bank in 0u8..8 {
        push_page_block(&mut out, bank + 3, ram_bank(&snapshot.memory, bank)?);
    }
    Ok(out)
}

fn write_2a3(snapshot: &Snapshot, paging: Paging2A3State) -> Result<Vec<u8>, WriteError> {
    let port_7ffd = (paging.ram_bank & 0b111)
        | if paging.shadow_screen { 0b0000_1000 } else { 0 }
        | if paging.rom_bit_lo { 0b0001_0000 } else { 0 }
        | if paging.locked { 0b0010_0000 } else { 0 };
    let port_1ffd = (paging.special_mode as u8)
        | ((paging.bits_1_2 & 0b11) << 1)
        | if paging.disk_motor { 0b0000_1000 } else { 0 };

    let mut out = write_header_and_registers(snapshot, 7, port_7ffd);
    out[86] = port_1ffd;
    for bank in 0u8..8 {
        push_page_block(&mut out, bank + 3, ram_bank(&snapshot.memory, bank)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1_header(pc: u16, byte12: u8) -> Vec<u8> {
        let mut h = vec![0u8; V1_HEADER_LEN];
        h[0] = 0x11; // A
        h[1] = 0x22; // F
        h[6..8].copy_from_slice(&pc.to_le_bytes());
        h[12] = byte12;
        h
    }

    /// Builds a v2/v3 additional header of the given total length (23 for
    /// v2, 54/55 for v3), with real PC / hardware mode / port state at
    /// their correct offsets and everything else zeroed.
    fn v2_v3_header(
        pc: u16,
        byte12: u8,
        additional_len: u16,
        hardware_mode: u8,
        port_state: u8,
    ) -> Vec<u8> {
        let mut bytes = v1_header(0, byte12);
        bytes.extend(additional_len.to_le_bytes());
        bytes.extend(pc.to_le_bytes());
        bytes.push(hardware_mode);
        bytes.push(port_state);
        bytes.extend(vec![0u8; additional_len as usize - 4]); // pc(2)+mode(1)+port(1) already written
        bytes
    }

    #[test]
    fn parses_v1_registers() {
        let mut bytes = v1_header(0x8000, 0);
        bytes.extend(vec![0u8; 0xC000]); // uncompressed 48K
        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.cpu.af, 0x1122);
        assert_eq!(snap.cpu.pc, 0x8000);
        assert_eq!(snap.paging, None);
    }

    #[test]
    fn v1_uncompressed_memory_lands_at_0x4000() {
        let mut bytes = v1_header(0x8000, 0);
        let mut mem = vec![0xAAu8; 0xC000];
        mem[0] = 0x99;
        bytes.extend(mem);

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.memory.len(), 1);
        assert_eq!(
            snap.memory[0],
            MemoryBlock::Absolute {
                address: 0x4000,
                data: {
                    let mut d = vec![0xAAu8; 0xC000];
                    d[0] = 0x99;
                    d
                }
            }
        );
    }

    #[test]
    fn v1_compressed_run_expands_and_stops_at_end_marker() {
        let mut bytes = v1_header(0x8000, 0b0010_0000); // compression bit set
        // A run of 10 copies of 0x42, then the end marker.
        bytes.extend([0xED, 0xED, 10, 0x42]);
        bytes.extend([0x00, 0xED, 0xED, 0x00]);

        let snap = parse(&bytes).unwrap();

        match &snap.memory[0] {
            MemoryBlock::Absolute { data, .. } => assert_eq!(data, &vec![0x42u8; 10]),
            other => panic!("expected Absolute, got {other:?}"),
        }
    }

    #[test]
    fn byte_12_value_255_is_treated_as_1_not_compressed() {
        let mut bytes = v1_header(0x8000, 255);
        bytes.extend(vec![0x07u8; 0xC000]); // must be read literally, not decompressed
        let snap = parse(&bytes).unwrap();

        match &snap.memory[0] {
            MemoryBlock::Absolute { data, .. } => assert_eq!(data[0], 0x07),
            other => panic!("expected Absolute, got {other:?}"),
        }
        assert_eq!(snap.border_color, 0); // byte 1 -> border bits (1>>1)&7 = 0
    }

    #[test]
    fn pc_zero_selects_v2_v3_path_using_additional_header_pc() {
        let bytes = v2_v3_header(0x9000, 0, 23, 0, 0); // v2, 48K

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.cpu.pc, 0x9000);
    }

    #[test]
    fn unsupported_hardware_mode_is_rejected() {
        let bytes = v2_v3_header(0x9000, 0, 23, 2, 0); // v2, SamRam

        assert_eq!(parse(&bytes), Err(Error::UnsupportedHardwareMode(2)));
    }

    #[test]
    fn v2_mode_3_is_128k_but_v3_mode_3_is_48k() {
        // This is the exact trap the format's own numbering sets: mode 3
        // means different hardware depending on version. Regression guard
        // for a real bug this caused against a real Fairlight - A Prelude
        // .z80 file, which is a v2 file with mode 3 (128K).
        let v2_mode_3 = v2_v3_header(0x9000, 0, 23, 3, 0);
        assert!(parse(&v2_mode_3).unwrap().paging.is_some());

        let v3_mode_3 = v2_v3_header(0x9000, 0, 54, 3, 0);
        assert_eq!(parse(&v3_mode_3).unwrap().paging, None);
    }

    #[test]
    fn v3_128k_modes_are_accepted() {
        for mode in [4u8, 5, 6] {
            let bytes = v2_v3_header(0x9000, 0, 54, mode, 0);
            assert!(
                parse(&bytes).unwrap().paging.is_some(),
                "mode {mode} should be 128K"
            );
        }
    }

    #[test]
    fn spectrum_plus_2_is_treated_as_128k_in_both_versions() {
        assert!(
            parse(&v2_v3_header(0x9000, 0, 23, 12, 0))
                .unwrap()
                .paging
                .is_some()
        );
        assert!(
            parse(&v2_v3_header(0x9000, 0, 54, 12, 0))
                .unwrap()
                .paging
                .is_some()
        );
    }

    #[test]
    fn port_state_decodes_into_paging_state() {
        // ram_bank=5 (0b101), shadow_screen=1 (bit3), rom_bank=1 (bit4),
        // locked=1 (bit5): 0b00111101.
        let bytes = v2_v3_header(0x9000, 0, 23, 3, 0b0011_1101);

        let paging = parse(&bytes).unwrap().paging.unwrap();

        assert_eq!(paging.ram_bank, 5);
        assert!(paging.shadow_screen);
        assert_eq!(paging.rom_bank, 1);
        assert!(paging.locked);
    }

    #[test]
    fn v2_page_blocks_map_to_real_addresses() {
        let mut bytes = v2_v3_header(0x9000, 0, 23, 0, 0);

        // Page 8 (0x4000-0x7fff), uncompressed (length 0xFFFF).
        bytes.extend((0xFFFFu16).to_le_bytes());
        bytes.push(8);
        let mut page8 = vec![0u8; 0x4000];
        page8[0] = 0x55;
        bytes.extend(&page8);

        // Page 4 (0x8000-0xbfff), compressed: 6 copies of 0x33.
        let compressed = [0xED, 0xED, 6, 0x33];
        bytes.extend((compressed.len() as u16).to_le_bytes());
        bytes.push(4);
        bytes.extend(compressed);

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.memory.len(), 2);
        assert_eq!(
            snap.memory[0],
            MemoryBlock::Absolute {
                address: 0x4000,
                data: {
                    let mut d = vec![0u8; 0x4000];
                    d[0] = 0x55;
                    d
                }
            }
        );
        match &snap.memory[1] {
            MemoryBlock::Absolute {
                address: 0x8000,
                data,
            } => assert_eq!(&data[..6], &[0x33u8; 6]),
            other => panic!("expected Absolute at 0x8000, got {other:?}"),
        }
    }

    #[test]
    fn v2_128k_page_blocks_map_to_ram_banks() {
        let mut bytes = v2_v3_header(0x9000, 0, 23, 3, 0);

        // Page 3 -> bank 0.
        bytes.extend((0xFFFFu16).to_le_bytes());
        bytes.push(3);
        let mut page3 = vec![0u8; 0x4000];
        page3[0] = 0xAB;
        bytes.extend(&page3);

        // Page 10 -> bank 7.
        bytes.extend((0xFFFFu16).to_le_bytes());
        bytes.push(10);
        bytes.extend(vec![0u8; 0x4000]);

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.memory.len(), 2);
        match &snap.memory[0] {
            MemoryBlock::RamBank { bank: 0, data } => assert_eq!(data[0], 0xAB),
            other => panic!("expected RamBank 0, got {other:?}"),
        }
        assert!(matches!(
            snap.memory[1],
            MemoryBlock::RamBank { bank: 7, .. }
        ));
    }

    #[test]
    fn unmapped_page_numbers_are_skipped_not_errored() {
        let mut bytes = v2_v3_header(0x9000, 0, 23, 0, 0);

        // Page 11 (Multiface ROM) -- not representable here, must be skipped.
        bytes.extend((0xFFFFu16).to_le_bytes());
        bytes.push(11);
        bytes.extend(vec![0u8; 0x4000]);

        let snap = parse(&bytes).unwrap();

        assert!(snap.memory.is_empty());
    }

    #[test]
    fn too_short_is_rejected() {
        assert_eq!(parse(&[0u8; 10]), Err(Error::TooShort));
    }

    /// Builds a full 55-byte-additional-header v3 file (the only length
    /// that carries byte 86, the +2A/+3 `0x1ffd` byte) with real hardware
    /// mode / port_7ffd / port_1ffd at their correct offsets.
    fn v3_2a3_header(pc: u16, hardware_mode: u8, port_7ffd: u8, port_1ffd: u8) -> Vec<u8> {
        let mut bytes = v2_v3_header(pc, 0, 55, hardware_mode, port_7ffd);
        // v2_v3_header already zero-padded up to the full 55-byte length;
        // byte 86 is the last byte of that header (offset 54 within it,
        // header starts at absolute offset 32: 32+54=86).
        let last = bytes.len() - 1;
        bytes[last] = port_1ffd;
        bytes
    }

    #[test]
    fn spectrum_plus_3_and_plus_2a_modes_are_accepted() {
        for mode in [7u8, 13] {
            let bytes = v3_2a3_header(0x9000, mode, 0, 0);
            let snap = parse(&bytes).unwrap();
            assert!(
                snap.paging.is_none(),
                "mode {mode} must not set 128K paging"
            );
            assert!(snap.paging_2a3.is_some(), "mode {mode} should be +2A/+3");
        }
    }

    #[test]
    fn plus_3_mode_with_short_header_is_rejected() {
        // Mode 7 claimed, but only a 54-byte header -- no byte 86 available.
        let bytes = v2_v3_header(0x9000, 0, 54, 7, 0);
        assert_eq!(parse(&bytes), Err(Error::Missing2A3PagingByte));
    }

    #[test]
    fn port_1ffd_decodes_into_paging_2a3_state() {
        // port_7ffd: ram_bank=5 (0b101), shadow_screen=1 (bit3),
        // rom_bit_lo=1 (bit4), locked=1 (bit5): 0b0011_1101.
        // port_1ffd: special_mode=1 (bit0), bits_1_2=0b10 (bits 2:1),
        // disk_motor=1 (bit3): 0b0000_1101.
        let bytes = v3_2a3_header(0x9000, 13, 0b0011_1101, 0b0000_1101);

        let paging = parse(&bytes).unwrap().paging_2a3.unwrap();

        assert_eq!(paging.ram_bank, 5);
        assert!(paging.shadow_screen);
        assert!(paging.rom_bit_lo);
        assert!(paging.locked);
        assert!(paging.special_mode);
        assert_eq!(paging.bits_1_2, 0b10);
        assert!(paging.disk_motor);
    }

    #[test]
    fn v3_2a3_page_blocks_map_to_ram_banks() {
        let mut bytes = v3_2a3_header(0x9000, 7, 0, 0);

        // Page 3 -> bank 0.
        bytes.extend((0xFFFFu16).to_le_bytes());
        bytes.push(3);
        let mut page3 = vec![0u8; 0x4000];
        page3[0] = 0xCD;
        bytes.extend(&page3);

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.memory.len(), 1);
        match &snap.memory[0] {
            MemoryBlock::RamBank { bank: 0, data } => assert_eq!(data[0], 0xCD),
            other => panic!("expected RamBank 0, got {other:?}"),
        }
    }

    fn sample_cpu() -> Cpu {
        let mut cpu = Cpu::new();
        cpu.af = 0x1122;
        cpu.bc = 0x3344;
        cpu.de = 0x5566;
        cpu.hl = 0x7788;
        cpu.af_ = 0x99AA;
        cpu.bc_ = 0xBBCC;
        cpu.de_ = 0xDDEE;
        cpu.hl_ = 0xFF00;
        cpu.ix = 0x1234;
        cpu.iy = 0x5678;
        cpu.sp = 0x9ABC;
        cpu.pc = 0x8000;
        cpu.i = 0x3F;
        cpu.r = 0x2A;
        cpu.iff1 = true;
        cpu.iff2 = true;
        cpu.im = 2;
        cpu
    }

    #[test]
    fn write_then_parse_round_trips_a_48k_snapshot() {
        let original = Snapshot {
            cpu: sample_cpu(),
            border_color: 4,
            memory: vec![
                MemoryBlock::Absolute {
                    address: 0x4000,
                    data: vec![1u8; 0x4000],
                },
                MemoryBlock::Absolute {
                    address: 0x8000,
                    data: vec![2u8; 0x4000],
                },
                MemoryBlock::Absolute {
                    address: 0xC000,
                    data: vec![3u8; 0x4000],
                },
            ],
            paging: None,
            paging_2a3: None,
        };

        let bytes = write(&original).unwrap();
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped, original);
    }

    #[test]
    fn write_accepts_a_single_merged_48k_block_too() {
        // The v1-parsed shape (one 49152-byte Absolute block) round-trips
        // just as well as the 3-block v2/v3 shape -- `write` always
        // produces the latter, but should accept either as input.
        let mut data = vec![0u8; 0xC000];
        data[0] = 0xAA; // 0x4000
        data[0x4000] = 0xBB; // 0x8000
        data[0x8000] = 0xCC; // 0xC000
        let snapshot = Snapshot {
            cpu: sample_cpu(),
            border_color: 0,
            memory: vec![MemoryBlock::Absolute {
                address: 0x4000,
                data,
            }],
            paging: None,
            paging_2a3: None,
        };

        let bytes = write(&snapshot).unwrap();
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped.cpu, snapshot.cpu);
        let get = |addr: u16| {
            round_tripped
                .memory
                .iter()
                .find_map(|b| match b {
                    MemoryBlock::Absolute { address, data } if *address == addr => Some(data[0]),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(get(0x4000), 0xAA);
        assert_eq!(get(0x8000), 0xBB);
        assert_eq!(get(0xC000), 0xCC);
    }

    #[test]
    fn write_48k_rejects_incomplete_ram() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: None,
            paging_2a3: None,
        };

        assert_eq!(write(&snapshot), Err(WriteError::Missing48KRam));
    }

    fn banked_memory(fill: impl Fn(u8) -> u8) -> Vec<MemoryBlock> {
        (0u8..8)
            .map(|bank| MemoryBlock::RamBank {
                bank,
                data: vec![fill(bank); 0x4000],
            })
            .collect()
    }

    #[test]
    fn write_then_parse_round_trips_a_128k_snapshot() {
        let original = Snapshot {
            cpu: sample_cpu(),
            border_color: 6,
            memory: banked_memory(|bank| bank * 10),
            paging: Some(PagingState {
                rom_bank: 1,
                ram_bank: 5,
                shadow_screen: true,
                locked: true,
            }),
            paging_2a3: None,
        };

        let bytes = write(&original).unwrap();
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped, original);
    }

    #[test]
    fn write_128k_rejects_a_missing_bank() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: Some(PagingState {
                rom_bank: 0,
                ram_bank: 0,
                shadow_screen: false,
                locked: false,
            }),
            paging_2a3: None,
        };

        assert_eq!(write(&snapshot), Err(WriteError::MissingRamBank(0)));
    }

    #[test]
    fn write_then_parse_round_trips_a_2a3_snapshot() {
        let original = Snapshot {
            cpu: sample_cpu(),
            border_color: 1,
            memory: banked_memory(|bank| bank + 100),
            paging: None,
            paging_2a3: Some(Paging2A3State {
                ram_bank: 2,
                shadow_screen: false,
                rom_bit_lo: true,
                locked: false,
                special_mode: true,
                bits_1_2: 0b11,
                disk_motor: true,
            }),
        };

        let bytes = write(&original).unwrap();
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped, original);
    }
}
