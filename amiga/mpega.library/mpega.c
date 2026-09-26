/*
 * PicoZorro mpega.library: the mpega.library 2.x API with the decoding on
 * the card (docs/REGISTERS-MPEG.md). The 68k does what the original does
 * besides decoding: file or hook I/O, sync and ID3v2 skipping, header and
 * Xing parsing, seek arithmetic, time. The card decodes and shapes the
 * output (freq_div, mono, gain) and returns one record per frame, so
 * MPEGA_decode_frame returns one frame per call like the original (0 for a
 * frame without samples).
 */
#include <exec/memory.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/utility.h>
#include "mpega_lib.h"

#define PROBE_BYTES (64 * 1024)
/* decode_frame gives up after this long without a record (Delay ticks). */
#define WAIT_TICKS  150

/* ---- MPEG audio headers ---- */

struct Hdr {
    UBYTE v;          /* 0: MPEG-1, 1: MPEG-2, 2: MPEG-2.5 */
    UBYTE layer;      /* 1..3 */
    UBYTE mode, pad, priv, copyright, original, crc;
    UWORD bitrate;    /* kbps */
    UWORD spf;        /* samples per frame */
    ULONG rate;       /* Hz */
    ULONG len;        /* frame bytes */
};

static const UWORD bitrates[2][3][15] = {
    { { 0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448 },
      { 0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384 },
      { 0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320 } },
    { { 0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256 },
      { 0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160 },
      { 0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160 } },
};
static const ULONG rates[3][3] = { { 44100, 48000, 32000 }, { 22050, 24000, 16000 }, { 11025, 12000, 8000 } };

/* As the original's SYNC_VALID: sync, a known version and layer, a real
 * bitrate (no free format), a known rate. */
static BOOL parse_hdr(const UBYTE *p, struct Hdr *h)
{
    UBYTE ver, lb, bri, sri;
    if (p[0] != 0xff || (p[1] & 0xe0) != 0xe0)
        return FALSE;
    ver = (p[1] >> 3) & 3;
    lb = (p[1] >> 1) & 3;
    bri = p[2] >> 4;
    sri = (p[2] >> 2) & 3;
    if (ver == 1 || lb == 0 || bri == 0 || bri == 15 || sri == 3)
        return FALSE;
    h->v = ver == 3 ? 0 : ver == 2 ? 1 : 2;
    h->layer = 4 - lb;
    h->crc = !(p[1] & 1);
    h->pad = (p[2] >> 1) & 1;
    h->priv = p[2] & 1;
    h->mode = p[3] >> 6;
    h->copyright = (p[3] >> 3) & 1;
    h->original = (p[3] >> 2) & 1;
    h->bitrate = bitrates[h->v ? 1 : 0][h->layer - 1][bri];
    h->rate = rates[h->v][sri];
    h->spf = h->layer == 1 ? 384 : (h->layer == 3 && h->v) ? 576 : 1152;
    if (h->layer == 1)
        h->len = (12 * (ULONG)h->bitrate * 1000 / h->rate + h->pad) * 4;
    else
        h->len = (ULONG)(h->spf / 8) * h->bitrate * 1000 / h->rate + h->pad;
    return TRUE;
}

static BOOL same_stream(const struct Hdr *a, const struct Hdr *b)
{
    return a->v == b->v && a->layer == b->layer && a->rate == b->rate;
}

/* a * b / c without overflow while b * c < 2^32. */
static ULONG muldiv(ULONG a, ULONG b, ULONG c)
{
    return (a / c) * b + (a % c) * b / c;
}

/* Rates are all multiples of 25 Hz: ms = frames * spf * 1000 / rate. */
static ULONG frames_to_ms(struct Stream *s, ULONG frames)
{
    return muldiv(frames, (ULONG)s->spf * 40, s->pub.frequency / 25);
}

static ULONG ms_to_frames(struct Stream *s, ULONG ms)
{
    return muldiv(ms, s->pub.frequency / 25, (ULONG)s->spf * 40);
}

static void fill_fields(struct Stream *s, const UBYTE *p)
{
    struct Hdr h;
    if (!parse_hdr(p, &h))
        return;
    s->pub.norm = h.v ? 2 : 1;
    s->pub.layer = h.layer;
    s->pub.mode = h.mode;
    if (!s->xing_frames)
        s->pub.bitrate = h.bitrate;
}

/* ---- the source: a file or the application's hook ---- */

static LONG src_call(struct Stream *s, MPEGA_ACCESS *a, APTR obj)
{
    return (LONG)CallHookPkt(s->hook, obj, a);
}

