# PicoZorro register window, version 3 (network)

The contract between the RP2350 firmware (`firmware/pz-core`) and the
Amiga driver (`picozorro.device`). Implemented and host-tested in
`pz_core::nic` / `pz_core::window` (`make fw-test`).

## General

- The window is 256 bytes at the board base assigned by Autoconfig
  (manufacturer 2011, product $5A). The board is 128 KiB (er_Type $C2):
  this window at A16 = 0, the USB / MPEG / update window
  (`docs/REGISTERS-USB.md`) at A16 = 1. A8-A15 are not decoded, so each
  window repeats every 256 bytes across its 64 KiB. Amiga code uses only
  base + $00..$FF and base + $10000..$100FF.
- All registers are 16 bits at even offsets, big-endian as the 68000 sees
  them. Access them with `move.w`. A 68020+ `move.l` to Zorro II space is
  split into two word cycles, high word (lower address) first; that is
  allowed everywhere and is what the order rules below assume.
- **Strobes**: the card looks at /UDS only; /LDS has no effect on any
  board (the One TH does not wire it). /UDS low is a word access, so an
  even-byte access counts as a word access; /UDS high is an odd-byte
  access. No PicoZorro driver issues even-byte accesses; Autoconfig writes
  are /UDS-only.
- **Odd-byte access** on the network registers ($06 and $10-$7E) is an
  error: the write is ignored, the read returns the word without side
  effects, and both set BUS_ERROR. SCRATCH takes an odd-byte write into
  its low byte; an even-byte write to it writes the whole word.
- Unused offsets and write-only registers read $FFFF; writes to unused
  offsets are ignored.
- Data port byte order: word k of a frame carries byte 2k on D15-D8 and
  byte 2k+1 on D7-D0, which is memory order for `move.w (port),(a0)+`. On
  an odd length the last word carries the last byte on D15-D8; its low
  byte is ignored on TX and reads 0 on RX.

## Map

| Offset | Name | Access | Meaning |
|---|---|---|---|
| $00 | MAGIC | RO | $505A ("PZ") |
| $02 | VERSION | RO | $0003: version 2 plus the data alias at $C0-$FE (version 2 reads $0002, version 0 $0001) |
| $04 | SCRATCH | RW | free for tests, byte access allowed (see Strobes) |
| $06 | INT | R, W1C | interrupt status, bits below |
| $08, $0A | BOOT_US | RO | cold start to "slave armed", us, 32 bit |
| $0C, $0E | CYCLES | RO | bus cycles served, 32 bit |
| $10 | INT_ENABLE | RW | same bit layout as INT |
| $12 | CTRL | RW | control, bits below |
| $14 | STATUS | RO | status, bits below |
| $16, $18, $1A | MAC | RO | station address, bytes 0-1, 2-3, 4-5 |
| $20 | TX_LEN | WO | open a TX frame of this many bytes (14..1514, no FCS) |
| $22 | TX_DATA | WO | TX data port |
| $24 | TX_COMMIT | WO | any value: frame complete |
| $30 | RX_LEN | RO | length of the head RX frame, 0 = queue empty |
| $32 | RX_DATA | RO | RX data port |
| $34 | RX_DONE | WO | any value: drop the head RX frame |
| $40-$56 | STATS | RO | six 32-bit counters, table below |
| $60-$76 | MCAST | RW | four multicast addresses, three words each |
| $78 | MCAST_VALID | RW | bits 3-0: entry n is valid (entry n = $60 + 6n) |
| $80 | BOOT_CTRL | WO | 1 = start the boot stream, see Boot ROM and boot stream |
| $82, $84 | BOOT_ADDR | WO | address of the hunk just allocated, 32 bit |
| $86 | BOOT_STAT | RO | bit 15 BUSY: no stream word published yet |
| $88, $8A | BOOT_DATA | RO | the boot stream, one word per read |
| $C0-$FE | DATA | RW | data port alias: a word read anywhere here is an RX_DATA read, a word write a TX_DATA write (VERSION $0003 and later) |

### INT ($06) and INT_ENABLE ($10)

| Bit | Name | Kind | Set when |
|---|---|---|---|
| 0 | RX_AVAIL | level | the RX queue is not empty; writing 1 has no effect |
| 1 | TX_DONE | latched | a committed frame was handed to the NIC (a TX slot came free) |
| 2 | LINK_CHANGE | latched | LINK_UP changed |
| 3 | RX_OVERRUN | latched | a received frame was dropped because the RX queue was full |
| 4 | BUS_ERROR | latched | a rule of this contract was broken (list below) |

