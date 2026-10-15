use machine::{Bus, MemoryBus};
use sinclair_keyboard::KeyboardMatrix;

use crate::Model;
use crate::keyboard::ZxSpectrumKey;
use crate::video;

/// Recent EAR-line history samples (`0` low / `1` high) for the tape
/// overlay's waveform bar -- same ring-buffer size/shape as `cpc::bus`'s
/// own `TAPE_WAVEFORM_SAMPLES`, sampled at a fixed cycle interval
/// (`TAPE_WAVEFORM_SAMPLE_INTERVAL_CYCLES`), not once per CPU
/// instruction, so consecutive characters represent equal real time and
/// a run of repeated characters' length is directly comparable to a real
/// pulse's width.
const TAPE_WAVEFORM_SAMPLES: usize = 128;
/// One sample roughly every 1/9th of a real bit-0 pulse (855 T-states) --
/// enough resolution that a genuine pulse shows up as several repeated
/// bar characters, not a single ambiguous one. ZX Spectrum's own clock is
/// already the `zx-tape` reference clock domain, unlike CPC, so this is a
/// plain T-state count with no cross-domain scaling.
const TAPE_WAVEFORM_SAMPLE_INTERVAL_CYCLES: u32 = 96;
/// Recent completed EAR-line pulse widths, in T-states, for the tape
/// overlay's numeric pulse list -- see `cpc::bus`'s identical
/// `TAPE_PULSE_WIDTH_SAMPLES` for the diagnostic reasoning (compare
/// against the standard ROM protocol's known-good widths: pilot ~2168T,
/// sync ~667/735T, bit0 ~855T, bit1 ~1710T).
const TAPE_PULSE_WIDTH_SAMPLES: usize = 16;

/// The 5 real Kempston joystick inputs -- port `0x1F`'s bit layout
/// (`000FUDLR`, active high, idle `0`), per the Kempston interface's own
/// documented behavior (see `CREDITS.md`). Fire/directions are independent
/// bits, held down until released, same "press/release, no debounce" shape
/// as `ZxSpectrumKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KempstonButton {
    Right,
    Left,
    Down,
    Up,
    Fire,
}

impl KempstonButton {
    const fn bit(self) -> u8 {
        match self {
            KempstonButton::Right => 0b0000_0001,
            KempstonButton::Left => 0b0000_0010,
            KempstonButton::Down => 0b0000_0100,
            KempstonButton::Up => 0b0000_1000,
            KempstonButton::Fire => 0b0001_0000,
        }
    }
}

/// The 5 real Fuller Box joystick inputs -- port `0x7f`'s bit layout,
/// World of Spectrum's own peripherals reference (see `CREDITS.md`),
/// quoted directly: "Results were obtained by reading from port 0x7f in
/// the form F---RLDU, with active bits low." So bit 7 = Fire, bits 3/2/
/// 1/0 = Right/Left/Down/Up, bits 6-4 unused, and (unlike Kempston)
/// **active low** -- a pressed input clears its bit, idle reads all-1s.
/// Address-decode precision beyond the literal port `0x7f` (e.g. whether
/// real hardware mirrors across other addresses the way Kempston's own
/// "usually only A5" does) isn't confirmed by any source found for this
/// -- `read_io`/`write_io` below match the literal byte only, the
/// conservative reading of what's actually documented, not a guessed
/// wider decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullerButton {
    Right,
    Left,
    Down,
    Up,
    Fire,
}

impl FullerButton {
    const fn bit(self) -> u8 {
        match self {
            FullerButton::Right => 0b0000_1000,
            FullerButton::Left => 0b0000_0100,
            FullerButton::Down => 0b0000_0010,
            FullerButton::Up => 0b0000_0001,
            FullerButton::Fire => 0b1000_0000,
        }
    }
}

/// The 3 real Kempston mouse buttons -- port `0xfadf`'s own bit layout
/// (bit0=Right, bit1=Left, bit2=Middle -- see `CREDITS.md`; a real "fourth
/// button" bit exists on some compatibles but isn't modeled here, no host
/// input source for it). Held down until released, same shape as
/// `KempstonButton`/`FullerButton`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Right,
    Left,
    Middle,
}

impl MouseButton {
    const fn bit(self) -> u8 {
        match self {
            MouseButton::Right => 0b0000_0001,
            MouseButton::Left => 0b0000_0010,
            MouseButton::Middle => 0b0000_0100,
        }
    }
}

/// Real 128K paging state -- the last value `OUT` to port `0x7ffd` (or its
/// mirrors), decoded bit-for-bit. Has no effect at all on a `Model::
/// Spectrum48` bus; real 48K hardware doesn't decode this port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Paging {
    rom_bank: u8,
    ram_bank: u8,
    shadow_screen: bool,
    /// Once set, real hardware ignores all further writes to `0x7ffd`
    /// until reset -- `write_io` below enforces this the same way.
    locked: bool,
}

/// 128K's ROM/RAM banks plus paging state, boxed (see `Memory::Banked128`)
/// so `Memory::Flat`'s much smaller `MemoryBus` doesn't force every
/// `ZxSpectrumBus` -- including every 48K one -- to reserve stack space for
/// data it never uses.
#[derive(Clone)]
struct Banked128 {
    rom: [Vec<u8>; 2],
    ram: [Vec<u8>; 8],
    paging: Paging,
}

/// +2A/+3 paging state -- both `0x7ffd` and `0x1ffd` combined, since ROM
/// selection depends on bits from both registers together and RAM mapping
/// depends on which mode `0x1ffd` bit 0 selects. Field names note which
/// register/bit each comes from. Verified against the official ZX Spectrum
/// +3 technical manual (see CREDITS.md) -- unlike the plain 128K `Paging`
/// struct above, whose `0x7ffd` decode is only commonly-documented, these
/// bit meanings come from the manufacturer's own manual.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Paging2A3 {
    /// `0x7ffd` bits 0-2: RAM bank at `0xC000`, only meaningful when
    /// `!special_mode`.
    ram_bank: u8,
    /// `0x7ffd` bit 3.
    shadow_screen: bool,
    /// `0x7ffd` bit 4: ROM select's low bit.
    rom_bit_lo: bool,
    /// `0x7ffd` bit 5. Freezes further writes to BOTH `0x7ffd` and
    /// `0x1ffd` once set (commonly-documented behavior, matching the
    /// single shared "paging locked" concept on plain 128K -- not directly
    /// stated bit-for-bit in the +3 manual excerpt this was verified
    /// against, so treat this specific claim as the same epistemic status
    /// as the 128K `Paging::locked` field above, not manual-verified).
    locked: bool,
    /// `0x1ffd` bit 0: `false` = normal (ROM/RAM select via the bits
    /// below), `true` = special all-RAM configuration.
    special_mode: bool,
    /// `0x1ffd` bits 2 and 1 combined into a 0-3 value (`(bit2 << 1) |
    /// bit1`). Meaning depends on `special_mode`: ROM select's high bit
    /// (only bit 2 matters, bit 1 ignored) when `false`; which of the 4
    /// documented all-RAM bank layouts (`ALL_RAM_CONFIGS`) when `true`.
    bits_1_2: u8,
    /// `0x1ffd` bit 3: floppy disk motor. Stored for completeness since
    /// it's part of the same register write, but nothing acts on it yet --
    /// no FDC is modeled (see `Model::Spectrum2A3`'s doc comment).
    disk_motor: bool,
}

impl Paging2A3 {
    /// ROM select's high bit, only meaningful in normal mode -- see
    /// `bits_1_2`'s doc comment.
    fn rom_bit_hi(&self) -> bool {
        self.bits_1_2 & 0b10 != 0
    }
}

/// The 4 real RAM-bank layouts `0x1ffd`'s special all-RAM mode maps across
/// the whole 64K address space (each window is 0x4000 bytes: `0x0000`,
/// `0x4000`, `0x8000`, `0xC000`), indexed by `Paging2A3::bits_1_2`.
/// Verified verbatim against the official +3 manual's own table -- not
/// derived or guessed.
const ALL_RAM_CONFIGS: [[usize; 4]; 4] = [[0, 1, 2, 3], [4, 5, 6, 7], [4, 5, 6, 3], [4, 7, 6, 3]];

/// +2A/+3's 4 ROM banks (vs. 128K's 2) plus its own combined paging state.
/// Boxed for the same reason as `Banked128`.
#[derive(Clone)]
struct Banked2A3 {
    rom: [Vec<u8>; 4],
    ram: [Vec<u8>; 8],
    paging: Paging2A3,
}

/// The three real ZX Spectrum memory shapes. 48K delegates to the
/// existing, already-tested `machine::MemoryBus` unchanged; 128K and
/// +2A/+3 each keep their own raw ROM/RAM bank arrays, since `MemoryBus`
/// has no concept of paging and extending it to support one would leak
/// Spectrum-specific logic into a CPU-agnostic crate every other machine
/// also depends on. See `CLAUDE.md` for why this split lives here rather
/// than as separate machine crates -- the actual difference is contained
/// entirely to this one file plus `ZxSpectrum`'s paging-related methods.
/// +2A/+3 is a distinct variant from 128K (not a generalized "N ROM
/// banks" version of the same struct) so the well-tested 128K path stays
/// completely untouched by this addition.
#[derive(Clone)]
enum Memory {
    Flat(MemoryBus),
    Banked128(Box<Banked128>),
    Banked2A3(Box<Banked2A3>),
}

/// Wires the Spectrum's memory map and its keyboard matrix into one `Bus`:
/// memory reads/writes delegate straight to `MemoryBus` on 48K, or this
/// bus's own bank arrays on 128K (see `Memory`); IO reads decode the
/// keyboard when the low byte's bit 0 is clear (matching real hardware,
/// which only decodes that one bit -- same convention as ZX81) via a shared
/// `sinclair_keyboard::KeyboardMatrix` -- see `keyboard::ZxSpectrumKey` for
/// the Spectrum's own row/bit table. Any other port reads back `0xFF`,
/// matching `MemoryBus`'s own unmapped-read convention. `write_io` decodes
/// the same port range for the ULA's border color (bits 0-2 of the written
/// value) -- real hardware also uses bits 3/4 here for the MIC/speaker
/// output, not modeled (no sound yet).
#[derive(Clone)]
pub(crate) struct ZxSpectrumBus {
    memory: Memory,
    /// Debugger port breakpoints (see `machine::PortWatcher`); not saved in states.
    pub(crate) port_watch: machine::PortWatcher,
    keyboard: KeyboardMatrix,
    /// Border color, bits 0-2 of the last value written to the ULA port
    /// (0xFE and its mirrors). Real hardware's border output doesn't
    /// support the BRIGHT attribute bit ink/paper get -- only these 3 bits.
    border_color: u8,
    /// The 1-bit beeper's current level -- bit 4 of the last value written
    /// to the same port as the border color (0xFE and mirrors). Present on
    /// both models: the 128K keeps the same beeper hardware as the 48K
    /// alongside its AY-3-8910 (both mix together on real hardware).
    speaker_level: bool,
    /// The AY-3-8910 PSG -- `Some` only for `Model::Spectrum128` (the 48K
    /// has no such chip at all). See `write_io`/`read_io` for the real
    /// port decode.
    ay: Option<ay_3_8910::Ay3_8910>,
    /// The Turbosound mod's second AY-3-8910, `Some` only when `--turbosound`
    /// was given AND `ay` itself is `Some` (never on 48K) -- see
    /// `set_turbosound`. `active_ay_chip` (`false` = the primary `ay`,
    /// `true` = this one) tracks which chip the last chip-select write to
    /// the register-select port picked; both fields are config-knob-style
    /// state set once after construction, same pattern `speaker_type`/
    /// `joystick_type` already use elsewhere in this codebase.
    ay2: Option<ay_3_8910::Ay3_8910>,
    active_ay_chip: bool,
    /// The currently loaded tape, if any -- `None` until `load_tape` is
    /// called. See `read_io`/`tick_tape` for how it drives port `0xFE`
    /// bit 6 (EAR input).
    tape: Option<zx_tape::TapePlayer>,
    /// `true`: any read of port 0xFE starts the tape (the keyboard scan reads it from power-on, so the tape
    /// then runs from boot). `false`: only `start_tape` does (the machine calls it when the ROM's `LD-BYTES`
    /// is entered). The machine sets `false`; a bare bus keeps `true`.
    tape_start_on_any_read: bool,
    /// The +3's NEC765 floppy disk controller -- `Some` only for
    /// `Model::Spectrum2A3` (no FDC at all on 48K/128K). See
    /// `read_io`/`write_io` for the real port decode.
    fdc: Option<nec765::Fdc<dsk::Disk>>,
    /// The disk exactly as inserted, so a save state that predates any
    /// write can put the medium back (see `restore_disk_image`).
    pristine_disk: Option<dsk::Disk>,
    /// Position (in T-states) within the current video frame -- the ULA's
    /// own notion of "where is the electron beam right now", used to look
    /// up contended-memory wait states. Reset to `0` every real frame
    /// (`ZxSpectrum::request_interrupt`) and advanced by `Bus::
    /// advance_tstates` after every CPU memory access. See `contend_memory`
    /// for how it's used and `CREDITS.md` for the timing numbers' source.
    frame_tstate: u32,
    /// Current Kempston joystick state -- port `0x1F`'s own bit layout
    /// (`000FUDLR`), updated directly by `press_kempston`/`release_kempston`
    /// rather than derived from anything else. Present unconditionally on
    /// every model (real Kempston is a bolt-on interface, not built into
    /// any Spectrum, but there's no real device it could collide with at
    /// this port -- see `read_io`'s own decode comment).
    kempston: u8,
    /// Current Fuller joystick state -- port `0x7f`'s own bit layout
    /// (see `FullerButton`), active-low so idle is all-1s (`0xFF`), not
    /// `0` the way Kempston's active-high idle state is. Present
    /// unconditionally, same "no real device collides with this port"
    /// reasoning as `kempston`.
    fuller: u8,
    /// Kempston mouse X counter -- a real, free-running 8-bit relative
    /// counter (wraps at `0`/`0xFF`, per the cited reference: "00-FF Roll
    /// over at 0 and $FF"), not an absolute screen position. Advanced by
    /// `move_mouse`, read (unmodified) by `read_io`'s X port.
    mouse_x: u8,
    /// Same as `mouse_x`, vertical.
    mouse_y: u8,
    /// Which mouse buttons are currently held -- bit0=right, bit1=left,
    /// bit2=middle (this internal storage is active-*high*, "pressed =
    /// bit set", for the same "press/release, no debounce" reasons every
    /// other button-shaped state in this file uses that convention;
    /// `read_io`'s button port inverts it to the real hardware's actual
    /// active-low wire convention on the way out, see `CREDITS.md`).
    mouse_buttons: u8,
    /// Scroll wheel ticks accumulated since the button port was last
    /// read -- the real port reports "wheel-delta value (0..15) since
    /// previous read" in its top nibble and implicitly resets on read,
    /// so this can't just be a plain persistent counter like `mouse_x`/
    /// `mouse_y`; `read_io` clears it back to `0` every time it's read.
    mouse_wheel: u8,
    /// Recent EAR-bit history ring buffer for the debug-overlay tape
    /// waveform panel -- mirrors `cpc::bus::CpcBus`'s identical fields,
    /// same reasoning: bounded-size, overwrite-oldest, sampled once per
    /// `tick_tape` call via `record_tape_waveform_sample`.
    tape_waveform: [u8; TAPE_WAVEFORM_SAMPLES],
    tape_waveform_cursor: usize,
    tape_waveform_len: usize,
    /// Cycles accumulated toward the next `TAPE_WAVEFORM_SAMPLE_INTERVAL_CYCLES`
    /// waveform-bar sample.
    tape_waveform_sample_debt: u32,
    /// Recent completed pulse widths (T-states) -- see
    /// `TAPE_PULSE_WIDTH_SAMPLES`'s own doc comment.
    tape_pulse_widths: [u16; TAPE_PULSE_WIDTH_SAMPLES],
    tape_pulse_widths_cursor: usize,
    tape_pulse_widths_len: usize,
    /// T-states elapsed since the EAR line last changed level --
    /// accumulates across `tick_tape` calls; pushed onto
    /// `tape_pulse_widths` and reset to `0` the instant the level flips.
    tape_cycles_since_edge: u32,
}

