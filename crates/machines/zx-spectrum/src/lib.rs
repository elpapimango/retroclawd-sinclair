mod bus;
mod keyboard;
mod save_state;
mod video;

use machine::{Bus, Clock, Machine, Region, RunStats};
use z80::Cpu;

use bus::ZxSpectrumBus;
pub use bus::{FullerButton, KempstonButton, MouseButton};
pub use keyboard::ZxSpectrumKey;

/// ZX Spectrum 48K ROM size (16K).
pub const ROM_SIZE: usize = 0x4000;
/// ZX Spectrum 128K/+2 ROM size (32K: the 128K editor ROM followed by the
/// 48K-compatible BASIC ROM, banks 0 and 1 respectively).
pub const ROM_SIZE_128K: usize = 0x8000;
/// ZX Spectrum +2A/+3 ROM size (64K: 4x16K banks). Real bank contents/order
/// verified against the official ZX Spectrum +3 manual (see CREDITS.md),
/// not guessed.
pub const ROM_SIZE_2A3: usize = 0x10000;
/// Fixed RAM base address on real hardware (48K only -- 128K has no single
/// contiguous RAM region, see `bus::Memory::Banked`).
pub const RAM_BASE: u16 = 0x4000;
/// ZX Spectrum 48K RAM size.
pub const RAM_SIZE: usize = 0xC000;

/// Which real ZX Spectrum model a `ZxSpectrum` is emulating -- determines
/// ROM size, memory layout (banked 128K/+2A/+3 vs flat 48K, see
/// `bus::Memory`), and which `.z80` snapshot hardware modes it can load
/// (see `load_snapshot`). Spectrum +2 uses `Spectrum128` -- its memory
/// model (8x16K RAM banks, 2 ROMs, `0x7ffd` paging) is identical to plain
/// 128K; the +2 designation is about the case/keyboard/built-in tape deck,
/// none of which this project models. `Spectrum2A3` is likewise shared by
/// the +2A and +3 -- both have the identical 4-ROM/`0x1ffd`-paged memory
/// model (verified against the official ZX Spectrum +3 manual, see
/// CREDITS.md); the +3's floppy disk controller is a separate, not yet
/// implemented concern (`bus::ZxSpectrumBus` tracks the `0x1ffd` disk-motor
/// bit already, since it's part of the same register, but nothing acts on
/// it yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Model {
    Spectrum48,
    Spectrum128,
    Spectrum2A3,
}

/// Cosmetic audio-shaping preference for `mix_audio_sample`'s output --
/// Fuse's libretro core exposes an equivalent "Speaker Type" option (see
/// `TODO.md`'s gap-analysis section, `CREDITS.md`). `Unfiltered` (the
/// default, and this codebase's only behavior before this option existed)
/// passes the raw mixed sample through untouched; `Beeper`/`TvSpeaker`
/// apply a simple one-pole low-pass filter, approximating how a small
/// speaker or a TV's own speaker each roll off a raw square wave's harsh
/// high-frequency edges. The two cutoff frequencies are reasonable
/// illustrative engineering choices, not measured/verified real hardware
/// or Fuse-algorithm values -- same epistemic status as `video::
/// NORMAL_LEVEL`/`BRIGHT_LEVEL`'s own "a commonly used convention, not a
/// hardware fact" framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum SpeakerType {
    #[default]
    Unfiltered,
    Beeper,
    TvSpeaker,
}

impl SpeakerType {
    /// Low-pass cutoff frequency, in Hz -- `None` for `Unfiltered` (no
    /// filtering at all). A smaller cutoff (`TvSpeaker`) sounds more
    /// muffled/rounded than a larger one (`Beeper`, closer to the raw
    /// signal).
    fn cutoff_hz(self) -> Option<f32> {
        match self {
            SpeakerType::Unfiltered => None,
            SpeakerType::Beeper => Some(8_000.0),
            SpeakerType::TvSpeaker => Some(5_000.0),
        }
    }
}

/// Which joystick convention arrow-keys+fire (`any_machine::key_event`'s
/// own GUI-side mapping) currently drives -- real software of the era
/// asked the player to pick one of these at startup, so this is a config
/// knob (`set_joystick_type`, defaults `Kempston`), not something all four
/// drive simultaneously. `Sinclair1`/`Sinclair2`/`Cursor` need no bus
/// state of their own at all: real Sinclair/Cursor/Protek/AGF joysticks
/// were wired straight onto specific keyboard-matrix keys (verified
/// against World of Spectrum's own peripherals reference, see
/// `CREDITS.md`), which this crate's existing, already-tested keyboard
/// matrix (`press_key`/`release_key`) already reads correctly -- so
/// selecting one of these just changes which `ZxSpectrumKey`s the GUI's
/// arrow-keys+fire synthesize, the same real keys typing e.g. "1" already
/// presses. `Fuller` is the one real exception: an actual separate I/O
/// port, see `bus::FullerButton`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum JoystickType {
    #[default]
    Kempston,
    /// Left joystick: keys 1(left)/2(right)/3(down)/4(up)/5(fire), read
    /// via port `0xf7fe`.
    Sinclair1,
    /// Right joystick: keys 6(left)/7(right)/8(down)/9(up)/0(fire), read
    /// via port `0xeffe`.
    Sinclair2,
    /// Keys 5(left)/6(down)/7(up)/8(right)/0(fire) -- the Cursor/Protek/AGF
    /// convention, spanning both Sinclair joystick ports' keys.
    Cursor,
    /// Fuller Box joystick -- a real, separate I/O port (`0x7f`), unlike
    /// the three above. See `bus::FullerButton`.
    Fuller,
}

/// A ZX Spectrum: a Z80 core, ROM/RAM per `Model`, a keyboard matrix, and
/// video output (`render_frame`, decoded from the bitmap+attribute screen
/// memory). No tape/sound yet either model.
#[derive(Clone)]
pub struct ZxSpectrum {
    cpu: Cpu,
    bus: ZxSpectrumBus,
    model: Model,
    region: Region,
    /// Whether `render_frame` pads the active picture with a border in the
    /// ULA's current border color (see `video::BORDER_PX`). On by default
    /// -- `set_border` is how a caller (e.g. `retroclawd --no-border`)
    /// turns it off. Not a `new()` parameter deliberately: nearly every
    /// caller (tests included) wants the default, and a setter avoids
    /// touching every existing call site.
    border: bool,
    /// Frames since the FLASH attribute last toggled ink/paper -- ticked
    /// once per real interrupt in `request_interrupt` (i.e. once per real
    /// video frame), matching the real ULA's own frame-driven FLASH
    /// counter. See `video::FLASH_TOGGLE_FRAMES`.
    flash_counter: u8,
    /// Current FLASH half-cycle: `render_frame` swaps ink/paper for
    /// FLASH-attributed cells while this is `true`.
    flash_phase: bool,
    /// CPU cycles (T-states) accumulated since the last audio sample was
    /// emitted, as a fractional accumulator (`clock_hz` doesn't divide
    /// `machine::AUDIO_SAMPLE_RATE` evenly -- an integer accumulator would
    /// drift the sample rate slightly flat over a long run). `f64` for the
    /// same reason: samples emitted over minutes of play need the
    /// remainder to stay precise, not just per-instruction.
    audio_cycle_accumulator: f64,
    /// Real AY-3-8910 clock is CPU clock / 2 on the ZX Spectrum 128K
    /// (~1.7734MHz vs 3.5469MHz) -- tracked as a separate cycle debt
    /// (rather than dividing each instruction's cycle count by 2 on the
    /// spot) so the odd-numbered T-state counts most Z80 instructions take
    /// don't lose half a cycle every other instruction.
    ay_cycle_debt: u32,
    /// Samples generated since the last `drain_audio` call, at
    /// `machine::AUDIO_SAMPLE_RATE`, mono, `-1.0..=1.0`.
    audio_buffer: Vec<f32>,
    /// Whether `step` should trap the ROM's own tape-loading routine (see
    /// `try_fast_tape_load_trap`) instead of letting it bit-bang through
    /// real-time pulses. Off by default -- opt-in via `set_fast_tape_load`,
    /// same "explicit, not surprising" stance as `set_tape_speed`'s own
    /// multiplier defaulting to real time.
    fast_tape_load: bool,
    /// See `SpeakerType`'s own doc comment.
    speaker_type: SpeakerType,
    /// Running low-pass filter state (previous output sample) for
    /// `mix_audio_sample`, used only when `speaker_type != Unfiltered`.
    speaker_filter_state: f32,
    /// See `JoystickType`'s own doc comment.
    joystick_type: JoystickType,
}

impl ZxSpectrum {
    /// `rom` is the real Spectrum ROM image for `model` (16K for
    /// `Spectrum48`, 32K for `Spectrum128`), supplied externally -- it's
    /// copyrighted and not bundled with this repo. `region` selects the
    /// market variant's clock/frame rate (see `Machine::clock_hz`/
    /// `frame_hz` below). Doesn't itself validate `rom.len()` against
    /// `ROM_SIZE`/`ROM_SIZE_128K` -- `retroclawd`'s CLI layer does that
    /// (same pattern as everywhere else ROMs are loaded in this codebase);
    /// a short ROM is silently zero-padded by `ZxSpectrumBus::new` rather
    /// than panicking.
    pub fn new(rom: Vec<u8>, region: Region, model: Model) -> Self {
        let mut bus = ZxSpectrumBus::new(model, rom);
        bus.set_tape_start_on_any_read(false);
        Self {
            cpu: Cpu::new(),
            bus,
            model,
            region,
            border: true,
            flash_counter: 0,
            flash_phase: false,
            audio_cycle_accumulator: 0.0,
            ay_cycle_debt: 0,
            audio_buffer: Vec::new(),
            fast_tape_load: false,
            speaker_type: SpeakerType::default(),
            speaker_filter_state: 0.0,
            joystick_type: JoystickType::default(),
        }
    }

    /// See the `border` field's doc comment.
    pub fn set_border(&mut self, enabled: bool) {
        self.border = enabled;
    }
    /// Sets one CPU register by name (`PC`, `AF`, `BC`, `DE`, `HL`, `IX`,
    /// `IY`, `SP`, `I`, `R`, `IM`, `IFF1`, `IFF2`, shadow pairs, and
    /// 8-bit aliases). Names are case-insensitive.
    pub fn set_register(&mut self, name: &str, value: u64) -> Result<(), String> {
        self.cpu.set_register(name, value)
    }

    /// See `SpeakerType`'s own doc comment.
    pub fn set_speaker_type(&mut self, speaker_type: SpeakerType) {
        self.speaker_type = speaker_type;
    }

    /// See `JoystickType`'s own doc comment.
    pub fn set_joystick_type(&mut self, joystick_type: JoystickType) {
        self.joystick_type = joystick_type;
    }

