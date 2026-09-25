/*
 * bpclient: the Amiga end of the register backplane: the card's register
 * window reached over a datagram link instead of the Zorro bus. Bus
 * cycles are collected into a batch and sent as one datagram over a link
 * (bplink.h), the serial link (serlink, the UART). On the UART writes are
 * posted and reads come back in one reply per batch. Over a lossy link
 * (UDP) every batch asks for a reply, and a batch whose reply does not
 * come within BP_UDP_TIMEOUT_MS goes again with the same seq, up to
 * BP_UDP_RETRIES times: the firmware answers a repeated seq from its reply
 * cache without running the batch again. /INT arrives as notification
 * datagrams. Shared by picozorro.device, picozorrousb.device,
 * mpega.library and pzflash. Every call comes from the process that opened
 * the link.
 */
#ifndef PZ_BPCLIENT_H
#define PZ_BPCLIENT_H

#include <exec/types.h>
#include "serlink.h"
#include "bplink.h"

#define BP_MAGIC    0x5042
#define BP_VERSION  1
#define BF_REPLY    0x01
#define BF_IS_REPLY 0x80
#define BF_INT      0x40
#define BOP_WRITE   1
#define BOP_READ    2
#define BOP_WRITE_N 3
#define BOP_READ_N  4
#define BOP_RESET   5
#define BOP_WINDOW  6     /* A16 for the rest of the batch */
#define BOP_MEM_WRITE 7   /* offset24, n16, n bytes: board memory space */
#define BOP_MEM_READ  8   /* offset24, n16: the bytes go into the reply */
/* Reply status (byte 6 of a reply) */
#define BST_OK        0
#define BST_MALFORMED 1   /* the ops after the bad one were not run */
#define BST_REFUSED   2   /* a memory op where the board has no memory; same */
#define BST_NONE      0xff /* c->status: no reply (timeout, not sent) */
#define S_UDS 2
#define S_LDS 1
#define S_WORD (S_UDS | S_LDS)

#define BP_REPLY_TIMEOUT_MS 2000   /* serial link: one try */
#define BP_UDP_TIMEOUT_MS   200    /* UDP: per try */
#define BP_UDP_RETRIES      3      /* UDP: tries after the first */
#define BP_UDP_SLOW_MS      500    /* UDP: per try, a batch marked with b_slow */
#define BP_REQ_HDR   6
#define BP_REPLY_HDR 8
#define BP_MEM_OP    6    /* a MEM op before its data */
/* Bytes per MEM op in bpc_mem_*_all on the serial link (c->mem_chunk is
 * the link's: BP_MEM_CHUNK_SER or BP_MEM_CHUNK_UDP, bplink.h). */
#define BP_MEM_CHUNK BP_MEM_CHUNK_SER
/* bpc_mem_write_all waits for a reply every this many batches on the
 * serial link (~15 KB, 0.15 s of the 1 Mbaud line). */
#define BP_MEM_SYNC  8

struct BpClient {
    const struct BpLinkOps *ops;
    APTR link;               /* the link's handle (serlink: struct SerLink *) */
    UWORD max;               /* largest datagram of the link */
    UWORD mem_chunk;         /* bytes per MEM op in bpc_mem_*_all */
    BOOL lossy;              /* UDP: every batch awaited, retries */
    UWORD seq;
    UWORD len;               /* bytes in req */
    BOOL irq;                /* last /INT notification: asserted */
    UBYTE status;            /* status of the last awaited reply, BST_* */
    ULONG timeouts;          /* batches given up (after the retries) */
    ULONG stray_errors;      /* error replies to posted batches (a posted
                              * batch that fails is answered anyway; over
                              * UDP its awaited reply counts here too) */
    ULONG retries;           /* UDP: batches sent again after a timeout */
    ULONG duplicates;        /* UDP: replies to a seq no longer awaited
                              * (the late or repeated reply of a retry) */
    BOOL slow;               /* this batch may take the card long (b_slow) */
    UBYTE req[SERLINK_MAX];
};

