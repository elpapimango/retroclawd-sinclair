use crate::bus::ZxSpectrumBus;

/// Real ZX Spectrum resolution: 256x192 pixels (32x24 character cells of
/// 8x8 pixels each).
pub(crate) const SCREEN_WIDTH: u32 = 256;
pub(crate) const SCREEN_HEIGHT: u32 = 192;

const PIXEL_BASE: u16 = 0x4000;
const ATTR_BASE: u16 = 0x5800;

/// Border thickness on every side when rendering with a border. A commonly
/// used convention among Spectrum emulators (not a broadcast-accurate PAL/
/// NTSC measurement -- real vertical border size differs between the two
/// since PAL is 312 scanlines/frame and NTSC is 262, which this does not
/// model; only `frame_hz`/`clock_hz` vary by region here, border size does
/// not).
pub(crate) const BORDER_PX: u32 = 32;

/// RGB component values for the 8-color Spectrum palette. There is no
/// single canonical 8-bit value since real hardware outputs analog video --
/// these are a commonly-used convention (0 off, 0xCD normal, 0xFF BRIGHT),
/// not an asserted hardware fact.
const NORMAL_LEVEL: u8 = 0xCD;
const BRIGHT_LEVEL: u8 = 0xFF;

/// Converts a pixel row (0-191) and byte-column (0-31) into its address in
/// the pixel bitmap (0x4000-0x57FF). Verified against two independent
/// hardware references: the address bit-interleaves the row's bits as
/// `0b010_Y7Y6_Y2Y1Y0_Y5Y4Y3_col` (fixed `010` selects the 0x4000 base;
/// Y7Y6 picks one of 3 "thirds" of the screen; Y2Y1Y0 is the pixel line
/// within a character cell; Y5Y4Y3 is the character row within that third)
/// -- an optimization for fast character printing (incrementing the high
/// byte alone moves to the next scanline within a cell), not something
/// anyone would design from scratch.
pub(crate) fn pixel_address(row: u16, col: u16) -> u16 {
    let third = (row >> 6) & 0b11;
    let char_row = (row >> 3) & 0b111;
    let pixel_line = row & 0b111;
    PIXEL_BASE | (third << 11) | (pixel_line << 8) | (char_row << 5) | col
}

/// Converts a character row (0-23) and column (0-31) into its address in
/// the attribute memory (0x5800-0x5AFF): `0x5800 + char_row*32 + col`, one
/// byte per 8x8 cell.
pub(crate) fn attr_address(char_row: u16, col: u16) -> u16 {
    ATTR_BASE + char_row * 32 + col
}

/// How many real interrupts (video frames) the ULA's FLASH counter takes to
/// toggle -- a fixed hardware rate, not ROM-configurable, so there's no
/// disassembly to verify it against the way `BORDER_PX`/`Zx81`'s NMI period
/// could be cross-checked against real ROM/hardware usage. Commonly cited
/// as 16 frames (~0.32s at 50Hz PAL, a ~1.56Hz full on/off cycle) across
/// independent Spectrum hardware references; treated as a fixed constant
/// here as-is, not derived from anything this codebase can independently
/// confirm.
pub(crate) const FLASH_TOGGLE_FRAMES: u8 = 16;

/// Decodes one attribute byte's ink/paper/bright components, swapping
/// ink/paper when the attribute's FLASH bit (bit 7) is set and `flash_phase`
/// is the "on" half of the blink cycle -- `flash_phase` is `ZxSpectrum`'s
/// own frame counter (see `FLASH_TOGGLE_FRAMES`), ticked once per real
/// interrupt, not derived from anything in this function.
fn color_rgb(color_index: u8, bright: bool) -> [u8; 3] {
    let level = if bright { BRIGHT_LEVEL } else { NORMAL_LEVEL };
    let (r, g, b) = match color_index & 0b111 {
        0 => (0, 0, 0), // black
        1 => (0, 0, 1), // blue
        2 => (1, 0, 0), // red
        3 => (1, 0, 1), // magenta
        4 => (0, 1, 0), // green
        5 => (0, 1, 1), // cyan
        6 => (1, 1, 0), // yellow
        _ => (1, 1, 1), // white
    };
    [r * level, g * level, b * level]
}