    /// Enables/disables the Turbosound mod's second AY-3-8910 -- a no-op
    /// on `Model::Spectrum48` (no AY at all there), same "ignored where
    /// it doesn't apply" convention `set_speaker_type`/`set_joystick_type`
    /// already use. See `bus::ZxSpectrumBus::set_turbosound`'s own doc
    /// comment for the real chip-select protocol this enables.
    pub fn set_turbosound(&mut self, enabled: bool) {
        self.bus.set_turbosound(enabled);
    }

    /// See `JoystickType`'s own doc comment.
    pub fn joystick_type(&self) -> JoystickType {
        self.joystick_type
    }

    /// Installs a tape for loading -- see `crates/zx-tape`. Replaces any
    /// tape already installed. Playback starts when the ROM's `LD-BYTES`
    /// loader is first entered (like pressing Play when `LOAD` asks for
    /// the tape), not on install and not at power-on; see
    /// `set_tape_start_on_any_read` for the older behaviour.
    pub fn load_tape(&mut self, tape: zx_tape::Tape) {
        self.bus.load_tape(tape);
    }

    /// Chooses when a loaded tape starts playing: `false` (the default) when
    /// the ROM's `LD-BYTES` is entered, `true` at the first read of port
    /// 0xFE by anything -- which the ROM's keyboard scan does from power-on,
    /// so the first blocks pass before a typed `LOAD ""` asks for them. Use
    /// `true` for a custom loader started from a snapshot.
    pub fn set_tape_start_on_any_read(&mut self, on: bool) {
        self.bus.set_tape_start_on_any_read(on);
    }

    /// Takes the tape out. Playback state goes with it.
    pub fn eject_tape(&mut self) {
        self.bus.eject_tape();
    }

    /// Takes the disk out of the +3's drive. Unsaved writes are lost with it
    /// -- export them first (`take_modified_disk_image`) if they matter.
    pub fn eject_disk(&mut self) {
        self.bus.eject_disk();
    }

    /// Sets the currently loaded tape's turbo speed multiplier (1 = real
    /// time). Call after `load_tape`; a no-op if no tape is installed.
    pub fn set_tape_speed(&mut self, multiplier: u32) {
        self.bus.set_tape_speed(multiplier);
    }

    /// Current tape playback status for debugger/log diagnostics.
    pub fn tape_playback_state(&self) -> Option<zx_tape::TapePlaybackState> {
        self.bus.tape_playback_state()
    }

    /// Current EAR line level (`true` = high) when tape is attached.
    pub fn tape_ear_bit(&self) -> Option<bool> {
        self.bus.tape_ear_bit()
    }

    /// Tape-domain cycles remaining before the next EAR edge.
    pub fn tape_cycles_until_next_edge(&self) -> Option<u32> {
        self.bus.tape_cycles_until_next_edge()
    }

    /// Writes bytes at an offset within a physical 128K-family RAM bank.
    /// Returns zero for 48K models or invalid bank/offset ranges.
    pub fn write_ram_bank(&mut self, bank: u8, address: u16, data: &[u8]) -> usize {
        self.bus.write_ram_bank(bank, address, data)
    }

    /// Human-readable summary of the current memory paging/banking
    /// configuration, for debug-overlay display -- `None` on 48K (no
    /// paging concept at all).
    pub fn overlay_memory_summary(&self) -> Option<String> {
        if let Some((rom_bank, ram_bank, shadow_screen, locked)) = self.bus.paging_state() {
            return Some(format!(
                "ROM{rom_bank} RAM{ram_bank}{}{}",
                if shadow_screen { " shadow" } else { "" },
                if locked { " locked" } else { "" }
            ));
        }
        if let Some((
            ram_bank,
            shadow_screen,
            _rom_bit_lo,
            locked,
            special_mode,
            _bits_1_2,
            _disk_motor,
        )) = self.bus.paging_2a3_state()
        {
            return Some(format!(
                "RAM{ram_bank}{}{}{}",
                if shadow_screen { " shadow" } else { "" },
                if locked { " locked" } else { "" },
                if special_mode { " special" } else { "" }
            ));
        }
        None
    }

    /// Current floppy-disk controller phase label, for debug-overlay
    /// display -- `None` off `Model::Spectrum2A3`.
    pub fn overlay_fdc_phase(&self) -> Option<String> {
        self.bus
            .fdc()
            .map(|fdc| fdc.dump_state().phase_label().to_string())
    }

    /// Current +2A/+3 floppy-disk motor line state, for debug-overlay
    /// display -- `None` off `Model::Spectrum2A3`.
    pub fn overlay_disk_motor(&self) -> Option<bool> {
        self.bus
            .paging_2a3_state()
            .map(|(.., disk_motor)| disk_motor)
    }

    /// Recent tape EAR-bit history (`0`=low, `1`=high), oldest first, for
    /// the debug-overlay tape waveform panel -- empty when no tape has
    /// ever been installed.
    pub fn overlay_tape_waveform(&self) -> Vec<u8> {
        self.bus.tape_waveform_samples()
    }

    /// Recent completed EAR-line pulse widths (T-states), oldest first,
    /// for the debug-overlay tape panel's numeric pulse list -- see
    /// `crate::bus::TAPE_PULSE_WIDTH_SAMPLES`'s own doc comment.
    pub fn overlay_tape_pulse_widths(&self) -> Vec<u16> {
        self.bus.tape_pulse_widths_samples()
    }

    /// Enables/disables the fast-tape-load ROM trap (see
    /// `try_fast_tape_load_trap`) -- a genuinely different mechanism from
    /// `set_tape_speed`'s turbo multiplier: that still plays every real
    /// pulse, just faster; this substitutes the ROM's own `LD-BYTES`
    /// routine's *effect* directly; each is independently toggleable.
    pub fn set_fast_tape_load(&mut self, enabled: bool) {
        self.fast_tape_load = enabled;
    }

    /// Real 48K ROM address of the `LD-BYTES` tape-loading routine -- also
    /// present, at this same address, in the 128K-family's own ROM 1 (the
    /// 48K-compatible BASIC ROM every 128K/+2/+2A/+3 falls back to for
    /// actual tape I/O). See `try_fast_tape_load_trap`/`CREDITS.md`.
    const LD_BYTES_ADDR: u16 = 0x0556;
    /// The routine's own real first opcode (`INC D`, clearing the zero flag
    /// before the pilot-tone search) -- `try_fast_tape_load_trap` checks
    /// this is actually still here before trapping, so a custom ROM that
    /// happens to jump through `0x0556` for something unrelated isn't
    /// silently hijacked.
    const LD_BYTES_FIRST_OPCODE: u8 = 0x14;

    /// If fast tape loading is enabled and execution has just reached the
    /// real ROM's `LD-BYTES` routine, substitutes that routine's entire
    /// effect directly from the currently-loaded tape's next block instead
    /// of letting it bit-bang through real-time EAR pulses -- the actual
    /// "instant load" mechanism (distinct from `set_tape_speed`'s turbo
    /// multiplier, which still plays every pulse, just faster). Returns
    /// `true` if it trapped (the caller should skip the normal `cpu.step`
    /// call for this "instruction").
    ///
    /// Real `LD-BYTES` calling convention (Sinclair ZX Spectrum ROM
    /// disassembly, see `CREDITS.md`): entered via `CALL` with `A` = the
    /// expected flag byte (block header vs data), carry set for LOAD/clear
    /// for VERIFY, `DE` = expected data length, `IX` = destination address;
    /// returns with carry set on success, clear on failure. This only
    /// engages when the tape's next block matches the exact common case
    /// (flag byte and length both match what the caller expects) --
    /// anything else (mismatched flag, wrong length, no tape, or playback
    /// positioned inside a non-standard `.tzx` block) falls through to the
    /// existing, already-correct real-time pulse path instead of guessing.
    fn try_fast_tape_load_trap(&mut self) -> bool {
        if !self.fast_tape_load || self.cpu.pc != Self::LD_BYTES_ADDR {
            return false;
        }
        if self.bus.peek(Self::LD_BYTES_ADDR) != Self::LD_BYTES_FIRST_OPCODE {
            return false;
        }
        let Some(block) = self.bus.take_next_tape_block() else {
            return false;
        };
        // flag + at least one payload byte + checksum.
        if block.len() < 2 {
            return false;
        }

        let expected_flag = self.cpu.a();
        let expected_len = self.cpu.de as usize;
        let payload_len = block.len() - 2;
        if block[0] != expected_flag || expected_len != payload_len {
            return false;
        }

        let checksum_ok = block.iter().fold(0u8, |acc, &b| acc ^ b) == 0;
        let loading = self.cpu.f() & 0x01 != 0; // carry set = LOAD, clear = VERIFY

        let mut ok = checksum_ok;
        for (i, &byte) in block[1..1 + payload_len].iter().enumerate() {
            let addr = self.cpu.ix.wrapping_add(i as u16);
            if loading {
                self.bus.write(addr, byte);
            } else if self.bus.read(addr) != byte {
                ok = false;
            }
        }

        self.cpu.ix = self.cpu.ix.wrapping_add(payload_len as u16);
        self.cpu.de = 0;
        self.cpu.set_f(if ok {
            self.cpu.f() | 0x01
        } else {
            self.cpu.f() & !0x01
        });

        // Simulate the routine's own `RET` back to whoever `CALL`ed it --
        // real hardware entered here via `CALL 0x0556`, so `(sp)` already
        // holds that caller's return address untouched (this trap never
        // executes any of the routine's own instructions, including the
        // internal stack manipulation described in its disassembly).
        let lo = self.bus.read(self.cpu.sp);
        let hi = self.bus.read(self.cpu.sp.wrapping_add(1));
        self.cpu.sp = self.cpu.sp.wrapping_add(2);
        self.cpu.pc = (hi as u16) << 8 | lo as u16;

        true
    }

    /// Installs a disk image for `Model::Spectrum2A3`'s floppy drive --
    /// mirrors `load_tape`/`load_snapshot`'s "concrete per-machine
    /// concern" shape. Errors on any other model, same "must match, no
    /// silent best-effort" stance as `load_snapshot`'s own model check --
    /// there's no FDC at all to install a disk into on 48K/128K.
    pub fn load_disk(&mut self, disk: dsk::Disk) -> Result<(), String> {
        if self.model != Model::Spectrum2A3 {
            return Err(
                "--disk requires --machine zx-spectrum-2a3 (no floppy disk controller on this model)"
                    .to_string(),
            );
        }
        self.bus.load_disk(disk);
        Ok(())
    }

    /// The disk currently in the +3's drive, with every write the FDC has
    /// made applied (`dsk::Disk::is_modified` says whether any happened).
    pub fn disk(&self) -> Option<&dsk::Disk> {
        self.bus.disk()
    }

