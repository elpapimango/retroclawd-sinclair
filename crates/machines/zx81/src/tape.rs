//! The ZX81's cassette signal: a `.p` file as a train of pulses (for `LOAD`), and a recording of the MIC output turned
//! back into the file (for `SAVE`).
//!
//! Format ("ZX80/ZX81 Cassette Signals" and "Cassette File Structure", Martin Korth's "Sinclair ZX Specifications",
//! <https://problemkaputt.de/zxdocs.htm>): bytes go out most significant bit first with no start or stop bits; a
//! pulse is 150 us high then 150 us low; a `0` bit is 4 pulses and a `1` bit is 9 pulses, each followed by 1300 us of
//! silence. A file is a leader of silence, a name of 1-127 bytes in the ZX81 character set whose last byte has bit 7
//! set, then the data: memory from 4009h up to `E_LINE`-1 (`E_LINE` is the word at 4014h), i.e. the `.p` file.

use zx_tape::{Pulse, Tape};

/// The CPU clock the pulse lengths are converted with.
const CPU_HZ: u64 = 3_250_000;
const PULSE_US: u64 = 150;
const SILENCE_US: u64 = 1300;
/// The leader before the file: ZXDOCS lists 5 s, "only some clock cycles required for ZX81"; one second keeps a
/// typed `LOAD ""` quick without cutting it close.
const LEADER_MS: u32 = 1000;
/// Silence after the last bit.
const TRAILER_MS: u32 = 200;

/// Where the saved data sits in memory, and where `E_LINE` is in the data.
const FILE_START: u16 = 0x4009;
const E_LINE_OFFSET: usize = 0x4014 - 0x4009;
/// Longest name a tape file may have.
const MAX_NAME: usize = 127;

fn tstates(us: u64) -> u32 {
    (us * CPU_HZ / 1_000_000) as u32
}

/// The ZX81 character codes of `stem` for a tape name: letters and digits only (upper-cased, other characters
/// dropped), at most 127, or `A` when nothing is left. The last byte carries bit 7 as the tape format wants.
#[must_use]
pub fn name_from_stem(stem: &str) -> Vec<u8> {
    let mut name: Vec<u8> = stem
        .chars()
        .filter_map(|c| match c.to_ascii_uppercase() {
            c @ '0'..='9' => Some(0x1C + (c as u8 - b'0')),
            c @ 'A'..='Z' => Some(0x26 + (c as u8 - b'A')),
            _ => None,
        })
        .take(MAX_NAME)
        .collect();
    if name.is_empty() {
        name.push(0x26); // A
    }
    mark_last(&mut name);
    name
}

fn mark_last(name: &mut [u8]) {
    for b in name.iter_mut() {
        *b &= 0x7F;
    }
    if let Some(last) = name.last_mut() {
        *last |= 0x80;
    }
}

fn push_bit(pulses: &mut Vec<Pulse>, one: bool) {
    let half = tstates(PULSE_US);
    for _ in 0..if one { 9 } else { 4 } {
        pulses.push(Pulse {
            cycles: half,
            level: true,
        });
        pulses.push(Pulse {
            cycles: half,
            level: false,
        });
    }
    pulses.push(Pulse {
        cycles: tstates(SILENCE_US),
        level: false,
    });
}

/// The tape a real `SAVE` of `data` under `name` (ZX81 character codes; the last byte is marked here) would make.
#[must_use]
pub fn encode(name: &[u8], data: &[u8]) -> Tape {
    let mut name = name.to_vec();
    name.truncate(MAX_NAME);
    mark_last(&mut name);
    let mut pulses = Vec::new();
    for byte in name.iter().chain(data) {
        for bit in (0..8).rev() {
            push_bit(&mut pulses, byte >> bit & 1 == 1);
        }
    }
    pulses.push(Pulse {
        cycles: tstates(u64::from(TRAILER_MS) * 1000),
        level: false,
    });
    Tape::from_pulses(pulses).with_leading_silence(LEADER_MS)
}

/// A file read back from a recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The name, ZX81 character codes, the last byte with bit 7 set.
    pub name: Vec<u8>,
    /// Memory from 4009h up to `E_LINE`-1: the `.p` file.
    pub data: Vec<u8>,
}

/// Rising edges further apart than this start a new bit (inside a bit they are 300 us apart, between bits 1600 us).
const SAME_BIT_GAP: u64 = 2_600;
/// Bits further apart than this are not one file (between bits of a file the gap is 1600 us).
const SAME_FILE_GAP: u64 = 9_000;

/// Reads the first complete file out of the MIC output's edges (`(cycle, level)` at each change). Noise is skipped:
/// the display's one-pulse retrace signal and anything that is not 4 or 9 pulses long.
#[must_use]
pub fn decode(edges: &[(u64, bool)]) -> Option<Recorded> {
    // Rising edges, grouped into bursts.
    let rising: Vec<u64> = edges.iter().filter(|e| e.1).map(|e| e.0).collect();
    // (time of the first pulse, time of the last, pulses)
    let mut bursts: Vec<(u64, u64, u32)> = Vec::new();
    for &t in &rising {
        match bursts.last_mut() {
            Some(burst) if t - burst.1 <= SAME_BIT_GAP => {
                burst.1 = t;
                burst.2 += 1;
            }
            _ => bursts.push((t, t, 1)),
        }
    }
    // Runs of bit bursts (3 or more pulses) that follow each other closely enough.
    let mut run: Vec<(u64, u64, bool)> = Vec::new(); // (start, end, the bit)
    let mut result = None;
    let mut finish = |run: &mut Vec<(u64, u64, bool)>| {
        if result.is_none() {
            result = parse(&run.iter().map(|b| b.2).collect::<Vec<_>>());
        }
        run.clear();
    };
    for &(start, end, pulses) in &bursts {
        let is_bit = pulses >= 3;
        let continues = run
            .last()
            .is_none_or(|prev| start - prev.1 <= SAME_FILE_GAP);
        if !is_bit || !continues {
            finish(&mut run);
        }
        if is_bit {
            run.push((start, end, pulses >= 7));
        }
    }
    finish(&mut run);
    result
}

