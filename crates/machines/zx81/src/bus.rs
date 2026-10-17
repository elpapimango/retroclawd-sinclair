use machine::Bus;
use sinclair_keyboard::KeyboardMatrix;

use crate::keyboard::Zx81Key;

/// Wires the ZX81's memory map and its keyboard matrix into one `Bus`:
/// memory reads/writes decode the address (see the memory map below); IO reads decode the
/// keyboard when the low byte's bit 0 is clear (matching real hardware,
/// which only decodes that one bit) via a shared `sinclair_keyboard::
/// KeyboardMatrix` -- see `keyboard::Zx81Key` for ZX81's own row/bit table.
/// Any other port reads back `0xFF`. `write_io` decodes the address for the NMI generator's start/stop control
/// -- verified against `roms/zx81/zx81.rom`'s own usage (not generic recollection): the reset
/// vector at `0x0000` does `OUT (0xFD),A` (bit 1 clear -- stop) before
/// anything else, and the display routine does `OUT (0xFE),A` (bit 1 set, bit 0 clear
/// -- start); `OUT (0xFF),A`, which `SAVE` does for every pulse, leaves it alone;
/// `crate::Zx81`'s `step` polls `nmi_generator_running` to time
/// real NMIs off it (see its doc comment for why the rate lives there, not
/// here).
///
/// **Memory map** (Martin Korth's "Sinclair ZX Specifications",
/// <https://problemkaputt.de/zxdocs.htm>, "memory mirroring"): the 8K ROM
/// answers at `0x0000-0x3FFF` and again at `0x8000-0xBFFF` (its select line
/// ignores A13 and A15); the RAM sits at `0x4000` and, when it is smaller
/// than 16K, repeats through `0x4000-0x7FFF` (a 1K machine has sixteen
/// copies of the same 1K); a data access at `0xC000-0xFFFF` ignores A15 and
/// sees `0x4000-0x7FFF`. RAM packs of 16K, 32K and 48K continue upward from
/// `0x4000`; a 56K pack replaces the shadow ROM and starts at `0x2000`.
/// Writes to a ROM cell are dropped.
#[derive(Clone)]
pub(crate) struct Zx81Bus {
    rom: Vec<u8>,
    ram: Vec<u8>,
    ram_start: u16,
    /// Debugger port breakpoints (see `machine::PortWatcher`); not saved in states.
    pub(crate) port_watch: machine::PortWatcher,
    keyboard: KeyboardMatrix,
    nmi_generator_running: bool,
    /// The tape being played (`LOAD`), if any: its level is bit 7 of a port `0xFE` read.
    tape: Option<zx_tape::TapePlayer>,
    /// The MIC output latch, and the changes of it since a `SAVE` began (`(cycle, level)`), when armed.
    mic: bool,
    mic_edges: Vec<(u64, bool)>,
    recording: bool,
    /// Bit 6 of a port `0xFE` read: 1 on a 50 Hz machine, 0 on a 60 Hz one. The ROM's keyboard routine (`0x02BB`) turns it
    /// into the top margin of the picture: 55 lines when set, 31 when clear.
    refresh_50hz: bool,
    /// Total CPU cycles at the start of the instruction being executed (set by `Zx81::step`), the time stamp of an edge.
    now: u64,
}

/// Recording stops growing here (about 36 seconds of the densest signal): a `SAVE` that never ends cannot eat memory.
const MAX_MIC_EDGES: usize = 4_000_000;

/// One byte of the address space: ROM or RAM, by index into that chip.
enum Cell {
    Rom(usize),
    Ram(usize),
}

impl Zx81Bus {
    /// `rom` is the 8K image; `ram_kb` the installed RAM (1, 2, 16, 32, 48 or 56).
    pub(crate) fn new(rom: Vec<u8>, ram_kb: usize) -> Self {
        Self {
            rom,
            ram: vec![0; ram_kb * 1024],
            ram_start: if ram_kb == 56 { 0x2000 } else { 0x4000 },
            port_watch: machine::PortWatcher::default(),
            keyboard: KeyboardMatrix::new(),
            nmi_generator_running: false,
            tape: None,
            mic: false,
            mic_edges: Vec::new(),
            recording: false,
            refresh_50hz: true,
            now: 0,
        }
    }