static BOOL src_open(struct Stream *s, char *name)
{
    if (s->hook) {
        MPEGA_ACCESS a;
        a.func = MPEGA_BSFUNC_OPEN;
        a.data.open.stream_name = name;
        a.data.open.buffer_size = s->ctrl.stream_buffer_size ? s->ctrl.stream_buffer_size & ~3 : 16384;
        a.data.open.stream_size = 0;
        s->handle = (APTR)src_call(s, &a, NULL);
        s->size = a.data.open.stream_size;
        return s->handle != NULL;
    }
    if (!name || !(s->fh = Open((STRPTR)name, MODE_OLDFILE)))
        return FALSE;
    Seek(s->fh, 0, OFFSET_END);
    s->size = Seek(s->fh, 0, OFFSET_BEGINNING);
    if (s->size < 0)
        s->size = 0;
    return TRUE;
}

static void src_close(struct Stream *s)
{
    if (s->hook && s->handle) {
        MPEGA_ACCESS a;
        a.func = MPEGA_BSFUNC_CLOSE;
        src_call(s, &a, s->handle);
        s->handle = NULL;
    }
    if (s->fh) {
        Close(s->fh);
        s->fh = 0;
    }
}

static LONG src_read(struct Stream *s, UBYTE *buf, LONG n)
{
    LONG r;
    if (s->hook) {
        MPEGA_ACCESS a;
        a.func = MPEGA_BSFUNC_READ;
        a.data.read.buffer = buf;
        a.data.read.num_bytes = n;
        r = src_call(s, &a, s->handle);
    } else {
        r = Read(s->fh, buf, n);
    }
    if (r > 0)
        s->pos += r;
    return r;
}

static BOOL src_seek(struct Stream *s, LONG pos)
{
    if (s->hook) {
        MPEGA_ACCESS a;
        a.func = MPEGA_BSFUNC_SEEK;
        a.data.seek.abs_byte_seek_pos = pos;
        if (src_call(s, &a, s->handle) != 0)
            return FALSE;
    } else if (Seek(s->fh, pos, OFFSET_BEGINNING) < 0) {
        return FALSE;
    }
    s->pos = pos;
    return TRUE;
}

/* ---- open: sync, header, Xing ---- */

static ULONG be32(const UBYTE *p)
{
    return (ULONG)p[0] << 24 | (ULONG)p[1] << 16 | (ULONG)p[2] << 8 | p[3];
}

static void parse_xing(struct Stream *s, const UBYTE *f, ULONG avail, const struct Hdr *h)
{
    ULONG o = 4 + (h->crc ? 2 : 0) + (h->v == 0 ? (h->mode == 3 ? 17 : 32) : (h->mode == 3 ? 9 : 17));
    ULONG fl;
    if (o + 8 > avail || o + 8 > h->len)
        return;
    if (!((f[o] == 'X' && f[o + 1] == 'i' && f[o + 2] == 'n' && f[o + 3] == 'g') ||
          (f[o] == 'I' && f[o + 1] == 'n' && f[o + 2] == 'f' && f[o + 3] == 'o')))
        return;
    fl = be32(f + o + 4);
    o += 8;
    if ((fl & 1) && o + 4 <= avail) {
        s->xing_frames = be32(f + o);
        o += 4;
    }
    if ((fl & 2) && o + 4 <= avail) {
        s->xing_bytes = be32(f + o);
        o += 4;
    }
    if ((fl & 4) && o + 100 <= avail) {
        CopyMem((APTR)(f + o), s->toc, 100);
        s->has_toc = TRUE;
    }
}

/* Find the first frame: skip an ID3v2 tag, then two headers in a row that
 * agree (as the original's synchronize()). Fills the stream fields. */