/// One model's contended-memory timing shape: the repeating wait-state
/// pattern, the T-state within the frame the first contended line starts
/// at, how many T-states apart each line's contended window recurs
/// (`period`), and how many of those T-states are actually part of the
/// pattern (`active_window`) before the line's uncontended
/// border/horizontal-retrace tail. Real hardware only applies any of this
/// while the ULA is drawing one of the 192 visible screen lines -- never
/// during the top/bottom border or vertical retrace. Numbers verified
/// against sinclair.wiki.zxnet.co.uk's "Contended memory" page and
/// worldofspectrum.org's 48K reference (see `CREDITS.md`), cross-checked
/// against each other.
struct ContentionProfile {
    pattern: &'static [u8],
    start: u32,
    period: u32,
    active_window: u32,
}

/// 48K: 8-T-state repeating pattern, 224 T-states/line (64+192+56 total
/// lines, of which only these 192 are ever contended).
const CONTENTION_48K: ContentionProfile = ContentionProfile {
    pattern: &[6, 5, 4, 3, 2, 1, 0, 0],
    start: 14335,
    period: 224,
    active_window: 128,
};

/// 128K/+2: same 8-T-state pattern as 48K, but the line is 228 T-states
/// (not 224) and the contended window starts 26 T-states later.
const CONTENTION_128K: ContentionProfile = ContentionProfile {
    pattern: &[6, 5, 4, 3, 2, 1, 0, 0],
    start: 14361,
    period: 228,
    active_window: 128,
};

/// +2A/+3/+2B/+3B: the Amstrad gate array's own, longer 10-T-state pattern.
/// Real hardware also only applies this while its MREQ line is active --
/// simplified here to always-active like the ULA-based models above (same
/// epistemic tier as this codebase's other commonly-documented, not
/// disassembly-verified, hardware facts -- see `CLAUDE.md`).
const CONTENTION_2A3: ContentionProfile = ContentionProfile {
    pattern: &[1, 0, 7, 6, 5, 4, 3, 2, 1, 0],
    start: 14361,
    period: 228,
    active_window: 130,
};

/// Number of real, drawn screen lines every model's contention pattern
/// applies across -- identical regardless of model (only the border/blank
/// line counts around them differ, and those are never contended).
const CONTENDED_SCREEN_LINES: u32 = 192;

impl ZxSpectrumBus {
    /// `rom` is 16K for `Model::Spectrum48`, 32K (2x16K banks, bank 0 then
    /// bank 1) for `Model::Spectrum128` -- `ZxSpectrum::new` is what
    /// validates the size actually supplied matches the model (see its own
    /// doc comment), this constructor trusts it.
    pub(crate) fn new(model: Model, rom: Vec<u8>) -> Self {
        let memory = match model {
            Model::Spectrum48 => {
                let mut mem = MemoryBus::new();
                mem.map_rom(0x0000, rom);
                mem.map_ram(crate::RAM_BASE, crate::RAM_SIZE);
                Memory::Flat(mem)
            }
            Model::Spectrum128 => {
                let mut rom0 = rom;
                let rom1 = rom0.split_off(rom0.len().min(0x4000));
                rom0.resize(0x4000, 0);
                let mut rom1 = rom1;
                rom1.resize(0x4000, 0);
                Memory::Banked128(Box::new(Banked128 {
                    rom: [rom0, rom1],
                    ram: std::array::from_fn(|_| vec![0u8; 0x4000]),
                    paging: Paging::default(),
                }))
            }
            Model::Spectrum2A3 => {
                let mut remaining = rom;
                let mut banks = Vec::with_capacity(4);
                for _ in 0..4 {
                    let idx = remaining.len().min(0x4000);
                    let rest = remaining.split_off(idx);
                    let mut bank = remaining;
                    bank.resize(0x4000, 0);
                    banks.push(bank);
                    remaining = rest;
                }
                Memory::Banked2A3(Box::new(Banked2A3 {
                    rom: banks.try_into().expect("exactly 4 banks split above"),
                    ram: std::array::from_fn(|_| vec![0u8; 0x4000]),
                    paging: Paging2A3::default(),
                }))
            }
        };
        Self {
            memory,
            port_watch: machine::PortWatcher::default(),
            keyboard: KeyboardMatrix::new(),
            border_color: 0,
            speaker_level: false,
            ay: matches!(model, Model::Spectrum128 | Model::Spectrum2A3)
                .then(ay_3_8910::Ay3_8910::new),
            ay2: None,
            active_ay_chip: false,
            tape: None,
            fdc: matches!(model, Model::Spectrum2A3).then(|| {
                let mut fdc = nec765::Fdc::new();
                fdc.set_motor(false);
                fdc
            }),
            pristine_disk: None,
            frame_tstate: 0,
            kempston: 0,
            fuller: 0xFF,
            mouse_x: 0,
            mouse_y: 0,
            mouse_buttons: 0,
            mouse_wheel: 0,
            tape_waveform: [0; TAPE_WAVEFORM_SAMPLES],
            tape_waveform_cursor: 0,
            tape_waveform_len: 0,
            tape_waveform_sample_debt: 0,
            tape_pulse_widths: [0; TAPE_PULSE_WIDTH_SAMPLES],
            tape_pulse_widths_cursor: 0,
            tape_pulse_widths_len: 0,
            tape_cycles_since_edge: 0,
            tape_start_on_any_read: true,
        }
    }

    /// Resets the contended-memory frame-position counter -- called once per
    /// real video frame (`ZxSpectrum::request_interrupt`), matching the real
    /// ULA/gate array's contention pattern restarting at the same point
    /// every frame.
    pub(crate) fn reset_frame_tstate(&mut self) {
        self.frame_tstate = 0;
    }

    pub(crate) fn press_kempston(&mut self, button: KempstonButton) {
        self.kempston |= button.bit();
    }

    pub(crate) fn release_kempston(&mut self, button: KempstonButton) {
        self.kempston &= !button.bit();
    }

    /// See `FullerButton`'s own doc comment for why this clears (not
    /// sets) the bit -- active low.
    pub(crate) fn press_fuller(&mut self, button: FullerButton) {
        self.fuller &= !button.bit();
    }

    pub(crate) fn release_fuller(&mut self, button: FullerButton) {
        self.fuller |= button.bit();
    }

    pub(crate) fn press_mouse_button(&mut self, button: MouseButton) {
        self.mouse_buttons |= button.bit();
    }

    pub(crate) fn release_mouse_button(&mut self, button: MouseButton) {
        self.mouse_buttons &= !button.bit();
    }

    /// Advances the mouse's free-running X/Y counters by a relative delta
    /// -- real hardware wraps at `0`/`0xFF` (see `mouse_x`'s own doc
    /// comment), which `u8::wrapping_add`'s own two's-complement wraparound
    /// matches exactly for either sign of `dx`/`dy`.
    pub(crate) fn move_mouse(&mut self, dx: i8, dy: i8) {
        self.mouse_x = self.mouse_x.wrapping_add(dx as u8);
        self.mouse_y = self.mouse_y.wrapping_add(dy as u8);
    }

    /// Accumulates a scroll-wheel tick -- see `mouse_wheel`'s own doc
    /// comment for why this doesn't just directly set a persistent value.
    /// Clamped to the real port's 4-bit range so a burst of scroll events
    /// between two reads can't silently overflow into the button bits
    /// below it.
    pub(crate) fn scroll_mouse(&mut self, delta: i8) {
        self.mouse_wheel = self.mouse_wheel.saturating_add_signed(delta).min(0x0F);
    }

    /// Which RAM bank (if any) `addr` currently maps to, reusing the exact
    /// paging logic `banked128_peek`/`banked2a3_peek` already implement --
    /// `None` for a ROM-mapped address (never contended), `Some(bank)` for
    /// RAM. 48K (`Memory::Flat`) has no banking concept at all, so it's
    /// handled separately by `is_contended_address` as a plain range check.
    fn resolve_ram_bank(&self, addr: u16) -> Option<u8> {
        match &self.memory {
            Memory::Flat(_) => None,
            Memory::Banked128(b) => match addr {
                0x0000..=0x3FFF => None,
                0x4000..=0x7FFF => Some(5),
                0x8000..=0xBFFF => Some(2),
                0xC000..=0xFFFF => Some(b.paging.ram_bank),
            },
            Memory::Banked2A3(b) => {
                if b.paging.special_mode {
                    // Special all-RAM mode maps even 0x0000-0x3fff to real
                    // RAM (see `banked2a3_peek`/`banked2a3_write`) -- going
                    // through the same `special_mode_bank` lookup here means
                    // this case is handled correctly for free, not
                    // hardcoded separately.
                    Some(special_mode_bank(&b.paging, addr) as u8)
                } else {
                    match addr {
                        0x0000..=0x3FFF => None,
                        0x4000..=0x7FFF => Some(5),
                        0x8000..=0xBFFF => Some(2),
                        0xC000..=0xFFFF => Some(b.paging.ram_bank),
                    }
                }
            }
        }
    }

