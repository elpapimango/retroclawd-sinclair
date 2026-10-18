#!/usr/bin/env bash
# Builds the media the scenario manifests load, with retroclawd-tools, into software/scenarios/ (not committed).
#   scenarios/build.sh [path to the retroclawd-tools target dir]   default ../../../retroclawd-tools/target/debug
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tools="${1:-$here/../../../../retroclawd-tools/target/debug}"
out="$here/../software/scenarios"
mkdir -p "$out"
for ext in tap sna z80 szx; do
  "$tools/rcld" build "$here/spectrum/hello.toml" -o "$out/spectrum-hello.$ext" --force
done
python3 "$here/../../../../retroclawd-tools/scripts/make_zx81_p.py" "$here/../software/zx81/hello.p"
for name in beep ay; do
  for ext in tap sna; do
    [ "$name$ext" = "aysna" ] && continue
    "$tools/rcld" build "$here/spectrum/$name.toml" -o "$out/spectrum-$name.$ext" --force
  done
done
