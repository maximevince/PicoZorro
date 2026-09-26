#!/bin/sh
# Install a picozorro image partitioned over SWD (probe-rs, docs/UPDATE.md):
# the A/B partition table of firmware/pt.json at flash offset 0, the image
# (TBYB off) in partition A, the first sector of B erased; then reset. After
# this the module can be updated from the Amiga (pzflash).
#   tools/pz_install.sh <firmware.elf>
# Without a probe the same content goes on as a UF2 file
# (`make fw-uf2`, firmware/picozorro-install.uf2, dropped on the module in
# BOOTSEL mode). `probe-rs run <elf>` or `picotool load <elf>` write a flat
# image at offset 0 and remove the table again.
set -e
ELF=${1:?usage: pz_install.sh <firmware.elf>}
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
picotool partition create "$HERE/../firmware/pt.json" "$T/pt.bin"
python3 "$HERE/mkpzf.py" --bin --no-tbyb "$ELF" "$T/a.bin"
head -c 4096 /dev/zero | tr '\0' '\377' > "$T/erased.bin"
# Partition A at 64 KiB, B at 64 + 2048 KiB (firmware/pt.json).
probe-rs download --chip RP235x --binary-format bin --base-address 0x10000000 "$T/pt.bin"
probe-rs download --chip RP235x --binary-format bin --base-address 0x10010000 "$T/a.bin"
probe-rs download --chip RP235x --binary-format bin --base-address 0x10210000 "$T/erased.bin"
probe-rs reset --chip RP235x
echo "pz_install: partition table + $ELF in A"
