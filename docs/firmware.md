# ROM images (Sinclair)

`../firmware.toml` lists the SHA-1 of the dumps each machine option accepts, for `retroclawd --firmware auto` (firmware fetched from a RomM server by hash). It holds hashes only; no ROM is needed for that.

ROM images are copyrighted and are **not** part of this repository. Put your own dumps in a local `roms/` folder
(git-ignored) or let `retroclawd --firmware auto` fetch them by hash. The application never bundles
ROMs; supply your own dumps. The application checks the size of every ROM and refuses a mismatch (a
Spectrum ROM is not silently loaded into a ZX81).

| Machine | Option | File here | Size |
|---|---|---|---|
| `zx81` | `--rom` | `zx81/zx81-550-original.rom` (also `zx81.rom`, `zx81a.rom`, `zx81b.rom`, `zxpand_zx81.rom`) | 8K |
| `zx-spectrum` (48K) | `--rom` | `zx-spectrum/48.rom` (also `spec48.rom`, `spec48-beckman.rom`, `spectrum.rom`, `80-lec.rom`) | 16K |
| `zx-spectrum-128` (128K/+2) | `--rom` | `zx-spectrum/128.rom` | 32K (two 16K ROMs) |
| `zx-spectrum-2a3` (+2A/+3) | `--rom` | supply the 64K ROM | 64K (four 16K ROMs) |

`retroclawd.toml` in the application repository points `zx81`, `zx-spectrum` at these files; the 128K and +2A/+3
entries are commented out / need a path.

```
retroclawd zx81 --rom machines/sinclair/roms/zx81/zx81-550-original.rom
retroclawd zx-spectrum --rom machines/sinclair/roms/zx-spectrum/48.rom
retroclawd zx-spectrum-128 --rom machines/sinclair/roms/zx-spectrum/128.rom
```

Without a window (a ROM never halts on its own, so cap the run): add
`--headless --max-cycles 2000000 --dump-screen screen.png`.