Latched bits clear when the driver writes 1 to them. /INT is asserted while
the board is configured and (INT & INT_ENABLE) != 0. The line is level
triggered and shared (/INT2 or /INT6, jumper).

Intended driver sequence: the interrupt server reads INT; if (INT &
enabled) is 0 it returns 0 in D0 (Z set) at once so the next server runs.
Otherwise it clears RX_AVAIL in INT_ENABLE if set, writes the latched bits
it saw back to INT, signals the driver task and returns non-zero. The task
reads frames until RX_LEN is 0, then sets RX_AVAIL in INT_ENABLE again; a
frame that arrived meanwhile raises /INT at once, so none is missed.

### CTRL ($12)

| Bit | Name | Meaning |
|---|---|---|
| 0 | ONLINE | 1: frames are received and sent. 0: RX frames are discarded (not counted), TX_COMMIT drops the frame and counts tx_errors |
| 1 | PROMISC | receive every frame |
| 2 | MULTICAST_ALL | receive every multicast frame |
| 14 | CLEAR_STATS | write 1: all STATS to 0; reads 0 |
| 15 | RESET_FIFOS | write 1: empty both queues, abort an open TX frame; reads 0 |

### STATUS ($14)

| Bits | Name | Meaning |
|---|---|---|
| 15 | LINK_UP | the PHY reports link |
| 14 | SPEED_100 | 100 Mbit/s (else 10) |
| 13 | FULL_DUPLEX | |
| 11-8 | TX_FREE | free TX slots, 0..4 |
| 7-0 | RX_QUEUED | frames in the RX queue, 0..16 |

## Sending a frame

1. Wait for TX_FREE > 0 (STATUS), or for TX_DONE.
2. Write the length to TX_LEN. Out of range (below 14, above 1514) or no
   free slot: BUS_ERROR, nothing opens.
3. Write ceil(len / 2) words to TX_DATA. Extra words are ignored and set
   BUS_ERROR.
4. Write TX_COMMIT. The firmware pads frames shorter than 60 bytes with
   zeros, and the NIC appends the FCS.

A commit with fewer words written than announced drops the frame and
counts tx_errors. TX_LEN while a frame is open drops the open one and
counts tx_errors. TX_DATA or TX_COMMIT without an open frame sets BUS_ERROR.

## Data port alias ($C0-$FE)

From VERSION $0003. Every word offset in $C0-$FE is the data port: a read
behaves exactly as an RX_DATA read, a write exactly as a TX_DATA write
(same frame bookkeeping, same BUS_ERROR rules, byte access is an error).
The alias exists so the driver can move a frame with long and multiple
moves, which only work on ascending addresses:

    movem.l (a1)+,d0-d7      ; 32 bytes from the frame in memory
    movem.l d0-d7,$C0(a0)    ; 16 word writes = 16 TX_DATA writes

    movem.l $C0(a0),d0-d7    ; 16 RX_DATA reads
    movem.l d0-d7,(a1)       ; 32 bytes into memory

A 68020+ splits each long into two word cycles, high word first, so the
word order on the bus is memory order. Measured on a TF536 (68030), a
`movem.l` burst runs back-to-back bus cycles; a word-at-a-time copy from
memory leaves a longer gap after each cycle. Frames are sent and
received in whole words: ceil(len / 2) words, the remainder through
RX_DATA / TX_DATA or the alias, as convenient.

## Receiving a frame

1. Read RX_LEN. 0 means the queue is empty.
2. Read ceil(len / 2) words from RX_DATA. Reads past the end, or with an
   empty queue, return $0000.
3. Write RX_DONE. It may come before all words were read; the rest of the
   frame is skipped. With an empty queue it does nothing.

RX_LEN does not change until RX_DONE. The queue holds 16 frames of up to
1514 bytes; when full, the newest frame is dropped (rx_overrun,
RX_OVERRUN).

Frames delivered: to the station address, broadcast, multicast whose
destination is a valid MCAST entry, every multicast with MULTICAST_ALL,
everything with PROMISC. Filtering is done in the firmware: the W5500's own
MAC filter (Sn_MR.MFEN) blocks all multicast, so the firmware turns it off
whenever PROMISC, MULTICAST_ALL or any MCAST entry is set.

The firmware reads the MCAST words while frames arrive, so the driver
changes an entry only while its MCAST_VALID bit is clear: clear the bit,
write the three words, set the bit.

## STATS ($40-$56)