    /// The disk image as a `.dsk` file if it has been modified since it
    /// was inserted or last marked saved; `None` when there's nothing to
    /// persist.
    pub fn take_modified_disk_image(&self) -> Option<Vec<u8>> {
        self.bus.modified_disk_image()
    }

    /// Call after writing `take_modified_disk_image`'s bytes out, so the
    /// same changes aren't reported as unsaved again.
    pub fn mark_disk_saved(&mut self) {
        self.bus.mark_disk_saved();
    }

    /// Performs an `OUT (port),value` on the I/O bus between instructions
    /// (the debugger's `write-port`), decoded exactly like a CPU `OUT`.
    pub fn write_port(&mut self, port: u16, value: u8) {
        self.bus.write_io(port, value);
    }

    /// Presses a key, holding it down until `release_key` is called.
    pub fn press_key(&mut self, key: ZxSpectrumKey) {
        self.bus.press_key(key);
    }

    /// Releases a previously pressed key. Releasing an already-released key
    /// is a no-op.
    pub fn release_key(&mut self, key: ZxSpectrumKey) {
        self.bus.release_key(key);
    }

    /// Presses a Kempston joystick input, holding it down until
    /// `release_kempston` is called.
    pub fn press_kempston(&mut self, button: KempstonButton) {
        self.bus.press_kempston(button);
    }

    /// Releases a previously pressed Kempston joystick input. Releasing an
    /// already-released one is a no-op.
    pub fn release_kempston(&mut self, button: KempstonButton) {
        self.bus.release_kempston(button);
    }

    /// Presses a Fuller joystick input, holding it down until
    /// `release_fuller` is called.
    pub fn press_fuller(&mut self, button: FullerButton) {
        self.bus.press_fuller(button);
    }

    /// Releases a previously pressed Fuller joystick input. Releasing an
    /// already-released one is a no-op.
    pub fn release_fuller(&mut self, button: FullerButton) {
        self.bus.release_fuller(button);
    }

    /// Presses a Kempston mouse button, holding it down until
    /// `release_mouse_button` is called. Unlike the joysticks above, the
    /// mouse is always present alongside whichever joystick is selected --
    /// real hardware: a mouse interface and a joystick interface are
    /// separate ports, both can coexist.
    pub fn press_mouse_button(&mut self, button: MouseButton) {
        self.bus.press_mouse_button(button);
    }

    /// Releases a previously pressed Kempston mouse button. Releasing an
    /// already-released one is a no-op.
    pub fn release_mouse_button(&mut self, button: MouseButton) {
        self.bus.release_mouse_button(button);
    }

    /// Advances the Kempston mouse's free-running X/Y position counters by
    /// a relative delta -- see `bus::ZxSpectrumBus::move_mouse`'s own doc
    /// comment for the real wraparound behavior this matches.
    pub fn move_mouse(&mut self, dx: i8, dy: i8) {
        self.bus.move_mouse(dx, dy);
    }

    /// Accumulates a Kempston mouse scroll-wheel tick -- see `bus::
    /// ZxSpectrumBus::scroll_mouse`'s own doc comment for why this
    /// accumulates rather than sets a persistent value.
    pub fn scroll_mouse(&mut self, delta: i8) {
        self.bus.scroll_mouse(delta);
    }

    /// Mixes the beeper's current 1-bit level with the AY's current output
    /// (128K only) into one sample, then applies `speaker_type`'s optional
    /// low-pass filter (see its own doc comment) -- a no-op pass-through
    /// when `Unfiltered` (the default), matching this codebase's original,
    /// only-ever behavior before that option existed. A known
    /// simplification, not a hardware fact: real mixing circuitry, relative
    /// levels, and any DC blocking are board-specific analog details this
    /// doesn't model -- these weights just keep the combined signal
    /// comfortably within `-1.0..=1.0` without either source drowning out
    /// the other.
    fn mix_audio_sample(&mut self) -> f32 {
        let beeper = if self.bus.speaker_level() { 0.3 } else { 0.0 };
        let ay = self.bus.ay().map(|ay| ay.output() * 0.5).unwrap_or(0.0);
        // Turbosound's second chip, if enabled -- same weight as the
        // primary chip; the final clamp below is what actually keeps two
        // AYs plus the beeper from clipping, same "known simplification,
        // not a hardware fact" stance this function's own doc comment
        // already takes for the single-AY case.
        let ay2 = self.bus.ay2().map(|ay| ay.output() * 0.5).unwrap_or(0.0);
        let raw = (beeper + ay + ay2).clamp(-1.0, 1.0);

        let Some(cutoff_hz) = self.speaker_type.cutoff_hz() else {
            return raw;
        };
        // Standard one-pole low-pass IIR: alpha derived from the cutoff
        // frequency at this codebase's fixed audio sample rate.
        let dt = 1.0 / machine::AUDIO_SAMPLE_RATE as f32;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
        let alpha = dt / (rc + dt);
        self.speaker_filter_state += alpha * (raw - self.speaker_filter_state);
        self.speaker_filter_state
    }

    /// Installs a parsed `.z80` snapshot (`z80_snapshot::parse`): replaces
    /// the whole CPU register set, writes every memory block into RAM,
    /// restores the border color, and (128K only) the paging state.
    /// `.z80` fully encodes register state (unlike ZX81's `.p`/`.p81`, see
    /// `zx81::Zx81::load_p_snapshot`), so there's no ROM-boot-state
    /// synthesis needed here -- just apply what the file says.
    ///
    /// Errors if the snapshot's own memory model (`snapshot.paging.
    /// is_some()`, i.e. whether it's a 128K-family `.z80`) doesn't match
    /// this `ZxSpectrum`'s `model` -- same "must match, no silent
    /// best-effort" stance as `AnyMachine::apply_snapshot`'s extension
    /// check. A 48K snapshot's `MemoryBlock::Absolute` blocks and a 128K
    /// snapshot's `MemoryBlock::RamBank` blocks aren't meaningful on the
    /// other model's bus at all, so there's no sensible partial behavior
    /// to fall back to.
    pub fn load_snapshot(&mut self, snapshot: z80_snapshot::Snapshot) -> Result<(), String> {
        match (
            self.model,
            snapshot.paging.is_some(),
            snapshot.paging_2a3.is_some(),
        ) {
            (Model::Spectrum48, false, false) => {}
            (Model::Spectrum128, true, false) => {}
            (Model::Spectrum2A3, false, true) => {}
            (Model::Spectrum48, _, _) => {
                return Err(
                    "snapshot is a 128K-family .z80 file, but this machine is --machine zx-spectrum (48K) -- use --machine zx-spectrum-128 or zx-spectrum-2a3"
                        .to_string(),
                );
            }
            (Model::Spectrum128, false, _) => {
                return Err(
                    "snapshot is a 48K .z80 file, but this machine is --machine zx-spectrum-128 -- use --machine zx-spectrum"
                        .to_string(),
                );
            }
            (Model::Spectrum128, true, true) => {
                return Err(
                    "snapshot is a +2A/+3 .z80 file, but this machine is --machine zx-spectrum-128 -- use --machine zx-spectrum-2a3"
                        .to_string(),
                );
            }
            (Model::Spectrum2A3, _, _) => {
                return Err(
                    "snapshot isn't a +2A/+3-shaped .z80 file -- use --machine zx-spectrum or zx-spectrum-128 for a 48K/128K snapshot"
                        .to_string(),
                );
            }
        }

        self.cpu = snapshot.cpu;
        self.bus.set_border_color(snapshot.border_color);
        for block in &snapshot.memory {
            match block {
                z80_snapshot::MemoryBlock::Absolute { address, data } => {
                    self.bus.load(*address, data);
                }
                z80_snapshot::MemoryBlock::RamBank { bank, data } => {
                    self.bus.load_ram_bank(*bank, data);
                }
            }
        }
        if let Some(paging) = snapshot.paging {
            self.bus.set_paging(
                paging.rom_bank,
                paging.ram_bank,
                paging.shadow_screen,
                paging.locked,
            );
        }
        if let Some(paging) = snapshot.paging_2a3 {
            self.bus.set_paging_2a3(
                paging.ram_bank,
                paging.shadow_screen,
                paging.rom_bit_lo,
                paging.locked,
                paging.special_mode,
                paging.bits_1_2,
                paging.disk_motor,
            );
        }
        Ok(())
    }

    /// Same idea as `load_snapshot` but for `.sna` files -- see
    /// `sna_snapshot`. `.sna`'s 128K extension predates the +2A/+3's
    /// second paging register, so there's no shape for it here at all
    /// (unlike `.z80`, which has a real +2A/+3-specific mode) -- `.sna` on
    /// `Model::Spectrum2A3` is always rejected outright.
    pub fn load_sna_snapshot(&mut self, snapshot: sna_snapshot::Snapshot) -> Result<(), String> {
        match (self.model, snapshot.paging.is_some()) {
            (Model::Spectrum48, false) => {}
            (Model::Spectrum128, true) => {}
            (Model::Spectrum48, true) => {
                return Err(
                    "snapshot is a 128K .sna file, but this machine is --machine zx-spectrum (48K) -- use --machine zx-spectrum-128"
                        .to_string(),
                );
            }
            (Model::Spectrum128, false) => {
                return Err(
                    "snapshot is a 48K .sna file, but this machine is --machine zx-spectrum-128 -- use --machine zx-spectrum"
                        .to_string(),
                );
            }
            (Model::Spectrum2A3, _) => {
                return Err(
                    "--machine zx-spectrum-2a3 has no .sna format of its own (it predates the +2A/+3) -- use a .z80 snapshot instead"
                        .to_string(),
                );
            }
        }

        self.cpu = snapshot.cpu;
        self.bus.set_border_color(snapshot.border_color);
        for block in &snapshot.memory {
            match block {
                sna_snapshot::MemoryBlock::Absolute { address, data } => {
                    self.bus.load(*address, data);
                }
                sna_snapshot::MemoryBlock::RamBank { bank, data } => {
                    self.bus.load_ram_bank(*bank, data);
                }
            }
        }
        if let Some(paging) = snapshot.paging {
            self.bus.set_paging(
                paging.rom_bank,
                paging.ram_bank,
                paging.shadow_screen,
                paging.locked,
            );
        }
        Ok(())
    }