/* Open / close the serial link; args "[baud [unit [device]]]". 0 = ok. */
LONG bpc_open(struct BpClient *c, const char *args);
/* Open over the given link with its own arguments. 0 = ok. */
LONG bpc_open_link(struct BpClient *c, const struct BpLinkOps *ops, const char *args);
void bpc_close(struct BpClient *c);
/* What Kickstart does to the card: bpc_attach_begin starts a batch with
 * /BUSRST and Autoconfig at $E90000; the caller adds its first reads;
 * bpc_attach_run sends it, up to three times, and returns the reply bytes
 * (all `bytes` of them) or -1. */
void bpc_attach_begin(struct BpClient *c);
LONG bpc_attach_run(struct BpClient *c, UBYTE *words, ULONG bytes);

void b_begin(struct BpClient *c);
/* The batch being built may keep the card busy: over UDP its tries wait
 * BP_UDP_SLOW_MS instead of BP_UDP_TIMEOUT_MS (the firmware answers after
 * the batch ran). */
void b_slow(struct BpClient *c);
void b_put(struct BpClient *c, UBYTE b);
void b_window(struct BpClient *c, UBYTE a16);
void b_write(struct BpClient *c, UBYTE reg, UBYTE strobes, UWORD v);
void b_read(struct BpClient *c, UBYTE reg);
/* WRITE_N of `alen` + `blen` bytes (memory order = data port order), `a`
 * first; an odd total is padded with a zero byte. */
void b_write_n2(struct BpClient *c, UBYTE reg, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen);
void b_read_n(struct BpClient *c, UBYTE reg, UWORD n);
/* MEM_WRITE / MEM_READ at byte `offset` from the board base (24 bit). The
 * caller keeps the batch within c->max (request) and the reply within
 * c->max - BP_REPLY_HDR bytes of read data. */
void b_mem_write(struct BpClient *c, ULONG offset, const UBYTE *data, UWORD len);
void b_mem_read(struct BpClient *c, ULONG offset, UWORD len);
/* Send the batch. With `words`, wait for the reply and copy its read data
 * (`bytes` at most) there; returns the bytes copied, -1 on timeout or an
 * error status. Without, the writes are posted: 0, or -1 if not sent
 * (over UDP: not acknowledged; an error reply counts in stray_errors, as
 * on the UART). A batch longer than c->max is not sent (-1). */
LONG b_run(struct BpClient *c, UBYTE *words, ULONG bytes);
UWORD bpc_rd1(struct BpClient *c, UBYTE a16, UBYTE reg);
void bpc_wr1(struct BpClient *c, UBYTE a16, UBYTE reg, UWORD v);
/* `len` bytes at `offset` in batches of one MEM op of up to c->mem_chunk
 * bytes. Writes are posted except every BP_MEM_SYNC-th batch and the last,
 * which wait for their reply (flow control; over UDP every one waits); 0 when all went through, -1
 * on a timeout or an error status of an awaited batch (c->status says
 * which). A refusal of a posted batch shows up in c->stray_errors. Reads
 * wait for each reply: 0, or -1 with c->status telling why (BST_REFUSED:
 * outside the board's memory). */
LONG bpc_mem_write_all(struct BpClient *c, ULONG offset, const UBYTE *data, ULONG len);
LONG bpc_mem_read_all(struct BpClient *c, ULONG offset, UBYTE *buf, ULONG len);
/* Take in waiting frames (notifications update c->irq). */
void bpc_drain(struct BpClient *c);
/* The link's wait (bplink.h): until a datagram may be there, a signal of
 * `sigs` or `ms`; returns the signals that fired. */
ULONG bpc_wait(struct BpClient *c, ULONG sigs, ULONG ms);
/* Datagrams the link dropped on the way in (serial: frame errors). */
ULONG bpc_link_errors(struct BpClient *c);
void bpc_log(struct BpClient *c, const UBYTE *text, UWORD len);

#endif
