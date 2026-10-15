//! Save-state format for `ZxSpectrum` -- serialized via `bincode` (see
//! `Machine::save_state`/`load_state`, which do the actual byte encoding).
//! Deliberately excludes ROM contents, the loaded disk image's own bytes,
//! and the loaded tape's own pulse/byte data -- all three stay externally
//! supplied (`--rom`/`--disk`/`--tape`), exactly like `.z80`/`.sna`
//! snapshots already never embed them either. Restoring a save state
//! requires the same `--rom` (and, if applicable, `--tape`/`--disk`) to
//! already be given -- see `zx_tape::TapePosition`/`nec765::FdcState`'s own
//! doc comments for what happens if a *different* tape/disk ends up
//! attached instead.

use crate::Model;
use crate::bus::ZxSpectrumBus;

/// 128K/+2A/+3's memory, mirroring `bus::Memory`'s own shape but as plain,
/// serializable data -- ROM banks excluded (external, see this module's
/// own doc comment). `paging`/`paging_2a3` reuse the exact tuple shapes
/// `ZxSpectrumBus::paging_state`/`paging_2a3_state` already return.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Memory {
    Flat(Vec<u8>),
    Banked128 {
        ram: [Vec<u8>; 8],
        paging: (u8, u8, bool, bool),
    },
    Banked2A3 {
        ram: [Vec<u8>; 8],
        paging: (u8, bool, bool, bool, bool, u8, bool),
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SaveState {
    pub(crate) model: Model,
    pub(crate) cpu: z80::Cpu,
    pub(crate) memory: Memory,
    pub(crate) keyboard: sinclair_keyboard::KeyboardMatrix,
    pub(crate) border_color: u8,
    pub(crate) speaker_level: bool,
    pub(crate) kempston: u8,
    pub(crate) fuller: u8,
    pub(crate) mouse: (u8, u8, u8, u8),
    pub(crate) frame_tstate: u32,
    pub(crate) ay: Option<ay_3_8910::Ay3_8910>,
    pub(crate) ay2: Option<ay_3_8910::Ay3_8910>,
    pub(crate) active_ay_chip: bool,
    pub(crate) tape_position: Option<zx_tape::TapePosition>,
    pub(crate) fdc_state: Option<nec765::FdcState>,
    /// The disk serialised as a `.dsk` file, present only once the FDC has
    /// written to it -- an unwritten disk is exactly the externally
    /// supplied image, so it stays external like ROM/tape bytes.
    pub(crate) modified_disk: Option<Vec<u8>>,
    pub(crate) flash_counter: u8,
    pub(crate) flash_phase: bool,
    pub(crate) audio_cycle_accumulator: f64,
    pub(crate) ay_cycle_debt: u32,
    pub(crate) fast_tape_load: bool,
    pub(crate) speaker_type: crate::SpeakerType,
    pub(crate) speaker_filter_state: f32,
    pub(crate) joystick_type: crate::JoystickType,
}

/// Dumps `bus`'s own live state into `Memory` -- one branch per `Model`,
/// using only the plain accessors `ZxSpectrumBus` already exposes (never
/// touching its private `Memory` enum directly, so this module has no
/// special access beyond what any other crate-internal caller could use).
pub(crate) fn dump_memory(model: Model, bus: &ZxSpectrumBus) -> Memory {
    match model {
        Model::Spectrum48 => Memory::Flat(bus.flat_ram().expect("Spectrum48 always has flat RAM")),
        Model::Spectrum128 => Memory::Banked128 {
            ram: std::array::from_fn(|bank| bus.ram_bank_data(bank as u8).unwrap().to_vec()),
            paging: bus.paging_state().unwrap(),
        },
        Model::Spectrum2A3 => Memory::Banked2A3 {
            ram: std::array::from_fn(|bank| bus.ram_bank_data(bank as u8).unwrap().to_vec()),
            paging: bus.paging_2a3_state().unwrap(),
        },
    }
}

/// Restores `memory` into `bus` -- the inverse of `dump_memory`. Trusts
/// the caller (`ZxSpectrum::load_state`) to have already checked `memory`'s
/// own shape matches the bus's real model (same "must match, checked
/// before this point" stance as `ZxSpectrum::load_snapshot`) -- applying a
/// `Memory::Flat` onto a banked bus or vice versa here would silently do
/// the wrong thing (`load_ram_bank` is a no-op on flat memory, `load` on
/// banked memory only reaches whatever's currently paged in), not error.
pub(crate) fn restore_memory(bus: &mut ZxSpectrumBus, memory: Memory) {
    match memory {
        Memory::Flat(ram) => {
            bus.load(crate::RAM_BASE, &ram);
        }
        Memory::Banked128 {
            ram,
            paging: (rom_bank, ram_bank, shadow_screen, locked),
        } => {
            for (bank, data) in ram.into_iter().enumerate() {
                bus.load_ram_bank(bank as u8, &data);
            }
            bus.set_paging(rom_bank, ram_bank, shadow_screen, locked);
        }
        Memory::Banked2A3 {
            ram,
            paging:
                (ram_bank, shadow_screen, rom_bit_lo, locked, special_mode, bits_1_2, disk_motor),
        } => {
            for (bank, data) in ram.into_iter().enumerate() {
                bus.load_ram_bank(bank as u8, &data);
            }
            bus.set_paging_2a3(
                ram_bank,
                shadow_screen,
                rom_bit_lo,
                locked,
                special_mode,
                bits_1_2,
                disk_motor,
            );
        }
    }
}
