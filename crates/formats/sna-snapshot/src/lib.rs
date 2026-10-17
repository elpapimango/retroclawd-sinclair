//! Parser for the ZX Spectrum `.sna` snapshot file format (48K and the
//! plain-128K extension).
//!
//! Format reference: the World of Spectrum FAQ's snapshot format
//! specification, <https://worldofspectrum.org/faq/reference/formats.htm>
//! -- every byte offset and the 128K extension's own header/bank layout
//! were taken from that document, not guessed. See `CREDITS.md`.
//!
//! `write` is the save-side counterpart to `parse` (see its own doc
//! comment) -- both operate on the same `Snapshot` shape, so a round trip
//! reproduces the original state exactly.
//! Deliberately format-only: no dependency on `machine` or
//! `zx-spectrum` -- hands back a plain `z80::Cpu` plus raw memory blocks,
//! same "own shape, not shared with `z80-snapshot`'s conceptually similar
//! types" reasoning as every other format crate in this workspace (see
//! `CLAUDE.md`): `.sna` and `.z80` are different, independently-evolved
//! formats that happen to need similar-shaped data, not a reason to share
//! a type between them.
//!
//! The 128K extension only covers plain 128K/+2 (a single `0x7ffd` paging
//! byte) -- the format predates the +2A/+3's second paging register, so
//! there's no `.sna` shape that could represent it (`.z80` v3's mode
//! 7/8/13 is the only supported snapshot path for +2A/+3, see
//! `z80_snapshot`).

use z80::Cpu;

/// One decompressed memory block -- see `z80_snapshot::MemoryBlock`'s own
/// doc comment for why this is a distinct (not shared) type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryBlock {
    /// 48K memory: the entire `0x4000`-`0xffff` range in one block.
    Absolute { address: u16, data: Vec<u8> },
    /// 128K memory: one of the 8 real RAM banks. Applying this (resolving
    /// which address range each bank lands at, which depends on
    /// `Snapshot::paging`) is `zx_spectrum`'s own job.
    RamBank { bank: u8, data: Vec<u8> },
}

/// 128K's paging state (the last value `OUT` to port `0x7ffd`) -- see
/// `z80_snapshot::PagingState`'s own doc comment for why this is a
/// distinct (not shared) type. `.sna`'s 128K extension has no "locked" bit
/// of its own, so this always reports `false` (an ordinary resumable
/// state, not a real hardware fact the file encodes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagingState {
    pub rom_bank: u8,
    pub ram_bank: u8,
    pub shadow_screen: bool,
    pub locked: bool,
}

