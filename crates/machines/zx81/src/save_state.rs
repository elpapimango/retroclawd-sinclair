//! Save-state format for `Zx81` -- serialized via `bincode` (see
//! `Machine::save_state`/`load_state`). Deliberately excludes ROM contents,
//! same reasoning as `zx_spectrum::save_state`'s own doc comment -- ROM
//! stays externally supplied via `--rom`. Much smaller than `zx_spectrum`'s
//! equivalent: no tape/audio/disk modeled for ZX81 at all yet (see
//! `TODO.md`).

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SaveState {
    pub(crate) cpu: z80::Cpu,
    pub(crate) ram: Vec<u8>,
    pub(crate) keyboard: sinclair_keyboard::KeyboardMatrix,
    pub(crate) nmi_generator_running: bool,
    pub(crate) nmi_cycles: u32,
    pub(crate) border: bool,
}
