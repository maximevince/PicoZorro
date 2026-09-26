# PicoZorro USB register window, version 1

The contract between the firmware's USB side and `picozorrousb.device`.
Implemented and host-tested in `pz_core::usb` (`make fw-test`). Served by
`picozorro` with the cargo feature `usb-host` (the host engine is
`pz_hal::usb_host`) on the bus, and over the UART link with the
`uart-backplane` feature. Driven by `picozorrousb.device`, backend `pz` on
the bus.

## Where it sits

The second 256-byte window of the 128 KiB board, at A16 = 1 (board base +
$10000; A16 = 0 is `docs/REGISTERS.md`). The
general rules of REGISTERS.md apply unchanged: 16-bit registers at even
offsets, big-endian, `move.w`; byte access is an error (BUS_ERROR, a byte
write is ignored, a byte read has no side effects); unused offsets and
write-only registers read $FFFF; data ports carry bytes in memory order.
Over the UART link the window is chosen per batch with op WINDOW. On the
bus the decode compares A23-A17 once the board is configured and passes
A16 to the firmware with the cycle (`zbus.pio`); before configuration it
compares A23-A16, so the board answers only at $E8xxxx.

## Design: records, not per-slot registers

The window carries transfer records instead of a register block per
transfer slot: a request record (28-byte header + OUT data) goes into a
request queue, a completion record (12-byte header + IN data) comes out of
a completion queue, matched by `seq`. Why: the record is exactly what
Poseidon hands a hardware driver per transfer (device address, endpoint,
type, direction, speed / split routing, max packet size, a setup packet,
a buffer, a NAK timeout), the Amiga driver keeps one code path for every
backend, and the queues are the network window's TX / RX pattern.
Every op goes through the queues: PING, PORT_STATUS, PORT_RESET,
XFER, ABORT, FORGET, RESET_TOGGLE. A streamed bulk IN (request flag
STREAM, below) puts several completion records with one
`seq` into the completion queue, in order, the last one without MORE. A
streamed bulk OUT takes its first record as an XFER and the rest as
XFER_DATA request records with the stream's `seq`, and acknowledges each
written record with a completion record without payload.

## Map (offsets in the A16 = 1 window)

| Offset | Name | Access | Meaning |
|---|---|---|---|
| $00 | MAGIC | RO | $5055 ("PU") |
| $02 | VERSION | RO | $0001 |
| $04 | INT | R, W1C | interrupt status, bits below |
| $06 | INT_ENABLE | RW | same layout as INT |
| $08 | CTRL | W | bit 15 RESET_QUEUES; reads 0 |
| $0A | STATUS | RO | bits below |
| $10 | REQ_LEN | WO | open a request record of this many bytes (28..1052) |
| $12 | REQ_DATA | WO | request data port |
| $14 | REQ_COMMIT | WO | any value: record complete |
| $20 | CPL_LEN | RO | length of the head completion record (12..1036), 0 = queue empty |
| $22 | CPL_DATA | RO | completion data port |
| $24 | CPL_DONE | WO | any value: drop the head completion record |
| $40-$4A | STATS | RO | three 32-bit counters, high word first (snapshot rule of REGISTERS.md) |
| $60-$9E | | | MPEG decoder registers, `docs/REGISTERS-MPEG.md` (when the firmware has the decoder) |
| $C0-$FE | | | firmware update registers, `docs/UPDATE.md` (a separate model, `pz_core::update`, present without a USB model too) |

### INT ($04) and INT_ENABLE ($06)

| Bit | Name | Kind | Set when |
|---|---|---|---|
| 0 | CPL_AVAIL | level | the completion queue is not empty |
| 1 | REQ_FREE | latched | the firmware took a request (a slot came free) |
| 2 | PORT_CHANGE | latched | CONNECTED or LOW_SPEED changed |
| 4 | BUS_ERROR | latched | a rule of this contract was broken |

/INT is shared with the network window: asserted while the board is
configured and either window has (INT & INT_ENABLE) != 0.

### STATUS ($0A)

| Bits | Name | Meaning |
|---|---|---|
| 15 | CONNECTED | a device is on the root port |
| 14 | LOW_SPEED | it is low speed (0 when not connected) |
| 12-8 | REQ_FREE | free request slots, 0..16 |
| 7-0 | CPL_QUEUED | records in the completion queue, 0..16 |

### STATS ($40-$4A)