    /// The RAM bank numbers this model's ULA/gate array actually contends --
    /// see `CONTENTION_48K`/`_128K`/`_2A3`'s own doc comments and
    /// `CREDITS.md`. 48K returns an empty slice since `is_contended_address`
    /// handles it as a plain range check instead (no banking to check
    /// membership against).
    fn contended_ram_banks(&self) -> &'static [u8] {
        match &self.memory {
            Memory::Flat(_) => &[],
            Memory::Banked128(_) => &[1, 3, 5, 7],
            Memory::Banked2A3(_) => &[4, 5, 6, 7],
        }
    }

    /// Whether `addr` is *capable* of being contended right now (i.e. maps
    /// to contended RAM) -- independent of frame position/screen-drawing
    /// timing, which `contend_memory` checks separately.
    fn is_contended_address(&self, addr: u16) -> bool {
        match &self.memory {
            Memory::Flat(_) => (0x4000..=0x7FFF).contains(&addr),
            _ => self
                .resolve_ram_bank(addr)
                .is_some_and(|bank| self.contended_ram_banks().contains(&bank)),
        }
    }

    /// The byte the ULA is currently outputting to the video data bus --
    /// what an `IN` from a genuinely unmapped port returns on real hardware
    /// ("floating"/"unattached" bus), instead of a fixed value. Only
    /// meaningful while the ULA is actively drawing one of the 192 visible
    /// screen lines; `0xFF` (idle bus, matching every other unmapped-read
    /// convention in this codebase) the rest of the time.
    ///
    /// Real hardware fetches two adjacent 8-pixel cells' worth of bitmap +
    /// attribute data every 8 T-states (`bitmap, attribute, bitmap+1,
    /// attribute+1`, at local offsets 3/4/5/6 within that block, the other 4
    /// idle) -- verified against sinclair.wiki.zxnet.co.uk's "Floating bus"
    /// page for 48K/128K specifically (see `CREDITS.md`); applied here to
    /// +2A/+3 too as an unverified-but-documented extension (that source
    /// has no +2A/+3-specific data), same epistemic tier as this file's
    /// other commonly-documented, not disassembly-verified, facts.
    fn floating_bus_byte(&self) -> u8 {
        let profile = self.contention_profile();
        if self.frame_tstate < profile.start {
            return 0xFF;
        }
        let rel = self.frame_tstate - profile.start;
        let line = rel / profile.period;
        if line >= CONTENDED_SCREEN_LINES {
            return 0xFF;
        }
        let pos = rel % profile.period;
        if pos >= profile.active_window {
            return 0xFF;
        }

        let block_index = pos / 8; // which pair of 8-pixel cells within the line
        let block_local = pos % 8; // position within that pair's 8-T-state fetch window
        let col_a = (block_index * 2) as u16;
        let col_b = col_a + 1;
        let row = line as u16;
        let char_row = row / 8;

        match block_local {
            3 => self.peek_screen(video::pixel_address(row, col_a)),
            4 => self.peek_screen(video::attr_address(char_row, col_a)),
            5 => self.peek_screen(video::pixel_address(row, col_b)),
            6 => self.peek_screen(video::attr_address(char_row, col_b)),
            _ => 0xFF, // idle T-states within the block
        }
    }

    /// This model's contention timing shape -- see `ContentionProfile`.
    fn contention_profile(&self) -> &'static ContentionProfile {
        match &self.memory {
            Memory::Flat(_) => &CONTENTION_48K,
            Memory::Banked128(_) => &CONTENTION_128K,
            Memory::Banked2A3(_) => &CONTENTION_2A3,
        }
    }

    /// The contention delay this model's pattern gives at frame position
    /// `tstate` -- `0` outside the active screen-drawing window. Pure
    /// function of a T-state position, not `self.frame_tstate` directly, so
    /// `contend_io` below can look it up at several successive positions
    /// within one I/O cycle without committing any of them until the whole
    /// cycle's total is known (see its own doc comment).
    fn delay_at(profile: &ContentionProfile, tstate: u32) -> u8 {
        if tstate < profile.start {
            return 0;
        }
        let rel = tstate - profile.start;
        let line = rel / profile.period;
        if line >= CONTENDED_SCREEN_LINES {
            return 0;
        }
        let pos = rel % profile.period;
        if pos >= profile.active_window {
            return 0;
        }
        profile.pattern[pos as usize % profile.pattern.len()]
    }

    pub(crate) fn border_color(&self) -> u8 {
        self.border_color
    }

    pub(crate) fn set_border_color(&mut self, color: u8) {
        self.border_color = color & 0b111;
    }

    pub(crate) fn speaker_level(&self) -> bool {
        self.speaker_level
    }

    pub(crate) fn ay(&self) -> Option<&ay_3_8910::Ay3_8910> {
        self.ay.as_ref()
    }

    pub(crate) fn ay_mut(&mut self) -> Option<&mut ay_3_8910::Ay3_8910> {
        self.ay.as_mut()
    }

    pub(crate) fn ay2(&self) -> Option<&ay_3_8910::Ay3_8910> {
        self.ay2.as_ref()
    }

    pub(crate) fn ay2_mut(&mut self) -> Option<&mut ay_3_8910::Ay3_8910> {
        self.ay2.as_mut()
    }

    pub(crate) fn active_ay_chip_raw(&self) -> bool {
        self.active_ay_chip
    }

    /// Restores `active_ay_chip` from a save state -- deliberately can
    /// never select the second chip unless `ay2` actually exists on
    /// *this* run (e.g. a state saved with `--turbosound`, reloaded
    /// without it), so `active_ay`/`active_ay_mut` below can never
    /// silently resolve to `None` -- and drop all AY audio -- just
    /// because a stale flag pointed at a chip that isn't there.
    pub(crate) fn set_active_ay_chip_raw(&mut self, second: bool) {
        self.active_ay_chip = second && self.ay2.is_some();
    }

    /// Enables/disables the Turbosound second AY-3-8910 -- a no-op if this
    /// model has no AY at all (48K), matching `set_turbosound` in
    /// `lib.rs`'s own "ignored where it doesn't apply" convention for
    /// `speaker_type`/`joystick_type`. Resets `active_ay_chip` back to the
    /// primary chip either way, a safe default whether turning the second
    /// chip on or off.
    pub(crate) fn set_turbosound(&mut self, enabled: bool) {
        self.ay2 = (enabled && self.ay.is_some()).then(ay_3_8910::Ay3_8910::new);
        self.active_ay_chip = false;
    }

    fn active_ay(&self) -> Option<&ay_3_8910::Ay3_8910> {
        if self.active_ay_chip {
            self.ay2.as_ref()
        } else {
            self.ay.as_ref()
        }
    }

    fn active_ay_mut(&mut self) -> Option<&mut ay_3_8910::Ay3_8910> {
        if self.active_ay_chip {
            self.ay2.as_mut()
        } else {
            self.ay.as_mut()
        }
    }

    /// Installs a tape for loading, replacing any tape already installed.
    /// It sits loaded but not playing until `start_tape` (the machine does
    /// that when the ROM loader is entered) -- or, with
    /// `set_tape_start_on_any_read(true)`, until port `0xFE` is read.
    pub(crate) fn load_tape(&mut self, tape: zx_tape::Tape) {
        self.tape = Some(zx_tape::TapePlayer::new(tape));
        self.tape_cycles_since_edge = 0;
    }

    /// Presses Play: a no-op without a tape, or once it is playing or finished.
    pub(crate) fn start_tape(&mut self) {
        if let Some(tape) = &mut self.tape {
            tape.play();
        }
    }

    pub(crate) fn set_tape_start_on_any_read(&mut self, on: bool) {
        self.tape_start_on_any_read = on;
    }

    /// Takes the tape out; the EAR input idles again.
    pub(crate) fn eject_tape(&mut self) {
        self.tape = None;
        self.tape_cycles_since_edge = 0;
    }

    /// Takes the disk out of the +3 drive (a no-op without an FDC).
    pub(crate) fn eject_disk(&mut self) {
        if let Some(fdc) = &mut self.fdc {
            fdc.eject_disk();
        }
        self.pristine_disk = None;
    }

    /// Sets the currently loaded tape's turbo speed multiplier (1 = real
    /// time). A no-op if no tape is installed yet.
    pub(crate) fn set_tape_speed(&mut self, multiplier: u32) {
        if let Some(tape) = &mut self.tape {
            tape.set_speed_multiplier(multiplier);
        }
    }

    /// Advances the currently loaded tape's playback by `cycles` real CPU
    /// cycles. A no-op if no tape is installed. Called once per CPU step
    /// from `ZxSpectrum::step`, same pattern as the AY chip's `tick`.
    pub(crate) fn tick_tape(&mut self, cycles: u32) {
        let mut edge_fired = false;
        if let Some(tape) = &mut self.tape {
            let level_before = tape.ear_bit();
            tape.tick(cycles);
            edge_fired = tape.ear_bit() != level_before;
        }
        // Pulse-width tracking (see `TAPE_PULSE_WIDTH_SAMPLES`'s own doc
        // comment) -- accumulates even with no tape installed, same
        // no-op-until-a-real-edge-fires shape as `cpc::bus::CpcBus`.
        self.tape_cycles_since_edge = self.tape_cycles_since_edge.saturating_add(cycles);
        if edge_fired {
            self.record_tape_pulse_width(self.tape_cycles_since_edge);
            self.tape_cycles_since_edge = 0;
        }
        // Time-uniform waveform-bar sampling: a `while` loop (not a single
        // conditional sample) in case one `tick_tape` call ever advances
        // more than one sample interval's worth of cycles.
        self.tape_waveform_sample_debt = self.tape_waveform_sample_debt.saturating_add(cycles);
        while self.tape_waveform_sample_debt >= TAPE_WAVEFORM_SAMPLE_INTERVAL_CYCLES {
            self.tape_waveform_sample_debt -= TAPE_WAVEFORM_SAMPLE_INTERVAL_CYCLES;
            self.record_tape_waveform_sample();
        }
    }

    /// Same ring-buffer sampling shape as `cpc::bus::CpcBus`'s identical
    /// method: idle-high (`1`) when no tape is installed or playback isn't
    /// actively running, matching a real stopped/absent cassette's EAR
    /// line.
    fn record_tape_waveform_sample(&mut self) {
        let sample = self
            .tape
            .as_ref()
            .and_then(|tape| {
                let state = tape.playback_state();
                (state.playing && !state.finished).then(|| u8::from(tape.ear_bit()))
            })
            .unwrap_or(1);
        self.tape_waveform[self.tape_waveform_cursor] = sample;
        self.tape_waveform_cursor = (self.tape_waveform_cursor + 1) % TAPE_WAVEFORM_SAMPLES;
        if self.tape_waveform_len < TAPE_WAVEFORM_SAMPLES {
            self.tape_waveform_len += 1;
        }
    }

    pub(crate) fn tape_waveform_samples(&self) -> Vec<u8> {
        let len = self.tape_waveform_len;
        if len == 0 {
            return Vec::new();
        }
        let start =
            (self.tape_waveform_cursor + TAPE_WAVEFORM_SAMPLES - len) % TAPE_WAVEFORM_SAMPLES;
        let mut out = Vec::with_capacity(len);
        for offset in 0..len {
            let idx = (start + offset) % TAPE_WAVEFORM_SAMPLES;
            out.push(self.tape_waveform[idx]);
        }
        out
    }

    fn record_tape_pulse_width(&mut self, width_cycles: u32) {
        let width = u16::try_from(width_cycles).unwrap_or(u16::MAX);
        self.tape_pulse_widths[self.tape_pulse_widths_cursor] = width;
        self.tape_pulse_widths_cursor =
            (self.tape_pulse_widths_cursor + 1) % TAPE_PULSE_WIDTH_SAMPLES;
        if self.tape_pulse_widths_len < TAPE_PULSE_WIDTH_SAMPLES {
            self.tape_pulse_widths_len += 1;
        }
    }

    pub(crate) fn tape_pulse_widths_samples(&self) -> Vec<u16> {
        let len = self.tape_pulse_widths_len;
        if len == 0 {
            return Vec::new();
        }
        let start = (self.tape_pulse_widths_cursor + TAPE_PULSE_WIDTH_SAMPLES - len)
            % TAPE_PULSE_WIDTH_SAMPLES;
        let mut out = Vec::with_capacity(len);
        for offset in 0..len {
            let idx = (start + offset) % TAPE_PULSE_WIDTH_SAMPLES;
            out.push(self.tape_pulse_widths[idx]);
        }
        out
    }

    pub(crate) fn tape_playback_state(&self) -> Option<zx_tape::TapePlaybackState> {
        self.tape.as_ref().map(zx_tape::TapePlayer::playback_state)
    }

    pub(crate) fn tape_ear_bit(&self) -> Option<bool> {
        self.tape.as_ref().map(zx_tape::TapePlayer::ear_bit)
    }

    pub(crate) fn tape_cycles_until_next_edge(&self) -> Option<u32> {
        self.tape
            .as_ref()
            .and_then(zx_tape::TapePlayer::cycles_until_next_edge)
    }

    /// For the fast-tape-load ROM trap (`ZxSpectrum::try_fast_tape_load_
    /// trap`): the next standard-protocol block's raw bytes, if playback is
    /// currently positioned at one -- see `zx_tape::TapePlayer::
    /// take_next_raw_block`'s own doc comment. `None` if no tape is
    /// installed at all. Returns an owned `Vec` (not a borrow) since the
    /// caller needs `&mut self.bus` again immediately after for the actual
    /// memory writes -- these blocks are small (a handful of KB at most),
    /// called once per block rather than once per byte, so the copy is
    /// cheap relative to what it replaces (the entire real-time pulse
    /// simulation for that block).
    pub(crate) fn take_next_tape_block(&mut self) -> Option<Vec<u8>> {
        self.tape
            .as_mut()?
            .take_next_raw_block()
            .map(<[u8]>::to_vec)
    }

    /// Installs a disk for the FDC to serve -- a no-op on any model other
    /// than `Model::Spectrum2A3` (there's no `fdc` to install into; see
    /// that field's own doc comment). `ZxSpectrum::load_disk` is where
    /// the real "wrong model" error is surfaced to the caller, same split
    /// as `load_ram_bank`'s own doc comment.
    pub(crate) fn load_disk(&mut self, disk: dsk::Disk) {
        if let Some(fdc) = &mut self.fdc {
            self.pristine_disk = Some(disk.clone());
            fdc.insert_disk(disk);
        }
    }

    /// The disk in the drive with every write applied.
    pub(crate) fn disk(&self) -> Option<&dsk::Disk> {
        self.fdc.as_ref()?.disk()
    }

    /// The modified disk serialised for a save state; `None` while the
    /// disk is untouched (then the externally supplied image is exact).
    pub(crate) fn modified_disk_image(&self) -> Option<Vec<u8>> {
        self.disk()
            .filter(|d| d.is_modified())
            .map(dsk::Disk::to_bytes)
    }

    /// Puts the drive's medium back to what a save state recorded: the
    /// captured modified image, or (for a state taken before any write)
    /// the disk as originally inserted.
    pub(crate) fn restore_disk_image(&mut self, image: Option<Vec<u8>>) -> Result<(), String> {
        let Some(fdc) = &mut self.fdc else {
            return Ok(());
        };
        match image {
            Some(bytes) => {
                let mut disk = dsk::parse(&bytes)
                    .map_err(|e| format!("save state disk image is invalid: {e}"))?;
                disk.mark_modified();
                fdc.insert_disk(disk);
            }
            None => {
                if fdc.disk().is_some_and(dsk::Disk::is_modified)
                    && let Some(pristine) = self.pristine_disk.clone()
                {
                    fdc.insert_disk(pristine);
                }
            }
        }
        Ok(())
    }

    /// Records that the current disk image has been persisted.
    pub(crate) fn mark_disk_saved(&mut self) {
        if let Some(disk) = self.fdc.as_mut().and_then(nec765::Fdc::disk_mut) {
            disk.clear_modified();
        }
    }

    pub(crate) fn press_key(&mut self, key: ZxSpectrumKey) {
        let (row, bit) = key.row_bit();
        self.keyboard.press(row, bit);
    }

    pub(crate) fn release_key(&mut self, key: ZxSpectrumKey) {
        let (row, bit) = key.row_bit();
        self.keyboard.release(row, bit);
    }

    pub(crate) fn peek(&self, addr: u16) -> u8 {
        match &self.memory {
            Memory::Flat(mem) => mem.peek(addr),
            Memory::Banked128(b) => banked128_peek(&b.rom, &b.ram, &b.paging, addr),
            Memory::Banked2A3(b) => banked2a3_peek(&b.rom, &b.ram, &b.paging, addr),
        }
    }

    pub(crate) fn load(&mut self, addr: u16, bytes: &[u8]) -> usize {
        match &mut self.memory {
            Memory::Flat(mem) => mem.load(addr, bytes),
            Memory::Banked128(_) => {
                let mut landed = 0;
                for (i, &b) in bytes.iter().enumerate() {
                    let a = addr.wrapping_add(i as u16);
                    if a < 0x4000 {
                        continue; // ROM: dropped, matching MemoryBus's own convention.
                    }
                    self.write(a, b);
                    landed += 1;
                }
                landed
            }
            // +2A/+3's "ROM" window is writable RAM in special mode (see
            // `banked2a3_write`), so unlike 128K, whether a low address
            // lands depends on live paging state, not a fixed skip -- just
            // route every byte through the real write path and count what
            // `write` doesn't itself guard against being ROM. `_` (not
            // binding the box) so this arm doesn't hold a live borrow of
            // `self.memory` across the `self.write`/`self.is_2a3_special_
            // mode` calls below, same reasoning as the `Banked128(_)` arm
            // above.
            Memory::Banked2A3(_) => {
                let mut landed = 0;
                for (i, &val) in bytes.iter().enumerate() {
                    let a = addr.wrapping_add(i as u16);
                    if a < 0x4000 && !self.is_2a3_special_mode() {
                        continue; // real ROM right now: dropped.
                    }
                    self.write(a, val);
                    landed += 1;
                }
                landed
            }
        }
    }

    /// `true` if this is a `Model::Spectrum2A3` bus currently in `0x1ffd`
    /// special all-RAM mode. `false` (including for every other model) if
    /// not -- used by `load` to decide whether the `0x0000`-`0x3fff`
    /// window is real ROM (dropped) or writable RAM right now.
    fn is_2a3_special_mode(&self) -> bool {
        matches!(&self.memory, Memory::Banked2A3(b) if b.paging.special_mode)
    }

    /// Reads directly from RAM bank 5 or 7 (the ULA's own video source,
    /// selected by `Paging::shadow_screen`) regardless of what's currently
    /// paged at `0xC000` -- unlike every other address, the screen banks
    /// are NOT affected by the `0xC000` page selector, since bank 5 lives
    /// at the fixed `0x4000`-`0x7fff` range and the ULA reads bank 7
    /// directly off the underlying RAM array, not through the CPU's
    /// address space, when shadow screen is selected. On 48K (no paging at
    /// all), this is identical to `peek`. `addr` must be a real screen
    /// address (`0x4000`-`0x5aff`, which `video::pixel_address`/
    /// `attr_address` always produce) -- meaningless otherwise.
    pub(crate) fn peek_screen(&self, addr: u16) -> u8 {
        match &self.memory {
            Memory::Flat(mem) => mem.peek(addr),
            Memory::Banked128(b) => {
                let bank = if b.paging.shadow_screen { 7 } else { 5 };
                b.ram[bank][(addr - 0x4000) as usize]
            }
            Memory::Banked2A3(b) => {
                let bank = if b.paging.shadow_screen { 7 } else { 5 };
                b.ram[bank][(addr - 0x4000) as usize]
            }
        }
    }

    /// Overwrites one of the 8 real RAM banks wholesale -- used by
    /// `ZxSpectrum::load_snapshot` for a `z80_snapshot::MemoryBlock::
    /// RamBank`. No-op on a `Model::Spectrum48` bus (shouldn't happen:
    /// `ZxSpectrum::load_snapshot` checks the model matches the snapshot
    /// before calling this).
    pub(crate) fn load_ram_bank(&mut self, bank: u8, data: &[u8]) {
        let ram = match &mut self.memory {
            Memory::Flat(_) => return,
            Memory::Banked128(b) => &mut b.ram,
            Memory::Banked2A3(b) => &mut b.ram,
        };
        let dest = &mut ram[bank as usize];
        let len = data.len().min(dest.len());
        dest[..len].copy_from_slice(&data[..len]);
    }

    pub(crate) fn write_ram_bank(&mut self, bank: u8, address: u16, data: &[u8]) -> usize {
        let ram = match &mut self.memory {
            Memory::Flat(_) => return 0,
            Memory::Banked128(b) => &mut b.ram,
            Memory::Banked2A3(b) => &mut b.ram,
        };
        let Some(dest) = ram.get_mut(usize::from(bank)) else {
            return 0;
        };
        let start = usize::from(address);
        if start >= dest.len() {
            return 0;
        }
        let len = data.len().min(dest.len() - start);
        dest[start..start + len].copy_from_slice(&data[..len]);
        len
    }

    /// Installs paging state directly -- used by `ZxSpectrum::
    /// load_snapshot` to restore a `.z80` snapshot's `PagingState` without
    /// going through the port-write path (which would be rejected if the
    /// snapshot's own state happened to already have `locked` set). No-op
    /// on `Model::Spectrum48`.
    pub(crate) fn set_paging(
        &mut self,
        rom_bank: u8,
        ram_bank: u8,
        shadow_screen: bool,
        locked: bool,
    ) {
        if let Memory::Banked128(b) = &mut self.memory {
            b.paging = Paging {
                rom_bank,
                ram_bank: ram_bank & 0b111,
                shadow_screen,
                locked,
            };
        }
    }

    /// Installs +2A/+3 paging state directly -- same reasoning as
    /// `set_paging`, used by `ZxSpectrum::load_snapshot` to restore a
    /// `.z80` snapshot's `Paging2A3State` bypassing both ports' normal
    /// locked-write check. No-op on any model other than `Spectrum2A3`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_paging_2a3(
        &mut self,
        ram_bank: u8,
        shadow_screen: bool,
        rom_bit_lo: bool,
        locked: bool,
        special_mode: bool,
        bits_1_2: u8,
        disk_motor: bool,
    ) {
        if let Memory::Banked2A3(b) = &mut self.memory {
            b.paging = Paging2A3 {
                ram_bank: ram_bank & 0b111,
                shadow_screen,
                rom_bit_lo,
                locked,
                special_mode,
                bits_1_2: bits_1_2 & 0b11,
                disk_motor,
            };
            if let Some(fdc) = &mut self.fdc {
                fdc.set_motor(disk_motor);
            }
        }
    }

    /// The entire 48K RAM range (`0x4000`-`0xffff`) as one flat image, for
    /// snapshot saving -- `None` on any banked model (128K/+2A/+3 have no
    /// single flat RAM range; see `ram_bank_data` instead). One-shot, not
    /// hot-path, so a plain `peek` loop is fine.
    pub(crate) fn flat_ram(&self) -> Option<Vec<u8>> {
        match &self.memory {
            Memory::Flat(_) => Some((0x4000..=0xFFFFu32).map(|a| self.peek(a as u16)).collect()),
            _ => None,
        }
    }

    /// Raw contents of one of the 8 real RAM banks (128K/+2A/+3 only), for
    /// snapshot saving -- independent of what's currently paged into the
    /// CPU's address space, same reasoning as `peek_screen`.
    pub(crate) fn ram_bank_data(&self, bank: u8) -> Option<&[u8]> {
        match &self.memory {
            Memory::Flat(_) => None,
            Memory::Banked128(b) => b.ram.get(bank as usize).map(Vec::as_slice),
            Memory::Banked2A3(b) => b.ram.get(bank as usize).map(Vec::as_slice),
        }
    }

    /// Current 128K paging state as `(rom_bank, ram_bank, shadow_screen,
    /// locked)`, for snapshot saving -- `None` off `Model::Spectrum128`.
    pub(crate) fn paging_state(&self) -> Option<(u8, u8, bool, bool)> {
        match &self.memory {
            Memory::Banked128(b) => Some((
                b.paging.rom_bank,
                b.paging.ram_bank,
                b.paging.shadow_screen,
                b.paging.locked,
            )),
            _ => None,
        }
    }

    /// Current +2A/+3 paging state as `(ram_bank, shadow_screen,
    /// rom_bit_lo, locked, special_mode, bits_1_2, disk_motor)`, for
    /// snapshot saving -- `None` off `Model::Spectrum2A3`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn paging_2a3_state(&self) -> Option<(u8, bool, bool, bool, bool, u8, bool)> {
        match &self.memory {
            Memory::Banked2A3(b) => Some((
                b.paging.ram_bank,
                b.paging.shadow_screen,
                b.paging.rom_bit_lo,
                b.paging.locked,
                b.paging.special_mode,
                b.paging.bits_1_2,
                b.paging.disk_motor,
            )),
            _ => None,
        }
    }

    // --- Save-state accessors: everything below is `keyboard`/`speaker_
    // level`/`kempston`/`frame_tstate`/`tape`/`fdc`'s own plain state, for
    // `ZxSpectrum::save_state`/`load_state`. `ay` already has `ay`/
    // `ay_mut`; `border_color`/`set_border_color` and `flat_ram`/
    // `ram_bank_data`/`paging_state`/`paging_2a3_state` already exist from
    // this session's earlier snapshot-saving work.

    pub(crate) fn keyboard(&self) -> KeyboardMatrix {
        self.keyboard
    }

    pub(crate) fn set_keyboard(&mut self, keyboard: KeyboardMatrix) {
        self.keyboard = keyboard;
    }

    pub(crate) fn set_speaker_level(&mut self, speaker_level: bool) {
        self.speaker_level = speaker_level;
    }

    pub(crate) fn kempston_raw(&self) -> u8 {
        self.kempston
    }

    pub(crate) fn set_kempston_raw(&mut self, kempston: u8) {
        self.kempston = kempston;
    }

    pub(crate) fn fuller_raw(&self) -> u8 {
        self.fuller
    }

    pub(crate) fn set_fuller_raw(&mut self, fuller: u8) {
        self.fuller = fuller;
    }

    pub(crate) fn mouse_raw(&self) -> (u8, u8, u8, u8) {
        (
            self.mouse_x,
            self.mouse_y,
            self.mouse_buttons,
            self.mouse_wheel,
        )
    }

    pub(crate) fn set_mouse_raw(&mut self, raw: (u8, u8, u8, u8)) {
        (
            self.mouse_x,
            self.mouse_y,
            self.mouse_buttons,
            self.mouse_wheel,
        ) = raw;
    }

    pub(crate) fn frame_tstate_raw(&self) -> u32 {
        self.frame_tstate
    }

    pub(crate) fn set_frame_tstate_raw(&mut self, frame_tstate: u32) {
        self.frame_tstate = frame_tstate;
    }

    /// `None` if no tape is currently installed -- same shape as `ay`/
    /// `ay_mut`.
    pub(crate) fn tape(&self) -> Option<&zx_tape::TapePlayer> {
        self.tape.as_ref()
    }

    pub(crate) fn tape_mut(&mut self) -> Option<&mut zx_tape::TapePlayer> {
        self.tape.as_mut()
    }

    /// `None` off `Model::Spectrum2A3` (no FDC at all on 48K/128K) -- same
    /// shape as `ay`/`ay_mut`.
    pub(crate) fn fdc(&self) -> Option<&nec765::Fdc<dsk::Disk>> {
        self.fdc.as_ref()
    }

    pub(crate) fn fdc_mut(&mut self) -> Option<&mut nec765::Fdc<dsk::Disk>> {
        self.fdc.as_mut()
    }
}

