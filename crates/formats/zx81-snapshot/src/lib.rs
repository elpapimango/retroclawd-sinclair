//! Parser for the ZX81 `.p`/`.p81` snapshot file formats.
//!
//! Reference: ZEsarUX's `src/snap/snap_zx8081.c`
//! (`new_load_zx81_p_snapshot_in_mem`), <https://github.com/chernandezba/zesarux>,
//! copyright Cesar Hernandez Bano, GNU GPLv3 -- read to understand the real
//! file layout (not copied; this is a fresh Rust implementation of the same
//! documented behavior). See `CREDITS.md` at the repo root.
//!
//! Both directions: `parse_p`/`parse_p81` read, `write_p`/`write_p81` write the
//! same bytes back (a `.p`/`.81` is exactly the memory from `0x4009` up to
//! `E_LINE`-1; a `.p81` is that behind a name header).
//!
//! Deliberately format-only, same reasoning as `z80-snapshot`: neither
//! variant of the ZX81 file format encodes any CPU register state at all
//! (unlike `.z80`) -- both are just a raw memory dump meant to be poked in
//! starting at address `0x4009` (system variables `0x4000`-`0x4008` are
//! fixed/derived, never stored in the file). Real ZX81 ROMs additionally
//! synthesize a specific fixed register/stack state so execution can resume
//! cleanly into `BASIC` after the poke -- that's `zx81`-machine-specific
//! knowledge (it needs the machine's actual `RAMTOP`), not part of the file
//! format itself, so it lives in `zx81::Zx81::load_p_snapshot`, not here.

use std::fmt;

/// A parsed `.p`/`.p81` snapshot: the raw bytes meant to be loaded starting
/// at `0x4009`, plus the `.p81`-only embedded program name if one was
/// present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Raw ZX81 character codes from the `.p81` name header, high bit
    /// still set on the terminating byte, undecoded. Turning these into
    /// printable ASCII needs the ZX81 character-set table, which hasn't
    /// been verified against real ROM data in this codebase -- decoding it
    /// isn't needed to load the program, so it's left raw rather than
    /// guessed. `None` for `.p`/`.81` files, which have no name header.
    pub program_name_raw: Option<Vec<u8>>,
    /// Bytes to load starting at `0x4009`.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `.p81`'s name header didn't end (a byte with bit 7 set) within the
    /// first 255 bytes -- ZEsarUX's own loader gives up at the same bound.
    MissingNameTerminator,
    /// A ZX80 `.o` file shorter than the ZX80's 40 bytes of system variables.
    Zx80TooShort(usize),
    /// A ZX80 `.o` file whose `E_LINE` (the word at offset `0x0A`) does not say where the file ends: the ZX80's `SAVE` writes
    /// exactly `0x4000..E_LINE`.
    Zx80LengthMismatch { e_line: u16, len: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MissingNameTerminator => {
                write!(
                    f,
                    "p81 name header has no terminating byte (bit 7 set) in the first 255 bytes"
                )
            }
            Error::Zx80TooShort(len) => write!(
                f,
                "a ZX80 .o file holds at least the 40 bytes of system variables, this one has {len}"
            ),
            Error::Zx80LengthMismatch { e_line, len } => write!(
                f,
                "a ZX80 .o file runs from 0x4000 up to E_LINE, but E_LINE is {e_line:#06x} (a file of {} bytes) and this one has {len}",
                usize::from(*e_line).wrapping_sub(0x4000)
            ),
        }
    }
}

impl std::error::Error for Error {}

const NAME_HEADER_SEARCH_LIMIT: usize = 255;

/// Parses a `.p` (or `.81`) file: no name header, the whole file is data
/// for `0x4009` onward.
pub fn parse_p(bytes: &[u8]) -> Snapshot {
    Snapshot {
        program_name_raw: None,
        data: bytes.to_vec(),
    }
}

/// Parses a `.p81` file: a ZX81-character-encoded program name (its last
/// byte has bit 7 set, marking the end) precedes the same `.p`-shaped data.
pub fn parse_p81(bytes: &[u8]) -> Result<Snapshot, Error> {
    let search_len = bytes.len().min(NAME_HEADER_SEARCH_LIMIT);
    let terminator = bytes[..search_len].iter().position(|b| b & 0x80 != 0);

    match terminator {
        Some(pos) => Ok(Snapshot {
            program_name_raw: Some(bytes[..=pos].to_vec()),
            data: bytes[pos + 1..].to_vec(),
        }),
        None => Err(Error::MissingNameTerminator),
    }
}

