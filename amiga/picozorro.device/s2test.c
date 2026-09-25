/*
 * s2test: command-level test of picozorro.device. Run with ENV:PZNET =
 * "loop": every written frame comes back to us with the MAC addresses
 * swapped, so reads, writes, events and error paths can be checked with no
 * network. Prints one PASS/FAIL line per check and a
 * summary; RC 0 when all pass.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/errors.h>
#include <devices/sana2.h>
#include <devices/newstyle.h>
#include <utility/tagitem.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <clib/alib_protos.h>
#include <stdio.h>
#include <string.h>
#include "../common/compiler.h"

#define DEV "Networks/picozorro.device"
#define TYPE_TEST 0x88b5

static int fails, passes;

static void check(const char *what, int ok, long a, long b)
{
    printf("%s  %s", ok ? "PASS" : "FAIL", what);
    if (!ok)
        printf("  (got %ld, want %ld)", a, b);
    printf("\n");
    if (ok)
        passes++;
    else
        fails++;
}
static void check_eq(const char *what, long got, long want)
{
    check(what, got == want, got, want);
}
#define CHECK_EQ(what, got, want) check_eq(what, (long)(got), (long)(want))

/* Our "abstract buffers" are plain byte arrays of 1536 bytes. */
static BOOL copy_to(REGARG(APTR to, a0), REGARG(APTR from, a1), REGARG(ULONG n, d0))
{
    if (n > 1536)
        return FALSE;
    CopyMem(from, to, n);
    return TRUE;
}

static BOOL copy_from(REGARG(APTR to, a0), REGARG(APTR from, a1), REGARG(ULONG n, d0))
{
    CopyMem(from, to, n);
    return TRUE;
}

static BOOL copy_fail(REGARG(APTR to, a0), REGARG(APTR from, a1), REGARG(ULONG n, d0))
{
    return FALSE;
}

static struct TagItem tags[] = {
    {S2_CopyToBuff, (ULONG)copy_to},
    {S2_CopyFromBuff, (ULONG)copy_from},
    {TAG_DONE, 0},
};
static struct TagItem tags_badrx[] = {
    {S2_CopyToBuff, (ULONG)copy_fail},
    {S2_CopyFromBuff, (ULONG)copy_from},
    {TAG_DONE, 0},
};
static struct TagItem tags_none[] = {{TAG_DONE, 0}};

static struct MsgPort *port;

static struct IOSana2Req *new_req(void)
{
    return (struct IOSana2Req *)CreateIORequest(port, sizeof(struct IOSana2Req));
}

static struct IOSana2Req *clone(struct IOSana2Req *base)
{
    struct IOSana2Req *r = new_req();
    if (r)
        CopyMem(base, r, sizeof(*r));
    return r;
}

static BYTE opendev(struct IOSana2Req *r, struct TagItem *t, ULONG flags)
{
    r->ios2_BufferManagement = t;
    return OpenDevice(DEV, 0, (struct IORequest *)r, flags);
}

static BYTE cmd(struct IOSana2Req *r, UWORD c)
{
    r->ios2_Req.io_Command = c;
    r->ios2_Req.io_Flags = 0;
    return DoIO((struct IORequest *)r);
}

static void start_read(struct IOSana2Req *r, ULONG type, UBYTE *buf, BOOL raw)
{
    r->ios2_Req.io_Command = CMD_READ;
    r->ios2_Req.io_Flags = raw ? SANA2IOF_RAW : 0;
    r->ios2_PacketType = type;
    r->ios2_Data = buf;
    /* BeginIO, not SendIO: SendIO clears io_Flags (and SANA2IOF_RAW with it). */
    BeginIO((struct IORequest *)r);
}

static BOOL done_within(struct IOSana2Req *r, int ticks)
{
    while (ticks-- > 0) {
        if (CheckIO((struct IORequest *)r))
            return TRUE;
        Delay(1);
    }
    return CheckIO((struct IORequest *)r) != NULL;
}

static BYTE write_frame(struct IOSana2Req *w, const UBYTE *dst, ULONG type, UBYTE *data, ULONG n, UWORD c)
{
    w->ios2_Req.io_Command = c;
    w->ios2_Req.io_Flags = 0;
    memcpy(w->ios2_DstAddr, dst, 6);
    w->ios2_PacketType = type;
    w->ios2_Data = data;
    w->ios2_DataLength = n;
    return DoIO((struct IORequest *)w);
}