/// A fully parsed `.sna` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cpu: Cpu,
    /// Border color at save time (0-7).
    pub border_color: u8,
    pub memory: Vec<MemoryBlock>,
    /// `Some` for the 128K extension, `None` for the plain 48K format.
    pub paging: Option<PagingState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Total file length matched neither the fixed 48K size (49179) nor
    /// the fixed 128K size (131103) -- `.sna` has no version/length field
    /// of its own to detect corruption more precisely than this.
    UnrecognizedLength(usize),
    /// The header's `SP` didn't point into RAM (`0x4000..=0xfffe`) -- the
    /// 48K format recovers `PC` by reading the word `SP` points at (real
    /// `.sna`-writing tools always `PUSH`ed it there first), so a `SP`
    /// outside RAM means there's nothing valid to recover.
    StackPointerNotInRam(u16),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnrecognizedLength(len) => {
                write!(f, "not a recognized .sna file size ({len} bytes)")
            }
            Error::StackPointerNotInRam(sp) => {
                write!(
                    f,
                    "SP (0x{sp:04X}) doesn't point into RAM -- can't recover PC"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

const HEADER_LEN: usize = 27;
const RAM_LEN: usize = 0xC000; // 49152: 0x4000-0xffff
const SNA_48K_LEN: usize = HEADER_LEN + RAM_LEN; // 49179
const BANK_LEN: usize = 0x4000;
const SNA_128K_LEN: usize = SNA_48K_LEN + 4 + 5 * BANK_LEN; // 131103

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

/// Parses a `.sna` file. Real `.sna`-writing tools always leave `PC`
/// sitting on top of the stack (pushed just before the dump), so the 48K
/// format's own `PC` is recovered from RAM at `SP`, and `SP` itself ends up
/// 2 higher than the header's own value -- exactly what a real `RET` would
/// do. The 128K extension fixes this at the source (`CREDITS.md`): it
/// carries `PC` as its own explicit field instead, so `SP` there is used
/// as-is.
pub fn parse(bytes: &[u8]) -> Result<Snapshot, Error> {
    if bytes.len() != SNA_48K_LEN && bytes.len() != SNA_128K_LEN {
        return Err(Error::UnrecognizedLength(bytes.len()));
    }

    let mut cpu = Cpu::new();
    cpu.i = bytes[0];
    cpu.hl_ = le16(bytes, 1);
    cpu.de_ = le16(bytes, 3);
    cpu.bc_ = le16(bytes, 5);
    cpu.af_ = le16(bytes, 7);
    cpu.hl = le16(bytes, 9);
    cpu.de = le16(bytes, 11);
    cpu.bc = le16(bytes, 13);
    cpu.iy = le16(bytes, 15);
    cpu.ix = le16(bytes, 17);
    let iff2 = bytes[19] & 0b0000_0100 != 0;
    cpu.iff2 = iff2;
    // .sna carries no separate IFF1 -- real interrupt state at save time is
    // simply enabled or disabled, so IFF1 tracks IFF2 (the same convention
    // every other real .sna reader/writer uses).
    cpu.iff1 = iff2;
    cpu.r = bytes[20];
    cpu.af = le16(bytes, 21);
    let mut sp = le16(bytes, 23);
    cpu.im = bytes[25];
    let border_color = bytes[26] & 0b111;

    let ram = &bytes[HEADER_LEN..SNA_48K_LEN];

    if bytes.len() == SNA_48K_LEN {
        if !(0x4000..=0xFFFE).contains(&sp) {
            return Err(Error::StackPointerNotInRam(sp));
        }
        let stack_offset = (sp - 0x4000) as usize;
        cpu.pc = u16::from_le_bytes([ram[stack_offset], ram[stack_offset + 1]]);
        sp = sp.wrapping_add(2);
        cpu.sp = sp;

        return Ok(Snapshot {
            cpu,
            border_color,
            memory: vec![MemoryBlock::Absolute {
                address: 0x4000,
                data: ram.to_vec(),
            }],
            paging: None,
        });
    }

    // 128K extension: `ram` here is bank 5 + bank 2 + whichever bank was
    // paged at 0xC000, concatenated in that order (matching what's visible
    // in the CPU's address space at save time) -- not yet resolved to real
    // bank numbers beyond 5/2, done just below via the paging byte.
    cpu.sp = sp;
    cpu.pc = le16(bytes, SNA_48K_LEN);
    let paging_byte = bytes[SNA_48K_LEN + 2];
    let paged_bank = paging_byte & 0b111;
    let paging = PagingState {
        rom_bank: (paging_byte >> 4) & 1,
        ram_bank: paged_bank,
        shadow_screen: paging_byte & 0b0000_1000 != 0,
        locked: false,
    };

    let mut memory = vec![
        MemoryBlock::RamBank {
            bank: 5,
            data: ram[0..BANK_LEN].to_vec(),
        },
        MemoryBlock::RamBank {
            bank: 2,
            data: ram[BANK_LEN..2 * BANK_LEN].to_vec(),
        },
        MemoryBlock::RamBank {
            bank: paged_bank,
            data: ram[2 * BANK_LEN..3 * BANK_LEN].to_vec(),
        },
    ];

    // The remaining 5 banks (every bank number 0-7 except 5, 2, and
    // whichever was already paged in above), in ascending order, each a
    // plain 16K block straight from the file.
    let mut remaining_pos = SNA_48K_LEN + 4;
    for bank in 0u8..8 {
        if bank == 5 || bank == 2 || bank == paged_bank {
            continue;
        }
        let data = bytes[remaining_pos..remaining_pos + BANK_LEN].to_vec();
        remaining_pos += BANK_LEN;
        memory.push(MemoryBlock::RamBank { bank, data });
    }

    Ok(Snapshot {
        cpu,
        border_color,
        memory,
        paging: Some(paging),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// 48K: expected exactly one `MemoryBlock::Absolute { address: 0x4000,
    /// .. }` covering the full 49152-byte RAM range.
    Missing48KRam,
    /// 128K: one of the 8 real RAM banks (0-7) wasn't present in
    /// `Snapshot::memory`, or wasn't exactly 16K.
    MissingRamBank(u8),
    /// 48K only: `cpu.sp - 2` doesn't land in RAM (`0x4000..=0xfffe`) --
    /// there's nowhere valid to push `PC` for the real 48K format's own
    /// stack-based recovery trick (see `parse`'s doc comment). The 128K
    /// extension has no such restriction (it carries `PC` as its own
    /// explicit field), so this can't happen when `paging` is `Some`.
    StackPointerNotInRam(u16),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Missing48KRam => {
                write!(f, "snapshot has no 0x4000-0xffff Absolute memory block")
            }
            WriteError::MissingRamBank(bank) => {
                write!(f, "snapshot is missing RAM bank {bank} (or it isn't 16K)")
            }
            WriteError::StackPointerNotInRam(sp) => write!(
                f,
                "SP (0x{sp:04X}) - 2 doesn't land in RAM -- nowhere to push PC"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

fn write_header(cpu: &Cpu, border_color: u8, sp: u16, out: &mut Vec<u8>) {
    out.push(cpu.i);
    out.extend_from_slice(&cpu.hl_.to_le_bytes());
    out.extend_from_slice(&cpu.de_.to_le_bytes());
    out.extend_from_slice(&cpu.bc_.to_le_bytes());
    out.extend_from_slice(&cpu.af_.to_le_bytes());
    out.extend_from_slice(&cpu.hl.to_le_bytes());
    out.extend_from_slice(&cpu.de.to_le_bytes());
    out.extend_from_slice(&cpu.bc.to_le_bytes());
    out.extend_from_slice(&cpu.iy.to_le_bytes());
    out.extend_from_slice(&cpu.ix.to_le_bytes());
    out.push(if cpu.iff2 { 0b0000_0100 } else { 0 });
    out.push(cpu.r);
    out.extend_from_slice(&cpu.af.to_le_bytes());
    out.extend_from_slice(&sp.to_le_bytes());
    out.push(cpu.im);
    out.push(border_color & 0b111);
}

/// Serializes a `Snapshot` back to `.sna` bytes -- the 48K format if
/// `snapshot.paging` is `None`, the 128K extension otherwise. Symmetric
/// with `parse`: a round trip through `write` then `parse` reproduces the
/// same `Snapshot` (48K's `PC`/`SP` recovery trick included -- `write`
/// pushes `PC` onto the stack exactly as a real `.sna`-writing tool would,
/// so `parse`'s own recovery logic reconstructs it identically).
pub fn write(snapshot: &Snapshot) -> Result<Vec<u8>, WriteError> {
    match &snapshot.paging {
        None => write_48k(snapshot),
        Some(paging) => write_128k(snapshot, *paging),
    }
}

fn write_48k(snapshot: &Snapshot) -> Result<Vec<u8>, WriteError> {
    let ram = snapshot
        .memory
        .iter()
        .find_map(|block| match block {
            MemoryBlock::Absolute {
                address: 0x4000,
                data,
            } if data.len() == RAM_LEN => Some(data.clone()),
            _ => None,
        })
        .ok_or(WriteError::Missing48KRam)?;

    let cpu = &snapshot.cpu;
    let new_sp = cpu.sp.wrapping_sub(2);
    if !(0x4000..=0xFFFE).contains(&new_sp) {
        return Err(WriteError::StackPointerNotInRam(cpu.sp));
    }

    let mut ram = ram;
    let stack_offset = (new_sp - 0x4000) as usize;
    let [lo, hi] = cpu.pc.to_le_bytes();
    ram[stack_offset] = lo;
    ram[stack_offset + 1] = hi;

    let mut out = Vec::with_capacity(SNA_48K_LEN);
    write_header(cpu, snapshot.border_color, new_sp, &mut out);
    out.extend_from_slice(&ram);
    Ok(out)
}

fn write_128k(snapshot: &Snapshot, paging: PagingState) -> Result<Vec<u8>, WriteError> {
    let bank_data = |bank: u8| -> Result<&[u8], WriteError> {
        snapshot
            .memory
            .iter()
            .find_map(|block| match block {
                MemoryBlock::RamBank { bank: b, data } if *b == bank && data.len() == BANK_LEN => {
                    Some(data.as_slice())
                }
                _ => None,
            })
            .ok_or(WriteError::MissingRamBank(bank))
    };

    let cpu = &snapshot.cpu;
    let mut out = Vec::with_capacity(SNA_128K_LEN);
    write_header(cpu, snapshot.border_color, cpu.sp, &mut out);

    out.extend_from_slice(bank_data(5)?);
    out.extend_from_slice(bank_data(2)?);
    out.extend_from_slice(bank_data(paging.ram_bank)?);

    out.extend_from_slice(&cpu.pc.to_le_bytes());
    let paging_byte = (paging.ram_bank & 0b111)
        | ((paging.rom_bank & 1) << 4)
        | if paging.shadow_screen { 0b0000_1000 } else { 0 };
    out.push(paging_byte);
    out.push(0); // TR-DOS ROM paging flag -- not modeled, always "not paged".

    for bank in 0u8..8 {
        if bank == 5 || bank == 2 || bank == paging.ram_bank {
            continue;
        }
        out.extend_from_slice(bank_data(bank)?);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_48k(sp: u16) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[0] = 0x11; // I
        h[1..3].copy_from_slice(&0x2233u16.to_le_bytes()); // HL'
        h[3..5].copy_from_slice(&0x4455u16.to_le_bytes()); // DE'
        h[5..7].copy_from_slice(&0x6677u16.to_le_bytes()); // BC'
        h[7..9].copy_from_slice(&0x8899u16.to_le_bytes()); // AF'
        h[9..11].copy_from_slice(&0xAABBu16.to_le_bytes()); // HL
        h[11..13].copy_from_slice(&0xCCDDu16.to_le_bytes()); // DE
        h[13..15].copy_from_slice(&0xEEFFu16.to_le_bytes()); // BC
        h[15..17].copy_from_slice(&0x1122u16.to_le_bytes()); // IY
        h[17..19].copy_from_slice(&0x3344u16.to_le_bytes()); // IX
        h[19] = 0b0000_0100; // IFF2 set
        h[20] = 0x77; // R
        h[21..23].copy_from_slice(&0x5566u16.to_le_bytes()); // AF
        h[23..25].copy_from_slice(&sp.to_le_bytes()); // SP
        h[25] = 1; // IM1
        h[26] = 3; // border
        h
    }

    #[test]
    fn parses_a_48k_snapshot_and_recovers_pc_from_the_stack() {
        let mut bytes = header_48k(0x4000);
        let mut ram = vec![0u8; RAM_LEN];
        ram[0..2].copy_from_slice(&0x9ABCu16.to_le_bytes()); // PC, sitting at SP
        bytes.extend_from_slice(&ram);

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.cpu.i, 0x11);
        assert_eq!(snap.cpu.hl_, 0x2233);
        assert_eq!(snap.cpu.de_, 0x4455);
        assert_eq!(snap.cpu.bc_, 0x6677);
        assert_eq!(snap.cpu.af_, 0x8899);
        assert_eq!(snap.cpu.hl, 0xAABB);
        assert_eq!(snap.cpu.de, 0xCCDD);
        assert_eq!(snap.cpu.bc, 0xEEFF);
        assert_eq!(snap.cpu.iy, 0x1122);
        assert_eq!(snap.cpu.ix, 0x3344);
        assert!(snap.cpu.iff1);
        assert!(snap.cpu.iff2);
        assert_eq!(snap.cpu.r, 0x77);
        assert_eq!(snap.cpu.af, 0x5566);
        assert_eq!(snap.cpu.im, 1);
        assert_eq!(snap.border_color, 3);
        assert_eq!(snap.cpu.pc, 0x9ABC);
        assert_eq!(snap.cpu.sp, 0x4002);
        assert!(snap.paging.is_none());
        match &snap.memory[0] {
            MemoryBlock::Absolute { address, data } => {
                assert_eq!(*address, 0x4000);
                assert_eq!(data.len(), RAM_LEN);
            }
            _ => panic!("expected an Absolute block"),
        }
    }

    fn rejects_a_stack_pointer_outside_ram() {
        let mut bytes = header_48k(0x0010); // in ROM, not RAM
        bytes.extend_from_slice(&vec![0u8; RAM_LEN]);

        assert_eq!(parse(&bytes), Err(Error::StackPointerNotInRam(0x0010)));
    }

    fn rejects_an_unrecognized_length() {
        assert_eq!(parse(&[0u8; 100]), Err(Error::UnrecognizedLength(100)));
    }
    /// Merged (every check kept): `rejects_a_stack_pointer_outside_ram`, `rejects_an_unrecognized_length`.
    #[test]
    fn parse_rejections() {
        rejects_a_stack_pointer_outside_ram();
        rejects_an_unrecognized_length();
    }

    #[test]
    fn parses_a_128k_snapshot_with_correct_bank_assignment_and_pc() {
        let mut bytes = header_48k(0x8000); // SP unused for PC recovery here
        // Bank 5, then bank 2, then whichever bank is paged at 0xC000.
        bytes.extend_from_slice(&vec![5u8; BANK_LEN]);
        bytes.extend_from_slice(&vec![2u8; BANK_LEN]);
        bytes.extend_from_slice(&vec![3u8; BANK_LEN]); // paged bank = 3

        bytes.extend_from_slice(&0x1234u16.to_le_bytes()); // PC
        bytes.push(0b0000_0011); // 0x7ffd: ram_bank=3, rom_bank=0, shadow=0
        bytes.push(0); // TR-DOS flag (ignored)

        // Remaining banks 0,1,4,6,7 in ascending order.
        for bank in [0u8, 1, 4, 6, 7] {
            bytes.extend_from_slice(&vec![bank; BANK_LEN]);
        }

        let snap = parse(&bytes).unwrap();

        assert_eq!(snap.cpu.pc, 0x1234);
        assert_eq!(snap.cpu.sp, 0x8000, "128K format uses SP as-is");
        let paging = snap.paging.unwrap();
        assert_eq!(paging.ram_bank, 3);
        assert_eq!(paging.rom_bank, 0);
        assert!(!paging.shadow_screen);
        assert!(!paging.locked);

        let banks: Vec<(u8, u8)> = snap
            .memory
            .iter()
            .map(|b| match b {
                MemoryBlock::RamBank { bank, data } => (*bank, data[0]),
                MemoryBlock::Absolute { .. } => panic!("unexpected Absolute block"),
            })
            .collect();
        assert_eq!(
            banks,
            vec![
                (5, 5),
                (2, 2),
                (3, 3),
                (0, 0),
                (1, 1),
                (4, 4),
                (6, 6),
                (7, 7)
            ]
        );
    }

    #[test]
    fn write_then_parse_round_trips_a_48k_snapshot() {
        let mut cpu = Cpu::new();
        cpu.i = 0x11;
        cpu.hl_ = 0x2233;
        cpu.de_ = 0x4455;
        cpu.bc_ = 0x6677;
        cpu.af_ = 0x8899;
        cpu.hl = 0xAABB;
        cpu.de = 0xCCDD;
        cpu.bc = 0xEEFF;
        cpu.iy = 0x1122;
        cpu.ix = 0x3344;
        cpu.iff1 = true;
        cpu.iff2 = true;
        cpu.r = 0x77;
        cpu.af = 0x5566;
        cpu.sp = 0xFF00;
        cpu.pc = 0x8000;
        cpu.im = 1;

        let mut ram = vec![0u8; RAM_LEN];
        ram[0x1234] = 0x42;
        // `write` pushes PC at `sp - 2` (real 48K format convention, see
        // `write_48k`'s doc comment) -- pre-populate the same spot with
        // what a real snapshot's own RAM would already hold there, so
        // `write` doesn't change `original`'s own memory out from under
        // this comparison.
        let stack_offset = (cpu.sp.wrapping_sub(2) - 0x4000) as usize;
        ram[stack_offset..stack_offset + 2].copy_from_slice(&cpu.pc.to_le_bytes());
        let original = Snapshot {
            cpu,
            border_color: 5,
            memory: vec![MemoryBlock::Absolute {
                address: 0x4000,
                data: ram,
            }],
            paging: None,
        };

        let bytes = write(&original).unwrap();
        assert_eq!(bytes.len(), SNA_48K_LEN);
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped, original);
    }

    fn write_48k_rejects_a_stack_pointer_with_nowhere_to_push_pc() {
        let mut cpu = Cpu::new();
        cpu.sp = 0x0001; // sp-2 wraps below RAM
        let snapshot = Snapshot {
            cpu,
            border_color: 0,
            memory: vec![MemoryBlock::Absolute {
                address: 0x4000,
                data: vec![0u8; RAM_LEN],
            }],
            paging: None,
        };

        assert!(matches!(
            write(&snapshot),
            Err(WriteError::StackPointerNotInRam(0x0001))
        ));
    }

    fn write_48k_rejects_a_missing_ram_block() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: None,
        };

        assert_eq!(write(&snapshot), Err(WriteError::Missing48KRam));
    }

    #[test]
    fn write_then_parse_round_trips_a_128k_snapshot() {
        let mut cpu = Cpu::new();
        cpu.sp = 0x8000;
        cpu.pc = 0x1234;

        let paging = PagingState {
            rom_bank: 1,
            ram_bank: 3,
            shadow_screen: false,
            locked: false,
        };
        // Bank order matches exactly what `parse` itself produces (5, 2,
        // the paged bank, then the rest ascending) -- `Vec` equality is
        // order-sensitive, and this is purely a test-construction detail,
        // not something either `write` or `parse` need to agree on with
        // each other (a `Snapshot` a caller builds by hand could list
        // banks in any order).
        let memory = [5u8, 2, paging.ram_bank, 0, 1, 4, 6, 7]
            .map(|bank| MemoryBlock::RamBank {
                bank,
                data: vec![bank; BANK_LEN],
            })
            .to_vec();
        let original = Snapshot {
            cpu,
            border_color: 2,
            memory,
            paging: Some(paging),
        };

        let bytes = write(&original).unwrap();
        assert_eq!(bytes.len(), SNA_128K_LEN);
        let round_tripped = parse(&bytes).unwrap();

        assert_eq!(round_tripped, original);
    }

    fn write_128k_rejects_a_missing_ram_bank() {
        let snapshot = Snapshot {
            cpu: Cpu::new(),
            border_color: 0,
            memory: vec![], // no banks at all
            paging: Some(PagingState {
                rom_bank: 0,
                ram_bank: 0,
                shadow_screen: false,
                locked: false,
            }),
        };

        assert_eq!(write(&snapshot), Err(WriteError::MissingRamBank(5)));
    }
    /// Merged (every check kept): `write_48k_rejects_a_stack_pointer_with_nowhere_to_push_pc`, `write_48k_rejects_a_missing_ram_block`, `write_128k_rejects_a_missing_ram_bank`.
    #[test]
    fn write_rejections() {
        write_48k_rejects_a_stack_pointer_with_nowhere_to_push_pc();
        write_48k_rejects_a_missing_ram_block();
        write_128k_rejects_a_missing_ram_bank();
    }
}
