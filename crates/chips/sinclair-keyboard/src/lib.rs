//! The 8x5 keyboard matrix shared by Sinclair's ZX81 and ZX Spectrum: both
//! read the keyboard via `IN`, selecting one or more of 8 half-rows through
//! a `0` bit in the port's high byte, with a pressed key reading back as a
//! `0` bit within that row, and multiple selected rows ANDed together (this
//! is how the ROM detects e.g. a shift key held with another key). Verified
//! against real hardware references for both machines -- see `crates/zx81`
//! and `crates/zx-spectrum`'s own keyboard modules for their exact row
//! tables, which differ from each other even though the matrix mechanism
//! itself does not.
//!
//! This crate knows nothing about either machine's specific key layout --
//! each machine owns its own key enum and (row, bit) mapping, and drives
//! this matrix with plain `(row, bit)` pairs.

/// The keyboard matrix's live state: one byte per row, each bit `0` for a
/// currently-pressed key in that row (bits 5-7 unused, kept `1`). Starts
/// with every key released (`0xFF` per row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyboardMatrix {
    rows: [u8; 8],
}

impl KeyboardMatrix {
    pub fn new() -> Self {
        Self { rows: [0xFF; 8] }
    }

    /// Marks the key at `(row, bit)` pressed. `row` is 0-7, `bit` is 0-4.
    pub fn press(&mut self, row: usize, bit: u8) {
        self.rows[row] &= !(1 << bit);
    }

    /// Marks the key at `(row, bit)` released.
    pub fn release(&mut self, row: usize, bit: u8) {
        self.rows[row] |= 1 << bit;
    }

    /// Reads the matrix as real hardware would for the given `IN` port
    /// address: rows whose high-byte bit is `0` are ANDed together. No row
    /// selected (every high-byte bit `1`) reads back `0xFF`.
    pub fn read(&self, port: u16) -> u8 {
        let row_select = (port >> 8) as u8;
        let mut result = 0xFFu8;
        for (row, &bits) in self.rows.iter().enumerate() {
            if row_select & (1 << row) == 0 {
                result &= bits;
            }
        }
        result
    }
}

impl Default for KeyboardMatrix {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the port address that selects exactly the given rows (0-7),
    /// mirroring real `IN A,(n)`: the row-select mask lives in the high
    /// byte, one `0` bit per selected row.
    fn select_rows(rows: &[usize]) -> u16 {
        let mut mask: u8 = 0xFF;
        for &row in rows {
            mask &= !(1 << row);
        }
        (mask as u16) << 8
    }

    fn all_keys_start_released() {
        let matrix = KeyboardMatrix::new();
        for row in 0..8 {
            assert_eq!(matrix.read(select_rows(&[row])), 0xFF);
        }
    }

    fn pressing_a_key_clears_its_bit_in_its_row_only() {
        let mut matrix = KeyboardMatrix::new();
        matrix.press(2, 0); // row 2, bit 0

        assert_eq!(matrix.read(select_rows(&[2])), 0b1111_1110);
        assert_eq!(matrix.read(select_rows(&[1])), 0xFF);
    }

    fn releasing_a_key_sets_its_bit_back() {
        let mut matrix = KeyboardMatrix::new();
        matrix.press(6, 0);
        matrix.release(6, 0);

        assert_eq!(matrix.read(select_rows(&[6])), 0xFF);
    }
    /// Merged (every check kept): `pressing_a_key_clears_its_bit_in_its_row_only`, `releasing_a_key_sets_its_bit_back`.
    #[test]
    fn press_and_release() {
        pressing_a_key_clears_its_bit_in_its_row_only();
        releasing_a_key_sets_its_bit_back();
    }

    #[test]
    fn selecting_multiple_rows_ands_them_together() {
        let mut matrix = KeyboardMatrix::new();
        matrix.press(0, 0);
        matrix.press(1, 0);

        assert_eq!(matrix.read(select_rows(&[0, 1])), 0b1111_1110);
    }

    fn no_row_selected_reads_all_released() {
        let mut matrix = KeyboardMatrix::new();
        matrix.press(7, 0);

        assert_eq!(matrix.read(select_rows(&[])), 0xFF);
    }
    /// Merged (every check kept): `all_keys_start_released`, `no_row_selected_reads_all_released`.
    #[test]
    fn released_reads() {
        all_keys_start_released();
        no_row_selected_reads_all_released();
    }
}