fn banked128_peek(rom: &[Vec<u8>; 2], ram: &[Vec<u8>; 8], paging: &Paging, addr: u16) -> u8 {
    match addr {
        0x0000..=0x3FFF => rom[paging.rom_bank as usize][addr as usize],
        0x4000..=0x7FFF => ram[5][(addr - 0x4000) as usize],
        0x8000..=0xBFFF => ram[2][(addr - 0x8000) as usize],
        0xC000..=0xFFFF => ram[paging.ram_bank as usize][(addr - 0xC000) as usize],
    }
}

/// `addr`'s real RAM bank when `0x1ffd`'s special all-RAM mode is active:
/// each 0x4000 window maps straight to `ALL_RAM_CONFIGS`, independent of
/// `rom`/`ram_bank`/`shadow_screen` -- special mode overrides normal
/// paging entirely, per the +3 manual.
fn special_mode_bank(paging: &Paging2A3, addr: u16) -> usize {
    ALL_RAM_CONFIGS[paging.bits_1_2 as usize][(addr / 0x4000) as usize]
}

fn banked2a3_peek(rom: &[Vec<u8>; 4], ram: &[Vec<u8>; 8], paging: &Paging2A3, addr: u16) -> u8 {
    if paging.special_mode {
        ram[special_mode_bank(paging, addr)][(addr % 0x4000) as usize]
    } else {
        let rom_bank = ((paging.rom_bit_hi() as usize) << 1) | paging.rom_bit_lo as usize;
        match addr {
            0x0000..=0x3FFF => rom[rom_bank][addr as usize],
            0x4000..=0x7FFF => ram[5][(addr - 0x4000) as usize],
            0x8000..=0xBFFF => ram[2][(addr - 0x8000) as usize],
            0xC000..=0xFFFF => ram[paging.ram_bank as usize][(addr - 0xC000) as usize],
        }
    }
}

