# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repo.

## What this repository is

The Sinclair (ZX81, ZX Spectrum) machine family of retroclawd, split out of the main repository (`elpapimango/retroclawd`, the application, referred to as "the host"). Everything here builds into `*_machine_plugin` shared libraries that the host application loads at run time. Shared crates (`machine`, `plugin-api`, `z80`, `mos6502`, `ay-3-8910`, `nec765`, `dsk`, `zx-tape`) come from `elpapimango/retroclawd-crates` as a git dependency pinned to a commit (`rev`); changing that rev means checking `docs/plugin-abi.md` in that repository. Directory layout is the same as in the host (tests find ROMs/software at `CARGO_MANIFEST_DIR/../../../roms|software`).

## Commands

- `cargo build --release --workspace` — builds the plugin libraries into `target/release/`
- `cargo test --workspace` — all tests (ROM-gated tests skip when a ROM is absent; `-- --ignored` runs the slow real-firmware ones, use `--release`)
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`
- Private host: `CARGO_NET_GIT_FETCH_WITH_CLI=true` and git credentials for `github.com/elpapimango/retroclawd`

## Crate notes

The notes below were written in the host repository before the split and refer to it in places (`retroclawd`'s GUI, `HostedMachine`, `emu_thread`, the CLI flags): those parts are host code and are documented in the host's CLAUDE.md.

- **`sinclair-keyboard`** — `KeyboardMatrix`: 8 bytes row state, `press`/
  `release`/`read` addressed by plain `(row, bit)` pairs; `read` implements
  real IN-port decode both Sinclair machines share -- row selected when its
  bit in high byte of port address is `0`, pressed key reads `0`, selecting
  several rows at once ANDs them together (how ROM detects e.g. shift key
  held w/ another key). Extracted from ZX81's original keyboard code once ZX
  Spectrum needed identical mechanism -- crate has no key-layout knowledge;
  each machine keeps own key enum + (row, bit) table.
- **`zx81` / `zx-spectrum`** — thin: `Cpu`, `Region` parameter (determines
  `clock_hz()` + `frame_hz()` — passed at construction, one per machine
  variant), memory map. ROM images supplied via `--rom`; repo private,
  commits own ROM copies under `roms/` (see `roms/README.md`) for dev
  convenience — keep repo private long as they're in history. Both wrap
  `machine::MemoryBus` in own `Zx81Bus`/`ZxSpectrumBus` (private to each
  crate) that also owns `sinclair_keyboard::KeyboardMatrix`;
  `Zx81Key`/`ZxSpectrumKey` (exported) name all 40 keys, map to
  `(row, bit)` -- two tables differ in exactly two keys despite
  otherwise-identical layout (ZX81's single `Shift`/`.` vs Spectrum's `CapsShift`/`SymShift`).
  `press_key`/`release_key` drive them -- no interactive input wired up yet
  (no GUI exists to drive it from), but both matrices + port decoding real,
  tested end-to-end through Z80 core.
  `render_frame` differs completely between two, because hardware does:
  `zx81` decodes `D_FILE` display-file pointer (system var at `0x400C`)
  into character rows, looks up each character's real 8x8 pixel bitmap
  directly from already-loaded ROM at `0x1E00 + code*8 + scanline_row` --
  actual ZX81 hardware mechanism (verified vs real ROM disassembly: ROM's
  `INITIAL` routine sets `I = 0x1E` at boot), not approximation. Codes
  64-127/192-255 (BASIC keyword tokens) have no real bitmap, render blank;
  128-191 = 0-63 w/ ink/paper swapped (inverse video). `zx-spectrum` has no
  character-code concept at all -- decodes raw pixel bitmap
  (`0x4000`-`0x57FF`, addressed by well-known "interleaved" formula
  `0x4000 | (Y7Y6 << 11) | (Y2Y1Y0 << 8) | (Y5Y4Y3 << 5) | col`, verified vs
  two independent hardware references) + separate attribute byte per 8x8
  cell (`0x5800`-`0x5AFF`: bit 7 flash, bit 6 bright, bits 5-3 paper, bits
  2-0 ink). FLASH swaps ink/paper while `ZxSpectrum::flash_phase` is on,
  toggled every `video::FLASH_TOGGLE_FRAMES` (16) real interrupts in
  `request_interrupt` (piggybacking the already-real once-per-frame `INT`,
  no separate timing mechanism needed -- unlike `zx81`'s NMI below, the
  real rate genuinely is frame-based). Both are still static, on-demand
  decodes of current memory state, not live/continuous rendering -- real
  ZX81 video generation is NMI-driven and coupled to CPU execution during
  retrace, and `render_frame` doesn't do pixel-level generation for either
  machine (see the crate-level `Zx81`/`ZxSpectrum` NMI/`INT` notes below
  for what *is* modeled: real interrupt timing, not real pixel output).
  `render_frame` pads the active picture with a `BORDER_PX`-wide (32px, a
  common emulator convention, not a broadcast-accurate PAL/NTSC
  measurement -- see `zx_spectrum::video::BORDER_PX`'s doc comment) border
  on every side by default, toggled via each struct's `set_border`
  (`retroclawd --no-border`/`--nb`; not a `new()` param since almost every
  caller wants the default -- see the `border` field's doc comment on
  either struct). `zx81`'s border is fixed white (no border-color
  register on real hardware); `zx-spectrum`'s tracks the ULA's real border
  color, the low 3 bits of the last value `OUT` to port `0xFE`'s mirrors
  (`ZxSpectrumBus::border_color`, decoded the same `port & 1 == 0` way as
  the keyboard read on that same port range). The actual pixel-compositing
  (`machine::with_border`) is shared -- CPU/bus-agnostic, lives in
  `machine` since `Machine::render_frame`'s RGB-tuple contract is already
  defined there.
  ZX81 keys: all 40 are named (`Period` is the `.` key; the arrow keys are Shift+5..8) and the symbols on the shifted keys are
  combo keys (`Plus` = Shift+K, `Asterisk` = Shift+B, `DoubleQuote` = Shift+P, `Colon`, `Semicolon`, `Question`, `Slash`,
  `LeftParen`, `RightParen`, `Dollar`, `Equals`, `Minus`, `Comma`, `GreaterThan`, `LessThan`, `Pound`), each typing its ASCII
  character, so text can be typed and pasted; the pairs come from the ROM's shifted key table (0x00A5). The ROM scans the keyboard
  once a frame and debounces it: typing faster than about 160 ms per key event loses keys (`--type-delay 160`, or
  `paste_delay_ms` in `[machines.zx81.input]`).
  Interrupts: `Cpu` supports both a maskable interrupt (`request_interrupt`,
  IM0/1/2-vectored, gated on `iff1`) and a real NMI (`request_nmi`, always
  accepted, fixed vector `0x0066`, clears only `iff1` not `iff2`, priority
  over a pending maskable interrupt). `zx-spectrum` fires the maskable one
  once per real video frame from `Machine::request_interrupt` (called by
  `machine::Clock::run`/retroclawd's `emu_thread` background thread, once
  per its own tick) -- matches the real ULA's `INT` line exactly. `zx81`
  has no once-per-frame interrupt, so doesn't override `request_interrupt` (stays the
  trait's no-op default). Its maskable interrupt comes from the refresh counter: `Zx81::step`
  asserts it while bit 6 of `R` is zero (the ROM's line counting at `0x0038` depends on it), and
  `Zx81Bus::fetch_opcode` makes an opcode fetch at `0x8000`+ a NOP when the byte's bit 6 is clear, which is how
  the ROM draws a picture by running the display file as code (hardware facts and sources are
  in the `rc-suite` ZX81 cases of `retroclawd-tools`). `Zx81::step` also fires real NMIs itself, at
  208 T-states (64us, the real PAL TV line rate) while
  `Zx81Bus::nmi_generator_running` is set -- decoded from bit 1 of the OUT
  port address, verified against `roms/zx81/zx81.rom`'s own `OUT (0xFD)`/
  `OUT (0xFE)` usage, not recollection. This lives inside `Zx81::step`
  rather than going through `Machine::request_interrupt`'s once-per-frame
  shape because 208 T-states is far finer-grained than once a frame. What
  the ROM's NMI handler does internally isn't modeled or needed -- once
  the interrupt timing is correct, the real ROM's own code handles the
  rest through this core's already-ZEXALL-accurate dispatch, same
  principle as `zx-spectrum` never needing its ROM's keyboard-scan
  internals understood.
  128K: `zx_spectrum::Model` (`Spectrum48`/`Spectrum128`/`Spectrum2A3`,
  `ZxSpectrum::new`'s 3rd param) — `Spectrum128` is also what `Spectrum +2`
  uses (memory-identical: same 8x16K RAM banks, same 2 ROMs, same
  `0x7ffd`; the +2 name is about the case/keyboard/tape deck, not modeled
  here). `bus::Memory` is `Flat(MemoryBus)` for 48K (untouched, existing
  path), `Banked128(Box<Banked128>)` for 128K (own raw `[Vec<u8>;2]` ROM
  banks + `[Vec<u8>;8]` RAM banks + paging state), or `Banked2A3
  (Box<Banked2A3>)` for +2A/+3 (`[Vec<u8>;4]` ROM banks, same 8 RAM banks,
  its own `Paging2A3` combining both paging registers) — entirely inside
  `zx-spectrum`'s `bus.rs` rather than extending `machine::MemoryBus` with
  banking support every other machine would then carry too. 128K:
  `0x0000`-`0x3fff` = paged ROM bank, `0x4000`-`0x7fff` = RAM bank 5
  (fixed), `0x8000`-`0xbfff` = RAM bank 2 (fixed), `0xC000`-`0xffff` =
  paged RAM bank -- port `0x7ffd` (real decode `port & 0x8002 == 0`,
  commonly documented, not disassembly-verified here) sets which RAM
  bank/ROM bank/shadow-screen/paging-lock; once locked, further writes are
  ignored until reset, matching real hardware. `ZxSpectrumBus::
  peek_screen` (used by `video::draw_cell` instead of `peek`) reads RAM
  bank 5 or 7 directly regardless of what's paged at `0xC000` -- the ULA's
  own video source, independent of the CPU's address space; identical to
  `peek` on 48K.
  +2A/+3 (`Model::Spectrum2A3`, `zx_spectrum::ROM_SIZE_2A3` = 64K, 4x16K
  banks): a distinct `Memory`/`Paging2A3` shape, not a generalized "N ROM
  banks" version of `Banked128`, so the well-tested 128K path stays
  completely untouched. Bit meanings verified against the official ZX
  Spectrum +3 technical manual (see `CREDITS.md`) -- a stronger epistemic
  status than 128K's `0x7ffd` decode above, which is only
  commonly-documented. `0x7ffd` behaves as on 128K except bit 4 is now
  only the ROM select's *low* bit. `0x1ffd` (port decode `port & 0xF002 ==
  0x1000`, derived by hand from a hardware test-suite reference's bit
  pattern for this port, cross-checked against the literal port value --
  see `CREDITS.md`): bit 0 selects normal mode (`false`) vs. special
  all-RAM mode (`true`); in normal mode, bit 2 is the ROM select's *high*
  bit (combines with `0x7ffd` bit 4 to choose 1 of 4 ROMs) and `0xC000`
  pages a RAM bank exactly like 128K; in special mode, bits 2:1 select one
  of 4 fixed whole-64K RAM layouts (`bus::ALL_RAM_CONFIGS`, verified
  verbatim against the manual's own table) that replace ROM entirely --
  even `0x0000`-`0x3fff` becomes real, writable RAM while special mode is
  active (`bus::banked2a3_write`'s job, distinct from `Banked128`'s
  always-drop-writes-below-`0x4000` behavior). Bit 3 of `0x1ffd` is the
  floppy disk motor -- stored (`Paging2A3::disk_motor`) since it's part of
  the same register write, but nothing acts on it yet; no FDC is modeled
  (tracked in `TODO.md` as the next +2A/+3 item). `0x1ffd`'s decode is a
  strict subset of `0x7ffd`'s laxer `0x8002` mask (every address matching
  `0x1ffd`'s pattern also matches `0x7ffd`'s), so `write_io` checks
  `0x1ffd`'s pattern first and only falls through to `0x7ffd`'s handling
  otherwise -- real +2A/+3 hardware must disambiguate the two ports
  somehow (that's the whole point of having two), but exactly how isn't
  documented in the sources this was checked against, so "most specific
  pattern wins" is this codebase's own resolution, not a hardware-verified
  mechanism. `.z80` snapshot loading for this model: `z80_snapshot::parse`
  classifies hardware modes 7/8/13 (Spectrum +3, a known XZX-Pro
  mis-encoding of +3, and Spectrum +2A -- all memory-identical) as
  `Model::Spectrum2A3`, decoding byte 35 (last OUT to `0x7ffd`) and byte 86
  (last OUT to `0x1ffd`, only present when the additional header is the
  full 55 bytes -- `Error::Missing2A3PagingByte` otherwise) into a
  `Paging2A3State`, kept as its own `Snapshot` field distinct from the
  128K-shaped `PagingState` (same "own shape, not a generalized one"
  reasoning as `Paging2A3` itself). `ZxSpectrum::load_snapshot` applies it
  via `ZxSpectrumBus::set_paging_2a3` (mirrors `set_paging`, bypasses the
  port-write locked check the same way).
  Audio: `ZxSpectrum::step` (not `request_interrupt` -- this needs a much
  finer per-instruction accumulator than once-a-frame, same reasoning as
  ZX81's NMI) ticks two independent accumulators after every CPU
  instruction and pushes a mixed sample into `audio_buffer` whenever
  `audio_cycle_accumulator` (in real CPU T-states, `f64` so its fractional
  remainder survives across instructions rather than drifting the
  effective sample rate flat) crosses `clock_hz() / machine::
  AUDIO_SAMPLE_RATE`; `ay_cycle_debt` (a `u32` remainder, not a per-step
  division) feeds the AY-3-8910 (128K/+2A/+3 only, `bus::ZxSpectrumBus::
  ay_mut`) cycles at the real CPU-clock/2 divider. `mix_audio_sample` sums the
  beeper's current level (`ZxSpectrumBus::speaker_level`, bit 4 of the
  last write to the same port as the border color -- present on both
  models, real 128K hardware keeps its beeper alongside the AY) with the
  AY's `output()` at hand-picked weights that keep the sum inside
  `-1.0..=1.0` -- not a modeled hardware fact, since real analog mixing
  levels are board-specific. AY ports: `0xfffd` selects a register (write)
  or reads the selected one back (read); `0xbffd` writes to it -- literal
  addresses per World of Spectrum's 128K reference (see `CREDITS.md`), the
  actual bit-mask decode (`port & 0xC002`) is the commonly-documented
  extension of those two addresses, same epistemic status as the `0x7ffd`
  paging decode above. `Machine::drain_audio` (`std::mem::take` on
  `audio_buffer`) is how `retroclawd`'s GUI actually gets this sound out
  -- see its own notes below. ZX81 has no audio modeled at all yet (not
  even its own real beeper) -- parked along with the rest of ZX81, see
  `TODO.md`. `SpeakerType` (`--speaker-type unfiltered|beeper|tv-speaker`,
  default `unfiltered` -- exactly this codebase's original, only-ever
  behavior) is a cosmetic post-mix low-pass filter in `mix_audio_sample`
  approximating a small speaker's frequency rolloff; the two cutoff
  frequencies are explicitly documented as reasonable engineering choices,
  not measured/verified hardware or Fuse-algorithm values (see
  `TODO.md`'s gap-analysis section for why -- Fuse's own libretro core has
  an equivalent option but its exact DSP coefficients weren't verifiable).
  **Turbosound** (`--turbosound`, 2026-09-20): a second `ay_3_8910::
  Ay3_8910` on the bus (`ay2: Option<...>`, `Some` only when the flag was
  given AND `ay` itself is `Some` -- a no-op on 48K, same convention as
  `SpeakerType`/`JoystickType`), selected via the real chip-select
  protocol at the *same* `0xfffd` port that already does register-select
  -- verified against ZEsarUX's own source (`src/soundchips/ay38912.c`,
  shallow-cloned and read directly this session, not the original
  planning phase's unverified web-search guess), see `CREDITS.md`: a
  write where `val & 0x9C == 0x9C` is a chip-select command, not a
  register-select (`val & 3 == 3` -> primary, `== 2` -> second, `== 0`/
  `1` -> SID/third-chip on real Turbosound Next hardware, neither
  modeled here -- still consumed as a chip-select attempt rather than
  falling through, matching ZEsarUX's own behavior for a config with
  fewer chips than asked for). `active_ay_chip: bool` tracks which is
  currently addressed; `active_ay`/`active_ay_mut` (private) resolve
  reads/register-selects/data-writes to whichever chip that is, so the
  two chips' register files stay genuinely independent (each is its own
  `Ay3_8910` instance). Both chips tick every `step()` and mix into
  `mix_audio_sample` at the same weight the single-AY case already used.
  Round-trips through save state (`ay2`/`active_ay_chip` new
  `SaveState` fields) -- restoring `active_ay_chip` can never select a
  chip that doesn't exist on the run doing the restoring, even from a
  state saved with `--turbosound` reloaded without it, so AY audio can't
  silently vanish from a stale flag.
  Kempston joystick: `ZxSpectrumBus::kempston: u8` (port `0x1F`/mirrors,
  decoded on bit 5 clear, `000FUDLR` bit layout, see `CREDITS.md`) via
  `press_kempston`/`release_kempston` (`KempstonButton`, re-exported from
  the crate root) -- present unconditionally on every model, real keyboard
  scans (always low byte `0xFE`, bit 5 set) never collide with it. GUI
  wiring: arrow keys + Space double as Kempston directions/fire alongside
  their existing ZX-keyboard mapping (`any_machine::kempston_button`).
  `JoystickType` (`Kempston`/`Sinclair1`/`Sinclair2`/`Cursor`, `--joystick`,
  default `Kempston`) picks which joystick convention arrow-keys+Space
  actually drive -- real ZX software of the era asked the player to
  choose one at startup, so this is a config knob (`set_joystick_type`,
  same shape as `set_border`/`SpeakerType`), not four simultaneously-active
  inputs. `Sinclair1`/`Sinclair2`/`Cursor` need zero new bus state at all:
  real Sinclair 1 (keys 1-5, port `0xf7fe`), Sinclair 2 (keys 6-0, port
  `0xeffe`), and Cursor/Protek/AGF (keys 5,6,7,8,0, spanning both those
  ports) joysticks were wired straight onto specific keyboard-matrix keys
  on real hardware (verified against World of Spectrum's own peripherals
  reference, see `CREDITS.md`) -- `any_machine::keyboard_joystick_key`
  just maps arrow-keys+Space onto the matching already-existing
  `ZxSpectrumKey`, reaching the already-tested matrix via the same
  `press_key`/`release_key` a real keypress uses. A real, confirmed-live
  consequence: pressing e.g. ArrowLeft under `Sinclair1` presses *both*
  `Digit1` (the joystick) and `Digit5` (the real ZX cursor-key convention
  `spectrum_key` already maps that same arrow key to) simultaneously --
  harmless, since software only ever reads whichever one it wants, same
  "rides the same physical keys" principle Kempston's own doubling
  already established. `Fuller` (fixed 2026-09-18, right after Sinclair/
  Cursor) IS a real, separate port -- `ZxSpectrumBus::fuller: u8` (port
  `0x7f`, `FullerButton`, **active low** unlike Kempston -- idle state is
  `0xFF` not `0`, bit layout `F---RLDU` per World of Spectrum, quoted
  directly in `CREDITS.md`) via `press_fuller`/`release_fuller`, same
  press/release shape as Kempston. Decode matches only the literal low
  byte `0x7f` (`port & 0x00FF == 0x7F`) -- no source found pinning down
  real hardware's actual address-decode precision the way Kempston's
  "usually only A5" is documented, so this deliberately doesn't guess a
  wider mirrored decode.
  Kempston mouse (fixed 2026-09-18, right after Fuller): `ZxSpectrumBus`
  gains `mouse_x`/`mouse_y: u8` (real free-running relative counters,
  wrap at `0`/`0xFF`, not absolute screen position) + `mouse_buttons: u8`
  (active-high internal storage, bit0=Right/bit1=Left/bit2=Middle, same
  "invert only on the way out" convention `fuller` doesn't need but this
  does since real hardware wires buttons active-low) + `mouse_wheel: u8`
  (accumulates since the button port was last read, real hardware's own
  "wheel-delta since previous read" semantic -- `read_io` resets it to
  `0` on every read of that port, a genuine read-side-effect same shape
  `tape`'s own EAR-bit read already has). Three real ports (`0xfbdf` X,
  `0xffdf` Y, `0xfadf` buttons+wheel), decode derived from `sinclair.
  wiki.zxnet.co.uk`'s own port table (see `CREDITS.md`) rather than just
  the 3 literal addresses -- bit 5 of the low byte clear (same bit
  Kempston's own joystick checks) AND bits 10:8 of the port select which
  of the three (`0b011`=X, `0b111`=Y, `0bx10`=buttons, bit 10 a genuine
  don't-care per the cited table). Checked in `read_io` BEFORE the
  Kempston joystick's own looser bit-5-only match (that check alone would
  otherwise swallow every mouse read too, since all 3 mouse ports also
  have bit 5 clear). **Real bug caught live** by this crate's own
  pre-existing keyboard tests failing (not a mouse-specific test): an
  earlier version of the mouse decode checked only bits 10:8, omitting
  the bit-5 requirement -- port `0xFBFE`, a perfectly ordinary keyboard-
  scan address whose row-select byte (`0xFB`) happens to also have bits
  10:8 `== 0b011`, was getting incorrectly swallowed as a mouse-X read.
  Fixed, and a dedicated regression test locks in the exact collision.
  Always present regardless of `--joystick` (real hardware: a mouse
  interface and a joystick interface are separate ports, both coexist),
  driven by `gui::input::mouse_input` (`MessageReader<MouseMotion>`/
  `MessageReader<MouseWheel>` -- Bevy 0.19's real names, renamed from
  `EventReader`/`EventWriter` in 0.17, not guessed -- + `Res<ButtonInput
  <bevy::input::mouse::MouseButton>>`) sending `emu_thread::MouseEvent`s
  down a new `MouseTxRes`/`mouse_tx` channel, same one-way-channel shape
  `InputTxRes`/`ResetTxRes` already established; `AnyMachine::mouse_event`
  dispatches it, a no-op on every machine but `ZxSpectrum`. Needed adding
  `bevy`'s own `mouse` cargo feature explicitly (same "not auto-enabled by
  `2d`'s transitive `bevy_window` the way one might assume" gotcha
  `keyboard` already required). Y motion is passed through unmodified,
  not inverted -- an unverified engineering choice, not a confirmed real
  Kempston-mouse-driver convention, same epistemic tier as `SpeakerType`'s
  filter cutoffs.
  Tape: `ZxSpectrum::step` also calls `bus.tick_tape(cycles)` right after
  the AY tick (same "needs per-instruction granularity" reasoning) --
  `ZxSpectrumBus::tape: Option<zx_tape::TapePlayer>` (mirrors `ay`'s
  `Option` shape), installed via `ZxSpectrum::load_tape`/`set_tape_speed`
  (`AnyMachine::apply_tape` is the CLI-facing dispatch, same "concrete
  per-machine concern" reasoning as `apply_snapshot` -- the ZX81 has its own tape, see below). `read_io`'s keyboard-port branch (port `0xFE`
  and mirrors) composes bit 6 from `tape.ear_bit()` when a tape is
  installed -- verified bit layout: bit 6 = EAR input, bits 5/7 always
  read 1 (World of Spectrum 48K reference, see `CREDITS.md`); `keyboard.
  read` already leaves bit 6 set (idle-high) by default, so this only
  clears it when the tape's current pulse is low. Playback does **not** start on
  that read (2026-10-14): the ROM's keyboard scan reads port `0xFE` from power-on, which used to start every
  loaded tape at boot, so a `LOAD ""` typed seconds later found its first blocks gone. `ZxSpectrum::step` presses
  Play (`bus.start_tape()`, a safe no-op once playing or finished) when `cpu.pc` reaches `LD-BYTES` (`0x0556`, the
  real `INC D` still there), like the user pressing Play when LOAD asks for the tape. The option `tape_start =
  any-read` (`set_tape_start_on_any_read(true)`, `--tape-start any-read`) restores the old start-at-the-first-read
  rule for a custom loader started from a snapshot; a bare `ZxSpectrumBus` keeps that rule, only `ZxSpectrum::new`
  turns it off. Generic
  across every `Model` -- 48K/128K/+2A/+3 all share the same port `0xFE`.
  Fast tape load (`--fast-tape-load`, opt-in, `ZxSpectrum::
  try_fast_tape_load_trap`, checked at the top of `step` before the normal
  `cpu.step` call): when `cpu.pc` reaches the real ROM `LD-BYTES` routine
  (`0x0556`, same address on the 128K-family's own 48K-compatible ROM 1 --
  verified against the real `INC D` opcode still being there before
  trapping, so a custom ROM using that address for something else isn't
  hijacked), substitutes the routine's entire effect directly from
  `bus.take_next_tape_block()` (checksum-verified memory write for LOAD or
  compare for VERIFY, correct carry-flag/register outcome matching the
  real calling convention -- see CREDITS.md, simulated `RET` back to the
  real caller via the return address already on the stack) instead of
  bit-banging real-time pulses -- near-instant regardless of tape length, a
  genuinely different mechanism from `set_tape_speed`'s turbo multiplier
  (still plays every real pulse, just faster). Only engages on the exact
  common case (flag byte and length both match); anything else (mismatch,
  no tape, or a `.tzx` Turbo/`.pzx` block the trap has no calling-
  convention knowledge of) falls through to the existing real-time path.
  Snapshot loading: `ZxSpectrum::load_snapshot(z80_snapshot::Snapshot) ->
  Result<(), String>` replaces the whole `Cpu`, writes every memory block
  (`z80_snapshot::MemoryBlock::Absolute` via `bus.load`, `::RamBank` via
  `bus.load_ram_bank` — 128K only), restores border color and (128K only)
  paging state — `.z80` fully encodes register state, so there's nothing
  else to synthesize. Errors (doesn't silently mis-load) if the snapshot's
  own model (`Snapshot::paging.is_some()`) doesn't match this
  `ZxSpectrum`'s `Model`. `z80_snapshot::parse` itself: version-aware
  hardware-mode acceptance, since the real spec's mode numbering *differs*
  between v2 and v3 (mode 3 means "128k" in v2 but "48k + M.G.T." in v3 —
  a real trap, caught via an actual v2-mode-3 `Fairlight - A Prelude`
  snapshot that needed real 128K support to load correctly, not a
  hypothetical). `Zx81::load_p_snapshot(zx81_snapshot::Snapshot)` is bigger
  because `.p`/`.p81` encode no registers at all: it writes the snapshot's
  bytes at `0x4009`, then synthesizes the fixed 9-byte system-variable
  header, a 4-byte stack patch, and every register value real ZX81 hardware
  derives from `RAMTOP` before resuming `BASIC` — constants taken from
  ZEsarUX (see `CREDITS.md`), `RAMTOP` at the top of the installed RAM
  (`0x43FF` for the stock 1K, `0x7FFF` with a 16K pack). Real `.p`/`.p81` files are routinely
  several KB (saved from a machine with a 16K+ rampack) — build the machine with a matching
  `--ram-kb`, or the load is truncated at the end of the installed RAM.
  Save states: new `save_state.rs` modules in both `zx_spectrum`/`zx81`
  (`SaveState`, `#[derive(Serialize, Deserialize)]`), serialized via
  `bincode` v2's `serde` compat feature
  (`bincode::serde::encode_to_vec`/`decode_from_slice`,
  `bincode::config::standard()`) into this project's own internal
  `.state` format — never meant for interop, unlike `.z80`/`.sna`, so
  compactness/speed over human-readability. Goes further than either
  snapshot format: round-trips every peripheral's own live timing state
  too (AY-3-8910 generator phase, the +3's FDC command/result-phase state
  machine, tape playback position), not just CPU+memory+paging. Two
  shapes per type, chosen by whether it has any externally-supplied-media
  entanglement: types with none (`z80::Cpu`, `ay_3_8910::Ay3_8910`,
  `sinclair_keyboard::KeyboardMatrix`) get direct serde derives added
  straight onto the real internal type; types that hold or are generic
  over external media (`nec765::Fdc<D>` holds `disk: Option<D>`,
  `zx_tape::TapePlayer` holds `tape: Tape`) instead get a small
  purpose-built mirror struct (`FdcState`/`TapePosition`) plus
  `dump_state(&self) -> XState`/`restore_state(&mut self, XState)` —
  bincode explicitly warns that `#[serde(skip)]` and friends "can and
  will cause issues ... and will result in lost data," so the external
  field is never annotated away, just never referenced by the mirror
  struct at all. Deliberately excludes ROM contents, the loaded disk
  image's own bytes, and the loaded tape's own pulse/byte data — matching
  this codebase's own `--rom` convention: all three stay externally
  supplied, and restoring a save state requires the same
  `--rom`/`--tape`/`--disk` already given, exactly like `.z80`/`.sna`
  already require for `--rom`. `Machine` trait's `save_state(&self) ->
  Vec<u8>`/`load_state(&mut self, &[u8]) -> Result<(), String>` (both
  non-default — a machine with no save-state support should be a compile
  error for callers expecting one) are what `retroclawd`'s
  `--dump-state`/`--load-state` actually call, straight through
  `AnyMachine::as_machine()`/`as_machine_mut()` — no new `AnyMachine`
  dispatch needed, since the format is uniform bytes-in/bytes-out
  regardless of machine. Each impl bincode-encodes/decodes its own
  private `SaveState` internally, so `machine` itself needs no
  `serde`/`bincode` dependency. `ZxSpectrum::load_state`/`Zx81` (N/A,
  single model) check `state.model` against `self.model` and error
  clearly before touching any state, same "must match, no silent
  best-effort" stance as `load_snapshot` — `restore_memory` itself can't
  safely detect a mismatch (applying a `Memory::Flat` payload onto banked
  memory, or vice versa, would silently do the wrong thing, not error).
- **ZX81 tape** (2026-10-16, `zx81::tape` + `Zx81Bus`): a `.p` / `.81` / `.p81` attached as `MediaKind::Tape` becomes a
  `zx_tape::Tape` (`tape::encode`: ZXDOCS "Cassette Signals" -- bytes MSB first, a pulse is 150 us high + 150 us low, a `0` is 4
  pulses and a `1` is 9, each bit followed by 1300 us of silence, a 1 s leader instead of the listed 5 s, then the name with
  bit 7 on its last byte, then the data = the `.p`; a nameless `.p` gets its file name's letters and digits, `A` if none) and
  plays through a `TapePlayer`. Port `0xFE` reads return the level in **bit 7** (0 = normal, 1 = pulse); the player idles
  *high* when stopped, so the bus only passes it through while the tape is playing (the Spectrum's bit 6 wants the idle-high).
  This replaced the old "bits 5-7 always read 1": an idle port read is now `0x7F`. Play is pressed when execution reaches
  `0x0347` (the `CALL` that starts reading, after `LOAD`'s name check at `0x0340`), the opcode checked. The MIC output is the
  retrace latch: a write to any port sets it high and a `0xFE` read sets it low, **only while the NMI generator is off**
  (ZXDOCS "I/O Ports"); its edges are recorded from the moment the ROM enters `SAVE` (`0x02F6`) and `tape::decode` reads the
  file back (rising edges grouped into bursts, 4 pulses = 0, 9 = 1; the display's single retrace pulses and anything else are
  skipped; the length comes from `E_LINE` inside the data). `ExportTape` returns `p`/`81` (the data) or `p81` (name + data);
  `--tape-save x.p` writes it at exit and the save-tape hotkey proposes `.p`. Also fixed on the way: `write_io` started the NMI
  generator on *any* port with bit 1 set, including `0xFF`, which `SAVE` writes for every pulse; it now follows `0xFD` (off) and
  `0xFE` (on) only. Real speed is 307 bit/s (a 16K program is minutes of signal; `--tape-speed` above 1 would break the
  ROM's timing loop, as on the Spectrum), so there is an opt-in **fast-load trap** (`fast_tape_load` / `--fast-tape-load`,
  `Zx81::set_fast_tape_load`, `try_fast_tape_load`): the ROM still plays the leader and matches the name from the real
  signal, and when it reaches its data loop (`0x0375`, `INC (IY+0x15)`, opcode checked) the file kept by `load_tape_file`
  is stored at `0x4009` (as many bytes as its own `E_LINE` says, which is where the ROM's loop would stop), `PC` is set
  to `0x0207` (the tail of `LOAD`, which discards the loop's return address itself) and the tape is consumed. Off by
  default; the suite loads a 2.9K program with it in well under 900 frames while the same run without it is still reading.
  A save state keeps the tape's position, the MIC latch and the cycle count (not the pulses: attach the same file before
  loading the state). Bit 6 of a port `0xFE` read is the refresh rate, 1 on a 50 Hz machine and 0 on a 60 Hz one (`--region
  ntsc`): the ROM's keyboard routine at `0x02BB` turns it into the picture's top margin, 55 lines or 31. Tape files in
  other formats (`.tzx`, `.csw`, `.voc`, `.pzx`) are not accepted for the ZX81: `zx-tape` stores their pulses in 3.5469 MHz
  T-states and has no way to rescale them to the ZX81's 3.25 MHz yet.
  **A real bug found by the 16K cases (2026-10-16)**: `video::decode_rows` stopped a full 32-character row without
  consuming its NEWLINE, so the expanded display file of a machine with 3K+ RAM (24 rows of 33 bytes) decoded as every
  other row empty and the last row, the prompt, was never shown: a 16K ZX81 drew a blank screen. Fixed, with a unit
  test and a suite case. A 16K machine also takes about 2.4 s (120 frames) to reach its prompt (the ROM tests the RAM),
  so type from frame 250, not 100.
  Checked by the `rc-suite` ZX81 cases: `LOAD ""` brings a program in, `SAVE "A"` produces the described file (`.p` and `.p81`),
  and what `SAVE` wrote loads back through the ROM.
- **`z80-snapshot`** — parses `.z80` snapshot files (versions 1-3), per the
  World of Spectrum FAQ's format spec (see `CREDITS.md`): `parse(bytes) ->
  Result<Snapshot, Error>` returns a real `z80::Cpu` + border color +
  `MemoryBlock`s (`Absolute{address,data}` for 48K, `RamBank{bank,data}`
  for 128K — a 128K page's target address isn't fixed, it depends on
  paging state the caller applies, see `zx_spectrum`'s notes above) +
  `paging: Option<PagingState>` (`Some` for 128K-family modes, decoded
  straight from the header's own last-OUT-to-`0x7ffd` byte, `None` for
  +2A/+3 too -- see `Paging2A3State` below) + `paging_2a3:
  Option<Paging2A3State>` (`Some` only for +2A/+3 modes, its own struct
  rather than folding into `PagingState` -- same "distinct shape" reasoning
  as `zx_spectrum::bus::Paging2A3`). Version detected via the real spec's
  own rule (v1 header's PC field of `0` means v2/v3, whose real PC +
  hardware mode live in the additional header instead). Hardware-mode
  acceptance is version-*aware*, not just a single table — the spec's own
  numbering differs between v2 and v3 (mode 3 means "128k" in v2 but "48k +
  M.G.T." in v3; `classify_hardware_mode` takes `is_v3` explicitly rather
  than assuming). 48K, 48K+If.1, 48K+M.G.T (disk interface ignored,
  memory-identical to 48K), 128K, 128K+If.1, 128K+M.G.T., Spectrum +2, and
  Spectrum +2A/+3 (v3-only modes 7/8/13 -- decodes byte 35 as the `0x7ffd`
  byte and byte 86 as the `0x1ffd` byte, erroring clearly via
  `Error::Missing2A3PagingByte` if the additional header is shorter than
  the 55 bytes needed to carry byte 86) are all supported; SamRam and clone
  hardware (Pentagon/Scorpion/Didaktik/TC-*/TS2068) use memory layouts this
  project doesn't model, so `parse` errors clearly (`UnsupportedHardwareMode`)
  rather than silently mis-loading. `ED ED xx yy` run-length decompression + the v1
  `00 ED ED 00` end marker + the byte-12-value-255 legacy quirk ("regarded
  as being 1") all implemented per spec, not guessed. `write` (the save
  side, see `sna-snapshot`'s own bullet below for the shared design) uses
  the `Snapshot` shape symmetrically, exactly as planned when it was
  load-only.
- **`sna-snapshot`** — parses `.sna` snapshot files (48K, per the World of
  Spectrum FAQ's snapshot format spec — see CREDITS.md), plus the
  plain-128K extension (one `0x7ffd` paging byte; predates the +2A/+3's
  second paging register, so `zx_spectrum::ZxSpectrum::load_sna_snapshot`
  rejects `Model::Spectrum2A3` outright — `.z80` is the only snapshot path
  for that model). Own `Snapshot`/`MemoryBlock`/`PagingState` types,
  deliberately not shared with `z80-snapshot`'s conceptually similar ones
  — same "own shape per format" reasoning as `Paging2A3` vs `Paging`. 48K's
  own quirk: the format has no explicit `PC` field at all — real
  `.sna`-writing tools always `PUSH`ed it onto the stack right before
  dumping memory, so `parse` recovers it by reading the word `SP` points
  at and leaves `SP` 2 higher, exactly what a real `RET` would do (the
  128K extension fixes this at the source with its own explicit `PC`
  field). `write` is the save-side counterpart (round trip verified by
  test): builds the same `Snapshot` shape back into bytes. `z80-snapshot`
  also gained its own `write` -- always a v3-shaped file (55-byte
  additional header) regardless of model, with every page stored
  uncompressed (the `length == 0xFFFF` sentinel `decode_page_blocks`
  already reads, so no RLE compressor was needed for a real, spec-legal
  save side). `ZxSpectrum::save_snapshot`/`save_sna_snapshot` build a
  `Snapshot`/`sna_snapshot::Snapshot` from the machine's own live state
  (new `ZxSpectrumBus` accessors: `flat_ram`, `ram_bank_data`,
  `paging_state`, `paging_2a3_state`); `retroclawd --dump-snapshot <path>`
  (headless, format by extension) is the CLI-facing save.
- **`szx-snapshot`** (2026-09-20) — parses/writes `.szx` (ZX-State), a
  chunked (RIFF-like) format: 8-byte file header (`"ZXST"` + version +
  machine id + flags) then a stream of 8-byte-header (4-char id + `u32`
  LE length) blocks, unrecognized ones safely skipped by length (same
  "must-skip-not-error" shape `zx_tape::parse_pzx` already uses). Every
  offset/bit-layout/machine-id came from `libspectrum`'s own real
  `szx.c` (shallow-cloned this session, not a doc summary — see
  CREDITS.md, including the real off-by-one the earlier planning pass's
  web research had gotten wrong on machine ids). Implements `Z80R`
  (registers, 37 bytes, version-gated tail — the same `z80::Cpu` type
  `z80-snapshot`/`sna-snapshot` already use, not a fourth register
  shape), `SPCR` (border + `0x7ffd`/`0x1ffd` ports — same bit meanings
  `z80_snapshot`'s own paging decode already established, this crate's
  own duplicated `PagingState`/`Paging2A3State`, matching `sna-snapshot`'s
  "own shape per format, not shared" precedent), `RAMP` (one real 16K
  page each, optional zlib compression via the `flate2` dependency
  `.csw` already added — write side always writes uncompressed, since
  read-compatibility with real compressed files from Fuse/ZEsarUX/
  Spectral is what matters, not this project's own writer bothering to
  compress), and `AY\0\0` (the single AY chip's 16 registers + current
  selection — Turbosound's second chip has no block of its own in the
  real format, not modeled here). New `Ay3_8910::read_register`/
  `selected_register` (small refactor of the existing `read()`) let a
  `&self` snapshot-saving context dump every register without a `&mut`
  borrow. `ZxSpectrum::load_szx_snapshot`/`save_szx_snapshot` mirror
  `load_snapshot`/`save_snapshot`'s exact model-mismatch-checking shape;
  `AnyMachine::apply_snapshot`/`save_snapshot` and `--snapshot`/
  `--dump-snapshot`/`--game-config`'s `media` extension-sniff `.szx`
  alongside `.z80`/`.sna`. Verified live against `libspectrum`'s own
  real test fixtures (`test/random.szx`, a genuine Fuse-written 1.5 file
  with a real zlib-compressed page, plus its minimal single-chunk
  fixtures) in addition to this project's own round-trip tests — see
  `TODO.md`.
- **`zx81-snapshot`** — parses `.p`/`.p81`/`.81` snapshot files. Neither
  format encodes CPU registers at all (unlike `.z80`) — both are just a raw
  memory dump meant to load starting at `0x4009` (`parse_p`, no header) or,
  for `.p81` only, that same dump preceded by a ZX81-character-encoded
  program name terminated by a high-bit-set byte (`parse_p81`, name bytes
  returned raw/undecoded — decoding to ASCII needs the ZX81 character-set
  table, not verified against real ROM data here, and isn't needed to load
  the program). Based on ZEsarUX's `snap_zx8081.c` (see `CREDITS.md`).
  Both directions since 2026-10-16: `write_p` (the data, nothing else) and `write_p81` (name header + data; a
  name that does not end in exactly one high-bit byte is an error). `Zx81::save_p_snapshot` collects memory
  from `0x4009` up to `E_LINE`-1 (the word at `0x4014`, what the ROM's `SAVE` writes; an `E_LINE` outside the
  installed RAM is an error, i.e. before the ROM has set the system variables up) and the plugin's
  `SaveSnapshot` returns it for `.p` / `.81`; `.p81` is refused (the emulator has no program name: use
  `rcsnap convert game.p game.p81 --name NAME`).
