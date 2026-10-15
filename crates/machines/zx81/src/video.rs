use crate::bus::Zx81Bus;

/// The `D_FILE` system variable: a 16-bit little-endian pointer (at this
/// fixed RAM address on every ZX81) to the start of the display file.
const DISPLAY_FILE_POINTER_ADDR: u16 = 0x400C;
/// The display file's row terminator -- coincidentally the same value as
/// the `HALT` opcode, which is unrelated; this is a real, documented ZX81
/// display-file format byte, not a bug.
const NEWLINE: u8 = 0x76;
/// Real ZX81 screen: 32 columns x 24 rows of 8x8-pixel characters.
const COLUMNS: usize = 32;
const ROWS: usize = 24;
const CHAR_SIZE: usize = 8;

pub(crate) const SCREEN_WIDTH: u32 = (COLUMNS * CHAR_SIZE) as u32;
pub(crate) const SCREEN_HEIGHT: u32 = (ROWS * CHAR_SIZE) as u32;

/// Border thickness on every side when rendering with a border. Same
/// convention/caveat as `zx_spectrum::video::BORDER_PX`: a commonly used
/// emulator convention, not a broadcast-accurate PAL/NTSC measurement --
/// real vertical border size differs between the two, not modeled here.
/// Unlike ZX Spectrum, real ZX81 hardware has no software-controlled
/// border color register -- the border is simply unmodulated video output,
/// which is white here to match the ROM's own white "paper" convention
/// (see `color_rgb` below and `crate::video`'s ink/paper handling).
pub(crate) const BORDER_PX: u32 = 32;
const BORDER_RGB: [u8; 3] = [255, 255, 255];

/// Reads up to `ROWS` rows of character codes from the display file at
/// `start`, splitting on `NEWLINE`. Each row is capped at `COLUMNS` bytes
/// even if no `NEWLINE` appears -- an uninitialized or garbage `D_FILE`
/// pointer must not be able to grow a row (or the row count) past what the
/// fixed-size pixel buffer in `render_frame` can hold, which would panic on
/// an out-of-bounds write rather than just look wrong.
///
/// `start` (the `D_FILE` pointer) always addresses a leading sentinel
/// `NEWLINE` that is not itself a displayable row -- confirmed via real ROM
/// disassembly: the display-file init routine (ROM 0x0403) does `LD B,0x19`
/// (25) then writes 25 `NEWLINE` bytes starting AT the pointer address
/// before returning, i.e. one sentinel plus the 24 real rows. Skipping it
/// here is what makes the 24 rows decoded below the 24 real screen rows --
/// without this, the loop wastes its first row slot on the empty sentinel
/// and comes up one real row short, which silently swallows the last
/// (bottom) row: exactly where the edit cursor sits once that row has grown
/// past its collapsed single-byte form.
fn decode_rows(bus: &Zx81Bus, start: u16) -> Vec<Vec<u8>> {
    let mut rows = Vec::with_capacity(ROWS);
    let mut addr = start;
    if bus.peek(addr) == NEWLINE {
        addr = addr.wrapping_add(1);
    }
    for _ in 0..ROWS {
        let mut row = Vec::new();
        for _ in 0..COLUMNS {
            let byte = bus.peek(addr);
            addr = addr.wrapping_add(1);
            if byte == NEWLINE {
                break;
            }
            row.push(byte);
        }
        rows.push(row);
    }
    rows
}

/// The character generator table lives inside the already-loaded ROM: on
/// real hardware the ROM sets `I = 0x1E` at boot and its own display
/// routine computes each character's pixel-row byte address as
/// `0x1E00 + (char_code * 8) + scanline_row` -- this is the actual ZX81
/// hardware mechanism (the Z80's own instruction-refresh addressing,
/// repurposed by the ROM as a font lookup), verified against a real ZX81
/// ROM disassembly, not an approximation. Only valid for `code < 64` --
/// codes 64-127 (BASIC keyword tokens) have no real bitmap here.
const CHAR_GENERATOR_BASE: u16 = 0x1E00;

fn font_row(bus: &Zx81Bus, code: u8, scanline_row: usize) -> u8 {
    let addr = CHAR_GENERATOR_BASE + (code as u16) * CHAR_SIZE as u16 + scanline_row as u16;
    bus.peek(addr)
}

