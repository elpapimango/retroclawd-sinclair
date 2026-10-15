/// One physical key on the ZX81's 40-key membrane keyboard, named rather
/// than exposed as a raw (row, bit) pair so a caller can't transpose the
/// hardware matrix table by hand.
///
/// Matrix verified against two independent hardware references (k1.spdns.de's
/// ZX81 technical specs and a ZX81-emulation writeup) and cross-checked
/// against this repo's own `IN A,(n)` implementation
/// (`z80::opcodes::base_stack_misc::in_a_n`, which builds
/// `port = (A << 8) | n`): a row is selected on the high byte of the port
/// address (A8..A15) when its bit is `0`; within a selected row, a pressed
/// key clears its bit (`0` = pressed). Selecting multiple rows in one read
/// ANDs them together -- this is how the real ROM detects e.g. SHIFT held
/// with another key. The matrix mechanism itself (not this row table, which
/// is ZX81-specific) lives in `sinclair_keyboard`, shared with `zx-spectrum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zx81Key {
    Shift,
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
    Dot,
    M,
    N,
    B,
}

impl Zx81Key {
    /// (row index 0-7, bit index 0-4) per the matrix table above.
    pub(crate) fn row_bit(self) -> (usize, u8) {
        use Zx81Key::*;
        match self {
            Shift => (0, 0),
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
            Dot => (7, 1),
            M => (7, 2),
            N => (7, 3),
            B => (7, 4),
        }
    }
}