| Offset | Counter |
|---|---|
| $40 | requests: records committed |
| $44 | completions: records posted |
| $48 | req_errors: request records dropped (bad length, queue full, short commit, REQ_LEN while one is open) |

## Submitting a request

1. REQ_FREE (STATUS) > 0. A driver may keep a credit: the free count it
   last read minus the records it sent since.
2. REQ_LEN = record length. Out of range or no free slot: BUS_ERROR,
   req_errors, nothing opens.
3. ceil(len / 2) words to REQ_DATA (more: BUS_ERROR, ignored).
4. REQ_COMMIT. Fewer words than announced: the record is dropped
   (req_errors).

## Reading completions

Read CPL_LEN; if not 0, ceil(len / 2) words from CPL_DATA, then CPL_DONE
(may come early, the rest is skipped). Completions arrive in any order;
the driver matches them to its requests by `seq` and **ignores a `seq` it
does not know** (a completion racing a queue reset can slip through).
Intended use of /INT: enable CPL_AVAIL; the handler reads records until
CPL_LEN is 0. PORT_CHANGE can replace polling PORT_STATUS for the root
hub's change endpoint.

## Records

All multi-byte fields are big-endian (68000 order). Max data per record:
1024 bytes; the Amiga driver splits longer transfers at multiples of the
endpoint's max packet size and stops early on a short packet.

Request, 28-byte header + `length` bytes of OUT data:

| Off | Size | Field |
|---|---|---|
| 0 | 2 | magic `0x5055` ("PU") |
| 2 | 1 | version, 1 |
| 3 | 1 | op |
| 4 | 2 | seq, echoed in the completion |
| 6 | 1 | device address 0-127 |
| 7 | 1 | endpoint 0-15, bit 7 = IN |
| 8 | 1 | transfer type: 0 control, 1 iso, 2 bulk, 3 interrupt |
| 9 | 1 | flags: bit 0 low speed, bit 1 PRE (low speed behind a full-speed hub), bit 2 no short packet, bit 3 allow runt, bit 4 STREAM (below) |
| 10 | 2 | max packet size |
| 12 | 2 | NAK timeout in ms, 0 = wait until aborted |
| 14 | 2 | length: bytes to transfer (<= 1024); for ABORT the seq to abort |
| 16 | 1 | split hub address (PRE only) |
| 17 | 1 | split hub port (PRE only) |
| 18 | 8 | setup packet (control only); STREAM: bytes 18-21 = total length |
| 26 | 2 | poll interval in ms for interrupt transfers (0 = 1) |

Completion, 12-byte header + `actual` bytes of IN data:

| Off | Size | Field |
|---|---|---|
| 0 | 2 | magic |
| 2 | 1 | version |
| 3 | 1 | op |
| 4 | 2 | seq |
| 6 | 1 | status (below) |
| 7 | 1 | flags: bit 0 MORE, another completion for this request follows (STREAM) |
| 8 | 2 | actual: bytes transferred / payload length |
| 10 | 2 | reserved |

Ops:

| Op | Name | What the firmware does |
|---|---|---|
| 0 | PING | completes with status 0, payload `pzusb` + build info |
| 1 | PORT_STATUS | payload: connected (0/1), speed (0 none, 1 low, 2 full), feature bits (bit 0: STREAM for bulk IN; bit 1: STREAM for bulk OUT; bit 2: the interval field is honoured) |
| 2 | PORT_RESET | bus reset on the root port (drops every cached pipe), then PORT_STATUS payload |
| 3 | XFER | one transfer as described by the header |
| 4 | ABORT | cancels the in-flight request whose seq is in `length`; that request completes with status 15 |
| 5 | FORGET | drops the cached pipes of `device address` (after SET_ADDRESS, after a disconnect) |
| 6 | RESET_TOGGLE | drops the cached pipe of (`device address`, `endpoint`): next transfer starts at DATA0 (after ClearFeature(ENDPOINT_HALT)) |
| 7 | XFER_DATA | the next record of a streamed bulk OUT whose seq is in `seq` (below) |

Status values follow Poseidon's `UHIOERR_*` so the Amiga driver copies
them into `io_Error`: 0 ok, 1 USB offline, 3 host error, 4 stall, 6
timeout (no answer from the device), 7 overflow (more data than asked), 8
CRC / bad response, 10 NAK timeout, 11 bad parameters, 13 babble, 15
aborted. A short IN transfer is status 0 with `actual` < `length`; the
Amiga driver turns that into `UHIOERR_RUNTPACKET` unless allow-runt is set.