static BOOL find_start(struct Stream *s)
{
    UBYTE *p = AllocVec(PROBE_BYTES, MEMF_ANY);
    LONG base = 0, n, i;
    struct Hdr h, h2;
    BOOL ok = FALSE;

    if (!p)
        return FALSE;
    n = src_read(s, p, PROBE_BYTES);
    if (n >= 10 && p[0] == 'I' && p[1] == 'D' && p[2] == '3') {
        base = 10 + ((LONG)(p[6] & 0x7f) << 21 | (LONG)(p[7] & 0x7f) << 14 | (p[8] & 0x7f) << 7 | (p[9] & 0x7f));
        if (p[5] & 0x10)
            base += 10;
        n = src_seek(s, base) ? src_read(s, p, PROBE_BYTES) : 0;
    }
    for (i = 0; i + 4 <= n; i++) {
        if (!parse_hdr(p + i, &h))
            continue;
        if ((ULONG)i + h.len + 4 <= (ULONG)n) {
            if (!parse_hdr(p + i + h.len, &h2) || !same_stream(&h, &h2))
                continue;
        } else if (!s->size || base + i + (LONG)h.len < s->size) {
            continue;   /* no second header in the probe and not the last frame */
        }
        ok = TRUE;
        break;
    }
    if (ok) {
        s->start = base + i;
        s->spf = h.spf;
        s->pub.norm = h.v ? 2 : 1;
        s->pub.layer = h.layer;
        s->pub.mode = h.mode;
        s->pub.bitrate = h.bitrate;
        s->pub.frequency = h.rate;
        s->pub.channels = h.mode == 3 ? 1 : 2;
        s->pub.private_bit = h.priv;
        s->pub.copyright = h.copyright;
        s->pub.original = h.original;
        parse_xing(s, p + i, n - i, &h);
        if (s->xing_frames) {
            s->pub.ms_duration = frames_to_ms(s, s->xing_frames);
            if (s->xing_bytes && s->pub.ms_duration)
                s->pub.bitrate = (WORD)muldiv(s->xing_bytes, 8, s->pub.ms_duration);
        } else if (s->size > s->start && h.bitrate) {
            s->pub.ms_duration = muldiv(s->size - s->start, 8, h.bitrate);
        }
    }
    FreeVec(p);
    return ok;
}

/* The output as MPEGA_CTRL asks: the layer's settings, mono or stereo by
 * the source's channels (as the original), freq_div 0 = automatic. */
static void choose_output(struct Stream *s)
{
    MPEGA_LAYER *l = s->pub.layer == 3 ? &s->ctrl.layer_3 : &s->ctrl.layer_1_2;
    MPEGA_OUTPUT *o = s->pub.channels == 2 ? &l->stereo : &l->mono;
    LONG div = o->freq_div, q = o->quality;
    BOOL mono = l->force_mono && s->pub.channels == 2;

    if (div == 0) {
        div = 1;
        while (div < 4 && o->freq_max > 0 && s->pub.frequency / div > (ULONG)o->freq_max)
            div *= 2;
    } else if (div != 2 && div != 4) {
        div = 1;
    }
    if (q < 0)
        q = 0;
    if (q > 2)
        q = 2;
    s->config = (div == 2 ? 1 : div == 4 ? 2 : 0) | (mono ? CFG_MONO : 0);
    s->pub.dec_channels = mono ? 1 : s->pub.channels;
    s->pub.dec_frequency = s->pub.frequency / div;
    s->pub.dec_quality = q;
}

static const MPEGA_CTRL default_ctrl = {
    NULL,
    { 0, { 1, 2, 44100 }, { 1, 2, 44100 } },
    { 0, { 1, 2, 44100 }, { 1, 2, 44100 } },
    0, 16384,
};

static void free_stream(struct Stream *s)
{
    if (s->card.win || s->card.bp) {
        card_stop(&s->card);
        card_close(&s->card);
    }
    src_close(s);
    FreeVec(s->buf);
    FreeVec(s);
}

/* ---- the library functions ---- */

MPEGA_STREAM *MPEGA_open(REGARG(char *name, a0), REGARG(MPEGA_CTRL *ctrl, a1), REGARG(struct MPEGABase *base, a6))
{
    struct Stream *s = AllocVec(sizeof(*s), MEMF_PUBLIC | MEMF_CLEAR);
    if (!s)
        return NULL;
    s->ctrl = ctrl ? *ctrl : default_ctrl;
    s->hook = s->ctrl.bs_access;
    s->scale = 100;
    s->buf = AllocVec(IN_CHUNK, MEMF_PUBLIC);
    if (!s->buf || !src_open(s, name) || !find_start(s) || !src_seek(s, s->start)) {
        free_stream(s);
        return NULL;
    }
    choose_output(s);
    if (card_open(&s->card) != 0) {
        free_stream(s);
        return NULL;
    }
    card_start(&s->card, s->config, s->scale);
    s->pub.handle = s;
    return &s->pub;
}

void MPEGA_close(REGARG(MPEGA_STREAM *m, a0), REGARG(struct MPEGABase *base, a6))
{
    if (m)
        free_stream((struct Stream *)m);
}

/* Input while the card has room: whole chunks, then the end marker. */
static void feed(struct Stream *s, UWORD status)
{
    UWORD free = (status >> 8) & 0xf;
    while (free-- && !s->end_sent) {
        LONG n = src_read(s, s->buf, IN_CHUNK);
        if (n <= 0) {
            card_feed(&s->card, s->buf, 0);
            s->end_sent = TRUE;
        } else {
            card_feed(&s->card, s->buf, n);
        }
    }
}