    pub(crate) fn set_refresh_50hz(&mut self, is_50hz: bool) {
        self.refresh_50hz = is_50hz;
    }

    pub(crate) fn tape_position(&self) -> Option<zx_tape::TapePosition> {
        self.tape.as_ref().map(zx_tape::TapePlayer::dump_state)
    }

    /// Puts an attached tape back where a save state had it (nothing happens without a tape).
    pub(crate) fn restore_tape_position(&mut self, position: zx_tape::TapePosition) {
        if let Some(tape) = &mut self.tape {
            tape.restore_state(position);
        }
    }

    pub(crate) fn mic(&self) -> bool {
        self.mic
    }

    pub(crate) fn restore_mic(&mut self, level: bool) {
        self.mic = level;
    }

    pub(crate) fn set_now(&mut self, cycles: u64) {
        self.now = cycles;
    }

    /// Installs a tape for `LOAD`; it sits stopped until `start_tape`.
    pub(crate) fn load_tape(&mut self, tape: zx_tape::Tape) {
        self.tape = Some(zx_tape::TapePlayer::new(tape));
    }

    /// Presses Play (a no-op without a tape, or once it plays or has finished).
    pub(crate) fn start_tape(&mut self) {
        if let Some(tape) = &mut self.tape {
            tape.play();
        }
    }

    pub(crate) fn eject_tape(&mut self) {
        self.tape = None;
    }

    pub(crate) fn tick_tape(&mut self, cycles: u32) {
        if let Some(tape) = &mut self.tape {
            tape.tick(cycles);
        }
    }

    /// Starts a fresh recording of the MIC output (the machine calls this as the ROM enters `SAVE`).
    pub(crate) fn begin_recording(&mut self) {
        self.recording = true;
        self.mic_edges.clear();
    }

    pub(crate) fn mic_edges(&self) -> &[(u64, bool)] {
        &self.mic_edges
    }

    fn set_mic(&mut self, level: bool) {
        if level != self.mic {
            self.mic = level;
            if self.recording && self.mic_edges.len() < MAX_MIC_EDGES {
                self.mic_edges.push((self.now, level));
            }
        }
    }

    pub(crate) fn nmi_generator_running(&self) -> bool {
        self.nmi_generator_running
    }

    /// Forces the NMI generator's state directly, bypassing the port write
    /// that normally controls it. Used when loading a `.p`/`.p81` snapshot:
    /// real ZX81 hardware is always mid-`SLOW` (NMI generator running) by
    /// the time a program has loaded and is sitting in `BASIC`, since
    /// nothing in that state would have turned it off.
    pub(crate) fn set_nmi_generator_running(&mut self, running: bool) {
        self.nmi_generator_running = running;
    }

    pub(crate) fn press_key(&mut self, key: Zx81Key) {
        let (row, bit) = key.row_bit();
        self.keyboard.press(row, bit);
    }

    pub(crate) fn release_key(&mut self, key: Zx81Key) {
        let (row, bit) = key.row_bit();
        self.keyboard.release(row, bit);
    }

    /// What answers at `addr`: a RAM cell (its index) or a ROM cell.
    fn cell(&self, addr: u16) -> Cell {
        let a = usize::from(addr);
        let start = usize::from(self.ram_start);
        let len = self.ram.len();
        if len >= 0x4000 {
            if (start..start + len).contains(&a) {
                return Cell::Ram(a - start);
            }
        } else if (0x4000..0x8000).contains(&a) {
            return Cell::Ram((a - 0x4000) % len);
        }
        if a >= 0xC000 {
            return self.cell((a & 0x7FFF) as u16);
        }
        Cell::Rom(a & 0x1FFF)
    }