fn parse(bits: &[bool]) -> Option<Recorded> {
    let bytes: Vec<u8> = bits
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| c.iter().fold(0u8, |acc, &b| acc << 1 | u8::from(b)))
        .collect();
    let name_end = bytes.iter().take(MAX_NAME).position(|b| b & 0x80 != 0)?;
    let data = &bytes[name_end + 1..];
    let e_line = u16::from_le_bytes([*data.get(E_LINE_OFFSET)?, *data.get(E_LINE_OFFSET + 1)?]);
    let len = usize::from(e_line.checked_sub(FILE_START)?);
    Some(Recorded {
        name: bytes[..=name_end].to_vec(),
        data: data.get(..len)?.to_vec(),
    })
}

/// A recording of what the machine would put on tape, for tests: the edges `encode` plays, from time zero.
#[cfg(test)]
fn edges_of(tape_pulses: &[Pulse]) -> Vec<(u64, bool)> {
    let mut t = 0u64;
    let mut level = false;
    let mut edges = Vec::new();
    for p in tape_pulses {
        if p.level != level {
            edges.push((t, p.level));
            level = p.level;
        }
        t += u64::from(p.cycles);
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pulses `encode` makes for one bit.
    fn bit_pulses(one: bool) -> Vec<Pulse> {
        let mut v = Vec::new();
        push_bit(&mut v, one);
        v
    }

    #[test]
    fn a_zero_bit_is_4_pulses_and_a_one_bit_is_9_then_1300_us_of_silence() {
        // ZXDOCS: each pulse 150 us high + 150 us low; "0" = 4 pulses, "1" = 9 pulses; 1300 us silence after.
        let zero = bit_pulses(false);
        let one = bit_pulses(true);
        assert_eq!(zero.len(), 4 * 2 + 1);
        assert_eq!(one.len(), 9 * 2 + 1);
        let half = tstates(150);
        assert_eq!(half, 487); // 150 us at 3.25 MHz
        assert!(
            zero[..8]
                .iter()
                .enumerate()
                .all(|(i, p)| p.cycles == half && p.level == (i % 2 == 0))
        );
        let silence = zero.last().unwrap();
        assert_eq!((silence.cycles, silence.level), (tstates(1300), false));
        assert_eq!(tstates(1300), 4225);
    }

    #[test]
    fn a_name_is_the_ascii_letters_and_digits_with_bit_7_on_the_last_byte() {
        // ZX81 character set: digits 1Ch-25h, letters 26h-3Fh.
        assert_eq!(name_from_stem("a1"), vec![0x26, 0x1D | 0x80]);
        assert_eq!(
            name_from_stem("my game!"),
            vec![0x32, 0x3E, 0x2C, 0x26, 0x32, 0x2A | 0x80]
        );
        assert_eq!(name_from_stem("!!!"), vec![0x26 | 0x80], "falls back to A");
        assert_eq!(name_from_stem(&"x".repeat(300)).len(), 127);
    }

    /// A file whose `E_LINE` (offset 11) makes it `len` bytes long.
    fn file(len: usize) -> Vec<u8> {
        let mut data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
        data[11..13].copy_from_slice(&(0x4009 + len as u16).to_le_bytes());
        data
    }

    fn pulses_of(name: &[u8], data: &[u8]) -> Vec<Pulse> {
        // Rebuild the pulse list the way `encode` does, so the test can walk it without the private Tape fields.
        let mut name = name.to_vec();
        mark_last(&mut name);
        let mut pulses = Vec::new();
        for byte in name.iter().chain(data) {
            for bit in (0..8).rev() {
                push_bit(&mut pulses, byte >> bit & 1 == 1);
            }
        }
        pulses
    }

    #[test]
    fn what_encode_plays_decode_reads_back() {
        let data = file(40);
        let name = name_from_stem("demo");
        let edges = edges_of(&pulses_of(&name, &data));
        let got = decode(&edges).unwrap();
        assert_eq!(got.name, name);
        assert_eq!(got.data, data);
    }

    #[test]
    fn decode_skips_retrace_noise_before_the_file_and_ignores_what_follows() {
        let data = file(20);
        let name = name_from_stem("a");
        let mut pulses = Vec::new();
        // Three frames of the display's single retrace pulse (a 1-pulse burst every 20 ms), then the file, then more.
        for _ in 0..3 {
            pulses.push(Pulse {
                cycles: 487,
                level: true,
            });
            pulses.push(Pulse {
                cycles: 65_000,
                level: false,
            });
        }
        pulses.extend(pulses_of(&name, &data));
        pulses.push(Pulse {
            cycles: 487,
            level: true,
        });
        pulses.push(Pulse {
            cycles: 65_000,
            level: false,
        });
        let got = decode(&edges_of(&pulses)).unwrap();
        assert_eq!(got.data, data);
    }

    #[test]
    fn decode_gives_nothing_for_a_truncated_file_or_no_signal() {
        let data = file(40);
        let name = name_from_stem("demo");
        let mut pulses = pulses_of(&name, &data);
        pulses.truncate(pulses.len() / 2);
        assert_eq!(decode(&edges_of(&pulses)), None);
        assert_eq!(decode(&[]), None);
    }
}
