mod bus;
mod keyboard;
mod save_state;
mod video;

use machine::{Clock, Machine, Region, RunStats};
use z80::Cpu;

use bus::Zx81Bus;
pub use keyboard::Zx81Key;

/// ZX81 ROM size (8K, the unexpanded machine's only ROM chip).
pub const ROM_SIZE: usize = 0x2000;
/// Fixed RAM base address on real hardware.
pub const RAM_BASE: u16 = 0x4000;
/// Unexpanded ZX81 RAM size (1K on-board, no 16K RAM pack).
pub const RAM_SIZE: usize = 0x0400;
/// The RAM sizes (in K) a machine can have: the on-board 1K, a 2K upgrade, and the 16K, 32K, 48K and 56K packs.
pub const RAM_KB_CHOICES: [usize; 6] = [1, 2, 16, 32, 48, 56];

/// Real ZX81 NMI generator rate while running: one pulse per TV line, 64us
/// (PAL) -- 3.25MHz clock_hz * 64e-6s = 208 T-states exactly, which is also
/// consistent with PAL's well-known 312.5 lines/frame * 50Hz = 15625 Hz
/// line rate (1/15625Hz = 64us). Applied for NTSC/Japan too as a documented
/// approximation, same stance as `clock_hz`'s own NTSC placeholder below --
/// real NTSC ZX81/TS1000 line timing isn't verified here.
const NMI_PERIOD_TSTATES: u32 = 208;

/// A ZX81: a Z80 core, an 8K ROM, 1K of RAM, a keyboard matrix, and video
/// output (`render_frame`, decoded from the display file using the real
/// ROM character generator). No tape yet. `step` fires a real NMI at
/// `NMI_PERIOD_TSTATES` while `Zx81Bus::nmi_generator_running` is set
/// (which the ROM's own `OUT (0xFE)`/`OUT (0xFD)` control, verified against
/// `roms/zx81/zx81.rom`'s actual usage -- see `bus::Zx81Bus`'s doc
/// comment), matching real hardware's continuous line-rate NMI generator.
/// `render_frame` itself still doesn't model real pixel-level video
/// generation (that's a much bigger undertaking -- the NMI's real purpose
/// on hardware is timing the Z80's own instruction-fetch pattern into a
/// picture, which this does not reproduce); it decodes a static snapshot
/// of current display-file state on demand. The NMI *is* what makes that
/// snapshot meaningful over time, though: it's what drives the ROM's
/// cursor-blink counter and its BASIC-execution HALT/NMI runtime loop, so
/// without it the display file was observed to freeze indefinitely (see
/// `TODO.md`'s Interrupts section).
#[derive(Clone)]
pub struct Zx81 {
    cpu: Cpu,
    bus: Zx81Bus,
    region: Region,
    /// T-states accumulated since the last NMI, while the generator is
    /// running. Not reset when the generator stops -- modeled as a gated
    /// free-running oscillator (its output is gated off, not its counting),
    /// so re-enabling resumes from wherever it left off rather than
    /// restarting a fresh period. No strong evidence either way from real
    /// hardware; this is the simpler of the two reasonable choices.
    nmi_cycles: u32,
    /// Whether `render_frame` pads the active picture with a border (see
    /// `video::BORDER_PX`). On by default -- `set_border` is how a caller
    /// (e.g. `retroclawd --no-border`) turns it off. Not a `new()`
    /// parameter deliberately: nearly every caller (tests included) wants
    /// the default, and a setter avoids touching every existing call site.
    border: bool,
}

impl Zx81 {
    /// `rom` is the real ZX81 ROM image (8K), supplied externally -- it's
    /// copyrighted and not bundled with this repo. `region` selects the
    /// market variant's clock/frame rate (see `Machine::clock_hz`/
    /// `frame_hz` below).
    pub fn new(rom: Vec<u8>, region: Region) -> Self {
        Self::build(rom, region, 1)
    }

    /// Like `new`, with `ram_kb` K of RAM installed: one of `RAM_KB_CHOICES`.
    pub fn with_ram_kb(rom: Vec<u8>, region: Region, ram_kb: usize) -> Result<Self, String> {
        if !RAM_KB_CHOICES.contains(&ram_kb) {
            return Err(format!(
                "unsupported ZX81 RAM size {ram_kb}K (choose one of {RAM_KB_CHOICES:?})"
            ));
        }
        Ok(Self::build(rom, region, ram_kb))
    }