/// Mirrors `banked2a3_peek`'s mapping for writes. Unlike normal mode
/// (where `0x0000`-`0x3fff` is always real ROM, writes dropped), special
/// mode maps that window to real, writable RAM too -- the +3 manual's
/// "special all-RAM configuration" name is literal, there's no ROM
/// visible anywhere in the address space while it's active.
fn banked2a3_write(ram: &mut [Vec<u8>; 8], paging: &Paging2A3, addr: u16, val: u8) {
    if paging.special_mode {
        ram[special_mode_bank(paging, addr)][(addr % 0x4000) as usize] = val;
    } else {
        match addr {
            0x0000..=0x3FFF => {} // ROM: writes dropped.
            0x4000..=0x7FFF => ram[5][(addr - 0x4000) as usize] = val,
            0x8000..=0xBFFF => ram[2][(addr - 0x8000) as usize] = val,
            0xC000..=0xFFFF => ram[paging.ram_bank as usize][(addr - 0xC000) as usize] = val,
        }
    }
}

impl Bus for ZxSpectrumBus {
    fn io_accessed(&mut self, port: u16, value: u8, write: bool) {
        self.port_watch.note(port, value, write);
    }

    /// Extra wait-state T-states for accessing `addr` right now -- `0` if
    /// `addr` isn't in a currently-contended RAM bank, or if the ULA/gate
    /// array isn't drawing one of the 192 visible screen lines at
    /// `frame_tstate`'s current position. See `ContentionProfile`/
    /// `CREDITS.md` for where the numbers come from.
    fn contend_memory(&mut self, addr: u16) -> u8 {
        if !self.is_contended_address(addr) {
            return 0;
        }
        Self::delay_at(self.contention_profile(), self.frame_tstate)
    }

    /// Extra wait-state T-states for an I/O access to `port` right now.
    /// Real hardware's I/O contention is a genuinely different, 4-branch
    /// model from memory contention (sinclair.wiki.zxnet.co.uk's "Contended
    /// I/O" page, see `CREDITS.md`), keyed on whether the port's high byte
    /// falls in the same contended range as memory *and* whether the port's
    /// bit 0 is set -- and unlike a single memory access, one I/O cycle can
    /// contain up to 4 separately-contended sub-segments, each checked at
    /// its own point in the frame (because the earlier segments' own delay
    /// shifts where the later ones land). `+2A/+3`'s Amstrad gate array only
    /// contends while MREQ is active, which it never is during an I/O
    /// cycle -- so this model has no I/O contention at all, unlike the
    /// ULA-based 48K/128K.
    ///
    /// This walks the segments using a *local* running position seeded from
    /// `self.frame_tstate`, without writing back to it -- every branch's
    /// segment lengths sum to exactly 4 (the real IO cycle length), so the
    /// total real time elapsed is always `4 + total_delay`, exactly what the
    /// caller's own subsequent `advance_tstates(delay + 4)` call already
    /// applies (see `Cpu::io_read`/`io_write`). Committing here too would
    /// double-count it.
    fn contend_io(&mut self, port: u16) -> u8 {
        if matches!(self.memory, Memory::Banked2A3(_)) {
            return 0;
        }

        let high_contended = self.is_contended_address(port);
        let low_bit_set = port & 1 != 0;
        // (needs its own contention check, segment length in T-states).
        let segments: &[(bool, u8)] = match (high_contended, low_bit_set) {
            (false, true) => &[(false, 4)],                                // N:4
            (false, false) => &[(false, 1), (true, 3)],                    // N:1, C:3
            (true, false) => &[(true, 1), (true, 3)],                      // C:1, C:3
            (true, true) => &[(true, 1), (true, 1), (true, 1), (true, 1)], // C:1, C:1, C:1, C:1
        };

        let profile = self.contention_profile();
        let mut cursor = self.frame_tstate;
        let mut total_delay = 0u8;
        for &(contended, len) in segments {
            if contended {
                let delay = Self::delay_at(profile, cursor);
                total_delay += delay;
                cursor += delay as u32 + len as u32;
            } else {
                cursor += len as u32;
            }
        }
        total_delay
    }

    fn advance_tstates(&mut self, n: u8) {
        self.frame_tstate += n as u32;
    }

    fn read(&mut self, addr: u16) -> u8 {
        match &mut self.memory {
            Memory::Flat(mem) => mem.read(addr),
            Memory::Banked128(b) => banked128_peek(&b.rom, &b.ram, &b.paging, addr),
            Memory::Banked2A3(b) => banked2a3_peek(&b.rom, &b.ram, &b.paging, addr),
        }
    }

    fn write(&mut self, addr: u16, val: u8) {
        match &mut self.memory {
            Memory::Flat(mem) => mem.write(addr, val),
            Memory::Banked128(b) => match addr {
                0x0000..=0x3FFF => {} // ROM: writes dropped, matching MemoryBus's own convention.
                0x4000..=0x7FFF => b.ram[5][(addr - 0x4000) as usize] = val,
                0x8000..=0xBFFF => b.ram[2][(addr - 0x8000) as usize] = val,
                0xC000..=0xFFFF => {
                    b.ram[b.paging.ram_bank as usize][(addr - 0xC000) as usize] = val
                }
            },
            Memory::Banked2A3(b) => banked2a3_write(&mut b.ram, &b.paging, addr, val),
        }
    }

    fn read_io(&mut self, port: u16) -> u8 {
        // AY register-select/read port (0xfffd and mirrors): "IN (0xfffd)
        // reads the value of the selected register" -- World of Spectrum's
        // 128K reference. `0xfffd`'s own low bits (bit0=1) never collide
        // with the keyboard's even-port decode below. Reads whichever chip
        // `active_ay_chip` currently points at -- see `write_io`'s own
        // Turbosound chip-select comment for how that's picked.
        if let Some(ay) = self.active_ay()
            && port & 0xC002 == 0xC000
        {
            return ay.read();
        }
        // FDC ports (0x2ffd/0x3ffd), Model::Spectrum2A3 only. Decode
        // assumed `port & 0xF002 == 0x2000`/`0x3000` by analogy with
        // 0x1ffd's already-verified 0xF002 family -- NOT independently
        // verified against the +3 manual or a hardware test suite the
        // way 0x1ffd/0x7ffd were (see CREDITS.md).
        if let Some(fdc) = &mut self.fdc {
            if port & 0xF002 == 0x2000 {
                return fdc.read_msr();
            }
            if port & 0xF002 == 0x3000 {
                return fdc.read_data();
            }
        }
        // Kempston joystick (port 0x1F and mirrors): real hardware "usually
        // only decodes A5" (bit 5 of the port address), ignoring every
        // other address bit -- see `CREDITS.md`. Checked after AY/FDC so
        // those keep priority on the one real (and practically never hit --
        // no software actually reads that address for anything else)
        // overlap, `0xC000`, which also has bit 5 clear.
        //
        // Kempston mouse (ports 0xfbdf/0xffdf/0xfadf) also has bit 5 clear
        // on every one of its three ports, so it's checked here, BEFORE
        // the joystick's own looser bit-5-only match below -- otherwise
        // the joystick decode would incorrectly swallow every mouse read
        // too. Real decode (`sinclair.wiki.zxnet.co.uk`'s own port table,
        // see `CREDITS.md`, cross-checked against all three literal
        // addresses): bit 5 clear (`0x0020`, real, genuinely part of the
        // cited table's own low byte -- not assumed by analogy with
        // Kempston's joystick decode above) AND bits 10:8 of the port
        // select which of the three -- `0b011` = X, `0b111` = Y, `0bx10`
        // = buttons (bit 10 a genuine don't-care there, not just an
        // address this project never verified -- the cited table itself
        // marks it `x`). A real bug caught live: an earlier version of
        // this check omitted the bit-5 requirement, which meant it
        // incorrectly matched real keyboard-scan ports too whenever the
        // row-select byte's own low 3 bits happened to equal `0b011`
        // (e.g. `0xFB`) -- caught by this crate's own existing keyboard
        // tests failing, not by a mouse test.
        if port & 0x0720 == 0x0300 {
            return self.mouse_x;
        }
        if port & 0x0720 == 0x0700 {
            return self.mouse_y;
        }
        if port & 0x0320 == 0x0200 {
            // Real port: "contains wheel-delta value (0..15) since
            // previous read" in the top nibble -- reading it is what
            // resets that delta back to 0, same "read has a side effect"
            // shape `tape`'s own EAR-bit read already has elsewhere in
            // this file. Buttons are wired active-low on real hardware
            // (see `CREDITS.md`); `mouse_buttons` stores the opposite,
            // active-high "is it currently held" convention every other
            // button-shaped state in this file uses, inverted here on the
            // way out.
            let value = (self.mouse_wheel << 4) | (!self.mouse_buttons & 0x0F);
            self.mouse_wheel = 0;
            return value;
        }
        if port & 0x0020 == 0 {
            return self.kempston;
        }
        // Fuller joystick (port 0x7f): see `FullerButton`'s own doc
        // comment for why this only matches the literal low byte, not a
        // wider mirrored decode -- no source found pinning down real
        // hardware's actual address-decode precision the way Kempston's
        // "usually only A5" is documented.
        if port & 0x00FF == 0x7F {
            return self.fuller;
        }
        if port & 0x0001 == 0 {
            let mut value = self.keyboard.read(port);
            // Real port 0xFE bit 6 = EAR input (verified against the
            // World of Spectrum 48K reference, see CREDITS.md);
            // `keyboard.read` already leaves bit 6 set (idle-high) by
            // default, so only clear it when a tape's current pulse is
            // low. With `tape_start_on_any_read`, reading this bit at all
            // also starts playback (see TapePlayer::play's own doc comment
            // for why that's a safe no-op once already playing or finished).
            let start = self.tape_start_on_any_read;
            if let Some(tape) = &mut self.tape {
                if start {
                    tape.play();
                }
                if !tape.ear_bit() {
                    value &= 0b1011_1111;
                }
            }
            value
        } else {
            // Genuinely unmapped (odd port, no AY/FDC decode above): real
            // hardware's floating/"unattached" bus, not a fixed value.
            self.floating_bus_byte()
        }
    }