    /// A read with no side effects (the debugger, the picture decode).
    pub(crate) fn peek(&self, addr: u16) -> u8 {
        match self.cell(addr) {
            Cell::Ram(i) => self.ram[i],
            Cell::Rom(i) => self.rom.get(i).copied().unwrap_or(0xFF),
        }
    }

    /// Stores `bytes` from `addr` into RAM; bytes aimed at a ROM cell are dropped. Returns how many were stored.
    pub(crate) fn load(&mut self, addr: u16, bytes: &[u8]) -> usize {
        let mut at = addr;
        let mut stored = 0;
        for &b in bytes {
            if let Cell::Ram(i) = self.cell(at) {
                self.ram[i] = b;
                stored += 1;
            }
            at = at.wrapping_add(1);
        }
        stored
    }

    /// The installed RAM, for save states.
    pub(crate) fn flat_ram(&self) -> Vec<u8> {
        self.ram.clone()
    }

    /// The address of the first RAM byte (`0x4000`, or `0x2000` with a 56K pack).
    pub(crate) fn ram_start(&self) -> u16 {
        self.ram_start
    }

    /// The address of the last RAM byte as the ROM's `RAMTOP` sees it.
    pub(crate) fn ram_end(&self) -> u16 {
        (usize::from(self.ram_start) + self.ram.len() - 1) as u16
    }

    pub(crate) fn keyboard(&self) -> KeyboardMatrix {
        self.keyboard
    }

    pub(crate) fn set_keyboard(&mut self, keyboard: KeyboardMatrix) {
        self.keyboard = keyboard;
    }
}

impl Bus for Zx81Bus {
    fn io_accessed(&mut self, port: u16, value: u8, write: bool) {
        self.port_watch.note(port, value, write);
    }

    fn read(&mut self, addr: u16) -> u8 {
        self.peek(addr)
    }

    fn write(&mut self, addr: u16, val: u8) {
        if let Cell::Ram(i) = self.cell(addr) {
            self.ram[i] = val;
        }
    }

    /// The video circuit watches opcode fetches: when A15 is high during an M1
    /// cycle and bit 6 of the byte is clear, it takes the byte as character
    /// data, forces zero onto the data bus (the CPU executes a NOP) and
    /// shows the character; a byte with bit 6 set is executed as it is,
    /// which in a display file is the `HALT` that ends each line ("ZX80/ZX81
    /// Video Mode Text and Blockgraphics", <https://problemkaputt.de/zxdocs.htm>).
    /// This is how the ROM runs the display file as code to draw the picture.
    fn fetch_opcode(&mut self, addr: u16) -> u8 {
        let byte = self.peek(addr);
        if addr >= 0x8000 && byte & 0x40 == 0 {
            0x00
        } else {
            byte
        }
    }

    /// Port `0xFE` and its mirrors (A0 low): the keyboard rows in bits 0-4, the refresh rate in bit 6 (1 = 50 Hz), and
    /// the tape input in bit 7 (0 = normal, 1 = pulse; ZXDOCS "I/O Ports"). The read also starts the vertical retrace, which drives the cassette
    /// output low -- but "in the ZX81, all of the above happens only if NMIs are disabled".
    fn read_io(&mut self, port: u16) -> u8 {
        if port & 0x0001 != 0 {
            return 0xFF;
        }
        if !self.nmi_generator_running {
            self.set_mic(false);
        }
        // The player idles high when stopped or finished (the Spectrum's convention); the ZX81's input idles at 0.
        let ear = self.tape.as_ref().is_some_and(|tape| {
            let state = tape.playback_state();
            state.playing && !state.finished && tape.ear_bit()
        });
        (self.keyboard.read(port) & 0x3F)
            | (u8::from(self.refresh_50hz) << 6)
            | (u8::from(ear) << 7)
    }