/// Renders one video frame from the machine's current state: decodes the
/// display file into character codes, looks up each one's real font
/// bitmap, and composites into a 256x192 RGB buffer. Codes 0-63 use their
/// real ROM bitmap; 128-191 are the same bitmap with ink/paper swapped
/// (inverse video); 64-127 and 192-255 (BASIC keyword tokens, which
/// shouldn't appear in a real display file but could via a raw `POKE`)
/// have no real bitmap and render blank -- deliberately not swapped even
/// when the inverse bit is set, since an inverse-video blank is still
/// blank.
pub(crate) fn render_frame(bus: &Zx81Bus, border: bool) -> (u32, u32, Vec<u8>) {
    let pointer_lo = bus.peek(DISPLAY_FILE_POINTER_ADDR) as u16;
    let pointer_hi = bus.peek(DISPLAY_FILE_POINTER_ADDR.wrapping_add(1)) as u16;
    let pointer = (pointer_hi << 8) | pointer_lo;

    let rows = decode_rows(bus, pointer);

    let mut rgb = vec![255u8; SCREEN_WIDTH as usize * SCREEN_HEIGHT as usize * 3];
    for (row_index, row) in rows.iter().enumerate() {
        for (col_index, &code) in row.iter().enumerate() {
            draw_char(&mut rgb, bus, row_index, col_index, code);
        }
    }

    if border {
        machine::with_border(
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &rgb,
            BORDER_PX,
            BORDER_PX,
            BORDER_RGB,
        )
    } else {
        (SCREEN_WIDTH, SCREEN_HEIGHT, rgb)
    }
}

