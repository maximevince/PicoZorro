/*
 * bplink: the datagram link under bpclient. The implementation here is
 * serlink (serial.device, COBS + CRC framing, the UART backplane); the
 * interface also carries a lossy link such as UDP (one datagram per
 * backplane datagram). A link belongs to the process that opened it.
 */
#ifndef PZ_BPLINK_H
#define PZ_BPLINK_H

#include <exec/types.h>

/* Largest datagram over UDP: one Ethernet frame, no IP fragmentation
 * (embassy-net / smoltcp on the module does not reassemble). */
#define UDPLINK_MAX 1400
/* Bytes per MEM op in bpc_mem_*_all (bpclient.h): inside SERLINK_MAX with
 * the headers on the UART; inside UDPLINK_MAX over UDP (a request carries
 * at most max - 12 bytes of MEM_WRITE data, a reply max - 8 of MEM_READ
 * data). */
#define BP_MEM_CHUNK_SER 1920
#define BP_MEM_CHUNK_UDP 1280

struct BpLinkOps {
    const char *name;
    /* Open with the link's own arguments (serlink: "[baud [unit [device]]]",
     * a UDP link: "[a.b.c.d [port]]"); NULL on failure. */
    APTR (*open)(const char *args);
    void (*close)(APTR l);
    /* One datagram made of a followed by b (b may be NULL). 0 = ok. */
    LONG (*send)(APTR l, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen);
    /* Next datagram: its length, *dg points into the link (valid until the
     * next call); 0 when none is waiting. Never blocks. */
    LONG (*recv)(APTR l, UBYTE **dg);
    /* Sleep until a datagram may be there, a signal in `sigs` fires, or
     * `ms` pass. Returns the signals of `sigs` that fired. */
    ULONG (*wait)(APTR l, ULONG sigs, ULONG ms);
    /* Signal that fires when data arrive (serlink: after recv returned 0;
     * UDP: 0, the socket is waited on through `wait`). */
    ULONG (*sigmask)(APTR l);
    /* Datagrams dropped on the way in (serlink: framing / CRC errors;
     * UDP: short or foreign datagrams). */
    ULONG (*errors)(APTR l);
    /* Log text to the PC (a LOG datagram, "LO" + text). */
    void (*log)(APTR l, const UBYTE *text, UWORD len);
    UWORD max_dgram;          /* largest datagram, both directions */
    UWORD mem_chunk;          /* bpc_mem_*_all's bytes per MEM op */
    /* TRUE: datagrams can be lost (UDP). bpclient then asks every batch for
     * a reply and retries a lost one with the same seq. */
    BOOL lossy;
};

extern const struct BpLinkOps serlink_ops;   /* serlink.c */

#endif
