/*
 * picozorro.device, UAE backend: the "hardware" is another SANA-II device
 * inside the emulator (ENV:PZNET = "uae a2065.device 0"), typically
 * Commodore's a2065.device on FS-UAE's emulated A2065 with slirp. Every
 * SANA-II command of picozorro.device runs for real; only the frames travel
 * through the lower device.
 *
 * Frames go through the lower device cooked, not raw: Commodore's
 * a2065.device 2.16 ignores SANA2IOF_RAW on CMD_WRITE (FS-UAE showed our
 * 342-byte frame leave as 356 bytes, wrapped in a second header), so writes
 * pass destination, type and payload, and reads rebuild the Ethernet header
 * from the addresses and type the lower device returns. Reads are typed
 * CMD_READs for IPv4, ARP and IPv6: the same driver accepts S2_READORPHAN
 * but never completes it. Other types do not pass through this backend.
 */
#include <exec/memory.h>
#include <exec/errors.h>
#include <utility/tagitem.h>
#include <proto/exec.h>
#include "device.h"

#define NREADS 8

struct UaeData {
    struct MsgPort *rxport;  /* lower read replies: pz->rx_signal */
    struct MsgPort *ctlport; /* synchronous control and writes */
    struct IOSana2Req *ctl;
    struct IOSana2Req *rd[NREADS];
    UBYTE *rdbuf[NREADS];
    BOOL pending[NREADS];
    BOOL opened, typed_reads;
    UBYTE mac[ETH_ALEN];
};

static const ULONG read_types[NREADS] = {0x0800, 0x0800, 0x0800, 0x0800, 0x0806, 0x0806, 0x86dd, 0x86dd};

/* Buffer hooks we give the lower device: plain copies into and out of our
 * contiguous buffers (callable from its interrupts). */
static BOOL lower_copy_to(REGARG(APTR to, a0), REGARG(APTR from, a1), REGARG(ULONG n, d0))
{
    if (n > BUF_SIZE - ETH_HLEN) /* reads land after room for the header */
        return FALSE;
    CopyMem(from, to, n);
    return TRUE;
}

static BOOL lower_copy_from(REGARG(APTR to, a0), REGARG(APTR from, a1), REGARG(ULONG n, d0))
{
    CopyMem(from, to, n);
    return TRUE;
}

static struct TagItem lower_tags[] = {
    {S2_CopyToBuff, (ULONG)lower_copy_to},
    {S2_CopyFromBuff, (ULONG)lower_copy_from},
    {TAG_DONE, 0},
};

static void parse(const char *args, char *dev, int devsize, ULONG *unit)
{
    int i = 0;
    /* skip the backend word ("uae") */
    while (*args && *args != ' ')
        args++;
    while (*args == ' ')
        args++;
    while (*args && *args != ' ' && i < devsize - 1)
        dev[i++] = *args++;
    dev[i] = 0;
    *unit = 0;
    while (*args == ' ')
        args++;
    while (*args >= '0' && *args <= '9')
        *unit = *unit * 10 + (ULONG)(*args++ - '0');
    if (!dev[0]) {
        const char *d = "a2065.device";
        for (i = 0; d[i]; i++)
            dev[i] = d[i];
        dev[i] = 0;
    }
}

static BYTE ctl_cmd(struct UaeData *d, UWORD cmd)
{
    d->ctl->ios2_Req.io_Command = cmd;
    d->ctl->ios2_Req.io_Flags = 0;
    return DoIO((struct IORequest *)d->ctl);
}

static void send_read(struct UaeData *d, int i)
{
    struct IOSana2Req *r = d->rd[i];
    r->ios2_Req.io_Command = d->typed_reads ? CMD_READ : S2_READORPHAN;
    r->ios2_Req.io_Flags = 0;
    r->ios2_PacketType = read_types[i];
    r->ios2_Data = d->rdbuf[i] + ETH_HLEN;
    SendIO((struct IORequest *)r);
    d->pending[i] = TRUE;
}