Status 6 and status 10 differ: a device that NAKs is there and keeps the
transfer going until its NAK timeout (10); a device that does not answer
at all (gone from its hub port, port disabled or suspended) ends it after
three attempts of the same packet in a row, within a few milliseconds
(6), whatever the NAK timeout, and the shared endpoint is free for the
other transfers again. This holds for every stage of a control transfer,
for bulk in both directions, and for low-speed devices behind a hub. A
stream ends with the record that failed.

### Streamed bulk IN

One request per 1 KiB costs a round trip to the requester per record; on
the bus that round trip (interrupt, driver task, the next request) takes
longer than the transfer itself. With flag STREAM (bulk IN only, when
PORT_STATUS reports the feature) one request covers the whole transfer:
bytes 18-21 hold the total length (32 bits), `length` the size of one
record (a multiple of the max packet size, <= 1024). The firmware reads
record after record and posts each as its own completion with the
request's `seq`; every completion but the last has MORE set. The stream
ends with the completion that completes the total, a short one (the
device sent a short packet), or one with a status other than 0. The NAK
timeout applies to each record. ABORT ends a stream like any request
(last completion: status 15). Completions of one stream arrive in order;
a full completion queue holds the stream back (the device is not asked
for more meanwhile).

### Streamed bulk OUT

The same for writes, when PORT_STATUS reports feature bit 1. The first
request is an XFER to a bulk OUT endpoint with flag STREAM: bytes 18-21
hold the total length, `length` the size of the data this record carries
(a multiple of the max packet size, <= 1024). The rest of the data
follows in XFER_DATA records (op 7) with the same `seq`, address and
endpoint, `length` = the data bytes in the record. Every record but the
last of a stream has the first one's size; the last brings the total.

The firmware writes the records in order and acknowledges each one when
it is on the device: a completion with the stream's `seq`, status 0,
`actual` = bytes written and no payload; every acknowledgement but the
last has MORE. The sender keeps at most two records unacknowledged (the
one being written and one waiting): it sends the first record and one
XFER_DATA, then one XFER_DATA per acknowledgement with MORE. The
completion without MORE ends the stream: status 0 after the last record,
otherwise the error.

- No short packet is sent between records. After the last one a
  zero-length packet goes out only if flag 2 (no short packet) of the
  first request is clear and the total is a multiple of the max packet
  size, as for one record.
- The NAK timeout applies to each record. When the next XFER_DATA does
  not arrive within 5 s the stream ends with status 6.
- ABORT ends a stream like any request (status 15). A root-port event or
  a bus reset ends it with status 15 (1 when the port is disconnected),
  a queue reset of the register window without a completion. After a
  PORT_RESET the stream's last completion follows the PORT_RESET's own,
  once the reset is done (about 60 ms).
- A record that breaks the rules above (size, address, endpoint) ends
  the stream with status 11. So does a sender that runs further ahead
  than two records: the record that has no room is dropped.
- An XFER_DATA whose `seq` has no stream in progress (it was on its way
  when the stream ended) is dropped without a completion; PING counts
  these in `data-dropped`, with the records of the previous case.

### Poll interval

Bytes 26-27 of an interrupt request set the pipe's poll interval in ms,
as the endpoint's bInterval gives it; the firmware clamps it to 1..255
(0 = 1). The request that opens the pipe sets it: an interrupt IN reader
already running for that endpoint, or a cached interrupt OUT pipe, keeps
its first interval. Without PORT_STATUS feature bit 2 the field is
ignored and every interrupt endpoint is polled at 1 ms. The Amiga driver
fills it from Poseidon's `iouh_Interval`.

## Queue reset

CTRL.RESET_QUEUES, /BUSRST and Autoconfig shut-up empty both queues and
void every request the firmware has taken: transfers in flight are
aborted and post no completion. /BUSRST and shut-up also clear
INT_ENABLE and acknowledge the latched bits. STATS survive.

## Sizes

16 request slots and 16 completion slots of 1052 bytes (33 KiB of
RP2350 RAM). 16 = the Amiga driver's in-flight limit (`SLOTS`, one per
waiting interrupt IN endpoint plus control and bulk).

## Not verified

- The 128 KiB decode on a real bus (`zbus.pio`; host tests and the
  RAM check only) and the bus-side cost of the data ports.
- A race of RESET_QUEUES with a completion being posted (one stale record
  can appear; the driver ignores unknown `seq`s). Host tests cover a reset
  between take and post, not the concurrent case.