LONG MPEGA_decode_frame(REGARG(MPEGA_STREAM *m, a0), REGARG(WORD **pcm, a1), REGARG(struct MPEGABase *base, a6))
{
    struct Stream *s = (struct Stream *)m;
    UBYTE hdr[REC_HDR];
    ULONG idle = 0, ticks = 0;

    if (!s || !pcm)
        return MPEGA_ERR_BADVALUE;
    if (s->ended)
        return MPEGA_ERR_EOF;          /* at once, as the original */
    for (;;) {
        UWORD st = card_status(&s->card);
        if (st == 0xffff)
            return MPEGA_ERR_EOF;          /* the card is gone */
        feed(s, st);
        if (st & 0xff) {
            UWORD flags, n;
            if (card_record(&s->card, hdr, pcm[0], pcm[1], MPEGA_PCM_SIZE) == 0)
                continue;
            if (((UWORD)hdr[12] << 8 | hdr[13]) != s->card.session)
                continue;                  /* the stream before a seek */
            flags = (UWORD)hdr[0] << 8 | hdr[1];
            if (flags & REC_END) {
                s->ended = TRUE;
                return MPEGA_ERR_EOF;
            }
            n = (UWORD)hdr[2] << 8 | hdr[3];
            fill_fields(s, hdr + 8);
            s->frames++;
            return n;
        }
        /* The card decodes ahead, so this is rare: poll a while, then
         * sleep a tick at a time. */
        if (s->card.win && ++idle < 2000)
            continue;
        idle = 0;
        if (++ticks > WAIT_TICKS)
            return MPEGA_ERR_EOF;
        Delay(1);
    }
}

LONG MPEGA_seek(REGARG(MPEGA_STREAM *m, a0), REGARG(ULONG ms, d0), REGARG(struct MPEGABase *base, a6))
{
    struct Stream *s = (struct Stream *)m;
    ULONG off;

    if (!s)
        return MPEGA_ERR_BADVALUE;
    if (s->pub.ms_duration && ms > s->pub.ms_duration)
        return MPEGA_ERR_EOF;
    if (s->has_toc && s->xing_bytes && s->pub.ms_duration) {
        /* The TOC: 100 points, byte position / 256 of the stream. */
        ULONG hund = s->pub.ms_duration / 100 ? s->pub.ms_duration / 100 : 1;
        ULONG pct = muldiv(ms, 256, hund);                    /* percent x 256 */
        ULONG a = pct >> 8, frac = pct & 0xff, fa, fb, pos;
        if (a > 99) {
            a = 99;
            frac = 0xff;
        }
        fa = s->toc[a];
        fb = a < 99 ? s->toc[a + 1] : 256;
        pos = fa * 256 + (fb - fa) * frac;                   /* x 65536 */
        off = muldiv(s->xing_bytes, pos >> 4, 4096);
    } else {
        off = muldiv(ms, s->pub.bitrate, 8);
    }
    if (s->size && s->start + (LONG)off >= s->size)
        return MPEGA_ERR_EOF;
    if (!src_seek(s, s->start + off))
        return MPEGA_ERR_EOF;
    s->end_sent = FALSE;
    s->ended = FALSE;
    card_start(&s->card, s->config, s->scale);
    s->frames = ms_to_frames(s, ms);
    return MPEGA_ERR_NONE;
}

LONG MPEGA_time(REGARG(MPEGA_STREAM *m, a0), REGARG(ULONG *ms, a1), REGARG(struct MPEGABase *base, a6))
{
    struct Stream *s = (struct Stream *)m;
    if (!s || !ms)
        return MPEGA_ERR_BADVALUE;
    *ms = frames_to_ms(s, s->frames);
    return MPEGA_ERR_NONE;
}

LONG MPEGA_find_sync(REGARG(UBYTE *buf, a0), REGARG(LONG size, d0), REGARG(struct MPEGABase *base, a6))
{
    struct Hdr h;
    LONG i;
    for (i = 0; buf && i + 4 <= size; i++)
        if (parse_hdr(buf + i, &h))
            return i;
    return MPEGA_ERR_NO_SYNC;
}

LONG MPEGA_scale(REGARG(MPEGA_STREAM *m, a0), REGARG(LONG percent, d0), REGARG(struct MPEGABase *base, a6))
{
    struct Stream *s = (struct Stream *)m;
    if (!s || percent < 1 || percent > 800)
        return MPEGA_ERR_BADVALUE;
    s->scale = (UWORD)percent;
    card_scale(&s->card, s->scale);
    return MPEGA_ERR_NONE;
}