| Offset | Counter |
|---|---|
| $40 | rx_ok: frames put into the RX queue |
| $44 | tx_ok: frames handed to the NIC |
| $48 | rx_dropped: frames the NIC delivered but the firmware could not take (too long, internal error) |
| $4C | tx_errors: frames dropped on the TX side (short commit, offline, NIC error) |
| $50 | rx_crc: CRC errors, if the NIC reports them (the W5500 datasheet shows no such counter: stays 0 there) |
| $54 | rx_overrun: frames dropped because the RX queue was full |

Each counter is high word at the lower offset. Reading the high word
snapshots the whole counter; the next read of its low word returns the
snapshot's low half, so a `move.l` never tears.

## Boot ROM and boot stream

With a boot ROM, Kickstart loads `picozorro.device`, `picozorrousb.device`
and `mpega.library` from the card at boot. Model: `pz_core::boot`, routed
by `pz_core::window`; stream side: `firmware/pz-app/src/boot_task.rs`;
68k side: `amiga/bootrom/boot.s`; image: `tools/mkboot.py` (`make boot`
writes `firmware/boot.img`, which the firmware embeds at build time,
`PZ_BOOT_IMG` overrides the path). A build without the image has no boot
ROM.

### Autoconfig and ROM mode

The boot ROM is offered when the image has one and it is not switched off
(below). The Autoconfig ROM then reads er_Type $D2 (DIAGVALID, bit 4, on
top of $C2) and er_InitDiagVec $0100; otherwise $C2 and $0000. The
firmware decides at start and again at every /BUSRST, never in between.
$0100 from the board base is window offset 0 through the 256-byte alias.

When the board is configured with the boot ROM offered, it is in **ROM
mode**:

- A read anywhere in the A16 = 0 window returns the ROM word at its
  offset (address & $FF), the whole word whatever the strobes, so a byte
  read finds its byte on the right lane. The registers, $80-$8A included,
  cannot be read. Reads at A16 = 1 are not affected.
- ROM mode ends with the read of the ROM's last word (or of any offset past
  it), with any write to the board in either window, and at /BUSRST or
  shut-up. It comes back at the next configuration.

Kickstart reads the start of the ROM three times, then copies the whole
ROM in ascending order, so its copy ends ROM mode. The drivers write
SCRATCH before they read MAGIC, so a ROM that was not copied does not hide
the registers from them.

### The boot ROM

`amiga/bootrom/boot.s`, at most 256 bytes (the window), an even length,
da_Size equal to that length (`mkboot.py` checks all three). It holds a
DiagArea (da_Config $90 = DAC_WORDWIDE | DAC_CONFIGTIME), a romtag, the
DiagPoint, the loader and the name "PicoZorro boot".

1. Kickstart copies da_Size bytes to RAM and calls DiagPoint (A0 = board
   base, A2 = the copy). DiagPoint adds the copy's address to rt_MatchTag,
   rt_EndSkip, rt_Name, rt_IdString and rt_Init, stores the board base in
   the copy and returns 1, so Kickstart keeps the copy. da_BootPoint points
   at the same `moveq #1,d0 / rts`; Kickstart wants it non-zero.
2. At romboot time Kickstart finds the romtag in the copy and
   InitResidents it. rt_Flags is 0, so InitResident calls rt_Init, the
   loader, with A6 = ExecBase.
3. The loader writes 1 to BOOT_CTRL, waits for BOOT_STAT not BUSY and
   checks the "PB" word. Per module it reads the hunk count; per hunk the
   size and MEMF flags, calls AllocMem and writes the result to BOOT_ADDR
   (`move.l`: $82, then $84). It waits for not BUSY again, then per hunk
   reads the address and the word count and copies that many words from
   BOOT_DATA to the address. It calls CacheClearU (exec V37 and up), reads
   the romtag address and calls InitResident(romtag, 0). A hunk count of 0
   ends the list; rt_Init returns 0.

The 68k side neither parses hunks nor relocates: the firmware does both.
There is no MakeResident: each module's own romtag is initialised with
InitResident, as Kickstart would a resident. The loader gives up on a
timeout (400000 polls of BOOT_STAT), a missing "PB" or an AllocMem that
returns 0 (written to BOOT_ADDR, then it stops). Modules already
initialised stay; hunks already allocated are not freed.

### Registers ($80-$8A)

Served while the board is configured and not in ROM mode. In the
`picozorro` firmware they are there with or without a boot image; without
one BOOT_STAT stays BUSY.

