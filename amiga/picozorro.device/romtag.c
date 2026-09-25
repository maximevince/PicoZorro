/*
 * picozorro.device: RomTag and autoinit tables. Linked first, so that the
 * first code in the file is LibNull (a device run as a program returns).
 */
#include <exec/resident.h>
#include <exec/nodes.h>
#include "device.h"

LONG LibNull(void)
{
    return -1;
}

struct PZBase *DevInit(REGPROTO(struct PZBase *pz, d0), REGPROTO(BPTR seglist, a0), REGPROTO(struct ExecBase *sys, a6));
void DevOpen(REGPROTO(struct IOSana2Req *io, a1), REGPROTO(ULONG unit, d0), REGPROTO(ULONG flags, d1),
             REGPROTO(struct PZBase *pz, a6));
BPTR DevClose(REGPROTO(struct IOSana2Req *io, a1), REGPROTO(struct PZBase *pz, a6));
BPTR DevExpunge(REGPROTO(struct PZBase *pz, a6));
LONG DevNull(void);
void DevBeginIO(REGPROTO(struct IOSana2Req *io, a1), REGPROTO(struct PZBase *pz, a6));
LONG DevAbortIO(REGPROTO(struct IOSana2Req *io, a1), REGPROTO(struct PZBase *pz, a6));

static const char dev_name[] = DEVICE_NAME;
static const char dev_id[] = "picozorro.device 1.0 (" DEVICE_DATE ")\r\n";
const char dev_version[] = "$VER: picozorro.device 1.0 (" DEVICE_DATE ")";

static const APTR dev_vectors[] = {
    (APTR)DevOpen, (APTR)DevClose, (APTR)DevExpunge, (APTR)DevNull,
    (APTR)DevBeginIO, (APTR)DevAbortIO, (APTR)-1,
};

static const ULONG dev_init[4] = {
    sizeof(struct PZBase), (ULONG)dev_vectors, 0, (ULONG)DevInit,
};

const struct Resident dev_romtag = {
    RTC_MATCHWORD,
    (struct Resident *)&dev_romtag,
    (APTR)(&dev_romtag + 1),
    RTF_AUTOINIT,
    DEVICE_VERSION,
    NT_DEVICE,
    0,
    (char *)dev_name,
    (char *)dev_id,
    (APTR)dev_init,
};