    fn build(rom: Vec<u8>, region: Region, ram_kb: usize) -> Self {
        Self {
            cpu: Cpu::new(),
            bus: Zx81Bus::new(rom, ram_kb),
            region,
            nmi_cycles: 0,
            border: true,
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

    /// Performs an `OUT (port),value` on the I/O bus between instructions
    /// (the debugger's `write-port`), decoded exactly like a CPU `OUT` --
    /// including the ZX81's own side effects (an `OUT` to the NMI-generator
    /// ports switches the display's NMI source on or off).
    pub fn write_port(&mut self, port: u16, value: u8) {
        use machine::Bus;
        self.bus.write_io(port, value);
    }

    /// Presses a key, holding it down until `release_key` is called.
    pub fn press_key(&mut self, key: Zx81Key) {
        self.bus.press_key(key);
    }

    /// Releases a previously pressed key. Releasing an already-released key
    /// is a no-op.
    pub fn release_key(&mut self, key: Zx81Key) {
        self.bus.release_key(key);
    }

    /// Installs a parsed `.p`/`.p81` snapshot (`zx81_snapshot::parse_p`/
    /// `parse_p81`). Unlike `.z80` (see `zx_spectrum::ZxSpectrum::
    /// load_snapshot`), neither ZX81 file format encodes any register
    /// state -- they're a raw memory dump only, always meant to be loaded
    /// starting at `0x4009`. Real ZX81 hardware synthesizes a fixed
    /// register/system-variable state around that dump so `BASIC` can
    /// resume cleanly, derived from `RAMTOP`; the constants below (the
    /// 9-byte system-variable header, the 4-byte stack patch, and every
    /// register value) are taken from ZEsarUX's `new_load_zx81_set_common_
    /// registers` (`src/snap/snap_zx8081.c`,
    /// <https://github.com/chernandezba/zesarux>, GPLv3, Cesar Hernandez
    /// Bano -- see `CREDITS.md`), with `RAMTOP` at the top of this
    /// machine's own RAM (`0x43FF` for the stock 1K, `0x7FFF` with a 16K pack).
    ///
    /// Real `.p`/`.p81` files are routinely several KB (a `BASIC` program
    /// plus variables plus workspace, saved from a machine with a 16K+
    /// rampack): load one into a machine built with a matching `ram_kb`,
    /// or it is truncated at the end of the installed RAM.
    pub fn load_p_snapshot(&mut self, snapshot: zx81_snapshot::Snapshot) {
        let ramtop = self.bus.ram_end();
        let sp_value = ramtop - 3;
        let ramtop_plus_one = ramtop.wrapping_add(1);

        self.bus.load(0x4009, &snapshot.data);

        let [sp_lo, sp_hi] = sp_value.to_le_bytes();
        let [top_lo, top_hi] = ramtop_plus_one.to_le_bytes();
        self.bus.load(
            0x4000,
            &[0xFF, 0x80, sp_lo, sp_hi, top_lo, top_hi, 0x00, 0xFE, 0xFF],
        );
        self.bus.load(sp_value, &[0x76, 0x06, 0x00, 0x3E]);

        self.cpu = Cpu::new();
        self.cpu.af = 0x0B85;
        self.cpu.bc = 0x00FF;
        self.cpu.de = 0x4399;
        self.cpu.hl = 0xC399;
        self.cpu.af_ = 0xE2A1;
        self.cpu.bc_ = 0x8102;
        self.cpu.de_ = 0x002B;
        self.cpu.hl_ = 0x0000;
        self.cpu.i = 0x1E;
        self.cpu.iff1 = false;
        self.cpu.iff2 = false;
        self.cpu.r = 0xDD;
        self.cpu.ix = 0x0281;
        self.cpu.iy = 0x4000;
        self.cpu.sp = sp_value;
        self.cpu.pc = 0x0207;

        // Real hardware is always mid-SLOW (NMI generator running) by the
        // time a program has loaded and is sitting in BASIC.
        self.bus.set_nmi_generator_running(true);
        self.nmi_cycles = 0;
    }

    /// The machine as a `.p`/`.81` file: memory from `0x4009` up to `E_LINE`-1
    /// (`E_LINE` is the word at `0x4014`), exactly what the ROM's `SAVE` writes.
    /// Fails when `E_LINE` does not point into the installed RAM, i.e. the
    /// system variables are not set up (before the ROM has run).
    pub fn save_p_snapshot(&self) -> Result<zx81_snapshot::Snapshot, String> {
        const E_LINE: u16 = 0x4014;
        const START: u16 = 0x4009;
        let e_line = u16::from_le_bytes([self.read_memory(E_LINE), self.read_memory(E_LINE + 1)]);
        let ram_end = self.bus.ram_end();
        if e_line <= START || e_line > ram_end {
            return Err(format!(
                "cannot save a .p: E_LINE is 0x{e_line:04X}, outside 0x{START:04X}-0x{ram_end:04X} \
                 (the ROM has not set up the system variables)"
            ));
        }
        Ok(zx81_snapshot::Snapshot {
            program_name_raw: None,
            data: (START..e_line).map(|a| self.read_memory(a)).collect(),
        })
    }

    pub(crate) fn save_state(&self) -> save_state::SaveState {
        save_state::SaveState {
            cpu: self.cpu,
            ram: self.bus.flat_ram(),
            keyboard: self.bus.keyboard(),
            nmi_generator_running: self.bus.nmi_generator_running(),
            nmi_cycles: self.nmi_cycles,
            border: self.border,
        }
    }

    pub(crate) fn load_state(&mut self, state: save_state::SaveState) {
        self.cpu = state.cpu;
        self.bus.load(self.bus.ram_start(), &state.ram);
        self.bus.set_keyboard(state.keyboard);
        self.bus
            .set_nmi_generator_running(state.nmi_generator_running);
        self.nmi_cycles = state.nmi_cycles;
        self.border = state.border;
    }
}

impl Machine for Zx81 {
    fn clock_hz(&self) -> u64 {
        match self.region {
            Region::Pal => 3_250_000,
            // Placeholder: real NTSC ZX81 / Timex Sinclair 1000 crystal not
            // yet verified against a hardware reference -- tracked as an
            // open question in the design spec.
            Region::Ntsc | Region::Japan => 3_250_000,
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

    /// Times real NMIs off the generator's rate (see `NMI_PERIOD_TSTATES`
    /// and the struct's own doc comment) in addition to stepping the CPU --
    /// this lives here rather than going through `Machine::request_interrupt`
    /// (called once per `frame_hz()` tick by `Clock::run`/the Bevy GUI loop)
    /// because the real NMI rate is far finer-grained than once a frame.
    fn step(&mut self) -> u32 {
        // The maskable interrupt is asserted while bit 6 of the refresh register is zero ("ZX80/ZX81 Video
        // Interrupts", <https://problemkaputt.de/zxdocs.htm>: INT is wired to A6 of the refresh address). The
        // ROM sets R before it runs a line of the display file so that the interrupt arrives when the line
        // has been drawn; the CPU accepts it only while its interrupts are enabled, as on hardware.
        if self.cpu.r & 0x40 == 0 {
            self.cpu.request_interrupt();
        }
        let cycles = self.cpu.step(&mut self.bus);
        if self.bus.nmi_generator_running() {
            self.nmi_cycles += cycles as u32;
            if self.nmi_cycles >= NMI_PERIOD_TSTATES {
                self.nmi_cycles -= NMI_PERIOD_TSTATES;
                self.cpu.request_nmi();
            }
        }
        cycles as u32
    }

    fn halted(&self) -> bool {
        self.cpu.halted
    }

    // No `request_interrupt` (maskable) override -- uses the `Machine`
    // trait's default no-op, because the ZX81 has no once-per-frame
    // interrupt: its INT comes from the refresh counter and is raised in
    // `step` (see there), and the NMI comes from the line generator. A
    // once-per-frame `INT` like the Spectrum's was tried and was wrong: it
    // landed in the middle of the ROM's line-counting code at `0x0038` at
    // moments the display routine never expects, corrupting the stack.

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
        self.nmi_cycles = 0;
    }

    fn render_frame(&self) -> Option<(u32, u32, Vec<u8>)> {
        Some(video::render_frame(&self.bus, self.border))
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
        self.load_state(state);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_tiny_program_to_halt_and_reports_registers() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        // LD A,0x2A ; HALT -- loaded into RAM since ROM writes are dropped.
        zx81.load(RAM_BASE, &[0x3E, 0x2A, 0x76]);
        zx81.set_pc(RAM_BASE);

        let stats = zx81.run(&mut |m| !m.halted());

        assert_eq!(stats.instructions, 2);
        assert!(zx81.halted());
        assert_eq!(zx81.registers()[0], ("AF", 0x2A00));
    }

    #[test]
    fn port_watches_latch_the_cpus_own_out() {
        use machine::{PortHit, PortWatch};
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        // LD A,0 ; OUT (0xFE),A ; HALT
        zx81.load(RAM_BASE, &[0x3E, 0x00, 0xD3, 0xFE, 0x76]);
        zx81.set_pc(RAM_BASE);
        assert!(zx81.set_port_watches(vec![PortWatch {
            port: 0x00FE,
            mask: 0x00FF,
            on_read: true,
            on_write: true,
        }]));
        zx81.step();
        assert_eq!(zx81.take_port_hit(), None);
        zx81.step();
        assert_eq!(
            zx81.take_port_hit(),
            Some(PortHit {
                port: 0x00FE,
                value: 0,
                write: true
            })
        );
    }

    #[test]
    fn write_port_is_decoded_like_a_cpu_out() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        assert!(!zx81.bus.nmi_generator_running());
        // The ROM's own ports: OUT (0xFE) starts the NMI generator, OUT (0xFD)
        // stops it.
        zx81.write_port(0x00FE, 0);
        assert!(zx81.bus.nmi_generator_running());
        zx81.write_port(0x00FD, 0);
        assert!(!zx81.bus.nmi_generator_running());
    }

    /// INT is asserted while bit 6 of R is zero ("ZX80/ZX81 Video Interrupts",
    /// <https://problemkaputt.de/zxdocs.htm>); the CPU takes it only with interrupts enabled.
    #[test]
    fn the_maskable_interrupt_follows_bit_6_of_r() {
        let run = |r: u64, iff1: u64| {
            let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
            zx81.load(RAM_BASE, &[0x00; 8]);
            zx81.set_pc(RAM_BASE);
            zx81.set_register("IM", 1).unwrap();
            zx81.set_register("IFF1", iff1).unwrap();
            zx81.set_register("R", r).unwrap();
            zx81.step();
            pc_of(&zx81)
        };
        assert_eq!(run(0x00, 1), 0x0038, "bit 6 clear, interrupts on: RST 38h");
        assert_eq!(run(0x40, 1), RAM_BASE as u64 + 1, "bit 6 set: no interrupt");
        assert_eq!(
            run(0x00, 0),
            RAM_BASE as u64 + 1,
            "interrupts off: not taken"
        );
    }

    fn pc_of(zx81: &Zx81) -> u64 {
        zx81.registers()
            .into_iter()
            .find(|(name, _)| *name == "PC")
            .unwrap()
            .1
    }

    #[test]
    fn nmi_does_not_fire_while_generator_is_stopped() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.load(RAM_BASE, &[0x00; 100]); // 100 NOPs
        zx81.set_pc(RAM_BASE);

        for _ in 0..100 {
            zx81.step();
        }

        // Plain sequential execution the whole way -- no NMI ever
        // redirected PC to 0x0066.
        assert_eq!(pc_of(&zx81), RAM_BASE as u64 + 100);
    }

    #[test]
    fn nmi_fires_after_enough_cycles_once_generator_is_started() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        // OUT (0xFE),A starts the generator (real ROM's own opcode), then
        // enough NOPs to cross NMI_PERIOD_TSTATES (208).
        let mut program = vec![0xD3, 0xFE];
        program.extend(std::iter::repeat_n(0x00u8, 100));
        zx81.load(RAM_BASE, &program);
        zx81.set_pc(RAM_BASE);

        let fired = (0..60).any(|_| {
            zx81.step();
            pc_of(&zx81) == 0x0066
        });

        assert!(fired, "NMI should have fired and vectored to 0x0066");
        // NMI pushed a 2-byte return address -- SP (default 0) wrapped
        // down by 2.
        assert_eq!(
            zx81.registers()
                .into_iter()
                .find(|(name, _)| *name == "SP")
                .unwrap()
                .1,
            0xFFFE
        );
    }