static void cancel_reads(struct UaeData *d)
{
    int i;
    for (i = 0; i < NREADS; i++) {
        if (d->pending[i]) {
            AbortIO((struct IORequest *)d->rd[i]);
            WaitIO((struct IORequest *)d->rd[i]);
            d->pending[i] = FALSE;
        }
    }
}

static void uae_close(struct PZBase *pz)
{
    struct UaeData *d = pz->be_data;
    int i;
    if (!d)
        return;
    if (d->opened) {
        cancel_reads(d);
        CloseDevice((struct IORequest *)d->ctl);
    }
    for (i = 0; i < NREADS; i++) {
        FreeVec(d->rdbuf[i]);
        FreeVec(d->rd[i]);
    }
    if (d->ctl)
        DeleteIORequest((struct IORequest *)d->ctl);
    if (d->rxport)
        DeleteMsgPort(d->rxport);
    if (d->ctlport)
        DeleteMsgPort(d->ctlport);
    FreeVec(d);
    pz->be_data = NULL;
}

static LONG uae_open(struct PZBase *pz, const char *args)
{
    struct UaeData *d;
    char dev[64];
    ULONG unit;
    int i;

    parse(args, dev, sizeof(dev), &unit);
    d = AllocVec(sizeof(*d), MEMF_PUBLIC | MEMF_CLEAR);
    if (!d)
        return -1;
    pz->be_data = d;
    d->typed_reads = TRUE;
    d->rxport = CreateMsgPort();
    d->ctlport = CreateMsgPort();
    if (!d->rxport || !d->ctlport)
        goto fail;
    d->ctl = (struct IOSana2Req *)CreateIORequest(d->ctlport, sizeof(struct IOSana2Req));
    if (!d->ctl)
        goto fail;
    d->ctl->ios2_BufferManagement = lower_tags;
    if (OpenDevice(dev, unit, (struct IORequest *)d->ctl, 0) != 0) {
        /* SANA-II drivers live in DEVS:Networks; exec only looks in DEVS:. */
        char path[80] = "Networks/";
        int k = 9, j;
        for (j = 0; dev[j] && k < (int)sizeof(path) - 1; j++)
            path[k++] = dev[j];
        path[k] = 0;
        for (j = 0; dev[j]; j++)
            if (dev[j] == ':' || dev[j] == '/')
                break;
        if (!dev[j] && OpenDevice(path, unit, (struct IORequest *)d->ctl, 0) == 0)
            goto opened;
        D(("uae: OpenDevice %s %ld failed: %ld", (ULONG)dev, unit, (LONG)d->ctl->ios2_Req.io_Error));
        goto fail;
    }
    D(("uae: %s unit %ld open", (ULONG)dev, unit));
opened:
    d->opened = TRUE;

    /* The lower device's hardware address becomes ours: slirp and the
     * lower driver only deliver frames addressed to it. */
    ctl_cmd(d, S2_GETSTATIONADDRESS);
    pz_copy(d->ctl->ios2_DstAddr, d->mac, ETH_ALEN);
    pz_copy(d->mac, d->ctl->ios2_SrcAddr, ETH_ALEN);
    ctl_cmd(d, S2_CONFIGINTERFACE); /* S2WERR_IS_CONFIGURED is fine */
    D(("uae: config err %ld wire %ld", (LONG)d->ctl->ios2_Req.io_Error, d->ctl->ios2_WireError));

    for (i = 0; i < NREADS; i++) {
        d->rd[i] = AllocVec(sizeof(struct IOSana2Req), MEMF_PUBLIC);
        d->rdbuf[i] = AllocVec(BUF_SIZE, MEMF_PUBLIC);
        if (!d->rd[i] || !d->rdbuf[i])
            goto fail;
        CopyMem(d->ctl, d->rd[i], sizeof(struct IOSana2Req));
        d->rd[i]->ios2_Req.io_Message.mn_ReplyPort = d->rxport;
    }
    pz->rx_signal = 1UL << d->rxport->mp_SigBit;
    return 0;

fail:
    uae_close(pz);
    return -1;
}

