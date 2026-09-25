/*
 * serlink: datagrams over serial.device (serlink.h).
 */
#include <proto/exec.h>
#include <exec/memory.h>
#include <exec/errors.h>
#include <devices/serial.h>
#include <devices/timer.h>
#include "serlink.h"
#include "bplink.h"

#define RAW_MAX (SERLINK_MAX + 2)
#define ENC_MAX (RAW_MAX + RAW_MAX / 254 + 3)

struct SerLink {
    struct MsgPort *rport, *wport, *tport;
    struct IOExtSer *rio, *wio;
    struct timerequest *tio;
    BOOL ser_open, timer_open, rpending;
    UBYTE rbyte;
    UWORD in_pos, in_len;
    UWORD acc_len;
    BOOL acc_over;
    ULONG frame_errors;
    UBYTE in[512];
    UBYTE acc[ENC_MAX];
    UBYTE raw[RAW_MAX];
    UBYTE enc[ENC_MAX];
};

static UWORD crc16(UWORD crc, const UBYTE *p, ULONG n)
{
    int i;
    while (n--) {
        crc ^= (UWORD)*p++ << 8;
        for (i = 0; i < 8; i++)
            crc = (crc & 0x8000) ? (crc << 1) ^ 0x1021 : crc << 1;
    }
    return crc;
}

/* COBS-encode `n` bytes into `dst` with a 0x00 before and after. */
static ULONG cobs_frame(const UBYTE *src, ULONG n, UBYTE *dst)
{
    ULONG code_at = 1, w = 2;
    UBYTE code = 1;
    dst[0] = 0;
    while (n--) {
        UBYTE b = *src++;
        if (b == 0) {
            dst[code_at] = code;
            code_at = w++;
            code = 1;
        } else {
            dst[w++] = b;
            if (++code == 0xff) {
                dst[code_at] = code;
                code_at = w++;
                code = 1;
            }
        }
    }
    dst[code_at] = code;
    dst[w++] = 0;
    return w;
}

/* Decode in place; returns the length or -1. */
static LONG cobs_decode(UBYTE *buf, ULONG len)
{
    ULONG r = 0, w = 0, i, code;
    while (r < len) {
        code = buf[r];
        if (code == 0 || r + code > len)
            return -1;
        for (i = 1; i < code; i++)
            buf[w++] = buf[r + i];
        r += code;
        if (code != 0xff && r < len)
            buf[w++] = 0;
    }
    return (LONG)w;
}

static ULONG parse_num(const char **pp)
{
    const char *p = *pp;
    ULONG n = 0;
    while (*p == ' ')
        p++;
    while (*p >= '0' && *p <= '9')
        n = n * 10 + (*p++ - '0');
    *pp = p;
    return n;
}

void serlink_close(struct SerLink *s)
{
    if (!s)
        return;
    if (s->rpending) {
        AbortIO((struct IORequest *)s->rio);
        WaitIO((struct IORequest *)s->rio);
    }
    if (s->ser_open)
        CloseDevice((struct IORequest *)s->rio);
    if (s->timer_open)
        CloseDevice((struct IORequest *)s->tio);
    if (s->rio)
        DeleteIORequest((struct IORequest *)s->rio);
    if (s->wio)
        DeleteIORequest((struct IORequest *)s->wio);
    if (s->tio)
        DeleteIORequest((struct IORequest *)s->tio);
    if (s->rport)
        DeleteMsgPort(s->rport);
    if (s->wport)
        DeleteMsgPort(s->wport);
    if (s->tport)
        DeleteMsgPort(s->tport);
    FreeVec(s);
}

struct SerLink *serlink_open(const char *args)
{
    struct SerLink *s;
    const char *p = args;
    char devname[40];
    ULONG baud, unit;
    int i = 0;

    s = AllocVec(sizeof(*s), MEMF_PUBLIC | MEMF_CLEAR);
    if (!s)
        return NULL;
    baud = parse_num(&p);
    unit = parse_num(&p);
    while (*p == ' ')
        p++;
    while (*p && *p != ' ' && i < sizeof(devname) - 1)
        devname[i++] = *p++;
    devname[i] = 0;
    if (!baud)
        baud = 115200;
    if (!devname[0])
        CopyMem("serial.device", devname, 14);

