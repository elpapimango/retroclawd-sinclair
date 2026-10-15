# Credits

Third-party references this codebase's implementation is based on -- none
of this code is copied from these sources; each is a fresh Rust
implementation of documented behavior, cited here because that behavior
(file layouts, byte offsets, register constants) came from reading them,
not from independent derivation.


## `.z80` snapshot format (`crates/z80-snapshot`)

Format specification: the World of Spectrum FAQ, "Z80 file format"
<https://worldofspectrum.org/faq/reference/z80format.htm> -- every header
byte offset, the version-detection rule (PC field of zero signaling v2/v3),
the `ED ED xx yy` compression scheme and its `00 ED ED 00` v1 end marker,
the byte-12-value-255 compatibility quirk, and the page-number-to-address
table for 48K/128K/SamRam modes all come from this document.

## ZX Spectrum 128K AY-3-8910 ports (`crates/zx-spectrum/src/bus.rs`)

The World of Spectrum FAQ, "128K reference"
<https://worldofspectrum.org/faq/reference/128kreference.htm> -- the
literal port addresses `0xfffd` (select/read a register) and `0xbffd`
(write to it).

## ZX Spectrum +2A/+3 memory paging (`crates/zx-spectrum/src/bus.rs`)

The official ZX Spectrum +3 technical manual, chapter 8
<https://worldofspectrum.org/ZXSpectrum128+3Manual/chapter8pt24.html> --
`0x1ffd`'s bit-by-bit meaning (paging mode, ROM select high bit, the 4
special all-RAM configurations' exact bank layouts, disk motor bit) and
how it combines with `0x7ffd`'s ROM-select bit to choose 1 of 4 ROMs, all
taken verbatim from the manufacturer's own manual, not a third-party
summary.

`0x1ffd`'s port address decode (`port & 0xF002 == 0x1000`) isn't stated as
a bitmask in that manual -- derived by hand from the bit pattern given in
a ZX Spectrum Next hardware test-suite reference,
<https://github.com/MrKWatkins/ZXSpectrumNextTests/blob/develop/ports.txt>,
and cross-checked against the literal port value `0x1FFD`.

## Contended memory timing (`crates/zx-spectrum/src/bus.rs`)

48K contended range (`0x4000-0x7FFF`), the 8-T-state repeating delay
pattern (`6,5,4,3,2,1,0,0`), the T-state within the frame the contended
window starts at (14335), and the 224-T-state line length -- the World of
Spectrum FAQ, "48K reference"
<https://worldofspectrum.org/faq/reference/48kreference.htm>. 128K/+2's
228-T-state line length and 14361 start T-state, +2A/+3's own 10-T-state
gate-array pattern (`1,0,7,6,5,4,3,2,1,0`) and contended-bank set (pages
4/5/6/7, vs 128K's 1/3/5/7) -- the Sinclair ZX Spectrum wiki, "Contended
memory" <https://sinclair.wiki.zxnet.co.uk/wiki/Contended_memory>, cross-
checked against the World of Spectrum reference above where both cover the
same ground (48K numbers agree exactly). Real +2A/+3 hardware also gates
contention on its MREQ line; this codebase treats it as always-active like
the ULA-based models, a documented simplification (see that file's own
`CONTENTION_2A3` doc comment).

The I/O contention model (4 branches keyed on port-high-byte-contended x
port-bit-0, and the fact that the +2A/+3 gate array has none at all since
MREQ is inactive during an I/O cycle) -- the Sinclair ZX Spectrum wiki,
"Contended I/O" <https://sinclair.wiki.zxnet.co.uk/wiki/Contended_I/O>.

## Floating/"unattached" bus reads (`crates/zx-spectrum/src/bus.rs`)

The video-fetch sequence a genuinely unmapped I/O port read returns
(bitmap, attribute, bitmap+1, attribute+1 at local offsets 3/4/5/6 of each
8-T-state contended block, idle the rest of the time) -- the Sinclair ZX
Spectrum wiki, "Floating bus"
<https://sinclair.wiki.zxnet.co.uk/wiki/Floating_bus>, confirmed for
48K/128K specifically; applied to +2A/+3 too as an unverified-but-
documented extension (that page has no +2A/+3-specific data -- see
`ZxSpectrumBus::floating_bus_byte`'s own doc comment).

## `.sna` snapshot format (`crates/sna-snapshot`)

Format specification (48K header layout, the 128K extension's own bank
order and extra fields): the World of Spectrum FAQ, snapshot format
specification <https://worldofspectrum.org/faq/reference/formats.htm>.

## Fast-tape-load ROM trap (`zx_spectrum::ZxSpectrum::try_fast_tape_load_trap`)

The real `LD-BYTES` ROM routine's address (`0x0556`, identical on the
128K-family's own 48K-compatible ROM 1), calling convention (`A` = expected
flag byte, carry set for LOAD/clear for VERIFY, `DE` = expected length,
`IX` = destination, carry set on return = success), and checksum algorithm
(XOR of every byte in the block, flag and trailing checksum both included,
must total zero) -- the Sinclair ZX Spectrum ROM disassembly, "0556: the
'LD-BYTES' subroutine" <https://skoolkid.github.io/rom/asm/0556.html>.

## Kempston joystick (`crates/zx-spectrum/src/bus.rs`)

Port `0x1F`'s bit layout (`000FUDLR`, active high) --
<https://chuntey.wordpress.com/2010/01/06/using-kempston-joystick-in-your-own-basic-programs/>.
The address decode ("usually only A5 [bit 5] is checked") is commonly
documented across independent Spectrum hardware references, same
epistemic tier as this file's other commonly-documented (not
disassembly-verified) port decodes.

