# ZX81 timing: 208 against 207 T-states per line (S6 known gap)

**Closed 2026-10-19.** The ULA's sync/NMI generator counts 207 cycles of the 3.25 MHz clock and resets (63.7 us,
a little short of PAL's 64 us); the ROM's display code is written for 207 T-states per line. Sources: Andy Rea's ZX81 ULA
schematics (cpcwiki), the CLK emulator's "The ZX80 and ZX81" notes; EightyOne and ZEsarUX use 207 as well. The old 208 had been
derived as 64 us x 3.25 MHz. `Zx81` now fires its NMI every 207 T-states and reports an exact frame rate of 3 250 000 / (312 x 207)
(50.32 Hz; 262 lines on NTSC) through `Machine::frame_rate` and the plugin's `MachineMetadata::frame_rate`, so the host's frame is
64584 T-states. The clock stays 3.25 MHz (the 6.5 MHz crystal halved); ZEsarUX's 3.2292 MHz is its own choice to make 312 x 207
lines come out at exactly 50 Hz. S6 measures 64584 plus at most one instruction per frame.

Found 2026-10-18 by `zx81.manifest.toml` scenario S6.

| | clock | T-states per line | per frame (312 lines) | frame rate |
|---|---|---|---|---|
| ZEsarUX 13.0 (`get-cpu-frequency` over ZRCP: 3229200) | 3.2292 MHz | 207 | 64584 | 50 Hz |
| retroclawd (`rc-scenarios`, 100 frames): 65003 per frame | 3.25 MHz | 208 (NMI generator period) | 65000 | 50 Hz |

The difference is 0.65 %. The emulator fires its line NMI every 208 T-states ("64 us, the real PAL TV line rate", `Zx81::step`),
while ZEsarUX counts 207 per line, so its frame is 312 x 207 cycles at a clock of 207 x 312 x 50 = 3 229 200 Hz.

Not settled: which figure real hardware has. Next step: read the ZX81 ULA timing in ZXDOCS and the Wilf Rigter / Nuno Ruivo
notes, then either change `Zx81` to 207 (and its `clock_hz`) or keep 208 and record why ZEsarUX differs. No emulator code was
changed to make the scenario pass.