    s->rport = CreateMsgPort();
    s->wport = CreateMsgPort();
    s->tport = CreateMsgPort();
    if (!s->rport || !s->wport || !s->tport)
        goto fail;
    s->rio = (struct IOExtSer *)CreateIORequest(s->rport, sizeof(struct IOExtSer));
    s->wio = (struct IOExtSer *)CreateIORequest(s->wport, sizeof(struct IOExtSer));
    s->tio = (struct timerequest *)CreateIORequest(s->tport, sizeof(struct timerequest));
    if (!s->rio || !s->wio || !s->tio)
        goto fail;
    if (OpenDevice(TIMERNAME, UNIT_MICROHZ, (struct IORequest *)s->tio, 0) != 0)
        goto fail;
    s->timer_open = TRUE;

    s->rio->io_SerFlags = SERF_XDISABLED | SERF_RAD_BOOGIE;
    if (OpenDevice(devname, unit, (struct IORequest *)s->rio, 0) != 0)
        goto fail;
    s->ser_open = TRUE;

    s->rio->IOSer.io_Command = SDCMD_SETPARAMS;
    s->rio->io_Baud = baud;
    s->rio->io_ReadLen = 8;
    s->rio->io_WriteLen = 8;
    s->rio->io_StopBits = 1;
    s->rio->io_RBufLen = 16384;
    s->rio->io_SerFlags = SERF_XDISABLED | SERF_RAD_BOOGIE;
    s->rio->io_ExtFlags = 0;
    if (DoIO((struct IORequest *)s->rio) != 0)
        goto fail;
    s->rio->IOSer.io_Command = CMD_CLEAR;
    DoIO((struct IORequest *)s->rio);

    /* The write request is a copy of the opened read request. */
    CopyMem(s->rio, s->wio, sizeof(struct IOExtSer));
    s->wio->IOSer.io_Message.mn_ReplyPort = s->wport;
    return s;

fail:
    serlink_close(s);
    return NULL;
}

ULONG serlink_sigmask(struct SerLink *s)
{
    return 1UL << s->rport->mp_SigBit;
}

ULONG serlink_frame_errors(struct SerLink *s)
{
    return s->frame_errors;
}

/* Keep a one-byte read pending: its completion is what wakes the wait. */
static void arm(struct SerLink *s)
{
    if (s->rpending)
        return;
    s->rio->IOSer.io_Command = CMD_READ;
    s->rio->IOSer.io_Data = &s->rbyte;
    s->rio->IOSer.io_Length = 1;
    SendIO((struct IORequest *)s->rio);
    s->rpending = TRUE;
}

/* Move what serial.device has into `in`. FALSE when there is nothing. */
static BOOL fill(struct SerLink *s)
{
    ULONG n;

    s->in_pos = s->in_len = 0;
    if (s->rpending) {
        if (!CheckIO((struct IORequest *)s->rio))
            return FALSE;
        WaitIO((struct IORequest *)s->rio);
        s->rpending = FALSE;
        if (s->rio->IOSer.io_Error == 0 && s->rio->IOSer.io_Actual == 1)
            s->in[s->in_len++] = s->rbyte;
    }
    s->rio->IOSer.io_Command = SDCMD_QUERY;
    if (DoIO((struct IORequest *)s->rio) == 0 && (n = s->rio->IOSer.io_Actual) != 0) {
        if (n > sizeof(s->in) - s->in_len)
            n = sizeof(s->in) - s->in_len;
        s->rio->IOSer.io_Command = CMD_READ;
        s->rio->IOSer.io_Data = s->in + s->in_len;
        s->rio->IOSer.io_Length = n;
        if (DoIO((struct IORequest *)s->rio) == 0)
            s->in_len += s->rio->IOSer.io_Actual;
    }
    return s->in_len != 0;
}