    /// Builds a `.z80`-writable `Snapshot` from this machine's current live
    /// state -- the save-side counterpart to `load_snapshot`. Always
    /// succeeds (every `Model` this crate supports has a real `.z80`
    /// hardware mode -- see `z80_snapshot::write`'s own doc comment).
    pub fn save_snapshot(&self) -> z80_snapshot::Snapshot {
        let (memory, paging, paging_2a3) = match self.model {
            Model::Spectrum48 => {
                let ram = self.bus.flat_ram().expect("Spectrum48 always has flat RAM");
                (
                    vec![z80_snapshot::MemoryBlock::Absolute {
                        address: RAM_BASE,
                        data: ram,
                    }],
                    None,
                    None,
                )
            }
            Model::Spectrum128 => {
                let memory = (0u8..8)
                    .map(|bank| z80_snapshot::MemoryBlock::RamBank {
                        bank,
                        data: self.bus.ram_bank_data(bank).unwrap().to_vec(),
                    })
                    .collect();
                let (rom_bank, ram_bank, shadow_screen, locked) = self.bus.paging_state().unwrap();
                (
                    memory,
                    Some(z80_snapshot::PagingState {
                        rom_bank,
                        ram_bank,
                        shadow_screen,
                        locked,
                    }),
                    None,
                )
            }
            Model::Spectrum2A3 => {
                let memory = (0u8..8)
                    .map(|bank| z80_snapshot::MemoryBlock::RamBank {
                        bank,
                        data: self.bus.ram_bank_data(bank).unwrap().to_vec(),
                    })
                    .collect();
                let (
                    ram_bank,
                    shadow_screen,
                    rom_bit_lo,
                    locked,
                    special_mode,
                    bits_1_2,
                    disk_motor,
                ) = self.bus.paging_2a3_state().unwrap();
                (
                    memory,
                    None,
                    Some(z80_snapshot::Paging2A3State {
                        ram_bank,
                        shadow_screen,
                        rom_bit_lo,
                        locked,
                        special_mode,
                        bits_1_2,
                        disk_motor,
                    }),
                )
            }
        };

        z80_snapshot::Snapshot {
            cpu: self.cpu,
            border_color: self.bus.border_color(),
            memory,
            paging,
            paging_2a3,
        }
    }

    /// Same idea as `save_snapshot` but for `.sna` -- errors on
    /// `Model::Spectrum2A3`, which `.sna` has no shape for at all (see
    /// `load_sna_snapshot`'s own doc comment).
    pub fn save_sna_snapshot(&self) -> Result<sna_snapshot::Snapshot, String> {
        let (memory, paging) = match self.model {
            Model::Spectrum48 => {
                let ram = self.bus.flat_ram().expect("Spectrum48 always has flat RAM");
                (
                    vec![sna_snapshot::MemoryBlock::Absolute {
                        address: RAM_BASE,
                        data: ram,
                    }],
                    None,
                )
            }
            Model::Spectrum128 => {
                let memory = (0u8..8)
                    .map(|bank| sna_snapshot::MemoryBlock::RamBank {
                        bank,
                        data: self.bus.ram_bank_data(bank).unwrap().to_vec(),
                    })
                    .collect();
                let (rom_bank, ram_bank, shadow_screen, locked) = self.bus.paging_state().unwrap();
                (
                    memory,
                    Some(sna_snapshot::PagingState {
                        rom_bank,
                        ram_bank,
                        shadow_screen,
                        locked,
                    }),
                )
            }
            Model::Spectrum2A3 => {
                return Err(
                    "--machine zx-spectrum-2a3 has no .sna format of its own (it predates the +2A/+3) -- save a .z80 snapshot instead"
                        .to_string(),
                );
            }
        };

        Ok(sna_snapshot::Snapshot {
            cpu: self.cpu,
            border_color: self.bus.border_color(),
            memory,
            paging,
        })
    }

    /// Same idea as `load_snapshot` but for `.szx` (ZX-State) files --
    /// see `szx_snapshot`. Unlike `.sna`, `.szx` has a real shape for
    /// every model this crate supports (same as `.z80`), so the
    /// model-mismatch cases mirror `load_snapshot`'s own exactly. If the
    /// file carried an `AY\0\0` block, its 16 registers and current
    /// selection are restored onto this machine's primary AY chip (not
    /// Turbosound's second chip -- the real `.szx` format has no block
    /// for that); silently ignored on a model with no AY chip modeled at
    /// all (48K).
    pub fn load_szx_snapshot(&mut self, snapshot: szx_snapshot::Snapshot) -> Result<(), String> {
        match (
            self.model,
            snapshot.paging.is_some(),
            snapshot.paging_2a3.is_some(),
        ) {
            (Model::Spectrum48, false, false) => {}
            (Model::Spectrum128, true, false) => {}
            (Model::Spectrum2A3, false, true) => {}
            (Model::Spectrum48, _, _) => {
                return Err(
                    "snapshot is a 128K-family .szx file, but this machine is --machine zx-spectrum (48K) -- use --machine zx-spectrum-128 or zx-spectrum-2a3"
                        .to_string(),
                );
            }
            (Model::Spectrum128, false, _) => {
                return Err(
                    "snapshot is a 48K .szx file, but this machine is --machine zx-spectrum-128 -- use --machine zx-spectrum"
                        .to_string(),
                );
            }
            (Model::Spectrum128, true, true) => {
                return Err(
                    "snapshot is a +2A/+3 .szx file, but this machine is --machine zx-spectrum-128 -- use --machine zx-spectrum-2a3"
                        .to_string(),
                );
            }
            (Model::Spectrum2A3, _, _) => {
                return Err(
                    "snapshot isn't a +2A/+3-shaped .szx file -- use --machine zx-spectrum or zx-spectrum-128 for a 48K/128K snapshot"
                        .to_string(),
                );
            }
        }

        self.cpu = snapshot.cpu;
        self.bus.set_border_color(snapshot.border_color);
        for block in &snapshot.memory {
            match block {
                szx_snapshot::MemoryBlock::Absolute { address, data } => {
                    self.bus.load(*address, data);
                }
                szx_snapshot::MemoryBlock::RamBank { bank, data } => {
                    self.bus.load_ram_bank(*bank, data);
                }
            }
        }
        if let Some(paging) = snapshot.paging {
            self.bus.set_paging(
                paging.rom_bank,
                paging.ram_bank,
                paging.shadow_screen,
                paging.locked,
            );
        }
        if let Some(paging) = snapshot.paging_2a3 {
            self.bus.set_paging_2a3(
                paging.ram_bank,
                paging.shadow_screen,
                paging.rom_bit_lo,
                paging.locked,
                paging.special_mode,
                paging.bits_1_2,
                paging.disk_motor,
            );
        }
        if let (Some(ay_state), Some(ay)) = (snapshot.ay, self.bus.ay_mut()) {
            for (reg, value) in ay_state.registers.into_iter().enumerate() {
                ay.select_register(reg as u8);
                ay.write(value);
            }
            ay.select_register(ay_state.current_register);
        }
        Ok(())
    }

    /// Same idea as `save_snapshot` but for `.szx` -- always succeeds
    /// (every `Model` this crate supports has a real `.szx` machine id,
    /// same as `.z80`). Unlike `.z80`'s single 48K-wide memory block,
    /// `.szx`'s `RAMP` chunks are always one real 16K page each, so a
    /// 48K snapshot's flat RAM is split into its 3 constituent pages
    /// here rather than reusing `save_snapshot`'s single-block shape.
    /// Includes an `AY\0\0` block whenever this machine has an AY chip
    /// at all -- Turbosound's second chip is never included (see
    /// `load_szx_snapshot`'s own doc comment).
    pub fn save_szx_snapshot(&self) -> szx_snapshot::Snapshot {
        let (memory, paging, paging_2a3) = match self.model {
            Model::Spectrum48 => {
                let ram = self.bus.flat_ram().expect("Spectrum48 always has flat RAM");
                let memory = [0x4000u16, 0x8000, 0xC000]
                    .into_iter()
                    .zip(ram.chunks(0x4000))
                    .map(|(address, data)| szx_snapshot::MemoryBlock::Absolute {
                        address,
                        data: data.to_vec(),
                    })
                    .collect();
                (memory, None, None)
            }
            Model::Spectrum128 => {
                let memory = (0u8..8)
                    .map(|bank| szx_snapshot::MemoryBlock::RamBank {
                        bank,
                        data: self.bus.ram_bank_data(bank).unwrap().to_vec(),
                    })
                    .collect();
                let (rom_bank, ram_bank, shadow_screen, locked) = self.bus.paging_state().unwrap();
                (
                    memory,
                    Some(szx_snapshot::PagingState {
                        rom_bank,
                        ram_bank,
                        shadow_screen,
                        locked,
                    }),
                    None,
                )
            }
            Model::Spectrum2A3 => {
                let memory = (0u8..8)
                    .map(|bank| szx_snapshot::MemoryBlock::RamBank {
                        bank,
                        data: self.bus.ram_bank_data(bank).unwrap().to_vec(),
                    })
                    .collect();
                let (
                    ram_bank,
                    shadow_screen,
                    rom_bit_lo,
                    locked,
                    special_mode,
                    bits_1_2,
                    disk_motor,
                ) = self.bus.paging_2a3_state().unwrap();
                (
                    memory,
                    None,
                    Some(szx_snapshot::Paging2A3State {
                        ram_bank,
                        shadow_screen,
                        rom_bit_lo,
                        locked,
                        special_mode,
                        bits_1_2,
                        disk_motor,
                    }),
                )
            }
        };

        let ay = self.bus.ay().map(|ay| szx_snapshot::AyState {
            current_register: ay.selected_register(),
            registers: std::array::from_fn(|reg| ay.read_register(reg as u8)),
        });

        szx_snapshot::Snapshot {
            cpu: self.cpu,
            border_color: self.bus.border_color(),
            memory,
            paging,
            paging_2a3,
            ay,
        }
    }

    /// Builds a full save state from this machine's current live state --
    /// see `save_state`'s own module doc comment for what's deliberately
    /// excluded (ROM, disk image bytes, tape pulse/byte data -- all
    /// externally supplied, same as `.z80`/`.sna`). Unlike `save_snapshot`,
    /// this also captures peripheral timing state (`ay`/`fdc`/tape
    /// position/contention frame position) a plain snapshot format was
    /// never designed to carry.
    pub(crate) fn save_state(&self) -> save_state::SaveState {
        save_state::SaveState {
            model: self.model,
            cpu: self.cpu,
            memory: save_state::dump_memory(self.model, &self.bus),
            keyboard: self.bus.keyboard(),
            border_color: self.bus.border_color(),
            speaker_level: self.bus.speaker_level(),
            kempston: self.bus.kempston_raw(),
            fuller: self.bus.fuller_raw(),
            mouse: self.bus.mouse_raw(),
            frame_tstate: self.bus.frame_tstate_raw(),
            ay: self.bus.ay().cloned(),
            ay2: self.bus.ay2().cloned(),
            active_ay_chip: self.bus.active_ay_chip_raw(),
            tape_position: self.bus.tape().map(|t| t.dump_state()),
            fdc_state: self.bus.fdc().map(|f| f.dump_state()),
            modified_disk: self.bus.modified_disk_image(),
            flash_counter: self.flash_counter,
            flash_phase: self.flash_phase,
            audio_cycle_accumulator: self.audio_cycle_accumulator,
            ay_cycle_debt: self.ay_cycle_debt,
            fast_tape_load: self.fast_tape_load,
            speaker_type: self.speaker_type,
            speaker_filter_state: self.speaker_filter_state,
            joystick_type: self.joystick_type,
        }
    }

