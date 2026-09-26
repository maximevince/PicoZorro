/*
 * PicoZorro mpega.library, private: the library base, a stream, the card.
 */
#ifndef PZ_MPEGA_LIB_H
#define PZ_MPEGA_LIB_H

#include "../common/compiler.h"
#include <exec/types.h>
#include <exec/libraries.h>
#include <dos/dos.h>
#include "libraries/mpega.h"
#include "../common/bpclient.h"

#define LIB_NAME     "mpega.library"
#define LIB_VERSION  2
#define LIB_REVISION 200   /* reports as 2.200: the PicoZorro build */
#define LIB_DATE     "26.9.2026"

struct MPEGABase {
    struct Library lib;
    BPTR seglist;
};

extern struct ExecBase *SysBase;
extern struct DosLibrary *DOSBase;
extern struct Library *UtilityBase;
extern struct Device *TimerBase;

/* ---- the card: docs/REGISTERS-MPEG.md (A16 = 1 window) ---- */

#define M_MAGIC     0x60
#define M_VERSION   0x62
#define M_CTRL      0x64
#define M_STATUS    0x66
#define M_CONFIG    0x68
#define M_SCALE     0x6a
#define M_SESSION   0x6c
#define M_IN_LEN    0x70
#define M_IN_DATA   0x72
#define M_IN_COMMIT 0x74
#define M_OUT_LEN   0x80
#define M_OUT_DATA  0x82
#define M_OUT_DONE  0x86
#define MPEG_MAGIC  0x4d50
#define MCMD_START   1
#define MCMD_STOP    2
#define CFG_MONO    0x10
#define REC_END     0x8000
#define REC_HDR     16
#define IN_CHUNK    2048

struct Card {
    /* bus: the A16 = 1 window; NULL on the backplane */
    volatile UWORD *win;
    /* backplane (ENV:PZMPEGA "bp [baud [unit [device]]]"): the link is
     * the opening task's */
    struct BpClient *bp;
    struct timerequest *treq;  /* backplane: TimerBase for bpclient */
    struct Task *owner;        /* backplane: the task the link belongs to */
    char args[64];             /* backplane: bpc_open's arguments */
    UWORD session;
};

LONG card_open(struct Card *c);
void card_close(struct Card *c);
UWORD card_status(struct Card *c);
/* CONFIG, SCALE, START; remembers the new SESSION. */
void card_start(struct Card *c, UWORD config, UWORD scale);
void card_stop(struct Card *c);
void card_scale(struct Card *c, UWORD scale);
/* One input chunk, 0..IN_CHUNK bytes (0: the end of the stream). */
void card_feed(struct Card *c, const UBYTE *buf, ULONG len);
/* The head record: its 16-byte header into hdr, the PCM planar into
 * pcm0 / pcm1 (either may be NULL: skipped), at most `max` samples per
 * channel; then OUT_DONE. Returns the record length, 0 = none. */
ULONG card_record(struct Card *c, UBYTE *hdr, WORD *pcm0, WORD *pcm1, ULONG max);

/* ---- a stream ---- */

struct Stream {
    MPEGA_STREAM pub;          /* first: the application's pointer */
    MPEGA_CTRL ctrl;           /* a copy (some players free theirs) */
    struct Hook *hook;         /* NULL: file I/O */
    APTR handle;               /* the hook's handle */
    BPTR fh;
    LONG size;                 /* stream bytes, 0 = unknown */
    LONG start;                /* first frame header */
    LONG pos;                  /* next byte to read from the source */
    BOOL end_sent;
    BOOL ended;                /* the END record came: EOF from now on */
    UWORD config, scale;
    UWORD spf;                 /* samples per frame of the source */
    ULONG frames;              /* frames decode_frame returned (MPEGA_time) */
    /* Xing / Info */
    ULONG xing_frames, xing_bytes;
    BOOL has_toc;
    UBYTE toc[100];
    struct Card card;
    UBYTE *buf;                /* IN_CHUNK staging */
};

#endif
