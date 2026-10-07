# Amiga software

Drivers, library and tools for the PicoZorro card, plain 68000 code for
AmigaOS 2.04 (V37) and up.

| Component | What it is |
|---|---|
| `picozorro.device` | SANA-II network driver for the card's Ethernet, for Roadshow, AmiTCP and other SANA-II stacks. `s2test` checks it command by command without a network (`ENV:PZNET` = `loop`). |
| `picozorrousb.device` | USB host-controller driver for Poseidon (legacy `IOUsbHWReq` interface): a root hub with one port, control, bulk and interrupt transfers. `usbtest` enumerates a device without Poseidon; `stortest` reads (and optionally writes) a USB stick through Poseidon's `usbscsi.device`. |
| `mpega.library` | Drop-in `mpega.library` 2.x: MP3 / MP2 decoded on the card, the PCM returned as the original library returns it, for players that use it. `mpegtest` drives it as a player does. |
| `pzflash` | Firmware update from the Amiga, see `docs/UPDATE.md`. |
| `bootrom` | The card's boot ROM (`boot.bin`, 256 bytes at most): Kickstart runs it at boot and it loads the drivers and the library from the card, so they need no files on disk. `tools/mkboot.py` puts it and the modules into the firmware's boot image (`make boot` at the top). |
| `pztest` | Finds the card and checks its scratch register with word and byte accesses. |
| `pztools/pzbench` | Zorro II bandwidth to the card's register window, per access pattern. |
| `pztools/pzload` | CPU load meter (window, optional log file). |
| `pztools/memdump` | Copies a range of the address space to a file (expansion ROMs, the card's registers). |
| `pztools/devinfo` | Mount parameters of every filesystem device. |
| `pztools/memcheck` | Walks exec's free memory lists and task lists without validating calls, for tracking down memory corruption. |

`common/` holds what the drivers share: `compiler.h` (register arguments)
and the developer links below.

## Building

The tree builds with bebbo's m68k-amigaos-gcc and vasm from a docker image
pinned by digest (`common/gcc.mk`). Once:

    make toolchain

pulls the image and fetches into `.toolchain/` what it does not carry: the
NDK 3.2 R4 from Aminet (`http://aminet.net/dev/misc/NDK3.2.lha`, for the
SANA-II and Roadshow headers; needs `lha`) and Poseidon's headers from
`github.com/rondoval/poseidon-backport` at a pinned commit. Then:

    make            # everything
    make clean

A local bebbo install works too: `make AMIGA_GCC=m68k-amigaos-gcc
AMIGA_VASM=vasmm68k_mot`. `make DEBUG=1` in a driver's directory builds it
with log lines on the serial port. `make PROF=1` in `picozorrousb.device`
builds the driver with E-clock timestamps around every interrupt IN report;
`pzusbprof` (built there too) prints them, `pzusbprof reset` zeroes them.

## Installing

With the card's boot ROM on (the default with a boot image in the
firmware), Kickstart loads `picozorro.device`, `picozorrousb.device` and
`mpega.library` from the card at boot; `pzflash BOOTROM ON|OFF` switches it
from the next reset. Installed from disk instead, or to run a newer build:

- `picozorro.device` to `DEVS:Networks/`. Roadshow: an interface file such
  as `DEVS:NetInterfaces/PicoZorro` with
  `device=picozorro.device`, `unit=0`, `configure=dhcp`. Other stacks take
  the same device name and unit 0.
- `picozorrousb.device` to `DEVS:USBHardware/`; select it, unit 0, in
  Poseidon's Trident prefs. `usbtest` opens it from `DEVS:`.
- `mpega.library` to `LIBS:`.
- `pzflash`, `pztest` and the tools anywhere in the path.

The drivers look for the card through expansion.library (manufacturer
2011, product $5A) and take its interrupt on /INT2. With the card's
interrupt on /INT6, set `ENV:PZNET` and `ENV:PZUSB` to `pz 6`
(`SetEnv SAVE PZNET "pz 6"`). `ENV:PZUSB` also takes `minpoll=N` after the
backend (`pz minpoll=4`): the floor for the poll interval of interrupt
endpoints in ms, default 10, 0 for none.

## Developer backends

Besides the card on the Zorro bus (backend `pz`), the drivers can reach the
card's registers, or a stand-in, over other links: `bp` (the register
window over a serial line, `common/bpclient.c` and `common/serlink.c`, to a
module running `picozorro` with the `uart-backplane` feature), `uae` (a
lower SANA-II device under FS-UAE for picozorro.device, a UDP tunnel for
picozorrousb.device), `ser` (picozorrousb.device's tunnel over a serial
line) and `loop` (picozorro.device, frames come straight back). They are
selected with `ENV:PZNET`, `ENV:PZUSB`, `ENV:PZMPEGA` and `pzflash BP`, and
let the drivers be developed against FS-UAE or a bench PC; the test bed
itself is not published yet.
