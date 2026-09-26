/*
 * mpegtest: drives mpega.library as a player does and dumps what
 * MPEGA_decode_frame returns, for comparison against a reference decoder.
 *
 *   mpegtest <file> <out> [freq_div [mono [seek_ms [hook]]]]
 *
 * out: per call a big-endian LONG (the return value), then for n > 0 the
 * n samples of channel 0 and, with dec_channels 2, the n of channel 1.
 * seek_ms > 0: after 20 frames, MPEGA_seek there and carry on. hook = 1:
 * the file through a bitstream hook, opened with a NULL name (as RiVA).
 */
#include <exec/memory.h>
#include <dos/dos.h>
#include <utility/hooks.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <stdio.h>
#include <stdlib.h>
#include "../common/compiler.h"
#include "libraries/mpega.h"

struct Library *MPEGABase;

/* The library's LVOs (mpega.fd, bias 30). */
static MPEGA_STREAM *m_open(char *name, MPEGA_CTRL *ctrl)
{
    register MPEGA_STREAM *r __asm("d0");
    register char *a0 __asm("a0") = name;
    register MPEGA_CTRL *a1 __asm("a1") = ctrl;
    register struct Library *a6 __asm("a6") = MPEGABase;
    __asm volatile("jsr -30(%%a6)" : "=r"(r), "+r"(a0), "+r"(a1) : "r"(a6) : "d1", "cc", "memory");
    return r;
}

static void m_close(MPEGA_STREAM *m)
{
    register MPEGA_STREAM *a0 __asm("a0") = m;
    register struct Library *a6 __asm("a6") = MPEGABase;
    __asm volatile("jsr -36(%%a6)" : "+r"(a0) : "r"(a6) : "d0", "d1", "a1", "cc", "memory");
}

static LONG m_decode(MPEGA_STREAM *m, WORD **pcm)
{
    register LONG r __asm("d0");
    register MPEGA_STREAM *a0 __asm("a0") = m;
    register WORD **a1 __asm("a1") = pcm;
    register struct Library *a6 __asm("a6") = MPEGABase;
    __asm volatile("jsr -42(%%a6)" : "=r"(r), "+r"(a0), "+r"(a1) : "r"(a6) : "d1", "cc", "memory");
    return r;
}

static LONG m_seek(MPEGA_STREAM *m, ULONG ms)
{
    register LONG r __asm("d0") = ms;
    register MPEGA_STREAM *a0 __asm("a0") = m;
    register struct Library *a6 __asm("a6") = MPEGABase;
    __asm volatile("jsr -48(%%a6)" : "+r"(r), "+r"(a0) : "r"(a6) : "d1", "a1", "cc", "memory");
    return r;
}

static LONG m_time(MPEGA_STREAM *m, ULONG *ms)
{
    register LONG r __asm("d0");
    register MPEGA_STREAM *a0 __asm("a0") = m;
    register ULONG *a1 __asm("a1") = ms;
    register struct Library *a6 __asm("a6") = MPEGABase;
    __asm volatile("jsr -54(%%a6)" : "=r"(r), "+r"(a0), "+r"(a1) : "r"(a6) : "d1", "cc", "memory");
    return r;
}

/* The bitstream hook: a2 = handle (the BPTR), a1 = MPEGA_ACCESS. */
static char *hook_file;

static ULONG hook_entry(REGARG(struct Hook *h, a0), REGARG(BPTR fh, a2), REGARG(MPEGA_ACCESS *a, a1))
{
    switch (a->func) {
    case MPEGA_BSFUNC_OPEN: {
        BPTR f = Open((STRPTR)hook_file, MODE_OLDFILE);
        if (f) {
            Seek(f, 0, OFFSET_END);
            a->data.open.stream_size = Seek(f, 0, OFFSET_BEGINNING);
        }
        return (ULONG)f;
    }
    case MPEGA_BSFUNC_CLOSE:
        Close(fh);
        return 0;
    case MPEGA_BSFUNC_READ:
        return (ULONG)Read(fh, a->data.read.buffer, a->data.read.num_bytes);
    case MPEGA_BSFUNC_SEEK:
        return Seek(fh, a->data.seek.abs_byte_seek_pos, OFFSET_BEGINNING) < 0 ? 1 : 0;
    }
    return 0;
}