fn draw_char(rgb: &mut [u8], bus: &Zx81Bus, row_index: usize, col_index: usize, code: u8) {
    let glyph = code & 0x7F;
    let has_real_bitmap = glyph < 64;
    let inverse = has_real_bitmap && (code & 0x80 != 0);

    for line in 0..CHAR_SIZE {
        let bits = if has_real_bitmap {
            font_row(bus, glyph, line)
        } else {
            0
        };
        for bit in 0..CHAR_SIZE {
            let mut ink = (bits >> (7 - bit)) & 1 == 1;
            if inverse {
                ink = !ink;
            }
            let value = if ink { 0u8 } else { 255u8 };
            let px = col_index * CHAR_SIZE + bit;
            let py = row_index * CHAR_SIZE + line;
            let offset = (py * SCREEN_WIDTH as usize + px) * 3;
            rgb[offset] = value;
            rgb[offset + 1] = value;
            rgb[offset + 2] = value;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine::MemoryBus;

    fn test_bus_with_rom(rom: Vec<u8>) -> Zx81Bus {
        let mut mem = MemoryBus::new();
        mem.map_rom(0x0000, rom);
        mem.map_ram(crate::RAM_BASE, crate::RAM_SIZE);
        Zx81Bus::new(mem)
    }

    fn test_bus() -> Zx81Bus {
        test_bus_with_rom(vec![0u8; crate::ROM_SIZE])
    }

    #[test]
    fn decode_rows_splits_on_newline_and_returns_24_rows() {
        let mut bus = test_bus();
        // Row 0: one character (code 1) then NEWLINE. Rows 1-23: empty
        // (collapsed) rows -- an immediate NEWLINE each.
        let mut d_file = vec![1u8, NEWLINE];
        d_file.extend(std::iter::repeat_n(NEWLINE, 23));
        bus.load(0x4100, &d_file);

        let rows = decode_rows(&bus, 0x4100);

        assert_eq!(rows.len(), 24);
        assert_eq!(rows[0], vec![1]);
        assert_eq!(rows[1], Vec::<u8>::new());
    }

    #[test]
    fn decode_rows_caps_a_row_at_32_bytes_when_no_newline_appears() {
        let mut bus = test_bus();
        // 40 bytes of character code 1, no terminator anywhere.
        bus.load(0x4100, &[1u8; 40]);

        let rows = decode_rows(&bus, 0x4100);

        assert_eq!(rows[0].len(), 32);
    }

    #[test]
    fn decode_rows_skips_the_leading_sentinel_newline_so_all_24_real_rows_are_read() {
        let mut bus = test_bus();
        // Real ZX81 D_FILE format, per the ROM's own init routine (0x0403):
        // a leading sentinel NEWLINE, then 24 real rows. Row 23 (the last)
        // has grown past its collapsed single-NEWLINE form to hold one
        // character -- exactly what happens on real hardware when the edit
        // cursor sits in the bottom row.
        let mut d_file = vec![NEWLINE]; // sentinel -- not a row
        d_file.extend(std::iter::repeat_n(NEWLINE, 23)); // rows 0-22: blank
        d_file.push(0x80); // row 23: cursor byte
        d_file.push(NEWLINE);
        bus.load(0x4100, &d_file);

        let rows = decode_rows(&bus, 0x4100);

        assert_eq!(rows.len(), 24);
        assert_eq!(
            rows[23],
            vec![0x80],
            "cursor row must not be swallowed by the sentinel"
        );
    }

    #[test]
    fn decode_rows_caps_at_24_rows() {
        let mut bus = test_bus();
        // 30 collapsed (immediately-terminated) rows.
        bus.load(0x4100, &[NEWLINE; 30]);

        let rows = decode_rows(&bus, 0x4100);

        assert_eq!(rows.len(), 24);
    }

    #[test]
    fn font_row_reads_the_real_rom_character_generator_table() {
        let mut rom = vec![0u8; crate::ROM_SIZE];
        // Character code 5, scanline 3: 0x1E00 + 5*8 + 3 = 0x1E2B.
        rom[0x1E2B] = 0b1010_0101;
        let bus = test_bus_with_rom(rom);

        assert_eq!(font_row(&bus, 5, 3), 0b1010_0101);
    }

    #[test]
    fn blank_display_file_renders_an_all_white_frame() {
        let mut bus = test_bus();
        bus.load(0x400C, &[0x00, 0x41]); // D_FILE pointer -> 0x4100
        bus.load(0x4100, &[NEWLINE; 24]); // 24 collapsed (blank) rows

        let (width, height, rgb) = render_frame(&bus, false);

        assert_eq!((width, height), (SCREEN_WIDTH, SCREEN_HEIGHT));
        assert!(rgb.iter().all(|&b| b == 255));
    }

    #[test]
    fn border_true_pads_by_border_px_on_every_side() {
        let mut bus = test_bus();
        bus.load(0x400C, &[0x00, 0x41]);
        bus.load(0x4100, &[NEWLINE; 24]);

        let (width, height, rgb) = render_frame(&bus, true);

        assert_eq!(width, SCREEN_WIDTH + BORDER_PX * 2);
        assert_eq!(height, SCREEN_HEIGHT + BORDER_PX * 2);
        // ZX81 has no border-color register -- always white, same as paper.
        assert!(rgb.iter().all(|&b| b == 255));
    }

    #[test]
    fn known_character_renders_its_real_rom_font_bitmap() {
        let mut rom = vec![0u8; crate::ROM_SIZE];
        // Character code 1, scanline 0: 0x1E00 + 1*8 + 0 = 0x1E08.
        rom[0x1E08] = 0b1111_0000;
        let mut bus = test_bus_with_rom(rom);
        bus.load(0x400C, &[0x00, 0x41]);
        let mut d_file = vec![1u8, NEWLINE];
        d_file.extend(std::iter::repeat_n(NEWLINE, 23));
        bus.load(0x4100, &d_file);

        let (_, _, rgb) = render_frame(&bus, false);

        // Row 0, columns 0-3 (the character's top scanline, left nibble): ink.
        for px in 0..4 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[0, 0, 0]);
        }
        // Columns 4-7 (right nibble): paper.
        for px in 4..8 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[255, 255, 255]);
        }
    }

    #[test]
    fn inverse_video_character_swaps_ink_and_paper() {
        let mut rom = vec![0u8; crate::ROM_SIZE];
        rom[0x1E08] = 0b1111_0000; // same font byte as the previous test
        let mut bus = test_bus_with_rom(rom);
        bus.load(0x400C, &[0x00, 0x41]);
        let mut d_file = vec![1u8 + 128, NEWLINE]; // inverse video: code 129
        d_file.extend(std::iter::repeat_n(NEWLINE, 23));
        bus.load(0x4100, &d_file);

        let (_, _, rgb) = render_frame(&bus, false);

        // Swapped versus the non-inverse test: columns 0-3 now paper.
        for px in 0..4 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[255, 255, 255]);
        }
        for px in 4..8 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[0, 0, 0]);
        }
    }

    #[test]
    fn token_range_character_renders_blank_even_when_inverse() {
        let mut bus = test_bus();
        bus.load(0x400C, &[0x00, 0x41]);
        // 200 & 0x7F = 72, which falls in the 64-127 token range.
        let mut d_file = vec![200u8, NEWLINE];
        d_file.extend(std::iter::repeat_n(NEWLINE, 23));
        bus.load(0x4100, &d_file);

        let (_, _, rgb) = render_frame(&bus, false);

        for px in 0..8 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[255, 255, 255]);
        }
    }

    #[test]
    fn overlong_row_and_row_count_do_not_panic_or_grow_the_buffer() {
        let mut rom = vec![0u8; crate::ROM_SIZE];
        rom[0x1E08] = 0xFF;
        let mut bus = test_bus_with_rom(rom);
        bus.load(0x400C, &[0x00, 0x41]);
        // 40 bytes of character code 1 with no terminator anywhere -- if
        // either safety cap were missing, the pixel-offset math in
        // `draw_char` could compute an out-of-bounds index into `rgb` and
        // panic instead of just producing a bounded, if visually
        // truncated, frame.
        bus.load(0x4100, &[1u8; 40]);

        let (width, height, rgb) = render_frame(&bus, false);

        assert_eq!((width, height), (SCREEN_WIDTH, SCREEN_HEIGHT));
        assert_eq!(rgb.len(), (SCREEN_WIDTH * SCREEN_HEIGHT * 3) as usize);
    }
}
