# Tools

PC-side helpers for building and installing PicoZorro (Python 3, POSIX sh).

| Tool | What it does |
|---|---|
| `mkpzf.py` | Makes a firmware update file (`.pzf`) from the firmware ELF, for `pzflash` on the Amiga (`make fw-pzf`, `docs/UPDATE.md`). With `--bin --no-tbyb`, the plain image for an SWD install; with `--uf2 pt.bin`, the install UF2 for a drop on the module in BOOTSEL mode (`make fw-uf2`). Needs `rust-objcopy` (cargo-binutils). |
| `mkboot.py` | Builds the firmware's boot image from the boot ROM (`amiga/bootrom/boot.bin`) and the Amiga drivers it loads at boot (`make boot`). |
| `pz_install.sh` | Installs the firmware with the A/B partition table, so it can be updated from the Amiga afterwards (`make fw-install`), over SWD with `probe-rs`. Needs `picotool` for the partition table. Without a probe, `make fw-uf2` writes the same content as a UF2 file. |