    fn write_io(&mut self, port: u16, val: u8) {
        if port & 0x0001 == 0 {
            self.border_color = val & 0b111;
            self.speaker_level = val & 0b0001_0000 != 0;
        }
        // AY-3-8910 ports, per World of Spectrum's 128K reference: "OUT
        // (0xfffd) selects a register 0-15", "OUT (0xbffd) writes to the
        // selected register". The exact address-decode mask (bits 15/14/1)
        // below is the commonly-documented extension of those two literal
        // addresses -- same epistemic status as the 0x7ffd paging decode
        // above (not verified against a 128K ROM disassembly).
        //
        // Turbosound chip-select: real protocol verified against
        // ZEsarUX's own source (`soundchips/ay38912.c`, not guessed or
        // taken from a doc summary -- see `CREDITS.md`). A write to the
        // register-select port where `val & 0x9C == 0x9C` (bits 7,4,3,2
        // all set -- the classic convention writes exactly `0xFF`/`0xFE`,
        // bits 6/5 left as don't-cares here since TBBlue's own panning
        // extension also uses them) is a chip-select command, not a
        // register-select: `val & 3 == 3` picks the primary chip back,
        // `val & 3 == 2` picks the second one. `val & 3` of `0` or `1`
        // (SID / a third chip, on real Turbosound Next hardware --
        // neither modeled here, only the classic 2-chip mod) still
        // consumes the write as a chip-select attempt rather than
        // falling through to register-select, same as ZEsarUX itself
        // does when asked to select a chip beyond how many it has
        // configured -- it just leaves the active chip unchanged.
        if self.ay.is_some() {
            if port & 0xC002 == 0xC000 {
                if self.ay2.is_some() && val & 0x9C == 0x9C {
                    match val & 3 {
                        3 => self.active_ay_chip = false,
                        2 => self.active_ay_chip = true,
                        _ => {}
                    }
                } else if let Some(ay) = self.active_ay_mut() {
                    ay.select_register(val);
                }
            } else if port & 0xC002 == 0x8000
                && let Some(ay) = self.active_ay_mut()
            {
                ay.write(val);
            }
        }
        // Real 128K paging port decode: A15 and A1 both clear (`port &
        // 0x8002 == 0`) -- commonly documented across independent Spectrum
        // 128K hardware references, not verified here against a 128K ROM
        // disassembly (unlike e.g. ZX81's NMI generator elsewhere in this
        // codebase, there's no ROM behavior this project can trace to
        // cross-check a port *decode* against; the ROM only ever executes
        // an `OUT` with the one address it expects, which is consistent
        // with this decode but doesn't prove no other bit matters).
        if let Memory::Banked128(b) = &mut self.memory
            && port & 0x8002 == 0
            && !b.paging.locked
        {
            b.paging.ram_bank = val & 0b111;
            b.paging.shadow_screen = val & 0b0000_1000 != 0;
            b.paging.rom_bank = (val >> 4) & 1;
            b.paging.locked = val & 0b0010_0000 != 0;
        }
        if let Memory::Banked2A3(b) = &mut self.memory {
            // `0x1ffd`'s decode (`port & 0xF002 == 0x1000`, derived by hand
            // from a hardware test-suite reference's bit pattern for this
            // port -- see CREDITS.md) is checked FIRST and is mutually
            // exclusive with the `0x7ffd` write below in the same call:
            // every address matching `0x1ffd`'s stricter pattern also
            // happens to satisfy `0x7ffd`'s laxer `0x8002` mask (that mask
            // only cares about bits 15/1, `0x1ffd`'s cares about 15/14/13/
            // 12/1 too), so without this ordering a single write to the
            // literal `0x1ffd` port would incorrectly also update `0x7ffd`'s
            // state. Real +2A/+3 hardware must decode these two registers
            // as mutually exclusive (that's the whole point of having two
            // separate ports) -- exactly how its silicon disambiguates them
            // isn't documented in the sources this was checked against, so
            // "most specific pattern wins" is this codebase's own
            // resolution, not a hardware-verified mechanism.
            if port & 0xF002 == 0x3000 {
                // FDC data write -- checked first, most-specific-pattern-
                // wins, same reasoning as 0x1ffd's own precedence below:
                // 0x2ffd/0x3ffd satisfy this laxer 0x8002==0 paging mask
                // too.
                if let Some(fdc) = &mut self.fdc {
                    fdc.write_data(val);
                }
            } else if port & 0xF002 == 0x2000 {
                // 0x2ffd (MSR) is read-only on real hardware -- a write
                // here is a no-op, but must still be excluded from the
                // 0x8002 paging mask below, same reasoning as the 0x3000
                // arm above.
            } else if port & 0xF002 == 0x1000 {
                if !b.paging.locked {
                    b.paging.special_mode = val & 0b0000_0001 != 0;
                    b.paging.bits_1_2 = (val >> 1) & 0b11;
                    b.paging.disk_motor = val & 0b0000_1000 != 0;
                    if let Some(fdc) = &mut self.fdc {
                        fdc.set_motor(b.paging.disk_motor);
                    }
                }
            } else if port & 0x8002 == 0 && !b.paging.locked {
                b.paging.ram_bank = val & 0b111;
                b.paging.shadow_screen = val & 0b0000_1000 != 0;
                b.paging.rom_bit_lo = val & 0b0001_0000 != 0;
                b.paging.locked = val & 0b0010_0000 != 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectrum48_bus_is_unaffected_by_0x7ffd_writes() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.write_io(0x7FFD, 0xFF);
        bus.load(0xC000, &[0x42]);

        // No paging on 48K -- 0xC000 is just ordinary fixed RAM.
        assert_eq!(bus.peek(0xC000), 0x42);
    }

    #[test]
    fn spectrum128_pages_a_ram_bank_into_0xc000_via_port_0x7ffd() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.load_ram_bank(3, &[0xAB]);

        bus.write_io(0x7FFD, 0b0000_0011); // ram_bank = 3
        assert_eq!(bus.peek(0xC000), 0xAB);

        bus.load_ram_bank(0, &[0xCD]);
        bus.write_io(0x7FFD, 0b0000_0000); // ram_bank = 0
        assert_eq!(bus.peek(0xC000), 0xCD);
    }

    #[test]
    fn spectrum128_banks_5_and_2_are_always_fixed_regardless_of_ram_bank() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.write(0x4000, 0x11); // bank 5
        bus.write(0x8000, 0x22); // bank 2
        bus.write_io(0x7FFD, 0b0000_0111); // page bank 7 into 0xC000

        assert_eq!(bus.peek(0x4000), 0x11);
        assert_eq!(bus.peek(0x8000), 0x22);
    }

    #[test]
    fn spectrum128_rom_bank_selects_which_rom_half_is_visible() {
        let mut rom = vec![0u8; 0x8000];
        rom[0] = 0xAA; // bank 0's first byte
        rom[0x4000] = 0xBB; // bank 1's first byte
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, rom);