    /// Restores a save state built by `save_state` -- errors on a model
    /// mismatch rather than silently misapplying it, same "must match"
    /// stance as `load_snapshot`. `ay`/`tape_position`/`fdc_state` being
    /// `Some` on a model that doesn't have that peripheral (or `None` on
    /// one that does) is simply a no-op for that piece, not an error --
    /// mismatches here are far less consequential than a memory-shape
    /// mismatch (e.g. a state saved before `load_tape` was ever called has
    /// `tape_position: None`, which just leaves whatever tape is currently
    /// installed alone).
    pub(crate) fn load_state(&mut self, state: save_state::SaveState) -> Result<(), String> {
        if state.model != self.model {
            return Err(format!(
                "save state is for {:?}, but this machine is {:?}",
                state.model, self.model
            ));
        }

        self.cpu = state.cpu;
        save_state::restore_memory(&mut self.bus, state.memory);
        self.bus.set_keyboard(state.keyboard);
        self.bus.set_border_color(state.border_color);
        self.bus.set_speaker_level(state.speaker_level);
        self.bus.set_kempston_raw(state.kempston);
        self.bus.set_fuller_raw(state.fuller);
        self.bus.set_mouse_raw(state.mouse);
        self.bus.set_frame_tstate_raw(state.frame_tstate);
        if let (Some(ay), Some(saved)) = (self.bus.ay_mut(), state.ay) {
            *ay = saved;
        }
        if let (Some(ay2), Some(saved)) = (self.bus.ay2_mut(), state.ay2) {
            *ay2 = saved;
        }
        self.bus.set_active_ay_chip_raw(state.active_ay_chip);
        if let (Some(tape), Some(position)) = (self.bus.tape_mut(), state.tape_position) {
            tape.restore_state(position);
        }
        self.bus.restore_disk_image(state.modified_disk)?;
        if let (Some(fdc), Some(fdc_state)) = (self.bus.fdc_mut(), state.fdc_state) {
            fdc.restore_state(fdc_state);
        }
        self.flash_counter = state.flash_counter;
        self.flash_phase = state.flash_phase;
        self.audio_cycle_accumulator = state.audio_cycle_accumulator;
        self.ay_cycle_debt = state.ay_cycle_debt;
        self.fast_tape_load = state.fast_tape_load;
        self.speaker_type = state.speaker_type;
        self.speaker_filter_state = state.speaker_filter_state;
        self.joystick_type = state.joystick_type;
        Ok(())
    }
}

impl Machine for ZxSpectrum {
    fn clock_hz(&self) -> u64 {
        match self.region {
            Region::Pal => 3_546_900,
            // Placeholder: no real Spectrum 48K variant shipped for these
            // markets -- kept equal to PAL until/unless one needs modeling.
            Region::Ntsc | Region::Japan => 3_546_900,
        }
    }

    fn frame_hz(&self) -> u64 {
        match self.region {
            Region::Pal => 50,
            Region::Ntsc | Region::Japan => 60,
        }
    }

    fn run(&mut self, running: &mut dyn FnMut(&dyn Machine) -> bool) -> RunStats {
        Clock::new(self.frame_hz()).run(self, running)
    }

    /// Steps the CPU one instruction, then advances the AY-3-8910 (128K
    /// only) by the same real time and emits however many audio samples
    /// that time crosses -- see the `audio_cycle_accumulator`/
    /// `ay_cycle_debt` fields' doc comments for why each needs its own
    /// accumulator rather than a per-step division.
    fn step(&mut self) -> u32 {
        let trapped = self.try_fast_tape_load_trap();
        if !trapped
            && self.cpu.pc == Self::LD_BYTES_ADDR
            && self.bus.peek(Self::LD_BYTES_ADDR) == Self::LD_BYTES_FIRST_OPCODE
        {
            // The ROM is asking for the tape: press Play (a no-op if it already runs).
            self.bus.start_tape();
        }
        let cycles = if trapped {
            // Near-zero: the entire point is collapsing what would be
            // thousands of real T-states of bit-banging into effectively
            // nothing, same as Fuse's own "instantly loads tape files"
            // description of this feature.
            4
        } else {
            self.cpu.step(&mut self.bus) as u32
        };

        self.ay_cycle_debt += cycles;
        let ay_cycles = self.ay_cycle_debt / 2;
        self.ay_cycle_debt %= 2;
        if let Some(ay) = self.bus.ay_mut() {
            ay.tick(ay_cycles);
        }
        if let Some(ay2) = self.bus.ay2_mut() {
            ay2.tick(ay_cycles);
        }

        self.bus.tick_tape(cycles);

        self.audio_cycle_accumulator += cycles as f64;
        let cycles_per_sample = self.clock_hz() as f64 / machine::AUDIO_SAMPLE_RATE as f64;
        while self.audio_cycle_accumulator >= cycles_per_sample {
            self.audio_cycle_accumulator -= cycles_per_sample;
            let sample = self.mix_audio_sample();
            self.audio_buffer.push(sample);
        }

        cycles
    }

    fn halted(&self) -> bool {
        self.cpu.halted
    }

    /// Real ZX Spectrum hardware: the ULA raises `INT` once per 50Hz
    /// (PAL)/60Hz (NTSC) video frame -- exactly what this fires once per,
    /// via `machine::Clock::run`/`retroclawd`'s `emu_thread` background
    /// thread, once per its own tick.
    /// Also ticks the FLASH counter here, piggybacking on the same
    /// once-per-frame call rather than needing its own timing mechanism --
    /// the FLASH counter's real rate genuinely is frame-based (see
    /// `video::FLASH_TOGGLE_FRAMES`), unlike ZX81's NMI which needs a much
    /// finer per-instruction accumulator (see `zx81::Zx81::step`).
    /// Also resets the bus's contended-memory frame-position counter here,
    /// same piggyback reasoning -- the ULA/gate array's own contention
    /// pattern restarts at the same point every real frame too.
    fn request_interrupt(&mut self) {
        self.cpu.request_interrupt();
        self.bus.reset_frame_tstate();

        self.flash_counter += 1;
        if self.flash_counter >= video::FLASH_TOGGLE_FRAMES {
            self.flash_counter = 0;
            self.flash_phase = !self.flash_phase;
        }
    }

    fn registers(&self) -> Vec<(&'static str, u64)> {
        self.cpu.registers()
    }

    fn pc(&self) -> u16 {
        self.cpu.pc
    }

    fn read_memory(&self, addr: u16) -> u8 {
        self.bus.peek(addr)
    }

    fn load(&mut self, addr: u16, bytes: &[u8]) -> usize {
        self.bus.load(addr, bytes)
    }

    fn set_pc(&mut self, addr: u16) {
        self.cpu.pc = addr;
    }

    fn reset(&mut self) {
        self.cpu = Cpu::new();
    }

