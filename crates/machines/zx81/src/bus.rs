use machine::{Bus, MemoryBus};
use sinclair_keyboard::KeyboardMatrix;

use crate::keyboard::Zx81Key;

/// Wires the ZX81's memory map and its keyboard matrix into one `Bus`:
/// memory reads/writes delegate straight to `MemoryBus`; IO reads decode the
/// keyboard when the low byte's bit 0 is clear (matching real hardware,
/// which only decodes that one bit) via a shared `sinclair_keyboard::
/// KeyboardMatrix` -- see `keyboard::Zx81Key` for ZX81's own row/bit table.
/// Any other port reads back `0xFF`, matching `MemoryBus`'s own
/// unmapped-read convention. `write_io` decodes bit 1 of the port address
/// for the NMI generator's start/stop control -- verified against
/// `roms/zx81/zx81.rom`'s own usage (not generic recollection): the reset
/// vector at `0x0000` does `OUT (0xFD),A` (bit 1 clear -- stop) before
/// anything else, and the display routine does `OUT (0xFE),A` (bit 1 set
/// -- start); `crate::Zx81`'s `step` polls `nmi_generator_running` to time
/// real NMIs off it (see its doc comment for why the rate lives there, not
/// here).
#[derive(Clone)]
pub(crate) struct Zx81Bus {
    mem: MemoryBus,
    /// Debugger port breakpoints (see `machine::PortWatcher`); not saved in states.
    pub(crate) port_watch: machine::PortWatcher,
    keyboard: KeyboardMatrix,
    nmi_generator_running: bool,
}

impl Zx81Bus {
    pub(crate) fn new(mem: MemoryBus) -> Self {
        Self {
            mem,
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

    pub(crate) fn peek(&self, addr: u16) -> u8 {
        self.mem.peek(addr)
    }

    pub(crate) fn load(&mut self, addr: u16, bytes: &[u8]) -> usize {
        self.mem.load(addr, bytes)
    }

    /// This machine's whole 1K RAM (`0x4000`-`0x43ff`), for save states --
    /// same peek-loop technique as `zx_spectrum::bus::ZxSpectrumBus::
    /// flat_ram`. No rampack concept here (see `crate::RAM_SIZE`), so unlike
    /// that method this never needs to return `None`.
    pub(crate) fn flat_ram(&self) -> Vec<u8> {
        (crate::RAM_BASE..=crate::RAM_BASE + crate::RAM_SIZE as u16 - 1)
            .map(|a| self.peek(a))
            .collect()
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
        self.mem.read(addr)
    }

    fn write(&mut self, addr: u16, val: u8) {
        self.mem.write(addr, val);
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
        Zx81Bus::new(MemoryBus::new())
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