/// The bytes of a `.p` (or `.81`) file: the data, nothing else.
pub fn write_p(snapshot: &Snapshot) -> Vec<u8> {
    snapshot.data.clone()
}

/// The bytes of a `.p81` file: the name header (its last byte has bit 7 set),
/// then the data. A snapshot without a name, or whose name does not end in a
/// byte with bit 7 set, cannot be written as a `.p81`.
pub fn write_p81(snapshot: &Snapshot) -> Result<Vec<u8>, Error> {
    let name = snapshot
        .program_name_raw
        .as_deref()
        .filter(|n| !n.is_empty() && n.len() <= NAME_HEADER_SEARCH_LIMIT)
        .ok_or(Error::MissingNameTerminator)?;
    let ends_at = name.iter().position(|b| b & 0x80 != 0);
    if ends_at != Some(name.len() - 1) {
        return Err(Error::MissingNameTerminator);
    }
    let mut bytes = name.to_vec();
    bytes.extend_from_slice(&snapshot.data);
    Ok(bytes)
}

/// The bytes of the system variables a ZX80 keeps at `0x4000` (`ERR_NR` .. `CH_ADD`, 40 bytes).
pub const ZX80_SYSVARS_LEN: usize = 0x28;
/// Offset of `E_LINE` (the end of the saved area) inside a ZX80 `.o` file.
const ZX80_E_LINE_OFFSET: usize = 0x0A;

/// A ZX80 `.o` program: memory from `0x4000` (the system variables) up to `E_LINE`, which is what the ZX80's `SAVE` puts on tape.
/// Unlike a ZX81 `.p` it starts at `0x4000`, not `0x4009`, and has no name -- the ZX80's tape routine writes the bytes with no
/// header at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zx80Program {
    /// Bytes to load starting at `0x4000`.
    pub data: Vec<u8>,
}

impl Zx80Program {
    /// `E_LINE`, the first address after the saved area (the word at `0x400A`).
    pub fn e_line(&self) -> u16 {
        u16::from_le_bytes([
            self.data[ZX80_E_LINE_OFFSET],
            self.data[ZX80_E_LINE_OFFSET + 1],
        ])
    }
}

/// Parses a ZX80 `.o` file. The only structure it has is that `E_LINE` at offset `0x0A` is where the file ends, so a file that
/// disagrees with its own `E_LINE` is refused (a ZX81 `.p` loaded by mistake, a truncated download).
pub fn parse_o(bytes: &[u8]) -> Result<Zx80Program, Error> {
    if bytes.len() < ZX80_SYSVARS_LEN {
        return Err(Error::Zx80TooShort(bytes.len()));
    }
    let e_line = u16::from_le_bytes([bytes[ZX80_E_LINE_OFFSET], bytes[ZX80_E_LINE_OFFSET + 1]]);
    if usize::from(e_line).wrapping_sub(0x4000) != bytes.len() {
        return Err(Error::Zx80LengthMismatch {
            e_line,
            len: bytes.len(),
        });
    }
    Ok(Zx80Program {
        data: bytes.to_vec(),
    })
}

