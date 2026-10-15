/// One physical key on the ZX Spectrum's 40-key keyboard, named rather than
/// exposed as a raw (row, bit) pair so a caller can't transpose the
/// hardware matrix table by hand.
///
/// Matrix verified against sinclair.wiki.zxnet.co.uk's Keyboard and ZX
/// Spectrum ULA pages: a row is selected on the high byte of the `IN` port
/// address (A8..A15) when its bit is `0`; within a selected row, a pressed
/// key clears its bit (`0` = pressed, bit 0 = the key nearest the row's
/// outside edge, bit 4 = nearest the middle). Selecting multiple rows in
/// one read ANDs them together. Differs from ZX81's matrix in exactly two
/// keys despite the otherwise-identical layout: row 0's outside key is
/// CAPS SHIFT (not ZX81's single SHIFT), and row 7 has SYM SHIFT where
/// ZX81 has `.`. The matrix mechanism itself lives in `sinclair_keyboard`,
/// shared with `zx81`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZxSpectrumKey {
    CapsShift,
    Z,
    X,
    C,
    V,
    A,
    S,
    D,
    F,
    G,
    Q,
    W,
    E,
    R,
    T,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit0,
    Digit9,
    Digit8,
    Digit7,
    Digit6,
    P,
    O,
    I,
    U,
    Y,
    Enter,
    L,
    K,
    J,
    H,
    Space,
    SymShift,
    M,
    N,
    B,
}

impl ZxSpectrumKey {
    /// (row index 0-7, bit index 0-4) per the matrix table above.
    pub(crate) fn row_bit(self) -> (usize, u8) {
        use ZxSpectrumKey::*;
        match self {
            CapsShift => (0, 0),
            Z => (0, 1),
            X => (0, 2),
            C => (0, 3),
            V => (0, 4),
            A => (1, 0),
            S => (1, 1),
            D => (1, 2),
            F => (1, 3),
            G => (1, 4),
            Q => (2, 0),
            W => (2, 1),
            E => (2, 2),
            R => (2, 3),
            T => (2, 4),
            Digit1 => (3, 0),
            Digit2 => (3, 1),
            Digit3 => (3, 2),
            Digit4 => (3, 3),
            Digit5 => (3, 4),
            Digit0 => (4, 0),
            Digit9 => (4, 1),
            Digit8 => (4, 2),
            Digit7 => (4, 3),
            Digit6 => (4, 4),
            P => (5, 0),
            O => (5, 1),
            I => (5, 2),
            U => (5, 3),
            Y => (5, 4),
            Enter => (6, 0),
            L => (6, 1),
            K => (6, 2),
            J => (6, 3),
            H => (6, 4),
            Space => (7, 0),
            SymShift => (7, 1),
            M => (7, 2),
            N => (7, 3),
            B => (7, 4),
        }
    }
}
