# PicoZorro MPEG audio registers, version 1

The contract between the firmware's MPEG decoder and PicoZorro's
`mpega.library`. Implemented and host-tested in `pz_core::mpeg`
(`make fw-test`, Miri clean), served by `picozorro --features mpeg` and
driven by `amiga/mpega.library`; decoding is bit-exact end to end over
the UART link.

## What it does

The Amiga library does what the original mpega.library does on the 68k:
- file or hook I/O;
- ID3 skipping and header / Xing parsing for `MPEGA_STREAM`;
- seek arithmetic and time keeping.

It streams the compressed bytes into the card. The card decodes them with
minimp3 (MPEG-1/2/2.5, layers I, II, III) and hands back **one record per
frame**: the frame's header and its PCM. Output shaping happens on the
card, not the 68k, which cuts register traffic 2 to 4 times:
- rate division (freq_div 2 or 4);
- stereo to mono;
- gain (MPEGA_scale).

One frame per record keeps the original's contract: `MPEGA_decode_frame`
returns exactly one frame, or 0 for a frame that gave no samples.

## Where it sits

- **Window:** offsets $60-$9E of the A16 = 1 window, next to the USB
  registers ($00-$4A, `docs/REGISTERS-USB.md`) and the update registers
  ($C0-$FE, `docs/UPDATE.md`).
- **Access rules:** REGISTERS.md's rules apply. Registers are 16-bit at
  even offsets, big-endian, accessed with `move.w`. A byte access is an
  error (the write is ignored). Unused offsets read $FFFF. Data ports
  carry bytes in memory order.
- **Presence:** MAGIC tells whether the firmware has the decoder.
- **Bus decode:** as the USB window (REGISTERS-USB.md): board base +
  $10000 on the bus, op WINDOW 1 over the UART link.

## Map

| Offset | Name | Access | Meaning |
|---|---|---|---|
| $60 | MAGIC | RO | $4D50 ("MP") |
| $62 | VERSION | RO | $0001 |
| $64 | CTRL | W | command: 1 START, 2 STOP (reads 0) |
| $66 | STATUS | RO | bits 11-8 IN_FREE (free input chunks, 0..8), 7-0 OUT_QUEUED (records, 0..6) |
| $68 | CONFIG | RW | bits 1-0 FREQ_DIV (0: 1, 1: 2, 2: 4), bit 4 MONO; taken at START |
| $6A | SCALE | RW | gain in percent, 1..800 (anything else counts as 100); applies from the next frame decoded (MPEGA_scale) |
| $6C | SESSION | RO | low 16 bits of the session number; every START / STOP / reset opens a new one |
| $70 | IN_LEN | WO | open an input chunk of this many bytes, 0..2048; 0 = end of stream |
| $72 | IN_DATA | WO | input data port |
| $74 | IN_COMMIT | WO | any value: chunk complete |
| $80 | OUT_LEN | RO | bytes in the head record (16 + PCM), 0 = none |
| $82, $84 | OUT_DATA | RO | output data port; both offsets, so `move.l` reads two words |
| $86 | OUT_DONE | WO | any value: drop the head record |
| $90-$9A | STATS | RO | three 32-bit counters, high word first (the high-word read latches the low word) |

## A stream

1. Write CONFIG and SCALE, then CTRL = START.
   - START empties both queues and resets the decoder.
   - Read SESSION after START. A record the firmware finished just as
     START came in can still reach the queue; drop every record whose
     SESSION differs.
2. Feed input chunks while IN_FREE > 0:
   - IN_LEN = n;
   - (n + 1) / 2 words to IN_DATA (the last byte of an odd chunk sits in
     the high byte);
   - IN_COMMIT.

   Start at the first frame header (skip ID3v2 on the Amiga). The
   firmware finds sync by itself after that, but junk costs bus time.
3. After the last byte, a chunk of length 0 marks the end of the stream.
4. Read records while OUT_QUEUED > 0:
   - OUT_LEN, then OUT_LEN / 2 words from OUT_DATA, then OUT_DONE;
   - the record after an END record is the next stream's (none until the
     next START).
5. Seek: CTRL = START again, then feed from the new file position.
6. CTRL = STOP when done: queues empty, decoder idle.

The firmware decodes ahead while it has input and a free output slot.

## Record

A 16-byte header, big-endian words, then the PCM.

| Word | Meaning |
|---|---|
| 0 | FLAGS: bit 15 END (no frame: the input is used up after the end marker), bit 14 LOST (bytes skipped to find sync before this frame) |
| 1 | SAMPLES per channel in this record, after FREQ_DIV (0: the frame gave no samples) |
| 2 | CHANNELS in the PCM, 1 or 2, after MONO |
| 3 | FRAME_BYTES: compressed bytes this frame used |
| 4, 5 | the frame's 4-byte MPEG header as it was in the stream (the library takes norm, layer, mode, bitrate, rate, private / copyright / original from it) |
| 6 | SESSION: the session the record belongs to |
| 7 | reserved, 0 |

**PCM:** planar, as `MPEGA_decode_frame` returns it. First SAMPLES words
of channel 0 (left or mono), then SAMPLES words of channel 1 if CHANNELS
is 2. Signed 16-bit, big-endian.

**When records appear:**
- Every frame the decoder consumes gives a record, so the library can
  count frames for `MPEGA_time` like the original.
- An END record comes once, after the last frame of a stream that got its
  end marker.

## Output shaping (firmware, `pz_core::mpeg::shape`)

- **FREQ_DIV 2 or 4:** each output sample is the mean of 2 or 4 input
  samples. A box filter: cheaper than the original's filterbank, better
  than plain decimation.
- **MONO:** (L + R) / 2.
- **SCALE:** x · scale / 100, saturated to 16 bits.
- **Order:** mono, then rate division, then gain.

## STATS ($90-$9A)

| Offset | Counter |
|---|---|
| $90 | frames: records with samples posted |
| $94 | skipped: records with 0 samples (sync, bit reservoir) |
| $98 | in_errors: input chunks dropped (bad length, queue full, short commit) |

## Sizes

- **Input:** 8 chunks of up to 2048 bytes.
- **Output:** 6 records of up to 16 + 4608 bytes (1152 samples × 2
  channels).
- **Decoder:** minimp3 needs a contiguous buffer. The firmware copies
  chunks into a 16 KiB linear buffer and compacts it as frames are used.

## Not verified

- Real-time playback on the bus: 176 KB/s of PCM at 44.1 kHz stereo,
  against the expected 1-2 MB/s Zorro II path.
- The UART link carries ~15 KB/s, so it tests decoding to a file, not
  playback.
