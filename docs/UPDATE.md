# Firmware updates from the Amiga

The card's firmware is updated from the Amiga, with no cable to a PC: the
module's flash holds two image slots (A/B), `pzflash` writes the new image
into the slot that is not running, and the RP2350's bootrom runs it on trial
until `pzflash` confirms it. An image that is not confirmed is dropped by
the bootrom, and the card boots the old one again.

## In short

On the PC (firmware toolchain, see `firmware/README.md`):

    make fw-pzf                  # firmware/picozorro.pzf

On the Amiga, with the `.pzf` copied over:

    pzflash INFO                 # running version, slot, trial state
    pzflash UPDATE picozorro.pzf # FLASH, ACTIVATE, CONFIRM in one go

`pzflash` is built in `amiga/pzflash` (`make -C amiga`). The module must
have been installed partitioned once (below); `pzflash INFO` says "not
partitioned (no updates)" otherwise.

## How it works

- Flash: an A/B pair of 2 MiB partitions (`firmware/pt.json`: A at 64 KiB,
  B at 2112 KiB), the partition table in sector 0 (made by `picotool
  partition create`). The bootrom maps the booted partition to 0x10000000
  (RP2350 datasheet 5.1.19), so one image runs from either slot;
  `firmware/pz-app/memory.x` limits the image to 2 MiB.
- The module runs from one slot and writes the update into the other,
  with the bootrom's low-level flash functions (storage offsets,
  datasheet 5.4.8.10/11) from a RAM function, interrupts off. The first
  sector (the IMAGE_DEF) is written last, after the rest is in flash and
  the stream's CRC-32 matched; then the whole image is read back through
  the untranslated XIP window (0x1c000000, datasheet 4.4) and its CRC-32
  compared. A partial image never carries a block the bootrom would pick.
- ACTIVATE: `reboot(REBOOT_TYPE_FLASH_UPDATE, .., XIP_BASE + start of the
  new slot)` (datasheet 5.4.8.24; the pico-examples OTA example passes
  the address the same way). The new image carries the try-before-you-buy
  flag (set by `tools/mkpzf.py` in its IMAGE_TYPE item, picobin.h
  `PICOBIN_IMAGE_TYPE_EXE_TBYB_BITS`), so the bootrom runs it on trial
  under the watchdog (datasheet 5.1.17).
- On trial the image says so (STATUS.BUY_PENDING, from `get_sys_info`
  BOOT_INFO, pico-sdk `BOOT_TBYB_AND_UPDATE_FLAG_BUY_PENDING`) and keeps
  the watchdog fed for 120 s. CONFIRM calls `explicit_buy` (datasheet
  5.4.8.4): the bootrom clears the flag and erases the first sector of the
  old slot, so the new image stays. No CONFIRM: the watchdog fires, the
  bootrom prefers the non-TBYB old image, and the module is back where it
  was.
- Across the update reboot the new image keeps the Autoconfig base: the
  old one leaves it in watchdog SCRATCH0/1 (the bootrom uses SCRATCH2-7
  for reboot parameters), the new one starts configured there, so the
  Amiga can talk to it without a reset. The bus build does this only when
  /BUSRST is high at boot and the stash is there; any other boot waits
  for /BUSRST. If the Amiga cannot reach the card after the reboot,
  reboot the Amiga for a fresh Autoconfig.

## First install and recovery

The A/B layout is written once from a PC. Without a probe, as a UF2 file:

    make fw-uf2                  # firmware/picozorro-install.uf2

Hold BOOTSEL while plugging the module's USB into the PC: the RP2350's ROM
bootloader shows up as a USB drive. Copy `picozorro-install.uf2` onto it.
The file writes the partition table at flash offset 0, the image (TBYB
off) into partition A and erases the first sector of B. Its blocks carry
the UF2 family `absolute`, which the bootrom writes at the addresses in the
file whatever the flash held before (datasheet 5.1.18, 5.5.3;
`firmware/pt.json` allows `absolute` downloads in the unpartitioned space,
so this also works on a module that is already partitioned). The bootrom
reboots when the copy is complete and the module starts from A. After that
every update goes through `pzflash`.

With an SWD probe, `make fw-install` (`tools/pz_install.sh`, probe-rs)
writes the same three pieces. The SWD route is the one in use; the UF2
route has not been exercised on a module yet.

`probe-rs run <elf>` (`make fw-run`) and `picotool load <elf>` (`make
fw-load`) write a flat image at offset 0 and so remove the partition table;
the install UF2 or `make fw-install` put table and image back.

A plain UF2 (`make fw-uf2 BIN=hello`, family `rp2350-arm-s`) dropped on a
partitioned module does not land at offset 0: the bootrom stores it in the
A/B partition that would not be chosen at the next boot (datasheet 5.5.3,
5.5.3.1), boots it with a flash update boot, and, as the image carries no
TBYB flag, erases the first sector of the other partition (datasheet
5.1.16). The partition table stays and the dropped image replaces the
running one; a `picozorro` image dropped that way still takes updates from
`pzflash`.

