# TODO / Roadmap

Backlog for the Sinclair (ZX81, ZX Spectrum) family, moved from the host repository's TODO.md. Host-wide items (plugin architecture, input, shaders, parked machines) stay in the host.

## ZX Spectrum roadmap

### DONE: baseline

Supported models are 48K, 128K/+2, and +2A/+3. Existing coverage includes banked memory, ULA
contention, contended I/O, floating bus, beeper/AY/Turbosound audio, keyboard and joystick variants,
Kempston mouse, tape and +3 disk loading, snapshots, save states, run-ahead, and debugger support.


### BACKLOG: external parity execution sequence

Execute the newly-added ZX parity items in this strict order:

1. Model and clone coverage parity (the section immediately below).
2. Storage/interface parity (start with the WD1793-family and related disk interfaces in the
   storage/expansion section).
3. Media/replay parity (RZX replay + rewind + libspectrum/Fuse tape/snapshot interop items in the
   media section).
4. Debugger parity: reverse debugging and instruction-history backstep navigation.

### BACKLOG: core parity track — model and clone coverage (gap vs Fuse/ZEsarUX)

Execute in this order:

1. Extend supported Spectrum-family models beyond 48/128/+2/+2A/+3 in hardware-risk order: +3e/SE,
   Timex TC2048/TC2068/TS2068, Pentagon variants, then Scorpion.
2. Add machine-specific timing/I/O tests for every new model; do not route clone behavior through
   ad-hoc flags on existing models.

### BLOCKED: ULA snow

Do not implement from the currently available uncertain descriptions. The missing fact is the exact
hardware trigger and corruption timing. Resume only with a reliable primary source or a validated
hardware test suite. Existing frame-position and screen-address helpers are sufficient once the
trigger is known.

### BLOCKED: Issue 2 versus Issue 3 keyboard-port behavior

The exact unassigned-bit levels still lack a sufficiently authoritative source. Do not guess. Resume
only with a citable hardware description or test.

### BACKLOG: parity extensions — ULA+

Add the real port-decoded palette RAM and extended-color decode path beside the normal
ink/paper/bright/flash path. This is a hardware extension, not a cosmetic shader option.

### BACKLOG: parity extensions — audio expansion

Stereo requires an end-to-end mono-to-stereo pipeline change before AY channel separation can be
correct. Do that once, then add:

- AY stereo layouts such as mono, ACB, and ABC.
- Optional byte-wide DAC peripherals such as Covox, Fuller audio, Melodik, Currah µSpeech, and
  SpecDrum only when their individual port protocols are verified.

Do not bolt nominally stereo controls onto the current mono `Vec<f32>` pipeline.

### BACKLOG: parity extensions — storage and expansion hardware

Each item requires hardware modeling, not just another parser. Execute in this order:

1. TR-DOS/Betadisk and related disk interfaces: WD1793-family controller plus +D, Didaktik 80/40,
   DISCiPLE, and Opus Discovery behavior, then `.trd`/`.scl`; consider `.fdi`, `.mgt`, and
   `.hobeta` afterward.
2. ZX Interface 1 and Microdrive: expansion hardware plus `.mdr`; RS232 and ZX Net may reuse that
   foundation.
3. DivIDE/DivMMC/ZXMMC/ZXCF/ZXATASP: block storage plus the applicable ROM/esxDOS behavior.
4. Multiface One/128/3: NMI freeze path and mapped cartridge memory.
5. ZX Printer: expansion-port protocol and an explicit host output format.

### BACKLOG: parity extensions — media formats and recording

Execute in this order:

1. Tape saving through MIC output (separate from snapshot saving).
2. Additional TZX blocks only when real media requires them; unknown data-bearing blocks must remain
   errors rather than guessed skips.
3. RZX deterministic input replay as its own recording/replay feature, not a snapshot format.
4. Rewind using an in-memory state ring. Keep it separate from run-ahead and disk save-state
   serialization.
5. Broaden tape/snapshot interop toward libspectrum/Fuse coverage (`.spc`, `.sta`, `.ltp`, `.zxs`,
   `.sp`, `.snp`, `.S`, +D snapshots) only when sample files and machine-level semantics are
   verified.

### BACKLOG: cross-emulator comparison addendum (ep128emu as additional source)

Source set to consult for all future ZX Spectrum work in this roadmap:

- https://docs.libretro.com/library/ep128emu/
- https://github.com/libretro/ep128emu-core
- https://github.com/istvan-v/ep128emu

Current comparison snapshot versus retroclawd's listed ZX baseline:

- retroclawd already lists broader Spectrum coverage (`48K/128K/+2/+2A/+3`, `.szx`, `.pzx`, `.csw`,
  Turbosound, multiple joystick/mouse paths) than ep128emu's ZX scope in its published docs/README.
- ep128emu remains useful as an additional implementation source for overlapping behavior
  (contention-sensitive timing paths, tape loader behavior, joystick mapping conventions).