        assert_eq!(bus.peek(0x0000), 0xAA);
        bus.write_io(0x7FFD, 0b0001_0000); // rom_bank = 1
        assert_eq!(bus.peek(0x0000), 0xBB);
    }

    #[test]
    fn spectrum128_locked_paging_ignores_further_0x7ffd_writes() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.write_io(0x7FFD, 0b0010_0000); // lock, ram_bank stays 0
        bus.write_io(0x7FFD, 0b0000_0101); // should be ignored -- locked

        bus.load_ram_bank(0, &[0x77]);
        assert_eq!(bus.peek(0xC000), 0x77); // still bank 0, not 5
    }

    #[test]
    fn peek_screen_reads_shadow_bank_7_when_selected_independent_of_paged_bank() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.load_ram_bank(7, &[0x99]);
        bus.write_io(0x7FFD, 0b0000_1010); // shadow_screen=1, ram_bank=2 (not 7)

        // Bank 7 isn't paged anywhere in the CPU's address space right now,
        // but the ULA's own screen read must still see it.
        assert_eq!(bus.peek_screen(0x4000), 0x99);
    }

    #[test]
    fn peek_screen_reads_normal_bank_5_by_default() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.write(0x4000, 0x55); // bank 5, fixed

        assert_eq!(bus.peek_screen(0x4000), 0x55);
    }

    #[test]
    fn speaker_bit_reflects_the_last_write_on_both_models() {
        let mut bus48 = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert!(!bus48.speaker_level());
        bus48.write_io(0x00FE, 0b0001_0000);
        assert!(bus48.speaker_level());
        bus48.write_io(0x00FE, 0b0000_0000);
        assert!(!bus48.speaker_level());

        let mut bus128 = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus128.write_io(0x00FE, 0b0001_0000);
        assert!(bus128.speaker_level());
    }

    #[test]
    fn spectrum48_has_no_ay_chip() {
        let bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert!(bus.ay().is_none());
    }

    #[test]
    fn ay_register_select_and_write_round_trip_through_the_real_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.write_io(0xFFFD, 6); // select register 6 (noise period)
        bus.write_io(0xBFFD, 0x15); // write data
        bus.write_io(0xFFFD, 6); // re-select (read uses the same port)

        assert_eq!(bus.read_io(0xFFFD), 0x15);
    }

    #[test]
    fn turbosound_off_by_default_and_0xfe_is_just_register_15() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        assert!(bus.ay2().is_none());

        // Without turbosound, 0xFE is a completely ordinary register
        // select (register 14) -- must NOT be intercepted as a
        // chip-select, matching a plain 128K's real behavior exactly.
        bus.write_io(0xFFFD, 0xFE);
        bus.write_io(0xBFFD, 0x07);
        bus.write_io(0xFFFD, 0xFE);
        assert_eq!(bus.read_io(0xFFFD), 0x07);
    }

    #[test]
    fn turbosound_chip_select_switches_which_ay_is_addressed() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.set_turbosound(true);
        assert!(bus.ay2().is_some());

        // Register 14 (the AY's I/O port A) stores its value verbatim
        // with no bit-width masking (unlike e.g. register 6's real 5-bit
        // noise period), so it round-trips exactly -- picked here purely
        // to keep this test about chip *selection*, not register masking.
        //
        // Primary chip (default-active): register 14 = 0x11.
        bus.write_io(0xFFFD, 14);
        bus.write_io(0xBFFD, 0x11);

        // Select the second chip (val & 3 == 2, per the real Turbosound
        // convention verified against ZEsarUX's source), write a
        // DIFFERENT value to the same register 14.
        bus.write_io(0xFFFD, 0xFE);
        bus.write_io(0xFFFD, 14);
        bus.write_io(0xBFFD, 0x22);
        assert_eq!(bus.read_io(0xFFFD), 0x22);

        // Select the primary chip back (val & 3 == 3) and confirm its
        // register 14 still holds its own, untouched value -- the two
        // chips' register files are genuinely independent.
        bus.write_io(0xFFFD, 0xFF);
        bus.write_io(0xFFFD, 14);
        assert_eq!(bus.read_io(0xFFFD), 0x11);
    }

    #[test]
    fn turbosound_chip_select_value_is_not_also_treated_as_a_register_select() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.set_turbosound(true);

        // Select register 14 on the primary chip and give it a known
        // value.
        bus.write_io(0xFFFD, 14);
        bus.write_io(0xBFFD, 0x33);

        // A chip-select write that (redundantly) reselects the primary
        // chip -- already active, so this is a no-op *for chip
        // selection*. Its value, 0xFF, also happens to look like
        // "select register 15" (`0xFF & 0x0F == 15`) if the chip-select
        // branch didn't fully own this write instead of falling through
        // to `select_register`.
        bus.write_io(0xFFFD, 0xFF);

        // Deliberately no explicit re-select here: if the chip-select
        // write incorrectly ALSO called `select_register(0xFF)`, this
        // would read back register 15's own (untouched, `0`) value
        // instead of register 14's `0x33` -- proving the two branches
        // are truly mutually exclusive, not just coincidentally
        // agreeing in the previous test (which always re-selects a
        // register explicitly before reading).
        assert_eq!(bus.read_io(0xFFFD), 0x33);
    }

    #[test]
    fn turbosound_disabling_resets_to_the_primary_chip() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.set_turbosound(true);
        bus.write_io(0xFFFD, 0xFE); // select second chip
        bus.set_turbosound(false);
        assert!(bus.ay2().is_none());

        // Now that turbosound is off, 0xFE must go back to being a plain
        // register select (register 14) on the sole remaining chip.
        bus.write_io(0xFFFD, 0xFE);
        bus.write_io(0xBFFD, 0x44);
        bus.write_io(0xFFFD, 0xFE);
        assert_eq!(bus.read_io(0xFFFD), 0x44);
    }

    #[test]
    fn set_paging_restores_state_directly_bypassing_the_locked_check() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.set_paging(1, 4, true, true);

        bus.load_ram_bank(4, &[0x66]);
        assert_eq!(bus.peek(0xC000), 0x66);
    }

    #[test]
    fn spectrum2a3_has_four_rom_banks_selected_by_7ffd_and_1ffd_together() {
        let mut rom = vec![0u8; 0x10000];
        rom[0] = 0xAA; // ROM 0
        rom[0x4000] = 0xBB; // ROM 1 (7ffd bit4)
        rom[0x8000] = 0xCC; // ROM 2 (1ffd bit2)
        rom[0xC000] = 0xDD; // ROM 3 (both bits)
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, rom);

        assert_eq!(bus.peek(0x0000), 0xAA);

        bus.write_io(0x7FFD, 0b0001_0000); // rom bit lo = 1
        assert_eq!(bus.peek(0x0000), 0xBB);

        bus.write_io(0x7FFD, 0b0000_0000); // back to rom bit lo = 0
        bus.write_io(0x1FFD, 0b0000_0100); // rom bit hi = 1 (bit 2)
        assert_eq!(bus.peek(0x0000), 0xCC);

        bus.write_io(0x7FFD, 0b0001_0000); // rom bit lo = 1 too
        assert_eq!(bus.peek(0x0000), 0xDD);
    }

    #[test]
    fn spectrum2a3_pages_a_ram_bank_into_0xc000_via_0x7ffd_same_as_128k() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.load_ram_bank(3, &[0xAB]);

        bus.write_io(0x7FFD, 0b0000_0011); // ram_bank = 3
        assert_eq!(bus.peek(0xC000), 0xAB);
    }

    #[test]
    fn spectrum2a3_special_mode_maps_all_four_windows_to_the_documented_ram_banks() {
        // Verified against the official +3 manual's table: 1ffd bits (2,1)
        // = (0,0) -> RAM 0,1,2,3 across the whole 64K map, replacing ROM
        // entirely (even the "ROM" window becomes real, writable RAM).
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.write_io(0x1FFD, 0b0000_0001); // special_mode = 1, config bits (2,1) = 00

        bus.write(0x0000, 0x11); // now real RAM (bank 0), not ROM
        bus.write(0x4000, 0x22); // bank 1
        bus.write(0x8000, 0x33); // bank 2
        bus.write(0xC000, 0x44); // bank 3

        assert_eq!(bus.peek(0x0000), 0x11);
        assert_eq!(bus.peek(0x4000), 0x22);
        assert_eq!(bus.peek(0x8000), 0x33);
        assert_eq!(bus.peek(0xC000), 0x44);

        // Confirm it actually landed in RAM banks 0-3, not just readable at
        // those addresses by coincidence -- load_ram_bank writes the same
        // underlying array `write` above must have used.
        bus.load_ram_bank(1, &[0x99]);
        assert_eq!(bus.peek(0x4000), 0x99);
    }

    #[test]
    fn spectrum2a3_special_mode_config_3_matches_the_documented_bank_layout() {
        // 1ffd bits (2,1) = (1,1) -> RAM 4,7,6,3 per the +3 manual's table.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.load_ram_bank(4, &[0x44]);
        bus.load_ram_bank(7, &[0x77]);
        bus.load_ram_bank(6, &[0x66]);
        bus.load_ram_bank(3, &[0x33]);

        bus.write_io(0x1FFD, 0b0000_0111); // special_mode=1, bits(2,1)=11

        assert_eq!(bus.peek(0x0000), 0x44);
        assert_eq!(bus.peek(0x4000), 0x77);
        assert_eq!(bus.peek(0x8000), 0x66);
        assert_eq!(bus.peek(0xC000), 0x33);
    }

    #[test]
    fn spectrum2a3_leaving_special_mode_restores_normal_rom_ram_mapping() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.write_io(0x1FFD, 0b0000_0001); // enter special mode
        bus.write_io(0x1FFD, 0b0000_0000); // leave special mode

        bus.load_ram_bank(0, &[0x11]); // must NOT show at 0x0000 anymore
        assert_ne!(bus.peek(0x0000), 0x11); // 0x0000 is ROM again (all-zero ROM)
        assert_eq!(bus.peek(0x0000), 0x00);
    }

    #[test]
    fn spectrum2a3_locked_paging_ignores_further_writes_to_both_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.write_io(0x7FFD, 0b0010_0000); // lock, ram_bank stays 0
        bus.write_io(0x7FFD, 0b0000_0101); // should be ignored -- locked
        bus.write_io(0x1FFD, 0b0000_0001); // should also be ignored -- locked

        bus.load_ram_bank(0, &[0x77]);
        assert_eq!(bus.peek(0xC000), 0x77); // still bank 0
        assert_eq!(bus.peek(0x0000), 0x00); // still normal mode (ROM), not special
    }

    #[test]
    fn spectrum2a3_has_an_ay_chip_like_128k() {
        let bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        assert!(bus.ay().is_some());
    }

    #[test]
    fn spectrum2a3_banks_5_and_2_stay_fixed_in_normal_mode() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.write(0x4000, 0x11); // bank 5
        bus.write(0x8000, 0x22); // bank 2
        bus.write_io(0x7FFD, 0b0000_0111); // page bank 7 into 0xC000

        assert_eq!(bus.peek(0x4000), 0x11);
        assert_eq!(bus.peek(0x8000), 0x22);
    }

    #[test]
    fn tape_ear_bit_is_idle_high_with_no_tape_installed() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        // Bit 6 set (idle) even before any tape is ever installed.
        assert_eq!(bus.read_io(0x00FE) & 0b0100_0000, 0b0100_0000);
    }

    #[test]
    fn tape_first_read_auto_starts_playback_and_bit6_reflects_the_first_pulse() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        let tape = zx_tape::parse_tap(&[0x01, 0x00, 0x00]).unwrap(); // starts level=false
        bus.load_tape(tape);

        // Not playing yet -- idle high.
        // (No direct "is playing" accessor; verified indirectly via
        // tick_tape + a second read below, matching how AY state is
        // tested elsewhere in this file.)
        let first_read = bus.read_io(0x00FE);
        assert_eq!(first_read & 0b0100_0000, 0); // first pulse is level=false

        bus.tick_tape(2168); // exactly one pilot pulse -- now level=true
        let second_read = bus.read_io(0x00FE);
        assert_eq!(second_read & 0b0100_0000, 0b0100_0000);
    }

    #[test]
    fn tape_speed_multiplier_is_applied() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        let tape = zx_tape::parse_tap(&[0x01, 0x00, 0x00]).unwrap();
        bus.load_tape(tape);
        bus.set_tape_speed(4);

        bus.read_io(0x00FE); // auto-starts playback
        bus.tick_tape(2168 / 4); // 1/4 the real cycles, but 4x speed -> one full pulse
        let read = bus.read_io(0x00FE);
        assert_eq!(read & 0b0100_0000, 0b0100_0000); // already on the second pulse (level=true)
    }

    #[test]
    fn tape_does_not_affect_keyboard_bits_of_the_same_read() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.press_key(ZxSpectrumKey::Q); // row 2 (A10), bit 0
        let tape = zx_tape::parse_tap(&[0x01, 0x00, 0xFF]).unwrap(); // level=false first
        bus.load_tape(tape);

        // Row-2 select mask, same convention as this file's other keyboard tests.
        let value = bus.read_io(0b1111_1011_1111_1110);
        assert_eq!(value & 0b0000_0001, 0); // Q still reads pressed
        assert_eq!(value & 0b0100_0000, 0); // tape bit still correct
    }

    fn build_minimal_dsk() -> Vec<u8> {
        // Same minimal single-track, single-512-byte-sector construction
        // as `dsk`'s own tests -- duplicated here since this is a
        // bus-level integration test and shouldn't depend on another
        // crate's #[cfg(test)]-only helper.
        const DISC_SIGNATURE: &[u8] = b"MV - CPCEMU Disk-File\r\nDisk-Info\r\n";
        const TRACK_SIGNATURE: &[u8] = b"Track-Info\r\n";
        const BLOCK_SIZE: usize = 0x100;

        let mut bytes = vec![0u8; BLOCK_SIZE];
        bytes[..DISC_SIGNATURE.len()].copy_from_slice(DISC_SIGNATURE);
        bytes[0x30] = 1;
        bytes[0x31] = 1;
        let track_size = (BLOCK_SIZE + 512) as u16;
        bytes[0x32..0x34].copy_from_slice(&track_size.to_le_bytes());

        let mut track = vec![0u8; BLOCK_SIZE];
        track[..TRACK_SIGNATURE.len()].copy_from_slice(TRACK_SIGNATURE);
        track[0x14] = 2;
        track[0x15] = 1;
        track[0x18] = 0; // C
        track[0x19] = 0; // H
        track[0x1A] = 1; // R
        track[0x1B] = 2; // N
        bytes.extend(track);

        let mut sector_data = vec![0xAAu8; 512];
        sector_data[0] = 0x11;
        bytes.extend(sector_data);

        bytes
    }

    #[test]
    fn only_spectrum2a3_decodes_the_fdc_ports() {
        let mut bus48 = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        let mut bus2a3 = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);

        assert_eq!(bus48.read_io(0x2FFD), 0xFF); // no FDC at all on 48K -- unmapped-read convention
        assert_eq!(bus2a3.read_io(0x2FFD), 0x80); // idle MSR: RQM=1, DIO=0
    }

    #[test]
    fn fdc_command_protocol_round_trips_through_the_real_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        let disk = dsk::parse(&build_minimal_dsk()).unwrap();
        bus.load_disk(disk);
        bus.write_io(0x1FFD, 0b0000_1000); // +3 disk motor on -- the drive isn't ready otherwise

        bus.write_io(0x3FFD, 0x0F); // Seek
        bus.write_io(0x3FFD, 0x00); // HD/US1/US0
        bus.write_io(0x3FFD, 0); // NCN = cylinder 0

        bus.write_io(0x3FFD, 0x46); // Read Data, MFM (what the +3 ROM really sends)
        bus.write_io(0x3FFD, 0x00); // HD/US1/US0
        bus.write_io(0x3FFD, 0); // C
        bus.write_io(0x3FFD, 0); // H
        bus.write_io(0x3FFD, 1); // R
        bus.write_io(0x3FFD, 2); // N
        bus.write_io(0x3FFD, 1); // EOT
        bus.write_io(0x3FFD, 0x1B); // GPL
        bus.write_io(0x3FFD, 0xFF); // DTL

        assert_eq!(bus.read_io(0x2FFD), 0x10, "first poll: not ready yet");
        assert_eq!(
            bus.read_io(0x2FFD),
            0xF0,
            "execution phase, controller -> CPU"
        );
        assert_eq!(bus.read_io(0x3FFD), 0x11); // the sector's own first byte
    }

    #[test]
    fn the_drive_is_not_ready_until_the_motor_is_switched_on() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.load_disk(dsk::parse(&build_minimal_dsk()).unwrap());
        bus.write_io(0x3FFD, 0x04); // Sense Drive Status
        bus.write_io(0x3FFD, 0x00);
        assert_eq!(
            bus.read_io(0x3FFD) & 0x20,
            0,
            "no RY while the motor is off"
        );
        bus.write_io(0x1FFD, 0b0000_1000);
        bus.write_io(0x3FFD, 0x04);
        bus.write_io(0x3FFD, 0x00);
        assert_eq!(bus.read_io(0x3FFD) & 0x20, 0x20, "RY once the motor spins");
    }

    #[test]
    fn plus3_disk_writes_are_persisted_and_survive_a_save_state() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.load_disk(dsk::parse(&build_minimal_dsk()).unwrap());
        bus.write_io(0x1FFD, 0b0000_1000);
        assert!(
            bus.modified_disk_image().is_none(),
            "untouched disk has nothing to save"
        );
        for b in [0x45, 0, 0, 0, 1, 2, 1, 0x2A, 0xFF] {
            bus.write_io(0x3FFD, b); // Write Data
        }
        bus.read_io(0x2FFD); // consume the not-ready poll
        for _ in 0..512 {
            bus.write_io(0x3FFD, 0x6B);
        }
        let image = bus
            .modified_disk_image()
            .expect("a written disk is modified");
        assert_eq!(
            dsk::parse(&image)
                .unwrap()
                .read_sector(0, 0, 1)
                .unwrap()
                .data,
            vec![0x6B; 512]
        );
        bus.mark_disk_saved();
        assert!(
            bus.modified_disk_image().is_none(),
            "saved changes aren't unsaved again"
        );
    }

    #[test]
    fn writing_the_fdc_data_port_does_not_corrupt_paging_state() {
        // 0x3ffd satisfies 0x7ffd's laxer 0x8002==0 mask too (same
        // overlap 0x1ffd already has) -- confirms the FDC branch takes
        // priority and paging is left untouched.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.load_ram_bank(3, &[0xAB]);
        bus.write_io(0x3FFD, 0x08); // Sense Interrupt Status -- an arbitrary real command byte

        bus.write_io(0x7FFD, 0b0000_0011); // ram_bank = 3, the real paging port
        assert_eq!(bus.peek(0xC000), 0xAB); // still worked -- 0x3ffd's earlier write didn't corrupt paging state
    }

    #[test]
    fn kempston_reads_back_the_pressed_buttons_bit_pattern() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert_eq!(bus.read_io(0x1F), 0x00); // idle

        bus.press_kempston(KempstonButton::Right);
        bus.press_kempston(KempstonButton::Fire);
        assert_eq!(bus.read_io(0x1F), 0b0001_0001);

        bus.release_kempston(KempstonButton::Right);
        assert_eq!(bus.read_io(0x1F), 0b0001_0000);
    }

    #[test]
    fn kempston_responds_on_every_mirror_with_bit5_of_the_port_clear() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.press_kempston(KempstonButton::Up);
        assert_eq!(bus.read_io(0x1F), 0b0000_1000);
        assert_eq!(bus.read_io(0x9F), 0b0000_1000); // another bit5-clear mirror (0x9F = 1001_1111)
    }

    #[test]
    fn kempston_does_not_intercept_a_real_keyboard_scan_port() {
        // 0xFE (and any real keyboard scan address, which always has a low
        // byte of 0xFE) has bit 5 set, so it's never mistaken for Kempston.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.press_kempston(KempstonButton::Fire);
        assert_eq!(bus.read_io(0xFEFE), 0xFF); // idle-high keyboard read, unaffected by Kempston state
    }

    #[test]
    fn releasing_an_already_released_kempston_button_is_a_no_op() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.release_kempston(KempstonButton::Up);
        assert_eq!(bus.read_io(0x1F), 0x00);
    }

    #[test]
    fn fuller_reads_back_the_pressed_buttons_bit_pattern_active_low() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert_eq!(bus.read_io(0x7F), 0xFF); // idle -- active low, so all-1s

        bus.press_fuller(FullerButton::Right);
        bus.press_fuller(FullerButton::Fire);
        assert_eq!(bus.read_io(0x7F), 0b0111_0111); // bits 7 and 3 cleared

        bus.release_fuller(FullerButton::Right);
        assert_eq!(bus.read_io(0x7F), 0b0111_1111); // only bit 7 (Fire) still clear
    }

    #[test]
    fn fuller_does_not_intercept_the_kempston_or_keyboard_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.press_fuller(FullerButton::Fire);
        assert_eq!(bus.read_io(0x1F), 0x00); // Kempston idle, unaffected
        assert_eq!(bus.read_io(0xFEFE), 0xFF); // keyboard idle-high, unaffected
    }

    #[test]
    fn releasing_an_already_released_fuller_button_is_a_no_op() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.release_fuller(FullerButton::Up);
        assert_eq!(bus.read_io(0x7F), 0xFF);
    }

    #[test]
    fn mouse_x_and_y_wrap_and_read_back_on_their_own_real_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert_eq!(bus.read_io(0xFBDF), 0); // X idle
        assert_eq!(bus.read_io(0xFFDF), 0); // Y idle

        bus.move_mouse(5, -3);
        assert_eq!(bus.read_io(0xFBDF), 5);
        assert_eq!(bus.read_io(0xFFDF), (-3i8) as u8); // wraps to 0xFD

        bus.move_mouse(-10, 0);
        assert_eq!(bus.read_io(0xFBDF), (5i8 - 10) as u8); // 5 + (-10) wraps
    }

    #[test]
    fn mouse_buttons_read_back_active_low_on_the_real_button_port() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        assert_eq!(bus.read_io(0xFADF), 0x0F); // idle -- active low, low nibble all-1s

        bus.press_mouse_button(MouseButton::Left);
        assert_eq!(bus.read_io(0xFADF), 0x0F & !0b0000_0010);

        bus.release_mouse_button(MouseButton::Left);
        assert_eq!(bus.read_io(0xFADF), 0x0F);
    }

    #[test]
    fn mouse_wheel_reports_ticks_since_last_read_then_resets() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.scroll_mouse(1);
        bus.scroll_mouse(1);
        bus.scroll_mouse(1);
        assert_eq!(bus.read_io(0xFADF) >> 4, 3);
        // Reading it once already reset the accumulator -- a second read
        // with no further scrolling must report 0, not the same 3 again.
        assert_eq!(bus.read_io(0xFADF) >> 4, 0);
    }

    #[test]
    fn mouse_never_intercepts_the_kempston_joystick_or_fuller_ports() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.move_mouse(7, 7);
        bus.press_mouse_button(MouseButton::Left);
        bus.press_kempston(KempstonButton::Fire);
        bus.press_fuller(FullerButton::Fire);

        assert_eq!(bus.read_io(0x1F), 0b0001_0000); // Kempston: only Fire set
        assert_eq!(bus.read_io(0x7F), 0b0111_1111); // Fuller: only Fire (bit7) clear
    }

    /// Real regression: an earlier version of the mouse X-port decode
    /// checked only bits 10:8 of the port (`== 0b011`), omitting the real
    /// table's own bit-5-clear requirement. Port `0xFBFE` -- a completely
    /// ordinary real keyboard-scan address (row-select byte `0xFB`,
    /// low byte `0xFE`) -- has bits 10:8 == 0b011 too (from `0xFB`'s own
    /// low 3 bits), so the incomplete decode swallowed real keyboard
    /// reads whenever the row mask happened to end in those bits, caught
    /// by this crate's own pre-existing keyboard tests failing after the
    /// mouse port decode was added, not by a mouse-specific test. Locked
    /// in here so it can't silently regress again.
    #[test]
    fn mouse_x_decode_does_not_swallow_a_keyboard_row_whose_mask_ends_in_011() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.move_mouse(9, 0); // mouse_x != 0, so a wrongly-matched read would be visibly wrong
        // Real keyboard idle-high read on row-select byte 0xFB (bits 10:8
        // of the resulting port == 0b011, same as the mouse X port).
        assert_eq!(bus.read_io(0xFBFE), 0xFF);
    }
}