Recovery when no image runs: the RP2350's ROM bootloader is always there.
Hold BOOTSEL while the module powers up and drop `picozorro-install.uf2`
again (or `make fw-install` over SWD). RP2350 chips of stepping A2 (`hello`
prints the stepping) have erratum RP2350-E10: their bootrom does not set up
the flash before it reads the partition table, and UF2 drops onto a
partitioned module can fail. A module without a partition table (new, or
flashed flat) is not affected; on an A2 a partitioned module is reinstalled
over SWD.

## Registers ($C0-$FE of the A16 = 1 window)

Model: `pz_core::update` (host tests). General rules as `docs/REGISTERS.md`
(16-bit, big-endian, byte access refused).

| Offset | Name | Access | Meaning |
|---|---|---|---|
| $C0 | UPD_CTRL | W | command: 1 BEGIN, 2 ABORT, 3 COMMIT, 4 ACTIVATE, 5 CONFIRM, 6 BOOTROM_OFF, 7 BOOTROM_ON |
| $C2 | UPD_STATUS | R | state and flags, below |
| $C4, $C6 | UPD_SIZE | RW | image length in bytes, high word first; set before BEGIN |
| $C8, $CA | UPD_CRC | RW | CRC-32 (zlib) of the image; set before BEGIN / COMMIT |
| $CC | UPD_DATA | W | data port, word k = bytes 2k, 2k+1; an odd last byte on D15-D8 |
| $D0, $D2 | UPD_DONE | R | bytes in flash so far |
| $D4, $D6 | UPD_RESULT | R | CRC-32 of the image as read back from flash |
| $D8 | UPD_SLOT_KB | R | size of a slot in KiB (0: not partitioned) |
| $DA | UPD_ERROR | R | error code when the state is error |
| $E0-$EE | FW_VERSION | R | running firmware, 16 ASCII bytes, NUL-padded |

UPD_STATUS: bits 3-0 state (0 idle, 1 receiving, 2 verifying, 3 ready,
4 rebooting, 5 error); bit 4 SPACE (the data port takes the next 4 KiB
sector); bit 5 BUSY (a command is being carried out: wait until clear);
bit 6 REFUSED (a write was refused since the last BEGIN); bit 8 SLOT_B
(running from B); bit 9 BUY_PENDING (on trial: CONFIRM keeps this image);
bit 10 PARTITIONED (an A/B pair exists: updates possible); bit 11 BOOTROM
(the card's boot ROM is on).

UPD_ERROR: 1 not partitioned, 2 bad size, 3 flash (read-back CRC wrong),
4 CRC mismatch (stream), 5 sequence (command not allowed now), 6 not an
image (no IMAGE_DEF in the first sector), 7 confirm failed, 8 short.

Sequence (what `pzflash` does): SIZE, CRC, BEGIN, wait !BUSY; per 4 KiB
sector: wait SPACE, write the sector's words to DATA; COMMIT, wait !BUSY,
state ready and RESULT = CRC; ACTIVATE, wait for the module to answer
again (FW_VERSION changes, BUY_PENDING set); CONFIRM, wait !BUSY.

SPACE is lock-step on purpose: the next sector only after the last one is
in flash. While the module erases and programs, core 0 has its interrupts
off; over the UART backplane the module's UART loses bytes then, so only
idempotent STATUS polls may fall into that window (they are retried). On
the bus it costs nothing measurable.

## Files and tools

- `.pzf` (`tools/mkpzf.py`): 64-byte header (magic `PZF1`, length, CRC-32,
  flags, the version string found after the image's `PZVERSN:` tag), then
  the image with the TBYB flag set. `make fw-pzf` builds one.
- `pzflash` (Amiga, `amiga/pzflash`): `pzflash [BP [baud [unit [device]]]]
  INFO | FLASH <file> | ACTIVATE | CONFIRM | UPDATE <file> | BOOTROM
  ON|OFF`. Without BP it uses the card on the bus (FindConfigDev, board
  base + $10000); with BP the same registers over the UART backplane. It
  checks the file's CRC before touching the module. BOOTROM switches the
  card's boot ROM, from the next Amiga reset.
- Version string: `PZ_VERSION` at build time, else `<crate version>-<git
  short hash>` (`firmware/pz-app/build.rs`).

## Core 1 and flash

During an erase or program XIP is off, and a flash access from core 1
bus-faults (datasheet 5.4.8.9). embassy-rp's `Flash` avoids that by
pausing core 1 (`flash.rs` `in_ram` -> `pause_core1`), which would stop the
bus slave; so the update path uses the bootrom functions itself and core 1
keeps running. For that, core 1's bus loop must stay in RAM:

- pz-core's bus-path functions and the Autoconfig ROM table are placed in
  `.data.zbus` on the target (`cfg_attr(... link_section)`), so the
  configured fast path stays off XIP even where the inliner keeps a
  function out of line.
- The reset path (Amiga reset or Autoconfig shut-up during an update)
  takes a handshake (`zbus::flash_exclusive` on core 0, `enter_flash_path`
  on core 1): core 1 waits while a flash write is on. The update task
  handles a pending command (an Amiga reset turns into ABORT) before it
  starts another sector.

## Status

The update path is tested over the UART backplane, from a PC and from
FS-UAE. Not verified on the Zorro bus yet: `pzflash` without BP, core 1
serving the bus while core 0 writes flash, an Amiga reset during an
update, and the new image resuming at the stashed base after the update
reboot. Signed images and rollback versions in OTP are not used.