| Offset | Name | Access | Meaning |
|---|---|---|---|
| $80 | BOOT_CTRL | WO | 1 (START): the stream from word 0, the addresses forgotten, a new session. Other values do nothing (but end ROM mode, as any write) |
| $82 | BOOT_ADDR_HI | WO | high word of the next hunk address, latched |
| $84 | BOOT_ADDR_LO | WO | low word: the latched high word and this are the next hunk address, in allocation order; at most 32 per session, more are ignored |
| $86 | BOOT_STAT | RO | bit 15 BUSY: no word published at the read position; bits 14-0 read 0 |
| $88, $8A | BOOT_DATA | RO | the next stream word; a word read moves on by one word. Two offsets so that `move.l` reads two words |

- Write with /UDS high (odd byte): ignored. Odd-byte read of BOOT_DATA:
  the word, without moving on. Neither sets BUS_ERROR.
- BOOT_DATA while BUSY reads $0000 and does not move on: poll BOOT_STAT
  first. BOOT_CTRL and BOOT_ADDR read $FFFF; writes to $86-$8A are ignored.
- A START at any time begins a new session; the old session's addresses
  and stream no longer count (a second boot starts over).
- The stream comes from a 48 KiB buffer in RAM that the boot task on core
  0 fills; it looks for START and new addresses every 200 us.

### Stream format

Big-endian words:

```text
$5042 ("PB")
per module:
  n                         hunk count
  n x (bytes.l, MEMF.l)     the loader AllocMems each and writes the address
                            -- BUSY until all n addresses are in --
  n x (address.l, words.l, words x the hunk's words)
  romtag.l                  address of the module's romtag
0                           no more modules
```

- The firmware publishes "PB" and the first module's header after START;
  once a module's n addresses are in, its hunks, its romtag address and
  the next module's header (or the final 0) together. BUSY is set in
  between: after START and after the addresses.
- address.l repeats the address the loader wrote. The words are the hunk's
  data, relocated: for each RELOC32 entry (target hunk, offset) the
  firmware adds the target hunk's address to the long at that offset. The
  part of a hunk past its data (BSS) is not sent; every hunk's MEMF has
  MEMF_CLEAR, so AllocMem zeroes it.
- MEMF: MEMF_PUBLIC | MEMF_CLEAR, plus MEMF_CHIP or MEMF_FAST from the
  load file's hunk size bits, or its extended flags.
- romtag.l: the address of the romtag's hunk plus its offset there.
  `mkboot.py` takes the $4AFC whose rt_MatchTag is relocated to itself.
- A bad image (relocation outside its hunk, more than 32 hunks, more than
  48 KiB of stream) ends the session; BOOT_STAT stays BUSY and the loader
  times out.

### Boot image

`tools/mkboot.py -o firmware/boot.img boot.bin <modules>` reads the
modules' AmigaOS load files (code, data and BSS hunks; RELOC32,
RELOC32SHORT / DREL32; symbol and debug hunks are skipped) and writes,
big-endian:

```text
"PZB1", modules.w, rom_len.w, rom (rom_len bytes)
per module: hunks.w, romtag_hunk.w, romtag_offset.l
  per hunk: mem_bytes.l, memf.l, data_bytes.l (even), data,
            relocs.l, relocs x (target_hunk.l, offset.l)
```

It refuses a stream over 48 KiB or more than 32 hunks in all. Modules are
streamed in command-line order; `make boot` passes picozorro.device,
picozorrousb.device, mpega.library.

### Switching the boot ROM off

UPD_CTRL BOOTROM_OFF / BOOTROM_ON in the update registers
(`docs/UPDATE.md`; `pzflash BOOTROM OFF|ON`). The setting is kept in flash
and applies from the next /BUSRST; UPD_STATUS bit 11 shows it. Off, the
board configures without DIAGVALID, there is no ROM mode, and the drivers
come from disk.

## Reset

Amiga reset (/BUSRST) and Autoconfig shut-up empty both queues, abort an
open TX frame and clear INT (latched bits), INT_ENABLE, CTRL (offline),
MCAST and MCAST_VALID. STATS, BOOT_US and CYCLES survive; only a firmware
restart clears them.

## MAC address

`52:5a` followed by the low 32 bits of the RP2350 OTP chip ID
(`pz_core::mac`): locally administered, unicast. MAC reads the same
value in every state; there is no way to change it from the Amiga side
(S2_CONFIGINTERFACE with another address is the driver's business).

## Not verified

- Whether the W5500 pads short TX frames itself; the firmware pads anyway.
- Whether the W5500 reports CRC errors at all.