fn draw_cell(rgb: &mut [u8], bus: &ZxSpectrumBus, char_row: u16, col: u16, flash_phase: bool) {
    // `peek_screen`, not `peek`: on 128K, the ULA reads whichever of RAM
    // bank 5/7 is currently selected for video directly, independent of
    // what's paged into the CPU's own 0xC000 slot (see its doc comment).
    // Identical to `peek` on 48K.
    let attr = bus.peek_screen(attr_address(char_row, col));
    let bright = attr & 0x40 != 0;
    let flash = attr & 0x80 != 0;
    let (paper, ink) = if flash && flash_phase {
        (attr & 0b111, (attr >> 3) & 0b111) // swapped
    } else {
        ((attr >> 3) & 0b111, attr & 0b111)
    };

    for line in 0..8u16 {
        let row = char_row * 8 + line;
        let byte = bus.peek_screen(pixel_address(row, col));
        for bit in 0..8u8 {
            let is_ink = (byte >> (7 - bit)) & 1 == 1;
            let color = color_rgb(if is_ink { ink } else { paper }, bright);
            let px = col as usize * 8 + bit as usize;
            let py = row as usize;
            let offset = (py * SCREEN_WIDTH as usize + px) * 3;
            rgb[offset] = color[0];
            rgb[offset + 1] = color[1];
            rgb[offset + 2] = color[2];
        }
    }
}

