/*
 * PicoZorro mpega.library: RomTag, library base, vectors. Linked first,
 * so that the first code in the file is LibNull (the library run as a
 * program returns).
 */
#include <exec/resident.h>
#include <exec/nodes.h>
#include <proto/exec.h>
#include "mpega_lib.h"

LONG LibNull(void)
{
    return -1;
}

struct ExecBase *SysBase;
struct DosLibrary *DOSBase;
struct Library *UtilityBase;
struct Device *TimerBase;

MPEGA_STREAM *MPEGA_open(REGPROTO(char *name, a0), REGPROTO(MPEGA_CTRL *ctrl, a1), REGPROTO(struct MPEGABase *base, a6));
void MPEGA_close(REGPROTO(MPEGA_STREAM *m, a0), REGPROTO(struct MPEGABase *base, a6));
LONG MPEGA_decode_frame(REGPROTO(MPEGA_STREAM *m, a0), REGPROTO(WORD **pcm, a1), REGPROTO(struct MPEGABase *base, a6));
LONG MPEGA_seek(REGPROTO(MPEGA_STREAM *m, a0), REGPROTO(ULONG ms, d0), REGPROTO(struct MPEGABase *base, a6));
LONG MPEGA_time(REGPROTO(MPEGA_STREAM *m, a0), REGPROTO(ULONG *ms, a1), REGPROTO(struct MPEGABase *base, a6));
LONG MPEGA_find_sync(REGPROTO(UBYTE *buf, a0), REGPROTO(LONG size, d0), REGPROTO(struct MPEGABase *base, a6));
LONG MPEGA_scale(REGPROTO(MPEGA_STREAM *m, a0), REGPROTO(LONG percent, d0), REGPROTO(struct MPEGABase *base, a6));

static const char lib_name[] = LIB_NAME;
static const char lib_id[] = "mpega 2.200 (" LIB_DATE ") PicoZorro, decoded on the card\r\n";
const char lib_version[] = "$VER: mpega.library 2.200 (" LIB_DATE ") PicoZorro";

static struct MPEGABase *LibInit(REGARG(struct MPEGABase *b, d0), REGARG(BPTR seglist, a0), REGARG(struct ExecBase *sys, a6))
{
    SysBase = sys;
    b->seglist = seglist;
    b->lib.lib_Node.ln_Type = NT_LIBRARY;
    b->lib.lib_Node.ln_Name = (char *)lib_name;
    b->lib.lib_Flags = LIBF_SUMUSED | LIBF_CHANGED;
    b->lib.lib_Version = LIB_VERSION;
    b->lib.lib_Revision = LIB_REVISION;
    b->lib.lib_IdString = (APTR)lib_id;
    /* Started from the card's boot ROM, this runs at romboot time,
     * before dos.library is up (priority -40 against -120): dos.library is
     * opened at the first open then. */
    DOSBase = (struct DosLibrary *)OpenLibrary("dos.library", 37);
    UtilityBase = OpenLibrary("utility.library", 37);
    if (!UtilityBase) {
        if (DOSBase)
            CloseLibrary((struct Library *)DOSBase);
        FreeMem((UBYTE *)b - b->lib.lib_NegSize, b->lib.lib_NegSize + b->lib.lib_PosSize);
        return NULL;
    }
    return b;
}

static BPTR LibExpunge(REGARG(struct MPEGABase *b, a6))
{
    BPTR seg;
    if (b->lib.lib_OpenCnt) {
        b->lib.lib_Flags |= LIBF_DELEXP;
        return 0;
    }
    seg = b->seglist;
    Remove(&b->lib.lib_Node);
    CloseLibrary(UtilityBase);
    if (DOSBase)
        CloseLibrary((struct Library *)DOSBase);
    FreeMem((UBYTE *)b - b->lib.lib_NegSize, b->lib.lib_NegSize + b->lib.lib_PosSize);
    return seg;
}

static struct MPEGABase *LibOpen(REGARG(struct MPEGABase *b, a6))
{
    if (!DOSBase && !(DOSBase = (struct DosLibrary *)OpenLibrary("dos.library", 37)))
        return NULL;
    b->lib.lib_OpenCnt++;
    b->lib.lib_Flags &= ~LIBF_DELEXP;
    return b;
}

static BPTR LibClose(REGARG(struct MPEGABase *b, a6))
{
    if (--b->lib.lib_OpenCnt == 0 && (b->lib.lib_Flags & LIBF_DELEXP))
        return LibExpunge(b);
    return 0;
}

static const APTR lib_vectors[] = {
    (APTR)LibOpen, (APTR)LibClose, (APTR)LibExpunge, (APTR)LibNull,
    (APTR)MPEGA_open, (APTR)MPEGA_close, (APTR)MPEGA_decode_frame, (APTR)MPEGA_seek,
    (APTR)MPEGA_time, (APTR)MPEGA_find_sync, (APTR)MPEGA_scale,
    (APTR)-1,
};

static const ULONG lib_init[4] = {
    sizeof(struct MPEGABase), (ULONG)lib_vectors, 0, (ULONG)LibInit,
};

const struct Resident lib_romtag = {
    RTC_MATCHWORD,
    (struct Resident *)&lib_romtag,
    (APTR)(&lib_romtag + 1),
    RTF_AUTOINIT,
    LIB_VERSION,
    NT_LIBRARY,
    0,
    (char *)lib_name,
    (char *)lib_id,
    (APTR)lib_init,
};
