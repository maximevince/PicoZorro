/*
 * serlink: datagrams over serial.device, framed as payload +
 * CRC-16/CCITT-FALSE, COBS-encoded, with 0x00 before and after. Used by
 * picozorrousb.device (USB tunnel) and by bpclient (register backplane).
 * One link per process; every call comes from the process that opened it.
 */
#ifndef PZ_SERLINK_H
#define PZ_SERLINK_H

#include <exec/types.h>

#define SERLINK_MAX 2048          /* largest datagram, both directions */

struct SerLink;

/* args: "[baud [unit [device]]]", default 115200 0 serial.device */
struct SerLink *serlink_open(const char *args);
void serlink_close(struct SerLink *l);
/* Signal that fires when bytes arrive (after serlink_recv returned 0). */
ULONG serlink_sigmask(struct SerLink *l);
/* Send one datagram made of a followed by b (b may be NULL). 0 = ok. */
LONG serlink_send(struct SerLink *l, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen);
/* Next checked datagram: its length, *dg points into the link (valid
 * until the next call); 0 when none is waiting (the read is re-armed). */
LONG serlink_recv(struct SerLink *l, UBYTE **dg);
/* Sleep until bytes arrive, a signal in `sigs` fires, or `ms` pass.
 * Returns the signals of `sigs` that fired. */
ULONG serlink_wait(struct SerLink *l, ULONG sigs, ULONG ms);
/* TRUE when serlink_recv may have something. */
BOOL serlink_pending(struct SerLink *l);
ULONG serlink_frame_errors(struct SerLink *l);

#endif