static struct Hook hook = { { NULL, NULL }, (ULONG (*)())hook_entry, NULL, NULL };

int main(int argc, char **argv)
{
    MPEGA_CTRL ctrl;
    MPEGA_STREAM *m;
    WORD *pcm[2];
    FILE *out;
    LONG div = argc > 3 ? atol(argv[3]) : 1, n, frames = 0, zero = 0, calls = 0;
    WORD mono = argc > 4 ? atol(argv[4]) : 0;
    ULONG seek_ms = argc > 5 ? atol(argv[5]) : 0, t = 0;
    BOOL use_hook = argc > 6 && atol(argv[6]);
    int rc = 20;

    if (argc < 3) {
        printf("usage: mpegtest <file> <out> [freq_div [mono [seek_ms [hook]]]]\n");
        return 20;
    }
    if (!(MPEGABase = OpenLibrary("mpega.library", 2))) {
        printf("mpegtest: no mpega.library 2\n");
        return 20;
    }
    printf("mpegtest: %s\n", (char *)MPEGABase->lib_IdString);
    ctrl.bs_access = use_hook ? &hook : NULL;
    hook_file = argv[1];
    ctrl.layer_1_2.force_mono = ctrl.layer_3.force_mono = mono;
    ctrl.layer_1_2.mono.freq_div = ctrl.layer_1_2.stereo.freq_div = div;
    ctrl.layer_3.mono.freq_div = ctrl.layer_3.stereo.freq_div = div;
    ctrl.layer_1_2.mono.quality = ctrl.layer_1_2.stereo.quality = 2;
    ctrl.layer_3.mono.quality = ctrl.layer_3.stereo.quality = 2;
    ctrl.layer_1_2.mono.freq_max = ctrl.layer_1_2.stereo.freq_max = 48000;
    ctrl.layer_3.mono.freq_max = ctrl.layer_3.stereo.freq_max = 48000;
    ctrl.check_mpeg = 1;
    ctrl.stream_buffer_size = 0;
    pcm[0] = AllocVec(2 * MPEGA_PCM_SIZE * sizeof(WORD), MEMF_ANY);
    pcm[1] = pcm[0] + MPEGA_PCM_SIZE;
    out = fopen(argv[2], "wb");
    m = m_open(use_hook ? NULL : argv[1], &ctrl);
    if (!m || !out || !pcm[0]) {
        printf("mpegtest: MPEGA_open failed (no card, or not MPEG audio)\n");
        goto done;
    }
    printf("mpegtest: MPEG-%ld layer %ld, %ld kbps, %ld Hz, %ld ch, mode %ld, %lu ms; out %ld Hz %ld ch q%ld%s\n",
           (LONG)m->norm, (LONG)m->layer, (LONG)m->bitrate, m->frequency, (LONG)m->channels, (LONG)m->mode,
           m->ms_duration, m->dec_frequency, (LONG)m->dec_channels, (LONG)m->dec_quality, use_hook ? ", hook" : "");
    for (;;) {
        LONG be;
        n = m_decode(m, pcm);
        calls++;
        be = n;
        fwrite(&be, 4, 1, out);
        if (n < 0 && n != MPEGA_ERR_BADFRAME)
            break;
        if (n > 0) {
            fwrite(pcm[0], 2, n, out);
            if (m->dec_channels == 2)
                fwrite(pcm[1], 2, n, out);
            frames++;
        } else if (n == 0) {
            zero++;
        }
        if (seek_ms && frames == 20) {
            LONG r = m_seek(m, seek_ms);
            m_time(m, &t);
            printf("mpegtest: seek %lu ms -> %ld, time %lu ms\n", seek_ms, r, t);
            seek_ms = 0;
            frames++;       /* only once */
        }
    }
    m_time(m, &t);
    printf("mpegtest: %ld calls, %ld frames, %ld empty, end %ld, time %lu ms\n", calls, frames, zero, n, t);
    rc = n == MPEGA_ERR_EOF ? 0 : 10;
done:
    if (m)
        m_close(m);
    if (out)
        fclose(out);
    FreeVec(pcm[0]);
    CloseLibrary(MPEGABase);
    return rc;
}