static void uae_get_mac(struct PZBase *pz, UBYTE mac[ETH_ALEN])
{
    pz_copy(((struct UaeData *)pz->be_data)->mac, mac, ETH_ALEN);
}

static LONG uae_online(struct PZBase *pz)
{
    struct UaeData *d = pz->be_data;
    int i;
    ctl_cmd(d, S2_ONLINE); /* already online is fine */
    for (i = 0; i < NREADS; i++)
        send_read(d, i);
    return 0;
}

static void uae_offline(struct PZBase *pz)
{
    cancel_reads(pz->be_data);
}

static LONG uae_send(struct PZBase *pz, const UBYTE *f, ULONG len)
{
    struct UaeData *d = pz->be_data;
    struct IOSana2Req *w = d->ctl;
    w->ios2_Req.io_Command = CMD_WRITE;
    w->ios2_Req.io_Flags = 0;
    w->ios2_PacketType = ((ULONG)f[12] << 8) | f[13];
    pz_copy(f, w->ios2_DstAddr, ETH_ALEN);
    w->ios2_DataLength = len - ETH_HLEN;
    w->ios2_Data = (APTR)(f + ETH_HLEN);
    DoIO((struct IORequest *)w);
    D(("uae: tx %ld bytes err %ld wire %ld", len, (LONG)w->ios2_Req.io_Error, w->ios2_WireError));
    return w->ios2_Req.io_Error;
}

static ULONG uae_poll_rx(struct PZBase *pz, UBYTE *buf)
{
    struct UaeData *d = pz->be_data;
    struct IOSana2Req *r;
    int i;

    while ((r = (struct IOSana2Req *)GetMsg(d->rxport)) != NULL) {
        for (i = 0; i < NREADS; i++)
            if (d->rd[i] == r)
                break;
        if (i == NREADS)
            continue;
        d->pending[i] = FALSE;
        if (r->ios2_Req.io_Error == 0) {
            ULONG n = r->ios2_DataLength;
            UBYTE *f = d->rdbuf[i];
            D(("uae: rx %ld bytes type %lx", n, r->ios2_PacketType));
            if (n > ETH_MTU)
                n = ETH_MTU;
            pz_copy(r->ios2_DstAddr, f, ETH_ALEN);
            pz_copy(r->ios2_SrcAddr, f + ETH_ALEN, ETH_ALEN);
            f[12] = (UBYTE)(r->ios2_PacketType >> 8);
            f[13] = (UBYTE)r->ios2_PacketType;
            n += ETH_HLEN;
            pz_copy(f, buf, n);
            send_read(d, i);
            return n;
        }
        D(("uae: read %ld error %ld wire %ld", (ULONG)i, (LONG)r->ios2_Req.io_Error, r->ios2_WireError));
        if (!d->typed_reads &&
            (r->ios2_Req.io_Error == IOERR_NOCMD || r->ios2_Req.io_Error == S2ERR_NOT_SUPPORTED)) {
            d->typed_reads = TRUE; /* no S2_READORPHAN below us */
            send_read(d, i);
        } else if (r->ios2_Req.io_Error != IOERR_ABORTED && r->ios2_Req.io_Error != S2ERR_OUTOFSERVICE) {
            send_read(d, i);
        }
    }
    return 0;
}

static void uae_set_filter(struct PZBase *pz, BOOL p, BOOL a, const struct McastEntry *t, UWORD n)
{
    /* The lower device's own filter stays as opened (own address and
     * broadcast); multicast through it is not set up here. */

}

static ULONG uae_bps(struct PZBase *pz)
{

    return 10000000;
}

const struct Backend backend_uae = {
    "uae", uae_open, uae_close, uae_get_mac, uae_online, uae_offline,
    uae_send, uae_poll_rx, uae_set_filter, uae_bps, NULL,
};