    #[test]
    fn load_p_snapshot_installs_data_registers_and_starts_nmi_generator() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        let snapshot = zx81_snapshot::Snapshot {
            program_name_raw: None,
            data: vec![0xAA, 0xBB],
        };

        zx81.load_p_snapshot(snapshot);

        assert_eq!(zx81.read_memory(0x4009), 0xAA);
        assert_eq!(zx81.read_memory(0x400A), 0xBB);
        assert_eq!(pc_of(&zx81), 0x0207);
        assert_eq!(
            zx81.registers()
                .into_iter()
                .find(|(n, _)| *n == "SP")
                .unwrap()
                .1,
            0x43FC
        );
        assert!(zx81.bus.nmi_generator_running());
    }

    #[test]
    fn save_p_snapshot_returns_memory_from_0x4009_to_e_line() {
        // A file whose E_LINE (system variable at 0x4014 = file offset 11) is 0x4090.
        let mut data = vec![0x11u8; 0x4090 - 0x4009];
        data[11..13].copy_from_slice(&0x4090u16.to_le_bytes());
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.load_p_snapshot(zx81_snapshot::Snapshot {
            program_name_raw: None,
            data: data.clone(),
        });

        let saved = zx81.save_p_snapshot().unwrap();

        assert_eq!(saved.data, data);
        assert_eq!(saved.program_name_raw, None);
    }

    #[test]
    fn save_p_snapshot_refuses_a_machine_whose_system_variables_are_unset() {
        let zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        assert!(zx81.save_p_snapshot().unwrap_err().contains("E_LINE"));
    }

    #[test]
    fn save_state_then_load_state_round_trips_state() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.load(RAM_BASE, &[0x11, 0x22, 0x33]);
        zx81.set_pc(0x4010);
        zx81.press_key(Zx81Key::Q);
        zx81.bus.set_nmi_generator_running(true);

        let bytes = Machine::save_state(&zx81);
        let mut restored = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        Machine::load_state(&mut restored, &bytes).unwrap();

        assert_eq!(restored.registers(), zx81.registers());
        assert_eq!(restored.read_memory(RAM_BASE), 0x11);
        assert!(restored.bus.nmi_generator_running());
        assert_eq!(scan_row(&mut restored, 0b1111_1011), 0b1111_1110);
    }

    #[test]
    fn load_state_rejects_garbage_bytes() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);

        assert!(Machine::load_state(&mut zx81, &[0xFF; 4]).is_err());
    }

    #[test]
    fn frame_hz_follows_region() {
        let pal = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        let ntsc = Zx81::new(vec![0; ROM_SIZE], Region::Ntsc);

        assert_eq!(pal.frame_hz(), 50);
        assert_eq!(ntsc.frame_hz(), 60);
    }

    /// Runs `LD A,<row_mask>` / `IN A,(0xFE)` / `HALT` through the real CPU
    /// and returns the resulting `A` register -- the same path the ROM's
    /// own keyboard scan uses, so this exercises `Zx81Bus::read_io` and
    /// `KeyboardMatrix` together rather than testing either in isolation.
    fn scan_row(zx81: &mut Zx81, row_mask: u8) -> u8 {
        zx81.load(RAM_BASE, &[0x3E, row_mask, 0xDB, 0xFE, 0x76]);
        zx81.set_pc(RAM_BASE);
        zx81.run(&mut |m| !m.halted());
        (zx81.registers()[0].1 >> 8) as u8
    }

    #[test]
    fn pressed_key_clears_its_bit_when_scanned_via_in_a_n() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.press_key(Zx81Key::Q); // row 2 (A10), bit 0

        // Select row 2 only: clear bit 2 of the row-select mask.
        assert_eq!(scan_row(&mut zx81, 0b1111_1011), 0b1111_1110);
    }

    #[test]
    fn released_key_reads_back_as_unpressed() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.press_key(Zx81Key::Enter); // row 6 (A14), bit 0
        zx81.release_key(Zx81Key::Enter);

        assert_eq!(scan_row(&mut zx81, 0b1011_1111), 0xFF);
    }

    #[test]
    fn scanning_two_rows_at_once_ands_them_together() {
        let mut zx81 = Zx81::new(vec![0; ROM_SIZE], Region::Pal);
        zx81.press_key(Zx81Key::Shift); // row 0 (A8), bit 0
        zx81.press_key(Zx81Key::A); // row 1 (A9), bit 0

        // Select rows 0 and 1 together, as the ROM does to detect
        // SHIFT-together-with-another-key.
        assert_eq!(scan_row(&mut zx81, 0b1111_1100), 0b1111_1110);
    }
}