int main(void)
{
    static UBYTE rxbuf[1536], rxbuf2[1536], txbuf[1536];
    struct IOSana2Req *a, *b, *w, *r, *r2, *ev;
    struct Sana2DeviceQuery q;
    struct NSDeviceQueryResult nsq;
    struct Sana2DeviceStats st;
    struct Sana2PacketTypeStats ts;
    UBYTE mac[6], peer[6] = {0x02, 0x11, 0x22, 0x33, 0x44, 0x55};
    ULONG i;
    BYTE err;

    port = CreateMsgPort();
    a = new_req();
    b = new_req();
    if (!port || !a || !b) {
        printf("FAIL  no memory\n");
        return 20;
    }

    /* --- open rules */
    err = opendev(b, tags_none, 0);
    CHECK_EQ("open without copy hooks is refused", err, IOERR_OPENFAIL);
    if (!err)
        CloseDevice((struct IORequest *)b);
    err = opendev(a, tags, 0);
    CHECK_EQ("open unit 0", err, 0);
    if (err) {
        printf("cannot continue\n");
        return 20;
    }
    err = opendev(b, tags, SANA2OPF_MINE);
    CHECK_EQ("exclusive open while open is refused", err, IOERR_OPENFAIL);
    if (!err)
        CloseDevice((struct IORequest *)b);
    b->ios2_Req.io_Message.mn_Length = sizeof(struct IOStdReq);
    err = opendev(b, tags, 0);
    CHECK_EQ("open with a short request is refused", err, IOERR_OPENFAIL);
    if (!err)
        CloseDevice((struct IORequest *)b);
    b->ios2_Req.io_Message.mn_Length = sizeof(struct IOSana2Req);

    /* --- queries */
    memset(&q, 0xee, sizeof(q));
    q.SizeAvailable = 18; /* up to and including DeviceLevel + AddrFieldSize */
    a->ios2_StatData = &q;
    CHECK_EQ("S2_DEVICEQUERY", cmd(a, S2_DEVICEQUERY), 0);
    CHECK_EQ("  SizeSupplied honours SizeAvailable", q.SizeSupplied, 18);
    CHECK_EQ("  AddrFieldSize 48", q.AddrFieldSize, 48);
    CHECK_EQ("  MTU not written past SizeAvailable", q.MTU, 0xeeeeeeee);
    q.SizeAvailable = sizeof(q);
    cmd(a, S2_DEVICEQUERY);
    CHECK_EQ("  MTU 1500", q.MTU, 1500);
    CHECK_EQ("  RawMTU 1514", q.RawMTU, 1514);
    CHECK_EQ("  HardwareType Ethernet", q.HardwareType, S2WireType_Ethernet);

    {
        struct IOStdReq *s = (struct IOStdReq *)a;
        s->io_Command = NSCMD_DEVICEQUERY;
        s->io_Data = &nsq;
        s->io_Length = sizeof(nsq);
        s->io_Flags = 0;
        err = DoIO((struct IORequest *)s);
        CHECK_EQ("NSCMD_DEVICEQUERY", err, 0);
        CHECK_EQ("  device type SANA-II", nsq.nsdqr_DeviceType, NSDEVTYPE_SANA2);
    }

    CHECK_EQ("unknown command -> IOERR_NOCMD", cmd(a, 0x7777), IOERR_NOCMD);

    CHECK_EQ("S2_GETSTATIONADDRESS", cmd(a, S2_GETSTATIONADDRESS), 0);
    memcpy(mac, a->ios2_DstAddr, 6);
    printf("      station address %02x:%02x:%02x:%02x:%02x:%02x\n", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    /* --- offline rules */
    w = clone(a);
    r = clone(a);
    r2 = clone(a);
    ev = clone(a);
    err = write_frame(w, peer, TYPE_TEST, txbuf, 60, CMD_WRITE);
    CHECK_EQ("write while offline -> S2ERR_OUTOFSERVICE", err, S2ERR_OUTOFSERVICE);
    CHECK_EQ("S2_ONLINE before configuration is refused", cmd(a, S2_ONLINE), S2ERR_BAD_STATE);

    ev->ios2_Req.io_Command = S2_ONEVENT;
    ev->ios2_Req.io_Flags = 0;
    ev->ios2_WireError = S2EVENT_ONLINE;
    SendIO((struct IORequest *)ev);

    memcpy(a->ios2_SrcAddr, mac, 6);
    CHECK_EQ("S2_CONFIGINTERFACE", cmd(a, S2_CONFIGINTERFACE), 0);
    CHECK_EQ("second S2_CONFIGINTERFACE -> S2ERR_BAD_STATE", cmd(a, S2_CONFIGINTERFACE), S2ERR_BAD_STATE);
    CHECK_EQ("  wire error IS_CONFIGURED", a->ios2_WireError, S2WERR_IS_CONFIGURED);
    check("S2_ONEVENT(ONLINE) returns on going online", done_within(ev, 50), 0, 1);
    WaitIO((struct IORequest *)ev);
    CHECK_EQ("  event mask ONLINE", ev->ios2_WireError, S2EVENT_ONLINE);

    ev->ios2_WireError = 1UL << 20;
    CHECK_EQ("S2_ONEVENT with an unknown event -> NOT_SUPPORTED", cmd(ev, S2_ONEVENT), S2ERR_NOT_SUPPORTED);

    /* --- type tracking */
    a->ios2_PacketType = TYPE_TEST;
    CHECK_EQ("S2_TRACKTYPE", cmd(a, S2_TRACKTYPE), 0);
    CHECK_EQ("S2_TRACKTYPE twice -> BAD_STATE", cmd(a, S2_TRACKTYPE), S2ERR_BAD_STATE);

    /* --- loop: write one frame, read it back cooked */
    for (i = 0; i < 100; i++)
        txbuf[i] = (UBYTE)(i * 7 + 1);
    start_read(r, TYPE_TEST, rxbuf, FALSE);
    err = write_frame(w, peer, TYPE_TEST, txbuf, 100, CMD_WRITE);
    CHECK_EQ("CMD_WRITE 100 bytes", err, 0);
    check("CMD_READ gets the frame back", done_within(r, 50), 0, 1);
    WaitIO((struct IORequest *)r);
    CHECK_EQ("  read ok", r->ios2_Req.io_Error, 0);
    CHECK_EQ("  length 100", r->ios2_DataLength, 100);
    check("  payload identical", memcmp(rxbuf, txbuf, 100) == 0, 0, 1);
    check("  source = the address we wrote to", memcmp(r->ios2_SrcAddr, peer, 6) == 0, 0, 1);

    /* raw read of a broadcast */
    start_read(r, TYPE_TEST, rxbuf, TRUE);
    err = write_frame(w, peer, TYPE_TEST, txbuf, 46, S2_BROADCAST);
    CHECK_EQ("S2_BROADCAST", err, 0);
    /* loop swaps: dst becomes our MAC again, so no BCAST flag on the way back */
    check("raw CMD_READ gets it", done_within(r, 50), 0, 1);
    WaitIO((struct IORequest *)r);
    CHECK_EQ("  raw length 46 + 14", r->ios2_DataLength, 60);
    check("  raw frame starts with our address", memcmp(rxbuf, mac, 6) == 0, 0, 1);
    check("  raw frame source is broadcast", rxbuf[6] == 0xff && rxbuf[11] == 0xff, 0, 1);

    err = write_frame(w, peer, TYPE_TEST, txbuf, 1501, CMD_WRITE);
    CHECK_EQ("oversize write -> S2ERR_MTU_EXCEEDED", err, S2ERR_MTU_EXCEEDED);
    err = write_frame(w, peer, TYPE_TEST, txbuf, 46, S2_MULTICAST);
    CHECK_EQ("S2_MULTICAST to a unicast address -> BAD_ADDRESS", err, S2ERR_BAD_ADDRESS);

    /* --- orphans: a type nobody reads */
    r2->ios2_Req.io_Command = S2_READORPHAN;
    r2->ios2_Req.io_Flags = 0;
    r2->ios2_Data = rxbuf2;
    SendIO((struct IORequest *)r2);
    write_frame(w, peer, 0x1234, txbuf, 50, CMD_WRITE);
    check("S2_READORPHAN gets an unread type", done_within(r2, 50), 0, 1);
    WaitIO((struct IORequest *)r2);
    CHECK_EQ("  orphan type 0x1234", r2->ios2_PacketType, 0x1234);

    /* --- second opener: both get their own copy */
    err = opendev(b, tags, 0);
    CHECK_EQ("second opener", err, 0);
    if (!err) {
        struct IOSana2Req *rb = clone(b);
        start_read(r, TYPE_TEST, rxbuf, FALSE);
        start_read(rb, TYPE_TEST, rxbuf2, FALSE);
        write_frame(w, peer, TYPE_TEST, txbuf, 64, CMD_WRITE);
        check("  opener 1 got the frame", done_within(r, 50), 0, 1);
        check("  opener 2 got the frame", done_within(rb, 50), 0, 1);
        WaitIO((struct IORequest *)r);
        WaitIO((struct IORequest *)rb);
        check("  both copies identical", memcmp(rxbuf, rxbuf2, 64) == 0, 0, 1);

        /* CloseDevice with a read still pending: it comes back aborted */
        start_read(rb, 0x4242, rxbuf2, FALSE);
        CloseDevice((struct IORequest *)b);
        check("  pending read of a closing opener is returned", CheckIO((struct IORequest *)rb) != NULL, 0, 1);
        WaitIO((struct IORequest *)rb);
        CHECK_EQ("  with IOERR_ABORTED", rb->ios2_Req.io_Error, IOERR_ABORTED);
        DeleteIORequest((struct IORequest *)rb);
    }

    /* --- a buffer hook that fails */
    err = opendev(b, tags_badrx, 0);
    if (!err) {
        struct IOSana2Req *rb = clone(b);
        start_read(rb, TYPE_TEST, rxbuf2, FALSE);
        start_read(r, TYPE_TEST, rxbuf, FALSE);
        write_frame(w, peer, TYPE_TEST, txbuf, 64, CMD_WRITE);
        done_within(rb, 50);
        WaitIO((struct IORequest *)rb);
        CHECK_EQ("CopyToBuff failure -> S2ERR_NO_RESOURCES", rb->ios2_Req.io_Error, S2ERR_NO_RESOURCES);
        CHECK_EQ("  wire error BUFF_ERROR", rb->ios2_WireError, S2WERR_BUFF_ERROR);
        done_within(r, 50);
        WaitIO((struct IORequest *)r);
        CHECK_EQ("  the other opener still got it", r->ios2_Req.io_Error, 0);
        CloseDevice((struct IORequest *)b);
        DeleteIORequest((struct IORequest *)rb);
    }

    /* --- AbortIO of a pending read */
    start_read(r, 0x4343, rxbuf, FALSE);
    Delay(5);
    AbortIO((struct IORequest *)r);
    WaitIO((struct IORequest *)r);
    CHECK_EQ("AbortIO of a pending read -> IOERR_ABORTED", r->ios2_Req.io_Error, IOERR_ABORTED);

    /* --- statistics */
    a->ios2_StatData = &st;
    CHECK_EQ("S2_GETGLOBALSTATS", cmd(a, S2_GETGLOBALSTATS), 0);
    printf("      sent %lu received %lu unknown %lu\n", st.PacketsSent, st.PacketsReceived, st.UnknownTypesReceived);
    check("  PacketsSent >= 5", st.PacketsSent >= 5, st.PacketsSent, 5);
    check("  PacketsReceived >= 5", st.PacketsReceived >= 5, st.PacketsReceived, 5);
    a->ios2_PacketType = TYPE_TEST;
    a->ios2_StatData = &ts;
    CHECK_EQ("S2_GETTYPESTATS", cmd(a, S2_GETTYPESTATS), 0);
    printf("      type %x: sent %lu received %lu\n", TYPE_TEST, ts.PacketsSent, ts.PacketsReceived);
    check("  type counters moved", ts.PacketsSent >= 4 && ts.PacketsReceived >= 4, ts.PacketsSent, 4);
    CHECK_EQ("S2_UNTRACKTYPE", cmd(a, S2_UNTRACKTYPE), 0);
    CHECK_EQ("S2_GETTYPESTATS after untrack -> BAD_STATE", cmd(a, S2_GETTYPESTATS), S2ERR_BAD_STATE);

    /* --- going offline returns pending reads */
    start_read(r, TYPE_TEST, rxbuf, FALSE);
    ev->ios2_Req.io_Command = S2_ONEVENT;
    ev->ios2_Req.io_Flags = 0;
    ev->ios2_WireError = S2EVENT_OFFLINE;
    SendIO((struct IORequest *)ev);
    Delay(5);
    CHECK_EQ("S2_OFFLINE", cmd(a, S2_OFFLINE), 0);
    check("  pending read returned", done_within(r, 50), 0, 1);
    WaitIO((struct IORequest *)r);
    CHECK_EQ("  with S2ERR_OUTOFSERVICE", r->ios2_Req.io_Error, S2ERR_OUTOFSERVICE);
    check("  S2_ONEVENT(OFFLINE) returned", done_within(ev, 50), 0, 1);
    WaitIO((struct IORequest *)ev);
    CHECK_EQ("S2_ONLINE again", cmd(a, S2_ONLINE), 0);
    CHECK_EQ("  write works again", write_frame(w, peer, TYPE_TEST, txbuf, 60, CMD_WRITE), 0);

    CloseDevice((struct IORequest *)a);
    printf("s2test: %d passed, %d failed\n", passes, fails);
    return fails ? 10 : 0;
}