BOOL serlink_pending(struct SerLink *s)
{
    return s->in_pos < s->in_len || (s->rpending && CheckIO((struct IORequest *)s->rio));
}

ULONG serlink_wait(struct SerLink *s, ULONG sigs, ULONG ms)
{
    ULONG rsig = 1UL << s->rport->mp_SigBit;
    ULONG tsig = 1UL << s->tport->mp_SigBit;
    ULONG got;

    if (serlink_pending(s))
        return SetSignal(0, sigs) & sigs;
    arm(s);
    s->tio->tr_node.io_Command = TR_ADDREQUEST;
    s->tio->tr_time.tv_secs = ms / 1000;
    s->tio->tr_time.tv_micro = (ms % 1000) * 1000;
    SendIO((struct IORequest *)s->tio);
    got = Wait(sigs | rsig | tsig);
    if (!CheckIO((struct IORequest *)s->tio))
        AbortIO((struct IORequest *)s->tio);
    WaitIO((struct IORequest *)s->tio);
    SetSignal(0, tsig);
    return got & sigs;
}

LONG serlink_send(struct SerLink *s, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen)
{
    UWORD crc;
    ULONG n = alen + blen;

    if (n > SERLINK_MAX)
        return -1;
    CopyMem((APTR)a, s->raw, alen);
    if (blen)
        CopyMem((APTR)b, s->raw + alen, blen);
    crc = crc16(0xffff, s->raw, n);
    s->raw[n++] = crc >> 8;
    s->raw[n++] = crc;
    s->wio->IOSer.io_Command = CMD_WRITE;
    s->wio->IOSer.io_Data = s->enc;
    s->wio->IOSer.io_Length = cobs_frame(s->raw, n, s->enc);
    return DoIO((struct IORequest *)s->wio) == 0 ? 0 : -1;
}

LONG serlink_recv(struct SerLink *s, UBYTE **dg)
{
    LONG n;

    for (;;) {
        while (s->in_pos < s->in_len) {
            UBYTE b = s->in[s->in_pos++];
            ULONG len;
            if (b != 0) {
                if (s->acc_len < sizeof(s->acc))
                    s->acc[s->acc_len++] = b;
                else
                    s->acc_over = TRUE;
                continue;
            }
            len = s->acc_len;
            s->acc_len = 0;
            if (s->acc_over) {
                s->acc_over = FALSE;
                s->frame_errors++;
                continue;
            }
            if (len == 0)
                continue;
            n = cobs_decode(s->acc, len);
            if (n < 2 || crc16(0xffff, s->acc, n - 2) != ((UWORD)s->acc[n - 2] << 8 | s->acc[n - 1])) {
                s->frame_errors++;
                continue;
            }
            *dg = s->acc;
            return n - 2;
        }
        if (!fill(s))
            break;
    }
    arm(s);
    return 0;
}

/* bplink.h: the serial link under bpclient: COBS framing, posted
 * batches. */
static APTR ops_open(const char *args)
{
    return serlink_open(args);
}

static void ops_close(APTR l)
{
    serlink_close(l);
}

static LONG ops_send(APTR l, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen)
{
    return serlink_send(l, a, alen, b, blen);
}

static LONG ops_recv(APTR l, UBYTE **dg)
{
    return serlink_recv(l, dg);
}

static ULONG ops_wait(APTR l, ULONG sigs, ULONG ms)
{
    return serlink_wait(l, sigs, ms);
}

static ULONG ops_sigmask(APTR l)
{
    return serlink_sigmask(l);
}

static ULONG ops_errors(APTR l)
{
    return serlink_frame_errors(l);
}

static void ops_log(APTR l, const UBYTE *text, UWORD len)
{
    serlink_send(l, (const UBYTE *)"LO", 2, text, len);
}

const struct BpLinkOps serlink_ops = {
    "serial", ops_open, ops_close, ops_send, ops_recv, ops_wait, ops_sigmask, ops_errors, ops_log,
    SERLINK_MAX, BP_MEM_CHUNK_SER, FALSE,
};
