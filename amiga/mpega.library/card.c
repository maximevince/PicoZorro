/*
 * The card side of PicoZorro's mpega.library: the MPEG registers
 * (docs/REGISTERS-MPEG.md) on the bus, or over the UART backplane for
 * FS-UAE (ENV:PZMPEGA "bp [baud [unit [device]]]").
 */
#include <exec/memory.h>
#include <libraries/configvars.h>
#include <devices/timer.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/expansion.h>
#include "mpega_lib.h"

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
/* Words per READ_N / WRITE_N batch on the backplane (a datagram holds 2048
 * bytes, header and ops included). */
#define BP_WORDS 960

#define REG(c, r) ((c)->win[(r) / 2])

/* Scratch for backplane reads (records come in pieces). */
static UBYTE *bp_tmp(struct Card *c)
{
    return (UBYTE *)(c->bp + 1);
}

LONG card_open(struct Card *c)
{
    char env[64];
    LONG n = GetVar("PZMPEGA", env, sizeof(env), GVF_GLOBAL_ONLY);

    c->win = NULL;
    c->bp = NULL;
    if (n >= 2 && env[0] == 'b' && env[1] == 'p') {
        UBYTE w[2];
        c->bp = AllocVec(sizeof(struct BpClient) + 2 * BP_WORDS, MEMF_PUBLIC | MEMF_CLEAR);
        if (!c->bp)
            return -1;
        /* bpclient times its replies with GetSysTime. */
        if (!TimerBase) {
            c->treq = (struct timerequest *)CreateIORequest(CreateMsgPort(), sizeof(struct timerequest));
            if (c->treq && OpenDevice(TIMERNAME, UNIT_VBLANK, (struct IORequest *)c->treq, 0) == 0)
                TimerBase = c->treq->tr_node.io_Device;
        }
        CopyMem(env + 2, c->args, sizeof(c->args) - 1);
        c->owner = FindTask(NULL);
        if (!TimerBase || bpc_open(c->bp, c->args) != 0) {
            FreeVec(c->bp);
            c->bp = NULL;
            return -1;
        }
        /* As Kickstart would: /BUSRST and Autoconfig, then MAGIC. */
        bpc_attach_begin(c->bp);
        b_window(c->bp, 1);
        b_read(c->bp, M_MAGIC);
        if (bpc_attach_run(c->bp, w, 2) != 2 || ((UWORD)w[0] << 8 | w[1]) != MPEG_MAGIC) {
            card_close(c);
            return -1;
        }
        return 0;
    } else {
        struct Library *ExpansionBase = OpenLibrary("expansion.library", 37);
        struct ConfigDev *cd;
        if (!ExpansionBase)
            return -1;
        cd = FindConfigDev(NULL, PZ_MANUFACTURER, PZ_PRODUCT);
        CloseLibrary(ExpansionBase);
        if (!cd)
            return -1;
        /* The A16 = 1 half of the 128 KiB board. */
        c->win = (volatile UWORD *)((UBYTE *)cd->cd_BoardAddr + 0x10000);
        if (REG(c, M_MAGIC) != MPEG_MAGIC) {
            c->win = NULL;
            return -1;
        }
        return 0;
    }
}

/* The backplane link's serial requests signal the task that opened it.
 * Players may open a stream in one task and decode in another (AmigaAMP
 * does): reopen the link in the calling task (no reset, the card keeps its
 * state). The bus has no such tie. */
static void bp_here(struct Card *c)
{
    struct Task *me = FindTask(NULL);
    if (c->bp && c->owner != me) {
        bpc_close(c->bp);
        bpc_open(c->bp, c->args);
        c->owner = me;
    }
}

void card_close(struct Card *c)
{
    bp_here(c);
    if (c->bp) {
        bpc_close(c->bp);
        FreeVec(c->bp);
        c->bp = NULL;
    }
    if (c->treq) {
        struct MsgPort *port = c->treq->tr_node.io_Message.mn_ReplyPort;
        if (TimerBase == c->treq->tr_node.io_Device) {
            CloseDevice((struct IORequest *)c->treq);
            TimerBase = NULL;
        }
        DeleteIORequest((struct IORequest *)c->treq);
        DeleteMsgPort(port);
        c->treq = NULL;
    }
    c->win = NULL;
}

UWORD card_status(struct Card *c)
{
    bp_here(c);
    if (c->win)
        return REG(c, M_STATUS);
    return bpc_rd1(c->bp, 1, M_STATUS);
}

void card_start(struct Card *c, UWORD config, UWORD scale)
{
    bp_here(c);
    if (c->win) {
        REG(c, M_CONFIG) = config;
        REG(c, M_SCALE) = scale;
        REG(c, M_CTRL) = MCMD_START;
        c->session = REG(c, M_SESSION);
    } else {
        UBYTE w[2];
        b_begin(c->bp);
        b_window(c->bp, 1);
        b_write(c->bp, M_CONFIG, S_WORD, config);
        b_write(c->bp, M_SCALE, S_WORD, scale);
        b_write(c->bp, M_CTRL, S_WORD, MCMD_START);
        b_read(c->bp, M_SESSION);
        if (b_run(c->bp, w, 2) == 2)
            c->session = (UWORD)w[0] << 8 | w[1];
    }
}