    /// Any port write ends the vertical retrace and so drives the cassette output high -- with the NMI
    /// generator off ("ZX80/ZX81 I/O Ports"). The generator itself is controlled by port `0xFD` (off) and `0xFE`
    /// (on); a write to `0xFF`, which `SAVE` makes for every pulse, leaves it alone.
    fn write_io(&mut self, port: u16, _val: u8) {
        if !self.nmi_generator_running {
            self.set_mic(true);
        }
        if port & 0x0002 == 0 {
            self.nmi_generator_running = false;
        } else if port & 0x0001 == 0 {
            self.nmi_generator_running = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bus() -> Zx81Bus {
        Zx81Bus::new(vec![0; crate::ROM_SIZE], 1)
    }

    /// A bus whose ROM byte at offset `n` is `n as u8`, so a mirror is easy to spot.
    fn bus_with_ram(ram_kb: usize) -> Zx81Bus {
        let rom: Vec<u8> = (0..crate::ROM_SIZE).map(|n| n as u8).collect();
        Zx81Bus::new(rom, ram_kb)
    }

    // Memory map facts: "Sinclair ZX Specifications", <https://problemkaputt.de/zxdocs.htm>, "memory mirroring".

    #[test]
    fn the_rom_answers_across_0x0000_0x3fff_and_at_0x8000_0xbfff() {
        let bus = bus_with_ram(1);
        for base in [0x0000u16, 0x2000, 0x8000, 0xA000] {
            assert_eq!(bus.peek(base + 0x123), 0x23, "ROM mirror at {base:#06x}");
        }
    }

    #[test]
    fn a_1k_ram_repeats_through_0x4000_0x7fff() {
        let mut bus = bus_with_ram(1);
        bus.write(0x4300, 0x5A);
        for addr in [0x4700u16, 0x5B00, 0x7F00] {
            assert_eq!(bus.peek(addr), 0x5A, "mirror at {addr:#06x}");
        }
        assert_eq!(bus.ram_end(), 0x43FF);
    }

    #[test]
    fn data_at_0xc000_and_above_is_the_ram_at_0x4000() {
        let mut bus = bus_with_ram(1);
        bus.write(0x4200, 0x6B);
        assert_eq!(bus.peek(0xC200), 0x6B);
        bus.write(0xC201, 0x6C);
        assert_eq!(
            bus.peek(0x4201),
            0x6C,
            "a write there lands in the same RAM"
        );
    }

    #[test]
    fn ram_packs_extend_the_ram_without_mirroring_inside_it() {
        let mut bus = bus_with_ram(16);
        bus.write(0x4400, 0x11);
        assert_eq!(bus.peek(0x4000), 0x00, "no 1K repeat with 16K installed");
        assert_eq!(bus.peek(0x4400), 0x11);
        assert_eq!(bus.ram_end(), 0x7FFF);
        let mut bus = bus_with_ram(48);
        bus.write(0xFFFF, 0x22);
        assert_eq!(bus.peek(0xFFFF), 0x22);
        assert_eq!(bus.ram_end(), 0xFFFF);
        let bus = bus_with_ram(56);
        assert_eq!(
            (bus.ram_start(), bus.ram_end()),
            (0x2000, 0xFFFF),
            "a 56K pack replaces the shadow ROM"
        );
    }

    #[test]
    fn an_opcode_fetch_above_0x8000_is_a_nop_when_bit_6_is_clear() {
        let mut bus = bus_with_ram(1);
        bus.load(0x4300, &[0x3C, 0x76, 0x80, 0x40]);
        // At the echo address: 0x3C and 0x80 have bit 6 clear (NOP); 0x76 and 0x40 have it set (run as they are).
        assert_eq!(bus.fetch_opcode(0xC300), 0x00);
        assert_eq!(bus.fetch_opcode(0xC301), 0x76);
        assert_eq!(bus.fetch_opcode(0xC302), 0x00);
        assert_eq!(bus.fetch_opcode(0xC303), 0x40);
        // Below 0x8000 a fetch is an ordinary read, and a data read above is too.
        assert_eq!(bus.fetch_opcode(0x4300), 0x3C);
        assert_eq!(bus.read(0xC300), 0x3C);
    }

    fn generator_starts_stopped() {
        assert!(!test_bus().nmi_generator_running());
    }

    fn out_0xfe_starts_the_generator() {
        let mut bus = test_bus();
        bus.write_io(0x00FE, 0x00); // real ROM's exact OUT (0xFE),A

        assert!(bus.nmi_generator_running());
    }

    fn out_0xfd_stops_the_generator() {
        let mut bus = test_bus();
        bus.write_io(0x00FE, 0x00); // start it first
        bus.write_io(0x00FD, 0x00); // real ROM's exact OUT (0xFD),A

        assert!(!bus.nmi_generator_running());
    }

    fn only_bit_1_of_the_port_address_matters() {
        let mut bus = test_bus();
        // Any address with bit 1 set starts it, not just the literal 0xFE
        // the ROM happens to use -- real hardware decodes the address bit,
        // not a specific byte value.
        bus.write_io(0xFF02, 0x00);

        assert!(bus.nmi_generator_running());
    }

    fn out_0xff_does_not_start_the_generator() {
        // SAVE writes port 0xFF for every pulse with the NMI generator off; it must stay off.
        let mut bus = test_bus();
        bus.write_io(0x00FF, 0x00);

        assert!(!bus.nmi_generator_running());
    }
    /// Merged (every check kept): `generator_starts_stopped`, `out_0xfe_starts_the_generator`, `out_0xfd_stops_the_generator`, `only_bit_1_of_the_port_address_matters`, `out_0xff_does_not_start_the_generator`.
    #[test]
    fn nmi_generator_ports() {
        generator_starts_stopped();
        out_0xfe_starts_the_generator();
        out_0xfd_stops_the_generator();
        only_bit_1_of_the_port_address_matters();
        out_0xff_does_not_start_the_generator();
    }

    #[test]
    fn the_tape_input_is_bit_7_of_a_port_0xfe_read() {
        let mut bus = test_bus();
        // No tape: 0 = normal (ZXDOCS), the idle keyboard bits stay high.
        assert_eq!(bus.read_io(0xFEFE), 0x7F);
        // A tape begins in silence (low) and then pulses high.
        let pulse = |level, cycles| zx_tape::Pulse { cycles, level };
        bus.load_tape(zx_tape::Tape::from_pulses([
            pulse(false, 100),
            pulse(true, 1000),
        ]));
        assert_eq!(bus.read_io(0xFEFE) & 0x80, 0, "stopped: normal");
        bus.start_tape();
        assert_eq!(bus.read_io(0xFEFE) & 0x80, 0, "playing, in the silence");
        bus.tick_tape(150);
        assert_eq!(bus.read_io(0xFEFE) & 0x80, 0x80, "a pulse reads 1");
    }

    #[test]
    fn the_mic_output_follows_port_writes_and_reads_while_the_nmi_generator_is_off() {
        let mut bus = test_bus();
        bus.begin_recording();
        bus.set_now(10);
        bus.write_io(0x00FF, 0); // any write: MIC high
        bus.set_now(20);
        bus.read_io(0x7FFE); // a port 0xFE read: MIC low
        bus.set_now(30);
        bus.write_io(0x00FE, 0); // starts the NMI generator (and, with it off before, MIC high)
        bus.set_now(40);
        bus.read_io(0x7FFE); // generator on now: no change
        assert_eq!(bus.mic_edges(), &[(10, true), (20, false), (30, true)]);
    }

    #[test]
    fn bit_6_of_a_port_0xfe_read_is_the_refresh_rate() {
        let mut bus = test_bus();
        assert_eq!(bus.read_io(0xFEFE), 0x7F, "50 Hz: bit 6 set");
        bus.set_refresh_50hz(false);
        assert_eq!(bus.read_io(0xFEFE), 0x3F, "60 Hz: bit 6 clear");
    }
}