/// The bytes of a `.o` file: the data, nothing else.
pub fn write_o(program: &Zx80Program) -> Vec<u8> {
    program.data.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_p_is_the_data_and_parse_p_reads_it_back() {
        let snap = parse_p(&[1, 2, 3]);
        assert_eq!(write_p(&snap), vec![1, 2, 3]);
    }

    fn write_p81_round_trips_a_named_file() {
        let bytes = [0x26, 0x27 | 0x80, 0xAA, 0xBB];
        let snap = parse_p81(&bytes).unwrap();
        assert_eq!(write_p81(&snap).unwrap(), bytes.to_vec());
    }

    fn write_p81_needs_a_name_that_ends_with_bit_7() {
        let nameless = parse_p(&[1, 2, 3]);
        assert_eq!(write_p81(&nameless), Err(Error::MissingNameTerminator));
        let unterminated = Snapshot {
            program_name_raw: Some(vec![0x26, 0x27]),
            data: vec![1],
        };
        assert_eq!(write_p81(&unterminated), Err(Error::MissingNameTerminator));
        // A bit-7 byte inside the name would end it early: not a valid header.
        let early = Snapshot {
            program_name_raw: Some(vec![0x26 | 0x80, 0x27 | 0x80]),
            data: vec![1],
        };
        assert_eq!(write_p81(&early), Err(Error::MissingNameTerminator));
    }
    /// Merged (every check kept): `write_p81_round_trips_a_named_file`, `write_p81_needs_a_name_that_ends_with_bit_7`.
    #[test]
    fn write_p81_cases() {
        write_p81_round_trips_a_named_file();
        write_p81_needs_a_name_that_ends_with_bit_7();
    }

    #[test]
    fn parse_p_has_no_name_and_treats_everything_as_data() {
        let snap = parse_p(&[1, 2, 3]);

        assert_eq!(snap.program_name_raw, None);
        assert_eq!(snap.data, vec![1, 2, 3]);
    }

    fn parse_p81_splits_name_header_from_data() {
        // "AB" (raw codes, arbitrary here since we don't decode them) with
        // the final byte's bit 7 set marking the end of the name.
        let bytes = [0x26, 0x27 | 0x80, 0xAA, 0xBB];

        let snap = parse_p81(&bytes).unwrap();

        assert_eq!(snap.program_name_raw, Some(vec![0x26, 0x27 | 0x80]));
        assert_eq!(snap.data, vec![0xAA, 0xBB]);
    }

    fn parse_p81_with_single_byte_name() {
        let bytes = [0x01 | 0x80, 0xAA, 0xBB];

        let snap = parse_p81(&bytes).unwrap();

        assert_eq!(snap.program_name_raw, Some(vec![0x01 | 0x80]));
        assert_eq!(snap.data, vec![0xAA, 0xBB]);
    }

    fn parse_p81_without_terminator_is_an_error() {
        let bytes = vec![0x01u8; 300]; // no bit-7 byte anywhere

        assert_eq!(parse_p81(&bytes), Err(Error::MissingNameTerminator));
    }
    /// Merged (every check kept): `parse_p81_splits_name_header_from_data`, `parse_p81_with_single_byte_name`, `parse_p81_without_terminator_is_an_error`.
    #[test]
    fn parse_p81_cases() {
        parse_p81_splits_name_header_from_data();
        parse_p81_with_single_byte_name();
        parse_p81_without_terminator_is_an_error();
    }

    /// A ZX80 file of `len` bytes whose `E_LINE` says `0x4000 + len`.
    fn zx80_file(len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        let e_line = 0x4000u16 + len as u16;
        bytes[0x0A..0x0C].copy_from_slice(&e_line.to_le_bytes());
        bytes
    }

    fn a_zx80_o_file_is_the_memory_from_4000_up_to_its_own_e_line() {
        let bytes = zx80_file(0x28 + 17);
        let program = parse_o(&bytes).unwrap();
        assert_eq!(program.e_line(), 0x4000 + 0x28 + 17);
        assert_eq!(program.data, bytes);
        assert_eq!(write_o(&program), bytes);
    }

    fn a_zx80_file_that_disagrees_with_its_e_line_or_is_too_short_is_refused() {
        let mut bytes = zx80_file(0x30);
        bytes.push(0);
        assert_eq!(
            parse_o(&bytes),
            Err(Error::Zx80LengthMismatch {
                e_line: 0x4030,
                len: 0x31
            })
        );
        assert_eq!(parse_o(&[0; 10]), Err(Error::Zx80TooShort(10)));
        // A ZX81 .p starts at 0x4009 with E_LINE at offset 0x0B, so its offset 0x0A is not an address of this file.
        let p = [0x00u8, 0x80, 0x76, 0x76, 0xD4, 0x43, 0, 0, 0, 0, 0x6E, 0x43];
        assert!(parse_o(&[&p[..], &[0; 40]].concat()).is_err());
        assert!(parse_o(&bytes).unwrap_err().to_string().contains("E_LINE"));
    }
    /// Merged (every check kept): `a_zx80_o_file_is_the_memory_from_4000_up_to_its_own_e_line`, `a_zx80_file_that_disagrees_with_its_e_line_or_is_too_short_is_refused`.
    #[test]
    fn zx80_o_files() {
        a_zx80_o_file_is_the_memory_from_4000_up_to_its_own_e_line();
        a_zx80_file_that_disagrees_with_its_e_line_or_is_too_short_is_refused();
    }
}