void card_stop(struct Card *c)
{
    bp_here(c);
    if (c->win)
        REG(c, M_CTRL) = MCMD_STOP;
    else
        bpc_wr1(c->bp, 1, M_CTRL, MCMD_STOP);
}

void card_scale(struct Card *c, UWORD scale)
{
    bp_here(c);
    if (c->win)
        REG(c, M_SCALE) = scale;
    else
        bpc_wr1(c->bp, 1, M_SCALE, scale);
}

void card_feed(struct Card *c, const UBYTE *buf, ULONG len)
{
    bp_here(c);
    ULONG i;
    if (c->win) {
        volatile UWORD *port = &REG(c, M_IN_DATA);
        REG(c, M_IN_LEN) = (UWORD)len;
        for (i = 0; i + 1 < len; i += 2)
            *port = (UWORD)buf[i] << 8 | buf[i + 1];
        if (len & 1)
            *port = (UWORD)buf[len - 1] << 8;
        REG(c, M_IN_COMMIT) = 0;
        return;
    }
    /* A chunk may span batches: the data port keeps its place. */
    i = 0;
    do {
        ULONG n = len - i > 2 * BP_WORDS ? 2 * BP_WORDS : len - i;
        b_begin(c->bp);
        b_window(c->bp, 1);
        if (i == 0)
            b_write(c->bp, M_IN_LEN, S_WORD, (UWORD)len);
        if (n)
            b_write_n2(c->bp, M_IN_DATA, buf + i, n, NULL, 0);
        i += n;
        if (i >= len)
            b_write(c->bp, M_IN_COMMIT, S_WORD, 0);
        b_run(c->bp, NULL, 0);
    } while (i < len);
}

/* k words from the data port: two per move.l ($82 and $84 are both it). */
static void read_words(volatile UWORD *port, WORD *dst, ULONG k)
{
    volatile ULONG *pair = (volatile ULONG *)port;
    while (k >= 2) {
        ULONG v = *pair;
        *dst++ = (WORD)(v >> 16);
        *dst++ = (WORD)v;
        k -= 2;
    }
    if (k)
        *dst = (WORD)*port;
}

static void skip_words(volatile UWORD *port, ULONG k)
{
    while (k--)
        (void)*port;
}

/* Word w of the record's PCM (planar) goes to pcm0[w] or pcm1[w - n]. */
static void put_pcm(WORD *pcm0, WORD *pcm1, ULONG n, ULONG max, ULONG w, WORD v)
{
    if (w < n) {
        if (pcm0 && w < max)
            pcm0[w] = v;
    } else if (pcm1 && w - n < max) {
        pcm1[w - n] = v;
    }
}

ULONG card_record(struct Card *c, UBYTE *hdr, WORD *pcm0, WORD *pcm1, ULONG max)
{
    ULONG len, words, n, w, i;
    bp_here(c);
    if (c->win) {
        volatile UWORD *port = &REG(c, M_OUT_DATA);
        len = REG(c, M_OUT_LEN);
        if (len < REC_HDR)
            return 0;
        for (i = 0; i < REC_HDR / 2; i++) {
            UWORD v = *port;
            hdr[2 * i] = v >> 8;
            hdr[2 * i + 1] = (UBYTE)v;
        }
        n = (UWORD)(hdr[2] << 8 | hdr[3]);
        words = (len - REC_HDR) / 2;
        for (i = 0; n && i * n < words; i++) {
            WORD *dst = i == 0 ? pcm0 : pcm1;
            if (dst && n <= max)
                read_words(port, dst, n);
            else
                skip_words(port, n);
        }
        REG(c, M_OUT_DONE) = 0;
        return len;
    }
    /* Backplane: OUT_LEN, then the header and PCM in READ_N pieces. */
    {
        UBYTE *t = bp_tmp(c);
        b_begin(c->bp);
        b_window(c->bp, 1);
        b_read(c->bp, M_OUT_LEN);
        if (b_run(c->bp, t, 2) != 2)
            return 0;
        len = (UWORD)(t[0] << 8 | t[1]);
        if (len < REC_HDR)
            return 0;
        words = (len + 1) / 2;
        n = 0;
        for (w = 0; w < words;) {
            ULONG k = words - w > BP_WORDS ? BP_WORDS : words - w;
            b_begin(c->bp);
            b_window(c->bp, 1);
            b_read_n(c->bp, M_OUT_DATA, (UWORD)k);
            if (b_run(c->bp, t, 2 * k) != (LONG)(2 * k)) {
                /* Lost on the line: the port has moved on, so the record
                 * cannot be read again. Drop it. */
                bpc_wr1(c->bp, 1, M_OUT_DONE, 0);
                return 0;
            }
            for (i = 0; i < k; i++, w++) {
                WORD v = (WORD)((UWORD)t[2 * i] << 8 | t[2 * i + 1]);
                if (w < REC_HDR / 2) {
                    hdr[2 * w] = t[2 * i];
                    hdr[2 * w + 1] = t[2 * i + 1];
                    if (w == 1)
                        n = (UWORD)v;
                } else {
                    put_pcm(pcm0, pcm1, n, max, w - REC_HDR / 2, v);
                }
            }
        }
        bpc_wr1(c->bp, 1, M_OUT_DONE, 0);
        return len;
    }
}