/// Renders one video frame from the machine's current state: for each of
/// the 32x24 character cells, reads its attribute byte (ink/paper/bright/
/// flash) and its 8 pixel-bitmap bytes (one per scanline), and composites
/// into a 256x192 RGB buffer. `flash_phase` is `ZxSpectrum`'s own frame
/// counter (see `FLASH_TOGGLE_FRAMES`) -- cells with the FLASH attribute
/// bit set swap ink/paper while it's the "on" half of the cycle. When
/// `border` is true, pads that buffer with `BORDER_PX` pixels of the
/// current border color on every side (see `ZxSpectrumBus::border_color`),
/// matching the real hardware's active picture sitting inside a larger
/// border area.
pub(crate) fn render_frame(
    bus: &ZxSpectrumBus,
    border: bool,
    flash_phase: bool,
) -> (u32, u32, Vec<u8>) {
    let mut rgb = vec![0u8; SCREEN_WIDTH as usize * SCREEN_HEIGHT as usize * 3];
    for char_row in 0..24u16 {
        for col in 0..32u16 {
            draw_cell(&mut rgb, bus, char_row, col, flash_phase);
        }
    }

    if border {
        let border_rgb = color_rgb(bus.border_color(), false);
        machine::with_border(
            SCREEN_WIDTH,
            SCREEN_HEIGHT,
            &rgb,
            BORDER_PX,
            BORDER_PX,
            border_rgb,
        )
    } else {
        (SCREEN_WIDTH, SCREEN_HEIGHT, rgb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Model;

    fn test_bus() -> ZxSpectrumBus {
        ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE])
    }

    #[test]
    fn pixel_address_matches_the_verified_interleaved_formula() {
        // row=0, col=0: third=0, char_row=0, pixel_line=0 -> 0x4000.
        assert_eq!(pixel_address(0, 0), 0x4000);
        // row=1 (pixel_line=1): 0x4000 | (1 << 8) = 0x4100.
        assert_eq!(pixel_address(1, 0), 0x4100);
        // row=8 (char_row=1, pixel_line=0): 0x4000 | (1 << 5) = 0x4020.
        assert_eq!(pixel_address(8, 0), 0x4020);
        // row=64 (third=1, char_row=0, pixel_line=0): 0x4000 | (1 << 11) = 0x4800.
        assert_eq!(pixel_address(64, 0), 0x4800);
        // col=1 at row=0: 0x4000 | 1 = 0x4001.
        assert_eq!(pixel_address(0, 1), 0x4001);
        // Last valid address: row=191, col=31.
        // third=2 (191>>6=2), char_row=7 (191>>3&7=7), pixel_line=7 (191&7=7).
        assert_eq!(
            pixel_address(191, 31),
            0x4000 | (2 << 11) | (7 << 8) | (7 << 5) | 31
        );
    }

    #[test]
    fn attr_address_matches_the_verified_formula() {
        assert_eq!(attr_address(0, 0), 0x5800);
        assert_eq!(attr_address(0, 1), 0x5801);
        assert_eq!(attr_address(1, 0), 0x5820);
        assert_eq!(attr_address(23, 31), 0x5800 + 23 * 32 + 31);
    }

    #[test]
    fn ink_pixel_uses_ink_color_and_paper_pixel_uses_paper_color() {
        let mut bus = test_bus();
        // Attribute at cell (0,0): ink=red(2), paper=white(7), not bright.
        // 0b0_111_010 = bright=0, paper=111(white), ink=010(red).
        bus.load(0x5800, &[0b0_111_010]);
        // Pixel byte for row 0: leftmost 4 pixels ink, rest paper.
        bus.load(0x4000, &[0b1111_0000]);

        let (_, _, rgb) = render_frame(&bus, false, false);

        // Column 0-3: ink (red, normal level).
        for px in 0..4 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[0xCD, 0, 0]);
        }
        // Column 4-7: paper (white, normal level).
        for px in 4..8 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[0xCD, 0xCD, 0xCD]);
        }
    }

    #[test]
    fn bright_attribute_uses_the_bright_color_level() {
        let mut bus = test_bus();
        // bright=1, paper=black(0), ink=cyan(5): 0b1_000_101.
        bus.load(0x5800, &[0b1_000_101]);
        bus.load(0x4000, &[0xFF]); // all 8 pixels ink

        let (_, _, rgb) = render_frame(&bus, false, false);

        for px in 0..8 {
            let offset = px * 3;
            assert_eq!(&rgb[offset..offset + 3], &[0, 0xFF, 0xFF]);
        }
    }

    fn flash_bit_swaps_ink_and_paper_only_during_the_on_phase() {
        let mut bus = test_bus();
        // flash=1, bright=0, paper=black(0), ink=green(4): 0b1_0_000_100.
        bus.load(0x5800, &[0b1000_0100]);
        bus.load(0x4000, &[0xFF]); // all 8 pixels "ink" per the bitmap

        let (_, _, off) = render_frame(&bus, false, false);
        let (_, _, on) = render_frame(&bus, false, true);

        // Off phase: normal ink (green) on all 8 pixels, same as no flash.
        for px in 0..8 {
            let offset = px * 3;
            assert_eq!(&off[offset..offset + 3], &[0, 0xCD, 0]);
        }
        // On phase: swapped -- now paper (green) shows where ink was
        // requested and vice versa; since the whole cell is "ink" bits,
        // swapping ink<->paper here means every pixel becomes the
        // (originally paper, now ink) color: black.
        for px in 0..8 {
            let offset = px * 3;
            assert_eq!(&on[offset..offset + 3], &[0, 0, 0]);
        }
    }

    fn non_flash_cells_are_unaffected_by_flash_phase() {
        let mut bus = test_bus();
        bus.load(0x5800, &[0b0_000_100]); // flash=0, paper=black, ink=green
        bus.load(0x4000, &[0xFF]);

        assert_eq!(
            render_frame(&bus, false, false).2,
            render_frame(&bus, false, true).2
        );
    }
    /// Merged (every check kept): `flash_bit_swaps_ink_and_paper_only_during_the_on_phase`, `non_flash_cells_are_unaffected_by_flash_phase`.
    #[test]
    fn flash_cells() {
        flash_bit_swaps_ink_and_paper_only_during_the_on_phase();
        non_flash_cells_are_unaffected_by_flash_phase();
    }

    fn border_false_returns_the_original_256x192_dimensions() {
        let bus = test_bus();

        let (w, h, rgb) = render_frame(&bus, false, false);

        assert_eq!((w, h), (SCREEN_WIDTH, SCREEN_HEIGHT));
        assert_eq!(rgb.len(), (SCREEN_WIDTH * SCREEN_HEIGHT * 3) as usize);
    }

    fn border_true_pads_by_border_px_and_paints_the_current_border_color() {
        use machine::Bus;

        let mut bus = test_bus();
        bus.write_io(0x00FE, 0b010); // OUT (0xFE),2 -- border = red

        let (w, h, rgb) = render_frame(&bus, true, false);

        assert_eq!(w, SCREEN_WIDTH + BORDER_PX * 2);
        assert_eq!(h, SCREEN_HEIGHT + BORDER_PX * 2);
        // Top-left corner pixel is deep in the border area -- red, normal
        // (not BRIGHT) level, matching color_rgb(2, false).
        assert_eq!(&rgb[0..3], &[0xCD, 0, 0]);
    }

    fn border_color_only_uses_low_3_bits_of_the_written_value() {
        use machine::Bus;

        let mut bus = test_bus();
        // Bits 3+ (MIC/speaker on real hardware) must not leak into color.
        bus.write_io(0x00FE, 0b1111_1101); // low 3 bits = 5 (cyan)

        assert_eq!(bus.border_color(), 0b101);
    }
    /// Merged (every check kept): `border_false_returns_the_original_256x192_dimensions`, `border_true_pads_by_border_px_and_paints_the_current_border_color`, `border_color_only_uses_low_3_bits_of_the_written_value`.
    #[test]
    fn border_rendering() {
        border_false_returns_the_original_256x192_dimensions();
        border_true_pads_by_border_px_and_paints_the_current_border_color();
        border_color_only_uses_low_3_bits_of_the_written_value();
    }
}