    fn render_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        Some(video::render_frame(
            &self.bus,
            self.border,
            self.flash_phase,
        ))
    }

    fn drain_audio(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.audio_buffer)
    }

    fn set_port_watches(&mut self, watches: Vec<machine::PortWatch>) -> bool {
        self.bus.port_watch.set_watches(watches);
        true
    }

    fn take_port_hit(&mut self) -> Option<machine::PortHit> {
        self.bus.port_watch.take_hit()
    }

    fn save_state(&self) -> Vec<u8> {
        bincode::serde::encode_to_vec(self.save_state(), bincode::config::standard())
            .expect("SaveState has no types bincode can fail to encode")
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        let (state, _): (save_state::SaveState, usize) =
            bincode::serde::decode_from_slice(bytes, bincode::config::standard())
                .map_err(|e| format!("failed to decode save state: {e}"))?;
        self.load_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_tiny_program_to_halt_and_reports_registers() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // LD A,0x2A ; HALT -- loaded into RAM since ROM writes are dropped.
        spectrum.load(RAM_BASE, &[0x3E, 0x2A, 0x76]);
        spectrum.set_pc(RAM_BASE);

        let stats = spectrum.run(&mut |m| !m.halted());

        assert_eq!(stats.instructions, 2);
        assert!(spectrum.halted());
        assert_eq!(spectrum.registers()[0], ("AF", 0x2A00));
    }

    #[test]
    fn port_watches_latch_the_cpus_own_in_and_out_only() {
        use machine::{PortHit, PortWatch};
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // LD A,5 ; OUT (0xFE),A ; IN A,(0xFE) ; HALT -- OUT/IN address the port as A<<8 | n.
        spectrum.load(RAM_BASE, &[0x3E, 0x05, 0xD3, 0xFE, 0xDB, 0xFE, 0x76]);
        spectrum.set_pc(RAM_BASE);
        assert!(spectrum.set_port_watches(vec![PortWatch {
            port: 0x00FE,
            mask: 0x00FF,
            on_read: false,
            on_write: true,
        }]));

        spectrum.step(); // LD A,5
        assert_eq!(spectrum.take_port_hit(), None);
        spectrum.step(); // OUT (0xFE),A
        assert_eq!(
            spectrum.take_port_hit(),
            Some(PortHit {
                port: 0x05FE,
                value: 5,
                write: true
            })
        );
        spectrum.step(); // IN A,(0xFE): reads are not watched
        assert_eq!(spectrum.take_port_hit(), None);

        // A debugger-initiated port write never trips a watch.
        spectrum.write_port(0x00FE, 3);
        assert_eq!(spectrum.take_port_hit(), None);
    }

    #[test]
    fn a_block_otir_trips_a_port_watch_on_its_first_iteration() {
        use machine::{PortHit, PortWatch};
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // LD HL,0x8000 ; LD BC,0x03FE ; OTIR -- three bytes to port 0xFE.
        spectrum.load(RAM_BASE, &[0x21, 0x00, 0x80, 0x01, 0xFE, 0x03, 0xED, 0xB3]);
        spectrum.load(0x8000, &[0xA1, 0xA2, 0xA3]);
        spectrum.set_pc(RAM_BASE);
        assert!(spectrum.set_port_watches(vec![PortWatch {
            port: 0x00FE,
            mask: 0x00FF,
            on_read: false,
            on_write: true,
        }]));
        spectrum.step(); // LD HL
        spectrum.step(); // LD BC
        spectrum.step(); // first OTIR iteration: B 3 -> 2, writes (HL) to port 0x02FE
        assert_eq!(
            spectrum.take_port_hit(),
            Some(PortHit {
                port: 0x02FE,
                value: 0xA1,
                write: true
            })
        );
        // The block instruction is not finished: the core rewound PC to repeat it.
        assert_eq!(spectrum.pc(), RAM_BASE + 6, "PC is back on the OTIR");
    }

    #[test]
    fn write_port_is_decoded_like_a_cpu_out() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.write_port(0x00FE, 0x05);
        assert_eq!(
            spectrum.bus.border_color(),
            5,
            "ULA port 0xFE sets the border"
        );

        // Any port with bit 0 clear is the ULA, so a mirror works too...
        spectrum.write_port(0xFFFE, 0x03);
        assert_eq!(spectrum.bus.border_color(), 3);
        // ...and one with bit 0 set is not.
        spectrum.write_port(0x00FF, 0x07);
        assert_eq!(spectrum.bus.border_color(), 3, "not a ULA address");
    }

    #[test]
    fn joystick_type_defaults_to_kempston_and_round_trips_through_the_setter() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        assert_eq!(spectrum.joystick_type(), JoystickType::Kempston);

        spectrum.set_joystick_type(JoystickType::Sinclair2);
        assert_eq!(spectrum.joystick_type(), JoystickType::Sinclair2);
    }

    #[test]
    fn frame_hz_follows_region() {
        let pal = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let ntsc = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Ntsc, Model::Spectrum48);

        assert_eq!(pal.frame_hz(), 50);
        assert_eq!(ntsc.frame_hz(), 60);
    }

    #[test]
    fn flash_phase_toggles_exactly_every_flash_toggle_frames_interrupts() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);

        for _ in 0..video::FLASH_TOGGLE_FRAMES - 1 {
            spectrum.request_interrupt();
            assert!(!spectrum.flash_phase, "toggled too early");
        }
        spectrum.request_interrupt();
        assert!(spectrum.flash_phase, "should have toggled on the Nth call");

        for _ in 0..video::FLASH_TOGGLE_FRAMES - 1 {
            spectrum.request_interrupt();
            assert!(spectrum.flash_phase, "toggled back too early");
        }
        spectrum.request_interrupt();
        assert!(!spectrum.flash_phase, "should have toggled back");
    }

    #[test]
    fn render_frame_reflects_flash_phase_via_request_interrupt() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // flash=1, paper=black(0), ink=green(4): attribute 0b1_0_000_100.
        spectrum.load(0x5800, &[0b1000_0100]);
        spectrum.load(0x4000, &[0xFF]);

        let (_, _, off) = spectrum.render_frame().unwrap();
        for _ in 0..video::FLASH_TOGGLE_FRAMES {
            spectrum.request_interrupt();
        }
        let (_, _, on) = spectrum.render_frame().unwrap();

        assert_ne!(off, on, "render_frame should reflect the flash toggle");
    }

    /// Runs `LD A,<row_mask>` / `IN A,(0xFE)` / `HALT` through the real CPU
    /// and returns the resulting `A` register -- the same path the ROM's
    /// own keyboard scan uses.
    fn scan_row(spectrum: &mut ZxSpectrum, row_mask: u8) -> u8 {
        spectrum.load(RAM_BASE, &[0x3E, row_mask, 0xDB, 0xFE, 0x76]);
        spectrum.set_pc(RAM_BASE);
        spectrum.run(&mut |m| !m.halted());
        (spectrum.registers()[0].1 >> 8) as u8
    }

    #[test]
    fn pressed_key_clears_its_bit_when_scanned_via_in_a_n() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.press_key(ZxSpectrumKey::Q); // row 2 (A10), bit 0

        // Select row 2 only: clear bit 2 of the row-select mask.
        assert_eq!(scan_row(&mut spectrum, 0b1111_1011), 0b1111_1110);
    }

    #[test]
    fn released_key_reads_back_as_unpressed() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.press_key(ZxSpectrumKey::Enter); // row 6 (A14), bit 0
        spectrum.release_key(ZxSpectrumKey::Enter);

        assert_eq!(scan_row(&mut spectrum, 0b1011_1111), 0xFF);
    }

    #[test]
    fn scanning_two_rows_at_once_ands_them_together() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.press_key(ZxSpectrumKey::CapsShift); // row 0 (A8), bit 0
        spectrum.press_key(ZxSpectrumKey::A); // row 1 (A9), bit 0

        // Select rows 0 and 1 together, as the ROM does to detect
        // CAPS SHIFT held with another key.
        assert_eq!(scan_row(&mut spectrum, 0b1111_1100), 0b1111_1110);
    }

    #[test]
    fn load_snapshot_installs_registers_memory_and_border() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let mut cpu = z80::Cpu::new();
        cpu.af = 0x1234;
        cpu.pc = 0x8000;
        let snapshot = z80_snapshot::Snapshot {
            cpu,
            border_color: 5,
            memory: vec![z80_snapshot::MemoryBlock::Absolute {
                address: 0x4000,
                data: vec![0x76],
            }],
            paging: None,
            paging_2a3: None,
        };

        spectrum.load_snapshot(snapshot).unwrap();

        assert_eq!(spectrum.registers()[0], ("AF", 0x1234));
        assert_eq!(spectrum.read_memory(0x4000), 0x76);
        assert_eq!(spectrum.bus.border_color(), 5);
    }

    #[test]
    fn load_snapshot_rejects_a_128k_snapshot_on_a_48k_machine() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let snapshot = z80_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: Some(z80_snapshot::PagingState {
                rom_bank: 0,
                ram_bank: 0,
                shadow_screen: false,
                locked: false,
            }),
            paging_2a3: None,
        };

        assert!(spectrum.load_snapshot(snapshot).is_err());
    }

    #[test]
    fn load_snapshot_installs_128k_ram_banks_and_paging() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        let snapshot = z80_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![z80_snapshot::MemoryBlock::RamBank {
                bank: 3,
                data: vec![0xAB],
            }],
            paging: Some(z80_snapshot::PagingState {
                rom_bank: 0,
                ram_bank: 3,
                shadow_screen: false,
                locked: false,
            }),
            paging_2a3: None,
        };

        spectrum.load_snapshot(snapshot).unwrap();

        assert_eq!(spectrum.read_memory(0xC000), 0xAB);
    }

    #[test]
    fn load_snapshot_installs_2a3_ram_banks_and_paging() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        let snapshot = z80_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![z80_snapshot::MemoryBlock::RamBank {
                bank: 3,
                data: vec![0xCD],
            }],
            paging: None,
            paging_2a3: Some(z80_snapshot::Paging2A3State {
                ram_bank: 3,
                shadow_screen: false,
                rom_bit_lo: false,
                locked: false,
                special_mode: false,
                bits_1_2: 0,
                disk_motor: false,
            }),
        };

        spectrum.load_snapshot(snapshot).unwrap();

        assert_eq!(spectrum.read_memory(0xC000), 0xCD);
    }

    #[test]
    fn load_snapshot_rejects_2a3_snapshot_on_128k_machine() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        let snapshot = z80_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: None,
            paging_2a3: Some(z80_snapshot::Paging2A3State {
                ram_bank: 0,
                shadow_screen: false,
                rom_bit_lo: false,
                locked: false,
                special_mode: false,
                bits_1_2: 0,
                disk_motor: false,
            }),
        };

        assert!(spectrum.load_snapshot(snapshot).is_err());
    }

    #[test]
    fn load_sna_snapshot_installs_registers_memory_and_border() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let mut cpu = z80::Cpu::new();
        cpu.af = 0x1234;
        cpu.pc = 0x8000;
        let snapshot = sna_snapshot::Snapshot {
            cpu,
            border_color: 5,
            memory: vec![sna_snapshot::MemoryBlock::Absolute {
                address: 0x4000,
                data: vec![0x76],
            }],
            paging: None,
        };

        spectrum.load_sna_snapshot(snapshot).unwrap();

        assert_eq!(spectrum.registers()[0], ("AF", 0x1234));
        assert_eq!(spectrum.read_memory(0x4000), 0x76);
        assert_eq!(spectrum.bus.border_color(), 5);
    }

    #[test]
    fn load_sna_snapshot_rejects_a_128k_snapshot_on_a_48k_machine() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let snapshot = sna_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: Some(sna_snapshot::PagingState {
                rom_bank: 0,
                ram_bank: 0,
                shadow_screen: false,
                locked: false,
            }),
        };

        assert!(spectrum.load_sna_snapshot(snapshot).is_err());
    }

    #[test]
    fn load_sna_snapshot_rejects_a_48k_snapshot_on_a_128k_machine() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        let snapshot = sna_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: None,
        };

        assert!(spectrum.load_sna_snapshot(snapshot).is_err());
    }

    #[test]
    fn load_sna_snapshot_rejects_2a3_outright() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        let snapshot = sna_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![],
            paging: None,
        };

        assert!(spectrum.load_sna_snapshot(snapshot).is_err());
    }

    #[test]
    fn load_sna_snapshot_installs_128k_ram_banks_and_paging() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        let snapshot = sna_snapshot::Snapshot {
            cpu: z80::Cpu::new(),
            border_color: 0,
            memory: vec![sna_snapshot::MemoryBlock::RamBank {
                bank: 3,
                data: vec![0xAB],
            }],
            paging: Some(sna_snapshot::PagingState {
                rom_bank: 1,
                ram_bank: 3,
                shadow_screen: false,
                locked: false,
            }),
        };

        spectrum.load_sna_snapshot(snapshot).unwrap();

        assert_eq!(spectrum.read_memory(0xC000), 0xAB);
    }

    #[test]
    fn save_then_load_snapshot_round_trips_48k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x11, 0x22, 0x33]);
        spectrum.set_pc(0x8000);
        spectrum.bus.set_border_color(4);

        let saved = spectrum.save_snapshot();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        restored.load_snapshot(saved).unwrap();

        assert_eq!(restored.registers(), spectrum.registers());
        assert_eq!(restored.read_memory(RAM_BASE), 0x11);
        assert_eq!(restored.bus.border_color(), 4);
    }

    #[test]
    fn save_then_load_sna_snapshot_round_trips_48k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x11, 0x22, 0x33]);
        spectrum.cpu.pc = 0x8000;
        spectrum.cpu.sp = 0x9000; // valid RAM SP -- .sna's 48K format needs sp-2 in RAM
        spectrum.bus.set_border_color(4);

        let saved = spectrum.save_sna_snapshot().unwrap();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        restored.load_sna_snapshot(saved).unwrap();

        assert_eq!(restored.registers(), spectrum.registers());
        assert_eq!(restored.read_memory(RAM_BASE), 0x11);
        assert_eq!(restored.bus.border_color(), 4);
    }

    #[test]
    fn save_then_load_snapshot_round_trips_128k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        spectrum.bus.load_ram_bank(3, &[0xAB]);
        spectrum.bus.write_io(0x7FFD, 0b0000_0011); // page bank 3 into 0xC000

        let saved = spectrum.save_snapshot();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        restored.load_snapshot(saved).unwrap();

        assert_eq!(restored.read_memory(0xC000), 0xAB);
    }

    #[test]
    fn save_snapshot_2a3_rejects_sna_but_not_z80() {
        let spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);

        assert!(spectrum.save_sna_snapshot().is_err());
        let _ = spectrum.save_snapshot(); // must not panic
    }

    #[test]
    fn save_then_load_szx_snapshot_round_trips_48k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x11, 0x22, 0x33]);
        spectrum.set_pc(0x8000);
        spectrum.bus.set_border_color(4);

        let saved = spectrum.save_szx_snapshot();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        restored.load_szx_snapshot(saved).unwrap();

        assert_eq!(restored.registers(), spectrum.registers());
        assert_eq!(restored.read_memory(RAM_BASE), 0x11);
        assert_eq!(restored.bus.border_color(), 4);
    }

    #[test]
    fn save_then_load_szx_snapshot_round_trips_128k_state_and_ay() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        spectrum.bus.load_ram_bank(3, &[0xAB]);
        spectrum.bus.write_io(0x7FFD, 0b0000_0011); // page bank 3 into 0xC000
        spectrum.bus.write_io(0xFFFD, 8); // select register 8 (channel A volume)
        spectrum.bus.write_io(0xBFFD, 0x0F); // full volume

        let saved = spectrum.save_szx_snapshot();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        restored.load_szx_snapshot(saved).unwrap();

        assert_eq!(restored.read_memory(0xC000), 0xAB);
        assert_eq!(restored.bus.ay().unwrap().read_register(8), 0x0F);
    }

    #[test]
    fn save_then_load_szx_snapshot_round_trips_2a3_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        spectrum.bus.load_ram_bank(6, &[0xCD]);
        spectrum.bus.write_io(0x7FFD, 0b0000_0110); // page bank 6 into 0xC000

        let saved = spectrum.save_szx_snapshot();
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        restored.load_szx_snapshot(saved).unwrap();

        assert_eq!(restored.read_memory(0xC000), 0xCD);
    }

    #[test]
    fn load_szx_snapshot_rejects_a_128k_file_on_a_48k_machine() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let snapshot_128k =
            ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128)
                .save_szx_snapshot();

        assert!(spectrum.load_szx_snapshot(snapshot_128k).is_err());
    }

    #[test]
    fn save_state_then_load_state_round_trips_48k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x11, 0x22, 0x33]);
        spectrum.set_pc(0x8000);
        spectrum.bus.set_border_color(4);
        spectrum.press_key(ZxSpectrumKey::A);
        spectrum.set_joystick_type(JoystickType::Sinclair1);

        let bytes = Machine::save_state(&spectrum);
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        Machine::load_state(&mut restored, &bytes).unwrap();

        assert_eq!(restored.registers(), spectrum.registers());
        assert_eq!(restored.read_memory(RAM_BASE), 0x11);
        assert_eq!(restored.bus.border_color(), 4);
        assert_eq!(restored.bus.keyboard(), spectrum.bus.keyboard());
        assert_eq!(restored.joystick_type(), JoystickType::Sinclair1);
    }

    #[test]
    fn save_state_then_load_state_round_trips_128k_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        spectrum.bus.load_ram_bank(3, &[0xAB]);
        spectrum.bus.write_io(0x7FFD, 0b0000_0011); // page bank 3 into 0xC000

        let bytes = Machine::save_state(&spectrum);
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        Machine::load_state(&mut restored, &bytes).unwrap();

        assert_eq!(restored.read_memory(0xC000), 0xAB);
    }

    #[test]
    fn save_state_then_load_state_round_trips_2a3_state() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        spectrum.bus.load_ram_bank(3, &[0xCD]);
        spectrum.bus.write_io(0x7FFD, 0b0000_0011); // page bank 3 into 0xC000

        let bytes = Machine::save_state(&spectrum);
        let mut restored = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        Machine::load_state(&mut restored, &bytes).unwrap();

        assert_eq!(restored.read_memory(0xC000), 0xCD);
    }

    #[test]
    fn load_state_rejects_a_save_state_from_a_different_model() {
        let spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let bytes = Machine::save_state(&spectrum);

        let mut wrong_model =
            ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);

        assert!(Machine::load_state(&mut wrong_model, &bytes).is_err());
    }

    #[test]
    fn load_state_rejects_garbage_bytes() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);

        assert!(Machine::load_state(&mut spectrum, &[0xFF; 4]).is_err());
    }

    #[test]
    fn stepping_generates_audio_samples_at_the_expected_rate() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // NOPs -- 4 T-states each, real Z80 timing (not the beeper's own
        // level, which stays silent here -- this test is only checking
        // that `step` emits samples at the right cadence).
        spectrum.load(RAM_BASE, &vec![0x00u8; 1000]);
        spectrum.set_pc(RAM_BASE);

        let mut total_cycles = 0u64;
        for _ in 0..1000 {
            total_cycles += spectrum.step() as u64;
        }
        let samples = spectrum.drain_audio();

        let expected = (total_cycles as f64 * machine::AUDIO_SAMPLE_RATE as f64
            / spectrum.clock_hz() as f64)
            .floor() as usize;
        // Within 1 of the exact expected count -- the accumulator's
        // fractional remainder means the exact instant of the last sample
        // can land on either side of the final instruction boundary.
        assert!(
            samples.len().abs_diff(expected) <= 1,
            "expected ~{expected} samples, got {}",
            samples.len()
        );
    }

    #[test]
    fn drain_audio_returns_silence_when_speaker_and_ay_are_both_quiet() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x00u8; 200]);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..200 {
            spectrum.step();
        }

        assert!(spectrum.drain_audio().iter().all(|&s| s == 0.0));
    }

    #[test]
    fn turbosound_second_chip_is_audible_and_mixed_in() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_128K], Region::Pal, Model::Spectrum128);
        spectrum.set_turbosound(true);

        // Drive the second chip directly through the real ports (same
        // protocol `bus.rs`'s own turbosound tests exercise): select it,
        // give channel A a short tone period, then unmute + max its
        // volume so it's producing real output, not silence.
        spectrum.bus.write_io(0xFFFD, 0xFE); // select 2nd chip
        spectrum.bus.write_io(0xFFFD, 0); // register 0: tone A fine tune
        spectrum.bus.write_io(0xBFFD, 0x10);
        spectrum.bus.write_io(0xFFFD, 7); // register 7: mixer
        spectrum.bus.write_io(0xBFFD, 0b1111_1110); // enable tone A only
        spectrum.bus.write_io(0xFFFD, 8); // register 8: channel A volume
        spectrum.bus.write_io(0xBFFD, 0x0F); // max, no envelope

        spectrum.load(RAM_BASE, &vec![0x00u8; 4000]);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..4000 {
            spectrum.step();
        }

        assert!(
            spectrum.drain_audio().iter().any(|&s| s != 0.0),
            "expected the second AY chip's tone to reach the mixed audio output"
        );
    }

    #[test]
    fn drain_audio_reflects_a_beeper_toggle() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        // OUT (0xFE),A with A's bit 4 set -- turns the beeper on -- then
        // enough NOPs to cross at least one audio sample boundary.
        let mut program = vec![0x3E, 0b0001_0000, 0xD3, 0xFE];
        program.extend(vec![0x00u8; 200]);
        spectrum.load(RAM_BASE, &program);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..204 {
            spectrum.step();
        }

        assert!(spectrum.drain_audio().iter().any(|&s| s > 0.0));
    }

    #[test]
    fn unfiltered_speaker_type_is_a_pure_passthrough_by_default() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let mut program = vec![0x3E, 0b0001_0000, 0xD3, 0xFE]; // beeper on
        program.extend(vec![0x00u8; 200]);
        spectrum.load(RAM_BASE, &program);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..204 {
            spectrum.step();
        }

        // The raw mixed sample is beeper-on (0.3) + no AY on 48K -- exactly
        // 0.3, unfiltered, matching this codebase's behavior before
        // `SpeakerType` existed at all.
        assert!(spectrum.drain_audio().contains(&0.3));
    }

    #[test]
    fn filtered_speaker_types_smooth_the_beeper_toggle_instead_of_jumping_instantly() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.set_speaker_type(SpeakerType::Beeper);
        let mut program = vec![0x3E, 0b0001_0000, 0xD3, 0xFE]; // beeper on
        program.extend(vec![0x00u8; 200]);
        spectrum.load(RAM_BASE, &program);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..204 {
            spectrum.step();
        }

        let samples = spectrum.drain_audio();
        assert!(
            samples.iter().any(|&s| s > 0.0 && s < 0.3),
            "expected a smoothed-in sample strictly between 0 and the raw 0.3 level, got {samples:?}"
        );
    }

    #[test]
    fn drain_audio_empties_the_buffer() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &[0x00u8; 200]);
        spectrum.set_pc(RAM_BASE);
        for _ in 0..200 {
            spectrum.step();
        }
        spectrum.drain_audio();

        assert!(spectrum.drain_audio().is_empty());
    }

    #[test]
    fn sym_shift_and_caps_shift_are_distinct_keys() {
        // Regression guard for the two keys that differ from ZX81's matrix:
        // SymShift is row 7 bit 1, not the same slot as ZX81's `.`.
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.press_key(ZxSpectrumKey::SymShift);

        // Row 7 selected: bit 1 (SymShift) clear, bit 0 (Space) still set.
        assert_eq!(scan_row(&mut spectrum, 0b0111_1111), 0b1111_1101);
    }

    fn build_minimal_dsk() -> Vec<u8> {
        // Same construction as `bus.rs`'s own test helper.
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
        track[0x18] = 0;
        track[0x19] = 0;
        track[0x1A] = 1;
        track[0x1B] = 2;
        bytes.extend(track);

        bytes.extend(vec![0u8; 512]);
        bytes
    }

    #[test]
    fn load_disk_rejects_non_2a3_models() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        let disk = dsk::parse(&build_minimal_dsk()).unwrap();

        assert!(spectrum.load_disk(disk).is_err());
    }

    #[test]
    fn load_disk_succeeds_on_spectrum2a3() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        let disk = dsk::parse(&build_minimal_dsk()).unwrap();

        assert!(spectrum.load_disk(disk).is_ok());
    }

    #[test]
    fn ejecting_a_disk_empties_the_drive_and_a_new_one_goes_in() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE_2A3], Region::Pal, Model::Spectrum2A3);
        spectrum
            .load_disk(dsk::parse(&build_minimal_dsk()).unwrap())
            .unwrap();
        assert!(spectrum.disk().is_some());
        spectrum.eject_disk();
        assert!(spectrum.disk().is_none());
        spectrum
            .load_disk(dsk::parse(&build_minimal_dsk()).unwrap())
            .unwrap();
        assert!(spectrum.disk().is_some());
        // No FDC on other models: ejecting is a quiet no-op.
        let mut plain = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        plain.eject_disk();
        plain.eject_tape();
    }

    // --- Fast-tape-load trap ---

    /// Builds a ROM with the real `LD-BYTES` opcode (`0x14`, `INC D`) at
    /// `0x0556` -- everything else zeroed, matching every other test's
    /// stub-ROM convention.
    fn rom_with_ld_bytes_opcode() -> Vec<u8> {
        let mut rom = vec![0u8; ROM_SIZE];
        rom[0x0556] = 0x14;
        rom
    }

    /// A single standard-protocol `.tap` block: `flag`, `payload`, and a
    /// correctly-computed trailing checksum (XOR of every byte, flag
    /// included, must be `0` for the whole block including the checksum
    /// itself -- see `try_fast_tape_load_trap`'s own doc comment).
    fn tap_block(flag: u8, payload: &[u8]) -> zx_tape::Tape {
        let checksum = payload.iter().fold(flag, |acc, &b| acc ^ b);
        let mut data = vec![flag];
        data.extend_from_slice(payload);
        data.push(checksum);

        let mut bytes = (data.len() as u16).to_le_bytes().to_vec();
        bytes.extend_from_slice(&data);
        zx_tape::parse_tap(&bytes).unwrap()
    }

    /// Sets up a `ZxSpectrum` sitting right at the moment `CALL 0x0556`
    /// would have just landed: `pc` at the routine's entry, a return
    /// address of `0x8000` already on the stack (at `sp`, matching what a
    /// real `CALL` would have pushed), and the given `LD-BYTES` calling-
    /// convention registers.
    fn spectrum_at_ld_bytes(flag: u8, de: u16, ix: u16, loading: bool) -> ZxSpectrum {
        let mut spectrum =
            ZxSpectrum::new(rom_with_ld_bytes_opcode(), Region::Pal, Model::Spectrum48);
        spectrum.load(0xFF00, &0x8000u16.to_le_bytes()); // fake caller's return address
        spectrum.cpu.pc = 0x0556;
        spectrum.cpu.sp = 0xFF00;
        spectrum.cpu.de = de;
        spectrum.cpu.ix = ix;
        spectrum.cpu.set_a(flag);
        spectrum.cpu.set_f(if loading { 0x01 } else { 0x00 }); // carry: load vs verify
        spectrum
    }

    #[test]
    fn fast_tape_load_trap_loads_a_matching_block_instantly() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 3, 0x9000, true);
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33]));
        spectrum.set_fast_tape_load(true);

        let cost = spectrum.step();

        assert_eq!(spectrum.read_memory(0x9000), 0x11);
        assert_eq!(spectrum.read_memory(0x9001), 0x22);
        assert_eq!(spectrum.read_memory(0x9002), 0x33);
        assert_eq!(spectrum.cpu.ix, 0x9003);
        assert_eq!(spectrum.cpu.de, 0);
        assert_eq!(
            spectrum.cpu.f() & 0x01,
            0x01,
            "carry should be set: success"
        );
        assert_eq!(spectrum.cpu.pc, 0x8000, "should RET to the real caller");
        assert_eq!(spectrum.cpu.sp, 0xFF02);
        assert!(
            cost < 100,
            "should cost near-nothing, not thousands of real T-states"
        );
    }

    #[test]
    fn fast_tape_load_trap_does_nothing_when_disabled() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 3, 0x9000, true);
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33]));
        // set_fast_tape_load(true) NOT called -- off by default.

        spectrum.step();

        // Real INC D executed instead: pc advanced by 1, d incremented,
        // nothing else touched.
        assert_eq!(spectrum.cpu.pc, 0x0557);
        assert_eq!(
            spectrum.read_memory(0x9000),
            0x00,
            "no data should have been written"
        );
    }

    #[test]
    fn fast_tape_load_trap_falls_through_on_flag_mismatch() {
        let mut spectrum = spectrum_at_ld_bytes(0x00, 3, 0x9000, true); // expects header (0x00)
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33])); // tape's next block is data (0xFF)
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(spectrum.cpu.pc, 0x0557, "should not have trapped");
        assert_eq!(spectrum.read_memory(0x9000), 0x00);
    }

    #[test]
    fn fast_tape_load_trap_falls_through_on_length_mismatch() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 99, 0x9000, true); // expects 99 bytes
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33])); // block only has 3
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(spectrum.cpu.pc, 0x0557, "should not have trapped");
    }

    #[test]
    fn fast_tape_load_trap_falls_through_when_rom_byte_does_not_match() {
        let mut rom = vec![0u8; ROM_SIZE];
        rom[0x0556] = 0x00; // NOP, not the real INC D -- a "custom ROM"
        let mut spectrum = ZxSpectrum::new(rom, Region::Pal, Model::Spectrum48);
        spectrum.load(0xFF00, &0x8000u16.to_le_bytes());
        spectrum.cpu.pc = 0x0556;
        spectrum.cpu.sp = 0xFF00;
        spectrum.cpu.de = 3;
        spectrum.cpu.ix = 0x9000;
        spectrum.cpu.set_a(0xFF);
        spectrum.cpu.set_f(0x01);
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33]));
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(
            spectrum.cpu.pc, 0x0557,
            "NOP executed normally, not trapped"
        );
        assert_eq!(spectrum.read_memory(0x9000), 0x00);
    }

    #[test]
    fn fast_tape_load_trap_clears_carry_on_bad_checksum_but_still_writes_data() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 3, 0x9000, true);
        // A deliberately wrong checksum byte (should be 0xFF, per
        // `tap_block`'s algorithm, not 0x00) -- built directly rather than
        // via `tap_block`, since `Tape` exposes no way to corrupt one after
        // the fact.
        let bytes = [0x05, 0x00, 0xFF, 0x11, 0x22, 0x33, 0x00];
        spectrum.load_tape(zx_tape::parse_tap(&bytes).unwrap());
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(
            spectrum.read_memory(0x9000),
            0x11,
            "real hardware writes as it goes"
        );
        assert_eq!(spectrum.cpu.f() & 0x01, 0, "carry clear: checksum failed");
        assert_eq!(spectrum.cpu.pc, 0x8000, "still returns to the caller");
    }

    #[test]
    fn fast_tape_load_trap_verify_mode_compares_without_writing() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 3, 0x9000, false); // carry clear: VERIFY
        spectrum.load(0x9000, &[0x11, 0x22, 0x33]); // memory already holds the expected bytes
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33]));
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(spectrum.cpu.f() & 0x01, 0x01, "carry set: verify matched");
        assert_eq!(spectrum.read_memory(0x9000), 0x11, "verify never writes");
    }

    #[test]
    fn fast_tape_load_trap_verify_mode_clears_carry_on_mismatch() {
        let mut spectrum = spectrum_at_ld_bytes(0xFF, 3, 0x9000, false); // VERIFY
        spectrum.load(0x9000, &[0x11, 0x22, 0xFF]); // last byte doesn't match the tape's
        spectrum.load_tape(tap_block(0xFF, &[0x11, 0x22, 0x33]));
        spectrum.set_fast_tape_load(true);

        spectrum.step();

        assert_eq!(spectrum.cpu.f() & 0x01, 0, "carry clear: verify mismatched");
    }

    /// Steps 6000 NOPs and says whether the tape produced any pulse.
    fn tape_ran_for_6000_nops(spectrum: &mut ZxSpectrum) -> bool {
        for _ in 0..6000 {
            spectrum.step();
        }
        !spectrum.overlay_tape_pulse_widths().is_empty()
    }

    #[test]
    fn a_tape_does_not_start_when_the_keyboard_port_is_read() {
        // The ROM's keyboard scan reads port 0xFE from power-on; a loaded tape must not run then.
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &vec![0x00u8; 1000]);
        spectrum.set_pc(RAM_BASE);
        spectrum.load_tape(tap_block(0x00, &[0x00, 0x00]));
        spectrum.bus.read_io(0x00FE);
        spectrum.bus.read_io(0x7FFE);
        assert!(!tape_ran_for_6000_nops(&mut spectrum));
    }

    #[test]
    fn a_tape_starts_when_the_rom_loader_is_entered() {
        let mut spectrum = spectrum_at_ld_bytes(0x00, 2, 0x8000, true);
        spectrum.load_tape(tap_block(0x00, &[0x00, 0x00]));
        // Not before: the loader at 0x0556 is where Play is pressed.
        assert!(spectrum.bus.tape_mut().is_some_and(|t| !t.finished()));
        spectrum.step(); // executes INC D at 0x0556 and presses Play first
        spectrum.load(RAM_BASE, &vec![0x00u8; 1000]);
        spectrum.set_pc(RAM_BASE);
        assert!(tape_ran_for_6000_nops(&mut spectrum));
    }

    #[test]
    fn any_read_mode_starts_the_tape_at_the_first_port_read() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.set_tape_start_on_any_read(true);
        spectrum.load(RAM_BASE, &vec![0x00u8; 1000]);
        spectrum.set_pc(RAM_BASE);
        spectrum.load_tape(tap_block(0x00, &[0x00, 0x00]));
        spectrum.bus.read_io(0x00FE);
        assert!(tape_ran_for_6000_nops(&mut spectrum));
    }

    #[test]
    fn tape_pulse_widths_report_real_pilot_tone_timing() {
        let mut spectrum = ZxSpectrum::new(vec![0; ROM_SIZE], Region::Pal, Model::Spectrum48);
        spectrum.load(RAM_BASE, &vec![0x00u8; 1000]); // NOPs, real per-instruction 4T granularity
        spectrum.set_pc(RAM_BASE);
        spectrum.load_tape(tap_block(0x00, &[0x00, 0x00]));
        // Press Play (the machine does this when the ROM's LD-BYTES is entered).
        spectrum.bus.start_tape();

        // Real per-instruction stepping (4T/NOP), not a single coarse
        // manual `tick_tape` batch -- edge detection happens once per
        // `tick_tape` call, so a batch coarser than real CPU-instruction
        // granularity would quantize the recorded width to the nearest
        // batch boundary instead of the real edge position.
        for _ in 0..6000 {
            spectrum.step();
        }

        let widths = spectrum.overlay_tape_pulse_widths();
        assert!(
            !widths.is_empty(),
            "expected at least one completed pulse width by now"
        );
        // Standard ROM pilot tone is 2168 T-states/pulse (see
        // CREDITS.md); a 2-byte block's pilot run (8063 or 3223 pulses)
        // is far longer than 24,000 T-states can traverse, so every
        // captured pulse here should still be pilot tone. ZX Spectrum has
        // no cross-clock-domain scaling (unlike CPC), so widths land
        // within a couple of T-states of the exact value -- the edge is
        // only ever detected at an instruction boundary (here, 4T NOPs),
        // not mid-instruction, so a pulse whose real edge falls partway
        // through one instruction gets attributed to whichever boundary
        // it's next observed at.
        for &width in &widths {
            assert!(
                (2166..=2170).contains(&width),
                "expected pilot-tone pulse width (~2168T), got {width} in {widths:?}"
            );
        }
    }
}