## Sinclair/Cursor joystick key mapping (`crates/zx-spectrum/src/lib.rs`, `apps/retroclawd/src/any_machine.rs`)

World of Spectrum's own peripherals reference,
<https://worldofspectrum.org/faq/reference/peripherals.htm>, fetched
directly this session -- quoted verbatim: Sinclair 1 "maps the joystick
directions and the fire button to the 1 (left), 2 (right), 3 (down), 4
(up) and 5 (fire) keys" (readable via port `0xf7fe`); Sinclair 2 "maps to
keys 6 (left), 7 (right), 8 (down), 9 (up) and 0 (fire)" (port `0xeffe`);
Cursor/Protek/AGF "maps to keys 5 (left), 6 (down), 7 (up), 8 (right) and
0 (fire)". Confirmed real: both ports are exactly the keyboard-matrix rows
`sinclair_keyboard`/`ZxSpectrumKey` already decode for those keys, so no
new bus/port code was needed at all -- only a host-input mapping
(`any_machine::keyboard_joystick_key`) choosing which real key arrow-keys
+Space synthesize.

## Fuller joystick (`crates/zx-spectrum/src/bus.rs`)

Same World of Spectrum peripherals reference as above, quoted verbatim:
"Results were obtained by reading from port 0x7f in the form F---RLDU,
with active bits low." Bit 7 = Fire, bits 3/2/1/0 = Right/Left/Down/Up,
bits 6-4 unused. Unlike Sinclair/Cursor, this genuinely is a separate,
real I/O port -- not a keyboard-matrix key. Address-decode precision
beyond the literal port `0x7f` (whether real hardware mirrors across
other addresses, the way Kempston's own "usually only A5" is documented
to) wasn't found in any source checked -- `read_io` matches only the
literal low byte, the conservative reading of what's actually
documented, not a guessed wider decode.

## Kempston mouse (`crates/zx-spectrum/src/bus.rs`)