#[cfg(test)]
mod contention_tests {
    use super::*;

    #[test]
    fn spectrum48_contends_screen_ram_but_not_rom_or_upper_ram() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start;

        assert_eq!(bus.contend_memory(0x4000), 6); // first T-state of the pattern
        assert_eq!(bus.contend_memory(0x7FFF), 6); // top of the contended range
        assert_eq!(bus.contend_memory(0x0000), 0); // ROM: never contended
        assert_eq!(bus.contend_memory(0x8000), 0); // above 0x7fff: never contended on 48K
    }

    #[test]
    fn spectrum48_pattern_repeats_every_8_tstates_and_flattens_to_zero() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        let start = CONTENTION_48K.start;

        let expected = [6u8, 5, 4, 3, 2, 1, 0, 0];
        for (i, &want) in expected.iter().enumerate() {
            bus.frame_tstate = start + i as u32;
            assert_eq!(bus.contend_memory(0x4000), want, "offset {i}");
        }
        // One full pattern cycle later, same delay.
        bus.frame_tstate = start + 8;
        assert_eq!(bus.contend_memory(0x4000), 6);
    }

    #[test]
    fn spectrum48_no_contention_before_screen_drawing_starts_or_during_border() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);

        bus.frame_tstate = CONTENTION_48K.start - 1;
        assert_eq!(bus.contend_memory(0x4000), 0);

        // Past the active 128-T-state window into that line's uncontended
        // border/retrace tail.
        bus.frame_tstate = CONTENTION_48K.start + CONTENTION_48K.active_window;
        assert_eq!(bus.contend_memory(0x4000), 0);

        // Past all 192 screen lines entirely.
        bus.frame_tstate =
            CONTENTION_48K.start + CONTENTION_48K.period * CONTENDED_SCREEN_LINES + 1;
        assert_eq!(bus.contend_memory(0x4000), 0);
    }

    #[test]
    fn spectrum128_contends_bank5_and_odd_banks_paged_at_0xc000_but_not_bank2() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum128, vec![0u8; crate::ROM_SIZE_128K]);
        bus.frame_tstate = CONTENTION_128K.start;

        assert_eq!(bus.contend_memory(0x4000), 6); // bank 5, fixed, always contended
        assert_eq!(bus.contend_memory(0x8000), 0); // bank 2, never contended

        bus.write_io(0x7FFD, 0b0000_0011); // ram_bank = 3 (odd -> contended)
        assert_eq!(bus.contend_memory(0xC000), 6);

        bus.write_io(0x7FFD, 0b0000_0010); // ram_bank = 2 (even -> not contended)
        assert_eq!(bus.contend_memory(0xC000), 0);
    }

    #[test]
    fn spectrum2a3_contends_pages_4_5_6_7_not_1_3_5_7() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.frame_tstate = CONTENTION_2A3.start;

        assert_eq!(bus.contend_memory(0x4000), 1); // bank 5, fixed, contended (first pattern value)
        assert_eq!(bus.contend_memory(0x8000), 0); // bank 2, never contended

        bus.write_io(0x7FFD, 0b0000_0001); // ram_bank = 1 (odd, but NOT in the 2A3's 4-7 set)
        assert_eq!(bus.contend_memory(0xC000), 0);

        bus.write_io(0x7FFD, 0b0000_0100); // ram_bank = 4 (in the 2A3's contended set)
        assert_eq!(bus.contend_memory(0xC000), 1);
    }

    #[test]
    fn reset_frame_tstate_returns_to_the_start_of_the_frame() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.advance_tstates(200);
        bus.reset_frame_tstate();
        assert_eq!(bus.frame_tstate, 0);
    }

    #[test]
    fn advance_tstates_accumulates() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.advance_tstates(10);
        bus.advance_tstates(7);
        assert_eq!(bus.frame_tstate, 17);
    }

    #[test]
    fn step_returns_more_tstates_for_the_same_instruction_when_it_runs_from_contended_memory_during_screen_draw()
     {
        // LD A,(HL) is 7 T-states with zero contention. Placed at 0x4000
        // (contended on every model) and stepped while `frame_tstate` sits
        // inside the active contended window, it should cost more.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.write(0x4000, 0x7E); // LD A,(HL)
        bus.write(0x8000, 0x00); // HL points at uncontended RAM -- isolates the fetch's own delay

        let mut cpu = z80::Cpu::new();
        cpu.pc = 0x4000;
        cpu.hl = 0x8000;

        bus.frame_tstate = CONTENTION_48K.start; // peak delay (6) at the very first T-state
        let contended_cost = cpu.step(&mut bus);
        assert!(
            contended_cost > 7,
            "expected more than the base 7 T-states, got {contended_cost}"
        );

        // Reset PC/frame position into the uncontended border area: same
        // instruction, base cost only.
        let mut bus2 = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus2.write(0x4000, 0x7E);
        bus2.write(0x8000, 0x00);
        let mut cpu2 = z80::Cpu::new();
        cpu2.pc = 0x4000;
        cpu2.hl = 0x8000;
        let uncontended_cost = cpu2.step(&mut bus2);
        assert_eq!(uncontended_cost, 7);
    }

    // --- I/O contention: the 4-branch model (high byte contended? x bit 0
    // set?), each verified against a hand-worked-out expected total using
    // the same 48K `6,5,4,3,2,1,0,0` pattern the memory-contention tests
    // above already exercise. ---

    #[test]
    fn io_contention_uncontended_high_byte_and_odd_port_is_never_contended() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start;
        assert_eq!(bus.contend_io(0xFFFF), 0); // N:4
    }

    #[test]
    fn io_contention_uncontended_high_byte_and_even_port_is_n1_c3() {
        // 0xFFFE: high byte 0xFF not contended, bit0 clear -- N:1, C:3. The
        // single check lands one T-state after `start` (pattern[1] = 5).
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start;
        assert_eq!(bus.contend_io(0xFFFE), 5);
    }

    #[test]
    fn io_contention_contended_high_byte_and_even_port_is_c1_c3() {
        // 0x7FFE: in the contended 0x4000-0x7fff range, bit0 clear -- C:1,
        // C:3. First check at `start` (pattern[0] = 6), second at
        // `start + 6 + 1` = `start + 7` (pattern[7] = 0).
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start;
        assert_eq!(bus.contend_io(0x7FFE), 6);
    }

    #[test]
    fn io_contention_contended_high_byte_and_odd_port_is_the_worst_case_c1x4() {
        // 0x7FFF: in the contended range, bit0 set -- C:1, C:1, C:1, C:1,
        // four independent checks. Positions: start(6) -> start+7(0) ->
        // start+8(6, wraps the 8-T pattern) -> start+15(0). Total 12.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start;
        assert_eq!(bus.contend_io(0x7FFF), 12);
    }

    #[test]
    fn io_contention_outside_the_screen_drawing_window_is_always_zero() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = 0; // long before the frame's contended window starts
        assert_eq!(bus.contend_io(0x7FFF), 0);
    }

    #[test]
    fn spectrum2a3_has_no_io_contention_at_all() {
        // The Amstrad gate array only contends while MREQ is active, which
        // it never is during an I/O cycle -- every port, at any frame
        // position, is always uncontended on this model.
        let mut bus = ZxSpectrumBus::new(Model::Spectrum2A3, vec![0u8; crate::ROM_SIZE_2A3]);
        bus.frame_tstate = CONTENTION_2A3.start;
        assert_eq!(bus.contend_io(0x7FFF), 0);
        assert_eq!(bus.contend_io(0x7FFE), 0);
        assert_eq!(bus.contend_io(0xFFFE), 0);
    }

    // --- Floating bus: reading a genuinely unmapped port (any odd port not
    // decoded by AY/FDC/keyboard) returns whatever byte the ULA is
    // currently outputting to the video data bus, per the real
    // bitmap/attribute/bitmap+1/attribute+1 fetch sequence at local offsets
    // 3/4/5/6 of each 8-T-state block. 0xFFFF is used throughout as "an
    // odd port nothing else decodes" on a 48K bus (no AY/FDC to collide
    // with).

    #[test]
    fn floating_bus_returns_idle_high_before_the_screen_drawing_window_starts() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate = CONTENTION_48K.start - 1;
        assert_eq!(bus.read_io(0xFFFF), 0xFF);
    }

    #[test]
    fn floating_bus_returns_the_current_cells_bitmap_and_attribute_bytes() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.write(video::pixel_address(0, 0), 0x11); // row 0, col 0 bitmap
        bus.write(video::attr_address(0, 0), 0x22); // row 0, col 0 attribute
        bus.write(video::pixel_address(0, 1), 0x33); // row 0, col 1 (the pair's second cell) bitmap
        bus.write(video::attr_address(0, 1), 0x44); // row 0, col 1 attribute

        let start = CONTENTION_48K.start;

        bus.frame_tstate = start; // local offset 0: idle
        assert_eq!(bus.read_io(0xFFFF), 0xFF);
        bus.frame_tstate = start + 3; // local offset 3: bitmap
        assert_eq!(bus.read_io(0xFFFF), 0x11);
        bus.frame_tstate = start + 4; // local offset 4: attribute
        assert_eq!(bus.read_io(0xFFFF), 0x22);
        bus.frame_tstate = start + 5; // local offset 5: bitmap+1
        assert_eq!(bus.read_io(0xFFFF), 0x33);
        bus.frame_tstate = start + 6; // local offset 6: attribute+1
        assert_eq!(bus.read_io(0xFFFF), 0x44);
        bus.frame_tstate = start + 7; // local offset 7: idle again
        assert_eq!(bus.read_io(0xFFFF), 0xFF);
    }

    #[test]
    fn floating_bus_returns_idle_high_once_past_the_screen_drawing_window() {
        let mut bus = ZxSpectrumBus::new(Model::Spectrum48, vec![0u8; crate::ROM_SIZE]);
        bus.frame_tstate =
            CONTENTION_48K.start + CONTENTION_48K.period * CONTENDED_SCREEN_LINES + 1;
        assert_eq!(bus.read_io(0xFFFF), 0xFF);
    }
}
