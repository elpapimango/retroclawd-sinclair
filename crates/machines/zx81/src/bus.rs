use machine::Bus;
use sinclair_keyboard::KeyboardMatrix;

use crate::keyboard::Zx81Key;

/// Wires the ZX81's memory map and its keyboard matrix into one `Bus`:
/// memory reads/writes decode the address (see the memory map below); IO reads decode the
/// keyboard when the low byte's bit 0 is clear (matching real hardware,
/// which only decodes that one bit) via a shared `sinclair_keyboard::
/// KeyboardMatrix` -- see `keyboard::Zx81Key` for ZX81's own row/bit table.
/// Any other port reads back `0xFF`. `write_io` decodes bit 1 of the port address
/// for the NMI generator's start/stop control -- verified against
/// `roms/zx81/zx81.rom`'s own usage (not generic recollection): the reset
/// vector at `0x0000` does `OUT (0xFD),A` (bit 1 clear -- stop) before
/// anything else, and the display routine does `OUT (0xFE),A` (bit 1 set
/// -- start); `crate::Zx81`'s `step` polls `nmi_generator_running` to time
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
}

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

    fn read_io(&mut self, port: u16) -> u8 {
        if port & 0x0001 == 0 {
            self.keyboard.read(port)
        } else {
            0xFF
        }
    }

    fn write_io(&mut self, port: u16, _val: u8) {
        self.nmi_generator_running = port & 0x0002 != 0;
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
    fn the_rom_ignores_writes() {
        let mut bus = bus_with_ram(1);
        bus.write(0x0123, 0xFF);
        bus.write(0x2123, 0xFF);
        assert_eq!(bus.peek(0x0123), 0x23);
        assert_eq!(
            bus.load(0x0000, &[1, 2, 3]),
            0,
            "load drops bytes aimed at ROM"
        );
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

    #[test]
    fn generator_starts_stopped() {
        assert!(!test_bus().nmi_generator_running());
    }

    #[test]
    fn out_0xfe_starts_the_generator() {
        let mut bus = test_bus();
        bus.write_io(0x00FE, 0x00); // real ROM's exact OUT (0xFE),A

        assert!(bus.nmi_generator_running());
    }

    #[test]
    fn out_0xfd_stops_the_generator() {
        let mut bus = test_bus();
        bus.write_io(0x00FE, 0x00); // start it first
        bus.write_io(0x00FD, 0x00); // real ROM's exact OUT (0xFD),A

        assert!(!bus.nmi_generator_running());
    }

    #[test]
    fn only_bit_1_of_the_port_address_matters() {
        let mut bus = test_bus();
        // Any address with bit 1 set starts it, not just the literal 0xFE
        // the ROM happens to use -- real hardware decodes the address bit,
        // not a specific byte value.
        bus.write_io(0xFF02, 0x00);

        assert!(bus.nmi_generator_running());
    }
}