Port table (X `0xfbdf`, Y `0xffdf`, buttons `0xfadf`) and full bit-mask
decode fetched directly from `sinclair.wiki.zxnet.co.uk`'s own Kempston
Mouse page, quoted from the fetch: X "xxxx x011 xx0x xxxx", Y "xxxx x111
xx0x xxxx", buttons "xxxx xx10 xx0x xxxx" ('x' = don't-care bit -- bit 5
of the low byte is a genuine, real don't-care-excluding `0` in all three,
not an assumption by analogy with Kempston's own joystick decode). Button
assignment from the same page: "D0:Right D1:Left D2:Middle". Active-low
polarity confirmed via a separate search converging on the same fact from
an independent source (a MiSTer FPGA core PR description) -- worth noting
an EARLIER search result had gotten Left/Right backwards (bit0=Left in
that one source), corrected by fetching the more authoritative wiki table
directly rather than trusting the first search summary. Wheel/middle-
button behavior ("wheel-delta value (0..15) since previous read" in the
button port's top nibble) also from the same page.

## `.p`/`.p81` snapshot format (`crates/zx81-snapshot`, `zx81::Zx81::load_p_snapshot`)

Reference implementation: **ZEsarUX** (ZX Second-Emulator and Released for
UniX) by Cesar Hernandez Bano, <https://github.com/chernandezba/zesarux>,
licensed under the GNU General Public License v3. Specifically
`src/snap/snap_zx8081.c` (`new_load_zx81_p_snapshot_in_mem`,
`new_load_zx81_set_common_registers`) -- the source used to understand the
real `.p`/`.p81` file layout (raw memory from `0x4009` onward, `.p81`'s
ZX81-character-encoded name header terminated by a high-bit byte) and the
fixed system-variable header, stack patch, and CPU register constants a
real ZX81 synthesizes around a loaded snapshot so `BASIC` can resume.

## Turbosound / second AY-3-8910 (`crates/machines/zx-spectrum/src/bus.rs`, 2026-09-20)

The real chip-select protocol (a write to the AY register-select port
where `val & 0x9C == 0x9C` is a chip-select command rather than a
register select, with `val & 3` picking which chip: `3`=primary,
`2`=second) was NOT guessed from the original plan's earlier web-search
summary ("writing 0xFE selects the second chip") -- that summary named
the right value but not the real mask/logic behind it, so this session
shallow-cloned ZEsarUX's own real source
(`github.com/chernandezba/zesarux`, `GNU GPL`) and read
`src/soundchips/ay38912.c`'s `ay_3_8912_out_port` function directly
(around its `puerto==65533` branch). Only the *protocol facts* extracted
from that reading -- which bits mean what -- went into this project's own
independent Rust implementation; no ZEsarUX code was copied. That
function's own comments (apparently a preserved exchange with whoever
added TBBlue/Turbosound-Next's panning extension) explain the mask
precisely: the classic 2-chip Turbosound convention is `val ==
0b111111XX` (bits 7-2 all set, bits 1:0 = chip select), and the wider
`0x9C` mask this project also uses is what additionally accepts
TBBlue's panning-extended form (which repurposes bits 6:5 for
left/right pan) as a chip-select write too, rather than a narrower
"only literal 0xFF/0xFE" check. This project only implements the
classic 2-chip form (no panning, no third chip) -- a `val & 3` of `0`
or `1` (SID / third-chip-select on real Turbosound Next hardware) is
still consumed as a chip-select attempt rather than falling through to
register-select, matching ZEsarUX's own behavior when asked to select a
chip beyond how many are configured (it just leaves the active chip
unchanged), not treated as an error.

## `.szx` (ZX-State) snapshot format (`crates/formats/szx-snapshot`, 2026-09-20)

File header shape (`"ZXST"` signature + major/minor version bytes +
machine id + flags), the 8-byte chunk header (4-char ASCII id + `u32`
little-endian length, unrecognized ids safely skippable by that length),
the `Z80R`/`SPCR`/`RAMP`/`AY\0\0` block layouts and every byte offset
within them, the real machine-id numbering (`0`=16K, `1`=48K, `2`=128K,
`3`=+2, `4`=+2A, `5`=+3, `6`=+3E, ...), and the `RAMP` page-number
convention (`5`/`2`/`0` for 48K's three blocks at `0x4000`/`0x8000`/
`0xC000`; a direct, unoffset bank index `0`-`7` for 128K-family) are all
from **`libspectrum`**'s own real implementation
(`github.com/fuse-emulator/libspectrum`, file `szx.c`), shallow-cloned
and read directly this session -- not a doc summary. An earlier planning
pass had done web-search-based research into this format and come away
with a machine-id table off by one (assumed `1`=48K/`2`=128K/`3`=+2/
`4`=+2A, when the real numbering reserves `0` for 16K, shifting
everything else up by one) and an internally-inconsistent claim about
whether the chunk header was 4 or 8 bytes; reading the real source
settled both. `libspectrum` is the de facto reference implementation
every cited emulator here (Fuse itself, and the format Spectral/ZEsarUX
both also read/write) is built on or interoperates with.

Verified against `libspectrum`'s own real test fixtures too, not just
this crate's own hand-built ones: `test/random.szx` (a genuine
Fuse-written version-1.5 file, including a real zlib-compressed `RAMP`
page) and the minimal single-chunk fixtures under `test/szx-chunks/`
(`Z80R.szx`/`SPCR.szx`/`RAMP.szx`) all load cleanly through this
project's own `--snapshot`/`--dump-screen`.

