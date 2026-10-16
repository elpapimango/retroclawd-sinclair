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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_p_is_the_data_and_parse_p_reads_it_back() {
        let snap = parse_p(&[1, 2, 3]);
        assert_eq!(write_p(&snap), vec![1, 2, 3]);
    }

    #[test]
    fn write_p81_round_trips_a_named_file() {
        let bytes = [0x26, 0x27 | 0x80, 0xAA, 0xBB];
        let snap = parse_p81(&bytes).unwrap();
        assert_eq!(write_p81(&snap).unwrap(), bytes.to_vec());
    }

    #[test]
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

    #[test]
    fn parse_p_has_no_name_and_treats_everything_as_data() {
        let snap = parse_p(&[1, 2, 3]);

        assert_eq!(snap.program_name_raw, None);
        assert_eq!(snap.data, vec![1, 2, 3]);
    }

    #[test]
    fn parse_p81_splits_name_header_from_data() {
        // "AB" (raw codes, arbitrary here since we don't decode them) with
        // the final byte's bit 7 set marking the end of the name.
        let bytes = [0x26, 0x27 | 0x80, 0xAA, 0xBB];

        let snap = parse_p81(&bytes).unwrap();

        assert_eq!(snap.program_name_raw, Some(vec![0x26, 0x27 | 0x80]));
        assert_eq!(snap.data, vec![0xAA, 0xBB]);
    }

    #[test]
    fn parse_p81_with_single_byte_name() {
        let bytes = [0x01 | 0x80, 0xAA, 0xBB];

        let snap = parse_p81(&bytes).unwrap();

        assert_eq!(snap.program_name_raw, Some(vec![0x01 | 0x80]));
        assert_eq!(snap.data, vec![0xAA, 0xBB]);
    }

    #[test]
    fn parse_p81_without_terminator_is_an_error() {
        let bytes = vec![0x01u8; 300]; // no bit-7 byte anywhere

        assert_eq!(parse_p81(&bytes), Err(Error::MissingNameTerminator));
    }
}