- Where emulators differ, prefer hardware-backed behavior and executable tests.

Continue only after completing the external parity sequence steps 1-4 above, then execute in this
order:

5. Build a ZX feature-delta checklist against ep128emu in overlapping areas (`.tap`/`.tzx` behavior,
   joystick mapping conventions, contention-sensitive timing paths, and snapshot expectations).
6. For each remaining ZX backlog item, consult the ep128emu sources above as an additional
   implementation reference before coding.
7. If ep128emu and other references disagree, require a hardware source or executable behavioral
   test before selecting behavior.

Do not import code blindly; treat ep128emu as a secondary implementation source, not a substitute
for hardware verification.

## ZX81 roadmap — resumed (2026-10-15)

### DONE: baseline

Current ZX81 support includes real keyboard scanning, NMI delivery, display-file rendering, reset,
`.p`/`.p81` loading, and internal save states.

### DONE (2026-10-15): memory model and video fetch (items 2 and 3 below, driven by the `rc-suite` ZX81 cases)

The memory map has the hardware's mirrors (ROM across `0x0000-0x3FFF` and `0x8000-0xBFFF`, RAM repeated through
`0x4000-0x7FFF`, data reads at `0xC000+` seeing `0x4000-0x7FFF`) and selectable RAM (`--ram-kb` 1, 2, 16, 32, 48, 56). An opcode fetch at `0x8000`+
is a NOP when bit 6 of the byte is clear (`Bus::fetch_opcode`, a default method of the shared `machine::Bus`),
and the maskable interrupt is asserted while bit 6 of `R` is zero. The ROM's own display routine therefore runs:
`FRAMES` counts 50 a second in SLOW mode. The picture is still decoded on demand from the display file, not
generated pixel by pixel from the fetch pattern.

### DONE (2026-10-16): save a `.p` / `.81` (found by the `rc-suite` ZX81 cases)

`--dump-snapshot x.p` / `.81` and the save-snapshot hotkey write memory from `0x4009` up to `E_LINE`-1
(`zx81-snapshot::write_p`, `Zx81::save_p_snapshot`, the plugin's `SaveSnapshot`), verified by the suite: the file
is that memory, `.81` equals `.p`, and loading a saved file autostarts the program again. `.p81` is not saved
(no program name in the emulator; `rcsnap convert` adds one).

### TODO (2026-10-16): ZX81 tape (found by the `rc-suite` ZX81 cases)

`--tape x.p` fails with `unsupported media kind for zx81 plugin: Tape (only snapshot is supported)`, and
`ExportTape` is unsupported too, so the ROM's `LOAD ""` / `SAVE "NAME"` never see a signal. Needed: the ZX81
tape signal (ZXDOCS, "Cassette": pulses on the MIC output, read through bit 7 of port `0xFE`, the EAR input), a
`.p` / `.p81` played as a tape (the name from a `.p81` header, none for a `.p`) and a recording of what `SAVE`
writes back out as a `.p`. The suite's gap case `LOAD "" reads a program from the tape input` (`retroclawd-tools`
`crates/rc-suite/tests/zx81.rs`) defines it; it has only ever failed on the missing media kind, so its typed
`J""` and the 2000-frame budget are unchecked until the feature exists.

### BACKLOG: external parity execution sequence (PARKED; gap vs ZEsarUX-class ZX81 emulators)

If the ZX81 track is resumed, execute in this strict order (do not reorder):

1. Tape path parity: implement tape input/output with verified pulse/ROM-loader semantics. Keep
   cassette audio as the only ZX81 audio target.
2. Memory model parity: add rampack support instead of the current fixed 1K RAM ceiling. **Done**, see above.
3. Video-behavior parity: move beyond on-demand display-file decode toward continuous video behavior
   needed for software-dependent display tricks. **Partly done**: the fetch and interrupt mechanism runs the ROM's
   display routine (see above); the picture is still decoded on demand, so tricks that change the display
   file mid-frame are not drawn yet.
4. Snapshot/media parity: add `.p`/`.p81` writing; decode `.p81` program names only after verifying
   the ZX81 character table against ROM data.
5. ROM behavior checks: add interactive machine-level checks that continue stepping through
   `HALT`/NMI cycles.

### BACKLOG: parity extensions (PARKED)

None while this roadmap is parked; resume only after the external parity sequence above is active.

### BACKLOG: cross-emulator comparison addendum (PARKED; ZEsarUX as additional source)

Source set to consult when this roadmap is resumed:

- https://github.com/chernandezba/zesarux

When additional trusted ZX81 emulator sources are selected, add them here and update the
external-parity checklist before implementation begins.

### BLOCKED: fictional ZX81 sound

ZX81 has no conventional sound generator; cassette audio is the relevant unmodeled audio path. Do
not add a fictional beeper.

