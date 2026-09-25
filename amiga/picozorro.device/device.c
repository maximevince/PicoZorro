/*
 * picozorro.device: SANA-II driver for the PicoZorro network card.
 * Device entry points, request handling, the device process.
 *
 * Model: every request that touches the interface goes to one process
 * (pz->port) and is handled there in order; the few that only read or
 * update driver state are answered at once in BeginIO. The process also
 * drains received frames from the backend and hands each to the matching
 * CMD_READ of every opener (SANA-II standard.txt: each opener gets its own
 * copy), or to one S2_READORPHAN.
 *
 * Locking: pz->lock (a SignalSemaphore, recursive for its owner) guards the
 * opener list, all per-opener request lists, the event list and the type
 * statistics. The request port is guarded by Exec (Disable).
 */
#include <stdarg.h>
#include <exec/memory.h>
#include <exec/execbase.h>
#include <exec/errors.h>
#include <dos/dostags.h>
#include <dos/var.h>
#include <devices/newstyle.h>
#include <utility/tagitem.h>
#include <utility/hooks.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/utility.h>
#include <proto/timer.h>

#include "device.h"

struct ExecBase *SysBase;
struct DosLibrary *DOSBase;
struct Library *UtilityBase;
struct Device *TimerBase;
struct PZBase *PZ;

static UWORD mcast_all_ranges; /* active multicast ranges: receive all multicast */

/* ------------------------------------------------------------ helpers */

static void new_list(struct MinList *l)
{
    l->mlh_Head = (struct MinNode *)&l->mlh_Tail;
    l->mlh_Tail = NULL;
    l->mlh_TailPred = (struct MinNode *)&l->mlh_Head;
}

#define FOR_LIST(l, n, next) \
    for ((n) = (APTR)(l)->mlh_Head; ((next) = (APTR)((struct MinNode *)(n))->mln_Succ) != NULL; (n) = (next))

#ifdef PZ_DEBUG
#ifdef __VBCC__
static void raw_put_char(REGARG(UBYTE c, d0), REGARG(struct ExecBase *sb, a6)) = "\tjsr\t-516(a6)";
#else
/* exec RawPutChar, LVO -516 (private) */
static void raw_put_char(UBYTE c, struct ExecBase *sb)
{
    register ULONG d0 __asm("d0") = c;
    register struct ExecBase *a6 __asm("a6") = sb;
    __asm volatile("jsr -516(%%a6)" : "+r"(d0) : "r"(a6) : "d1", "a0", "a1", "cc", "memory");
}
#endif

/* Lines are formatted into a LineBuf. A backend with a log hook owns the
 * serial port: the lines queue in `ring` (any context may log, the
 * interrupt server included) and the device process sends them. */
#define LOG_RING 4096
static UBYTE ring[LOG_RING];
static UWORD ring_head, ring_tail;

struct LineBuf {
    UWORD len;
    UBYTE text[160];
};

static void put_line(REGARG(UBYTE c, d0), REGARG(struct LineBuf *lb, a3))
{
    if (c && lb->len < sizeof(lb->text) - 2)
        lb->text[lb->len++] = c;
}

void pz_log(const char *fmt, ...)
{
    struct PZBase *pz = PZ;
    struct LineBuf lb;
    va_list ap;
    UWORD i;

    lb.len = 0;
    RawDoFmt((STRPTR)"[pz] ", NULL, (void (*)())put_line, &lb);
    va_start(ap, fmt);  /* m68k: a pointer to the stacked arguments, as RawDoFmt wants */
    RawDoFmt((STRPTR)fmt, (APTR)ap, (void (*)())put_line, &lb);
    va_end(ap);
    lb.text[lb.len++] = '\r';
    lb.text[lb.len++] = '\n';
    if (!pz || !pz->be || !pz->be->log) {
        for (i = 0; i < lb.len; i++)
            raw_put_char(lb.text[i], SysBase);
        return;
    }
    Disable();
    for (i = 0; i < lb.len; i++) {
        UWORD next = (ring_head + 1) % LOG_RING;
        if (next == ring_tail)
            break;                      /* full: drop the rest */
        ring[ring_head] = lb.text[i];
        ring_head = next;
    }
    Enable();
}

/* Device process only. */
void pz_log_flush(struct PZBase *pz)
{
    UBYTE chunk[200];
    UWORD n;

    if (!pz->be_open || !pz->be->log)
        return;
    for (;;) {
        n = 0;
        Disable();
        while (ring_tail != ring_head && n < sizeof(chunk)) {
            chunk[n++] = ring[ring_tail];
            ring_tail = (ring_tail + 1) % LOG_RING;
        }
        Enable();
        if (!n)
            break;
        pz->be->log(pz, chunk, n);
    }
}
#endif

void pz_copy(const void *from, void *to, ULONG n)
{
    CopyMem((APTR)from, to, n);
}

static void set_addr(UBYTE *dst, const UBYTE *src)
{
    int i;
    for (i = 0; i < SANA2_MAX_ADDR_BYTES; i++)
        dst[i] = i < ETH_ALEN ? src[i] : 0;
}

static BOOL addr_eq(const UBYTE *a, const UBYTE *b)
{
    int i;
    for (i = 0; i < ETH_ALEN; i++)
        if (a[i] != b[i])
            return FALSE;
    return TRUE;
}

static void fail(struct IOSana2Req *io, BYTE err, ULONG wire)
{
    io->ios2_Req.io_Error = err;
    io->ios2_WireError = wire;
}

static void reply(struct IOSana2Req *io)
{
    if (!(io->ios2_Req.io_Flags & IOF_QUICK))
        ReplyMsg(&io->ios2_Req.io_Message);
}

static struct TypeStats *find_type(struct PZBase *pz, ULONG type)
{
    struct TypeStats *t, *next;
    FOR_LIST(&pz->types, t, next)
        if (t->type == type)
            return t;
    return NULL;
}

/* ------------------------------------------------------------ events */

void pz_event(struct PZBase *pz, ULONG events)
{
    struct IOSana2Req *io, *next;
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->events, io, next) {
        if (io->ios2_WireError & events) {
            Remove((struct Node *)io);
            io->ios2_WireError &= events;
            io->ios2_Req.io_Error = 0;
            ReplyMsg(&io->ios2_Req.io_Message);
        }
    }
    ReleaseSemaphore(&pz->lock);
}

#define EVENTS_SUPPORTED (S2EVENT_ERROR | S2EVENT_TX | S2EVENT_RX | S2EVENT_ONLINE | S2EVENT_OFFLINE | \
                          S2EVENT_BUFF | S2EVENT_HARDWARE | S2EVENT_SOFTWARE)

/* Abort the requests on a list with the given error (lock held). */
static void abort_list(struct MinList *l, BYTE err, ULONG wire)
{
    struct IOSana2Req *io;
    while ((io = (struct IOSana2Req *)RemHead((struct List *)l)) != NULL) {
        fail(io, err, wire);
        ReplyMsg(&io->ios2_Req.io_Message);
    }
}

static void abort_opener(struct Opener *op, BYTE err, ULONG wire)
{
    abort_list(&op->reads, err, wire);
    abort_list(&op->orphans, err, wire);
}

/* ------------------------------------------------------------ receive */

static BOOL wanted(struct PZBase *pz, const UBYTE *dst, BOOL bcast)
{
    UWORD i;
    if (bcast || pz->prom_openers)
        return TRUE;
    if (!(dst[0] & 1))
        return addr_eq(dst, pz->mac);
    if (mcast_all_ranges)
        return TRUE;
    for (i = 0; i < pz->mcast_n; i++)
        if (addr_eq(dst, pz->mcast[i].addr))
            return TRUE;
    return FALSE;
}

static BOOL type_match(ULONG want, ULONG got)
{
    /* SANA-II ethernet.txt: a type field of 1500 or less is an 802.3
     * length; readers of any type <= 1500 get 802.3 frames. */
    if (got <= ETH_MTU)
        return want <= ETH_MTU;
    return want == got;
}

static void deliver(struct PZBase *pz, struct Opener *op, struct IOSana2Req *io, UBYTE *f, ULONG len,
                    ULONG type, UBYTE rflags)
{
    BOOL raw = (io->ios2_Req.io_Flags & SANA2IOF_RAW) != 0;
    APTR data = raw ? f : f + ETH_HLEN;
    ULONG n = raw ? len : len - ETH_HLEN;

    io->ios2_Req.io_Flags = (io->ios2_Req.io_Flags & (SANA2IOF_RAW | IOF_QUICK)) | rflags;
    io->ios2_PacketType = type;
    set_addr(io->ios2_SrcAddr, f + ETH_ALEN);
    set_addr(io->ios2_DstAddr, f);
    io->ios2_DataLength = n;
    if (CALL_COPY(op->copy_to, io->ios2_Data, data, n)) {
        io->ios2_Req.io_Error = 0;
        io->ios2_WireError = 0;
    } else {
        fail(io, S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
        pz_event(pz, S2EVENT_BUFF | S2EVENT_RX | S2EVENT_ERROR);
    }
    ReplyMsg(&io->ios2_Req.io_Message);
}

static BOOL filter_ok(struct Opener *op, struct IOSana2Req *io, UBYTE *f)
{
    if (!op->filter)
        return TRUE;
    return CallHookPkt(op->filter, io, (io->ios2_Req.io_Flags & SANA2IOF_RAW) ? f : f + ETH_HLEN) != 0;
}

void pz_frame_received(struct PZBase *pz, UBYTE *f, ULONG len)
{
    struct Opener *op, *nop;
    struct IOSana2Req *io, *nio;
    struct TypeStats *ts;
    ULONG type;
    UBYTE rflags = 0;
    BOOL bcast, matched = FALSE;
    int i;

    if (len < ETH_HLEN || len > ETH_FRAME_MAX) {
        pz->stats.BadData++;
        return;
    }
    bcast = TRUE;
    for (i = 0; i < ETH_ALEN; i++)
        if (f[i] != 0xff)
            bcast = FALSE;
    if (bcast)
        rflags = SANA2IOF_BCAST;
    else if (f[0] & 1)
        rflags = SANA2IOF_MCAST;
    if (!wanted(pz, f, bcast))
        return;

    pz->stats.PacketsReceived++;
    type = ((ULONG)f[12] << 8) | f[13];

    ObtainSemaphore(&pz->lock);
    ts = find_type(pz, type);
    if (ts) {
        ts->s.PacketsReceived++;
        ts->s.BytesReceived += len - ETH_HLEN;
    }
    FOR_LIST(&pz->openers, op, nop) {
        FOR_LIST(&op->reads, io, nio) {
            if (type_match(io->ios2_PacketType, type) && filter_ok(op, io, f)) {
                Remove((struct Node *)io);
                deliver(pz, op, io, f, len, type, rflags);
                matched = TRUE;
                break;
            }
        }
    }
    if (!matched) {
        FOR_LIST(&pz->openers, op, nop) {
            io = (struct IOSana2Req *)op->orphans.mlh_Head;
            if (io->ios2_Req.io_Message.mn_Node.ln_Succ) {
                Remove((struct Node *)io);
                deliver(pz, op, io, f, len, type, rflags);
                matched = TRUE;
                break;
            }
        }
    }
    if (!matched) {
        pz->stats.UnknownTypesReceived++;
        if (ts)
            ts->s.PacketsDropped++;
    }
    ReleaseSemaphore(&pz->lock);
}

/* ------------------------------------------------------------ filter */

static void apply_filter(struct PZBase *pz)
{
    pz->filter_dirty = FALSE;
    if (pz->online)
        pz->be->set_filter(pz, pz->prom_openers != 0, mcast_all_ranges != 0, pz->mcast, pz->mcast_n);
}

static void mcast_change(struct PZBase *pz, struct IOSana2Req *io, BOOL add)
{
    BOOL range = io->ios2_Req.io_Command == S2_ADDMULTICASTADDRESSES ||
                 io->ios2_Req.io_Command == S2_DELMULTICASTADDRESSES;
    const UBYTE *a = io->ios2_SrcAddr;
    UWORD i;

    if (!(a[0] & 1) || (range && !(io->ios2_DstAddr[0] & 1))) {
        fail(io, S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
        return;
    }
    if (range && !addr_eq(io->ios2_SrcAddr, io->ios2_DstAddr)) {
        /* A real range: receive all multicast while any is active. */
        if (add)
            mcast_all_ranges++;
        else if (mcast_all_ranges)
            mcast_all_ranges--;
        apply_filter(pz);
        return;
    }
    for (i = 0; i < pz->mcast_n; i++)
        if (addr_eq(pz->mcast[i].addr, a))
            break;
    if (add) {
        if (i < pz->mcast_n) {
            pz->mcast[i].refs++;
            return;
        }
        if (pz->mcast_n == MCAST_MAX) {
            fail(io, S2ERR_NO_RESOURCES, S2WERR_MULTICAST_FULL);
            return;
        }
        pz_copy(a, pz->mcast[pz->mcast_n].addr, ETH_ALEN);
        pz->mcast[pz->mcast_n].refs = 1;
        pz->mcast_n++;
    } else {
        if (i == pz->mcast_n) {
            fail(io, S2ERR_BAD_STATE, S2WERR_BAD_MULTICAST);
            return;
        }
        if (--pz->mcast[i].refs)
            return;
        pz->mcast[i] = pz->mcast[--pz->mcast_n];
    }
    apply_filter(pz);
}

/* ------------------------------------------------------------ transmit */

static void do_write(struct PZBase *pz, struct IOSana2Req *io)
{
    struct Opener *op = io->ios2_BufferManagement;
    BOOL raw = (io->ios2_Req.io_Flags & SANA2IOF_RAW) != 0;
    ULONG n = io->ios2_DataLength, off = raw ? 0 : ETH_HLEN, type;
    UBYTE *f = pz->txbuf;
    struct TypeStats *ts;

    if (!pz->online) {
        fail(io, S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
        return;
    }
    if (raw ? (n > ETH_FRAME_MAX || n < ETH_HLEN) : n > ETH_MTU) {
        fail(io, raw && n < ETH_HLEN ? S2ERR_BAD_ARGUMENT : S2ERR_MTU_EXCEEDED, S2WERR_GENERIC_ERROR);
        return;
    }
    if (!raw) {
        switch (io->ios2_Req.io_Command) {
        case S2_BROADCAST: {
            int i;
            for (i = 0; i < ETH_ALEN; i++)
                f[i] = 0xff;
            break;
        }
        case S2_MULTICAST:
            if (!(io->ios2_DstAddr[0] & 1)) {
                fail(io, S2ERR_BAD_ADDRESS, S2WERR_BAD_MULTICAST);
                return;
            }
            /* fall through */
        default:
            pz_copy(io->ios2_DstAddr, f, ETH_ALEN);
        }
        pz_copy(pz->mac, f + ETH_ALEN, ETH_ALEN);
        f[12] = (UBYTE)(io->ios2_PacketType >> 8);
        f[13] = (UBYTE)io->ios2_PacketType;
    }
    if (!CALL_COPY(op->copy_from, f + off, io->ios2_Data, n)) {
        fail(io, S2ERR_NO_RESOURCES, S2WERR_BUFF_ERROR);
        pz_event(pz, S2EVENT_BUFF | S2EVENT_TX | S2EVENT_ERROR);
        return;
    }
    if (pz->be->send(pz, f, off + n)) {
        fail(io, S2ERR_TX_FAILURE, S2WERR_GENERIC_ERROR);
        pz_event(pz, S2EVENT_TX | S2EVENT_ERROR);
        return;
    }
    pz->stats.PacketsSent++;
    type = ((ULONG)f[12] << 8) | f[13];
    ObtainSemaphore(&pz->lock);
    if ((ts = find_type(pz, type)) != NULL) {
        ts->s.PacketsSent++;
        ts->s.BytesSent += off + n - ETH_HLEN;
    }
    ReleaseSemaphore(&pz->lock);
}

/* ------------------------------------------------------------ online state */

static void go_online(struct PZBase *pz, struct IOSana2Req *io)
{
    if (pz->online)
        return;
    if (pz->be->online(pz)) {
        fail(io, S2ERR_NO_RESOURCES, S2WERR_GENERIC_ERROR);
        pz_event(pz, S2EVENT_HARDWARE | S2EVENT_ERROR);
        return;
    }
    pz->online = TRUE;
    if (TimerBase)
        GetSysTime(&pz->stats.LastStart);
    apply_filter(pz);
    pz_event(pz, S2EVENT_ONLINE);
}

static void go_offline(struct PZBase *pz)
{
    struct Opener *op, *next;
    if (!pz->online)
        return;
    pz->be->offline(pz);
    pz->online = FALSE;
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->openers, op, next)
        abort_opener(op, S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
    ReleaseSemaphore(&pz->lock);
    pz_event(pz, S2EVENT_OFFLINE);
}

/* ------------------------------------------------------------ process side */

static void queue_on(struct PZBase *pz, struct MinList *l, struct IOSana2Req *io)
{
    ObtainSemaphore(&pz->lock);
    AddTail((struct List *)l, (struct Node *)io);
    ReleaseSemaphore(&pz->lock);
}

/* The backend is opened on the first request, not in OpenDevice: opening
 * a disk-based lower device (a2065.device) while our own OpenDevice is
 * still being served by ramlib fails. */
static BOOL ensure_backend(struct PZBase *pz)
{
    if (pz->be_open)
        return TRUE;
    if (pz->be->open(pz, pz->be_args) != 0) {
        D(("backend open failed"));
        return FALSE;
    }
    pz->be->get_mac(pz, pz->hw_mac);
    pz_copy(pz->hw_mac, pz->mac, ETH_ALEN);
    pz->be_open = TRUE;
    D(("backend open, mac %02lx:%02lx:%02lx:%02lx:%02lx:%02lx", (ULONG)pz->mac[0], (ULONG)pz->mac[1],
       (ULONG)pz->mac[2], (ULONG)pz->mac[3], (ULONG)pz->mac[4], (ULONG)pz->mac[5]));
    return TRUE;
}

/* Handle one request in the process. Returns TRUE when it was queued (the
 * reply comes later), FALSE when it is done and must be replied now. */
static BOOL handle(struct PZBase *pz, struct IOSana2Req *io)
{
    struct Opener *op = io->ios2_BufferManagement;
    ULONG mask;

    if (!ensure_backend(pz)) {
        fail(io, S2ERR_NO_RESOURCES, S2WERR_GENERIC_ERROR);
        pz_event(pz, S2EVENT_HARDWARE | S2EVENT_ERROR);
        return FALSE;
    }

    D(("cmd %ld flags %lx type %lx len %ld", (ULONG)io->ios2_Req.io_Command, (ULONG)io->ios2_Req.io_Flags,
       io->ios2_PacketType, io->ios2_DataLength));
    switch (io->ios2_Req.io_Command) {
    case CMD_READ:
    case S2_READORPHAN:
        if (!pz->online) {
            fail(io, S2ERR_OUTOFSERVICE, S2WERR_UNIT_OFFLINE);
            return FALSE;
        }
        queue_on(pz, io->ios2_Req.io_Command == CMD_READ ? &op->reads : &op->orphans, io);
        return TRUE;

    case CMD_WRITE:
    case S2_BROADCAST:
    case S2_MULTICAST:
        do_write(pz, io);
        return FALSE;

    case CMD_FLUSH: {
        struct Opener *o, *next;
        ObtainSemaphore(&pz->lock);
        FOR_LIST(&pz->openers, o, next)
            abort_opener(o, IOERR_ABORTED, 0);
        abort_list(&pz->events, IOERR_ABORTED, 0);
        ReleaseSemaphore(&pz->lock);
        return FALSE;
    }

    case S2_CONFIGINTERFACE:
        if (pz->configured) {
            fail(io, S2ERR_BAD_STATE, S2WERR_IS_CONFIGURED);
            return FALSE;
        }
        /* The address is fixed by the hardware; a request for another one
         * is accepted but the station address stays (GETSTATIONADDRESS
         * reports what is used). */
        pz->configured = TRUE;
        go_online(pz, io);
        return FALSE;

    case S2_ONLINE:
        if (!pz->configured) {
            fail(io, S2ERR_BAD_STATE, S2WERR_NOT_CONFIGURED);
            return FALSE;
        }
        go_online(pz, io);
        return FALSE;

    case S2_OFFLINE:
        go_offline(pz);
        return FALSE;

    case S2_ONEVENT:
        mask = io->ios2_WireError;
        if (mask & ~EVENTS_SUPPORTED) {
            fail(io, S2ERR_NOT_SUPPORTED, S2WERR_BAD_EVENT);
            return FALSE;
        }
        if ((mask & S2EVENT_ONLINE) && pz->online) {
            io->ios2_WireError = S2EVENT_ONLINE;
            return FALSE;
        }
        if ((mask & S2EVENT_OFFLINE) && !pz->online) {
            io->ios2_WireError = S2EVENT_OFFLINE;
            return FALSE;
        }
        queue_on(pz, &pz->events, io);
        return TRUE;

    case S2_GETSTATIONADDRESS:
        set_addr(io->ios2_SrcAddr, pz->mac);
        set_addr(io->ios2_DstAddr, pz->hw_mac);
        return FALSE;

    case S2_ADDMULTICASTADDRESS:
    case S2_ADDMULTICASTADDRESSES:
        mcast_change(pz, io, TRUE);
        return FALSE;

    case S2_DELMULTICASTADDRESS:
    case S2_DELMULTICASTADDRESSES:
        mcast_change(pz, io, FALSE);
        return FALSE;

    default:
        fail(io, IOERR_NOCMD, 0);
        return FALSE;
    }
}

static const struct Backend *pick_backend(char *args, LONG size)
{
    /* ENV:PZNET selects the backend: "pz [2|6]" (default), "uae
     * <device> <unit>", "loop", "bp [baud [unit [device]]]". */
    LONG n = GetVar("PZNET", args, size, GVF_GLOBAL_ONLY);
    if (n <= 0)
        args[0] = 0;
    if (args[0] == 'b' && args[1] == 'p')
        return &backend_bp;
    if (args[0] == 'u')
        return &backend_uae;
    if (args[0] == 'l')
        return &backend_loop;
    return &backend_pz;
}

static void proc_main(void)
{
    struct PZBase *pz = PZ;
    struct MsgPort *tport = NULL;
    ULONG portsig, sigs;
    struct IOSana2Req *io;

    pz->port = CreateMsgPort();
    tport = CreateMsgPort();
    pz->rxbuf = AllocVec(BUF_SIZE, MEMF_PUBLIC);
    pz->txbuf = AllocVec(BUF_SIZE, MEMF_PUBLIC);
    if (tport && (pz->treq = (struct timerequest *)CreateIORequest(tport, sizeof(struct timerequest))) != NULL &&
        OpenDevice(TIMERNAME, UNIT_MICROHZ, (struct IORequest *)pz->treq, 0) == 0)
        TimerBase = pz->treq->tr_node.io_Device;

    pz->startup_error = IOERR_OPENFAIL;
    pz->be = pick_backend(pz->be_args, sizeof(pz->be_args));
    pz->be_open = FALSE;
    D(("process up: backend %s, args '%s', port %lx timer %lx", (ULONG)pz->be->name, (ULONG)pz->be_args,
       (ULONG)pz->port, (ULONG)TimerBase));
    if (pz->port && pz->rxbuf && pz->txbuf)
        pz->startup_error = 0;

    if (pz->startup_error == 0) {
        Signal(pz->opener_task, SIGF_SINGLE);
        portsig = 1UL << pz->port->mp_SigBit;
        for (;;) {
            sigs = Wait(portsig | pz->rx_signal | SIGBREAKF_CTRL_C | SIGBREAKF_CTRL_F);
            if (sigs & SIGBREAKF_CTRL_C)
                break;
            while ((io = (struct IOSana2Req *)GetMsg(pz->port)) != NULL) {
                if (!handle(pz, io))
                    ReplyMsg(&io->ios2_Req.io_Message);
            }
            if (pz->filter_dirty)
                apply_filter(pz);
            if (pz->online) {
                ULONG n;
                while ((n = pz->be->poll_rx(pz, pz->rxbuf)) != 0)
                    pz_frame_received(pz, pz->rxbuf, n);
            }
#ifdef PZ_DEBUG
            pz_log_flush(pz);
#endif
        }
        /* Last close: everything of the openers was aborted there. */
        while ((io = (struct IOSana2Req *)GetMsg(pz->port)) != NULL) {
            fail(io, IOERR_ABORTED, 0);
            ReplyMsg(&io->ios2_Req.io_Message);
        }
        if (pz->online) {
            pz->be->offline(pz);
            pz->online = FALSE;
        }
        if (pz->be_open)
            pz->be->close(pz);
        pz->be_open = FALSE;
    }

    if (TimerBase)
        CloseDevice((struct IORequest *)pz->treq);
    TimerBase = NULL;
    if (pz->treq)
        DeleteIORequest((struct IORequest *)pz->treq);
    pz->treq = NULL;
    if (tport)
        DeleteMsgPort(tport);
    FreeVec(pz->rxbuf);
    FreeVec(pz->txbuf);
    pz->rxbuf = pz->txbuf = NULL;
    if (pz->port)
        DeleteMsgPort(pz->port);
    pz->port = NULL;
    pz->configured = FALSE;
    pz->rx_signal = 0;

    /* Tell Open (failure) or Close (shutdown) that we are gone; Forbid
     * keeps us from running again before Exec removes this task. */
    Forbid();
    pz->proc = NULL;
    Signal(pz->opener_task, SIGF_SINGLE);
}

static BOOL start_process(struct PZBase *pz)
{
    static const struct TagItem tags[] = {
        {NP_Entry, (ULONG)proc_main},
        {NP_Name, (ULONG)DEVICE_NAME},
        {NP_Priority, 5},
        {NP_StackSize, 8192},
        {TAG_DONE, 0},
    };
    pz->opener_task = FindTask(NULL);
    SetSignal(0, SIGF_SINGLE);
    pz->proc = CreateNewProc(tags);
    if (!pz->proc)
        return FALSE;
    Wait(SIGF_SINGLE);
    return pz->startup_error == 0;
}

static void stop_process(struct PZBase *pz)
{
    pz->opener_task = FindTask(NULL);
    SetSignal(0, SIGF_SINGLE);
    Signal(&pz->proc->pr_Task, SIGBREAKF_CTRL_C);
    Wait(SIGF_SINGLE);
}

/* ------------------------------------------------------------ entry points */

struct PZBase *DevInit(REGARG(struct PZBase *pz, d0), REGARG(BPTR seglist, a0), REGARG(struct ExecBase *sys, a6))
{
    SysBase = sys;
    PZ = pz;
    pz->seglist = seglist;
    pz->lib.lib_Node.ln_Type = NT_DEVICE;
    pz->lib.lib_Node.ln_Name = DEVICE_NAME;
    pz->lib.lib_Flags = LIBF_SUMUSED | LIBF_CHANGED;
    pz->lib.lib_Version = DEVICE_VERSION;
    pz->lib.lib_Revision = DEVICE_REVISION;
    pz->lib.lib_IdString = "picozorro.device 1.0 (" DEVICE_DATE ")";
    InitSemaphore(&pz->lock);
    new_list(&pz->openers);
    new_list(&pz->types);
    new_list(&pz->events);
    /* Started from the card's boot ROM, this runs at romboot time,
     * before dos.library is up (priority -40 against -120): dos.library is
     * opened at the first open then. */
    DOSBase = (struct DosLibrary *)OpenLibrary("dos.library", 37);
    UtilityBase = OpenLibrary("utility.library", 37);
    if (!UtilityBase) {
        if (DOSBase)
            CloseLibrary((struct Library *)DOSBase);
        FreeMem((UBYTE *)pz - pz->lib.lib_NegSize, pz->lib.lib_NegSize + pz->lib.lib_PosSize);
        return NULL;
    }
    return pz;
}

BPTR DevExpunge(REGARG(struct PZBase *pz, a6))
{
    BPTR seg;
    if (pz->lib.lib_OpenCnt) {
        pz->lib.lib_Flags |= LIBF_DELEXP;
        return 0;
    }
    seg = pz->seglist;
    Remove(&pz->lib.lib_Node);
    CloseLibrary(UtilityBase);
    if (DOSBase)
        CloseLibrary((struct Library *)DOSBase);
    FreeMem((UBYTE *)pz - pz->lib.lib_NegSize, pz->lib.lib_NegSize + pz->lib.lib_PosSize);
    return seg;
}

void DevOpen(REGARG(struct IOSana2Req *io, a1), REGARG(ULONG unit, d0), REGARG(ULONG flags, d1),
             REGARG(struct PZBase *pz, a6))
{
    struct Opener *op = NULL;
    struct TagItem *tags;

    pz->lib.lib_OpenCnt++; /* no expunge while we may Wait below */
    io->ios2_Req.io_Error = IOERR_OPENFAIL;
    if (!DOSBase && !(DOSBase = (struct DosLibrary *)OpenLibrary("dos.library", 37)))
        goto out;
    D(("open unit %ld flags %lx len %ld", unit, flags, (ULONG)io->ios2_Req.io_Message.mn_Length));
    if (unit != 0 || io->ios2_Req.io_Message.mn_Length < sizeof(struct IOSana2Req)) {
        D(("open: bad unit or request size"));
        goto out;
    }

    ObtainSemaphore(&pz->lock);
    if (pz->exclusive || ((flags & SANA2OPF_MINE) && pz->open_count) || (pz->open_count == 0 && pz->proc)) {
        D(("open: exclusive conflict or process still stopping"));
        goto unlock;
    }
    op = AllocVec(sizeof(*op), MEMF_PUBLIC | MEMF_CLEAR);
    if (!op)
        goto unlock;
    tags = io->ios2_BufferManagement;
    op->copy_to = (CopyFn)GetTagData(S2_CopyToBuff, 0, tags);
    op->copy_from = (CopyFn)GetTagData(S2_CopyFromBuff, 0, tags);
    op->filter = (struct Hook *)GetTagData(S2_PacketFilter, 0, tags);
    op->flags = (UBYTE)flags;
    new_list(&op->reads);
    new_list(&op->orphans);
    if (!op->copy_to || !op->copy_from) {
        D(("open: no CopyToBuff/CopyFromBuff"));
        io->ios2_WireError = S2WERR_NULL_POINTER;
        goto unlock;
    }
    if (pz->open_count == 0 && !start_process(pz)) {
        D(("open: process start failed"));
        goto unlock;
    }
    D(("open ok, %ld openers", (ULONG)pz->open_count + 1));

    AddTail((struct List *)&pz->openers, (struct Node *)op);
    pz->open_count++;
    if (flags & SANA2OPF_MINE)
        pz->exclusive = TRUE;
    if (flags & SANA2OPF_PROM) {
        pz->prom_openers++;
        pz->filter_dirty = TRUE;
        Signal(&pz->proc->pr_Task, SIGBREAKF_CTRL_F);
    }
    ReleaseSemaphore(&pz->lock);

    io->ios2_BufferManagement = op;
    io->ios2_Req.io_Unit = &pz->unit;
    io->ios2_Req.io_Error = 0;
    pz->unit.unit_OpenCnt++;
    pz->lib.lib_Flags &= ~LIBF_DELEXP;
    return;

unlock:
    ReleaseSemaphore(&pz->lock);
out:
    FreeVec(op);
    pz->lib.lib_OpenCnt--;
}

BPTR DevClose(REGARG(struct IOSana2Req *io, a1), REGARG(struct PZBase *pz, a6))
{
    struct Opener *op = io->ios2_BufferManagement;
    struct IOSana2Req *e, *next;
    BOOL last;

    ObtainSemaphore(&pz->lock);
    abort_opener(op, IOERR_ABORTED, 0);
    FOR_LIST(&pz->events, e, next) {
        if (e->ios2_BufferManagement == op) {
            Remove((struct Node *)e);
            fail(e, IOERR_ABORTED, 0);
            ReplyMsg(&e->ios2_Req.io_Message);
        }
    }
    Remove((struct Node *)op);
    pz->open_count--;
    if (op->flags & SANA2OPF_MINE)
        pz->exclusive = FALSE;
    if (op->flags & SANA2OPF_PROM) {
        pz->prom_openers--;
        pz->filter_dirty = TRUE;
    }
    last = pz->open_count == 0;
    ReleaseSemaphore(&pz->lock);

    if (last)
        stop_process(pz);
    else if (op->flags & SANA2OPF_PROM)
        Signal(&pz->proc->pr_Task, SIGBREAKF_CTRL_F);
    FreeVec(op);

    io->ios2_Req.io_Device = (struct Device *)-1;
    io->ios2_Req.io_Unit = (struct Unit *)-1;
    pz->unit.unit_OpenCnt--;
    if (--pz->lib.lib_OpenCnt == 0 && (pz->lib.lib_Flags & LIBF_DELEXP))
        return DevExpunge(pz);
    return 0;
}

LONG DevNull(void)
{
    return 0;
}

static const UWORD supported_cmds[] = {
    CMD_READ, CMD_WRITE, CMD_FLUSH,
    S2_DEVICEQUERY, S2_GETSTATIONADDRESS, S2_CONFIGINTERFACE,
    S2_ADDMULTICASTADDRESS, S2_DELMULTICASTADDRESS, S2_MULTICAST, S2_BROADCAST,
    S2_TRACKTYPE, S2_UNTRACKTYPE, S2_GETTYPESTATS, S2_GETSPECIALSTATS, S2_GETGLOBALSTATS,
    S2_ONEVENT, S2_READORPHAN, S2_ONLINE, S2_OFFLINE,
    S2_ADDMULTICASTADDRESSES, S2_DELMULTICASTADDRESSES,
    NSCMD_DEVICEQUERY,
    0,
};

/* Commands answered at once in the caller's context. Returns FALSE for
 * those that must go to the process. */
static BOOL quick(struct PZBase *pz, struct IOSana2Req *io)
{
    switch (io->ios2_Req.io_Command) {
    case NSCMD_DEVICEQUERY: {
        struct IOStdReq *sio = (struct IOStdReq *)io;
        struct NSDeviceQueryResult *r = sio->io_Data;
        if (!r || sio->io_Length < sizeof(*r)) {
            io->ios2_Req.io_Error = IOERR_BADLENGTH;
            break;
        }
        r->nsdqr_DevQueryFormat = 0;
        r->nsdqr_SizeAvailable = sizeof(*r);
        r->nsdqr_DeviceType = NSDEVTYPE_SANA2;
        r->nsdqr_DeviceSubType = 0;
        r->nsdqr_SupportedCommands = (APTR)supported_cmds;
        sio->io_Actual = sizeof(*r);
        break;
    }
    case S2_DEVICEQUERY: {
        struct Sana2DeviceQuery q, *out = io->ios2_StatData;
        ULONG n;
        if (!out) {
            fail(io, S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            break;
        }
        q.SizeAvailable = out->SizeAvailable;
        n = q.SizeAvailable < sizeof(q) ? q.SizeAvailable : sizeof(q);
        q.SizeSupplied = n;
        q.DevQueryFormat = 0;
        q.DeviceLevel = 0;
        q.AddrFieldSize = ETH_ALEN * 8;
        q.MTU = ETH_MTU;
        q.BPS = pz->be_open ? pz->be->bps(pz) : 10000000;
        q.HardwareType = S2WireType_Ethernet;
        q.RawMTU = ETH_FRAME_MAX;
        if (n > 8)
            pz_copy((UBYTE *)&q + 8, (UBYTE *)out + 8, n - 8);
        if (n >= 8)
            out->SizeSupplied = n;
        break;
    }
    case S2_GETGLOBALSTATS:
        if (!io->ios2_StatData) {
            fail(io, S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            break;
        }
        pz_copy(&pz->stats, io->ios2_StatData, sizeof(pz->stats));
        break;
    case S2_GETSPECIALSTATS: {
        struct Sana2SpecialStatHeader *h = io->ios2_StatData;
        if (!h) {
            fail(io, S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
            break;
        }
        h->RecordCountSupplied = 0;
        break;
    }
    case S2_TRACKTYPE:
    case S2_UNTRACKTYPE:
    case S2_GETTYPESTATS: {
        struct TypeStats *t;
        ObtainSemaphore(&pz->lock);
        t = find_type(pz, io->ios2_PacketType);
        if (io->ios2_Req.io_Command == S2_TRACKTYPE) {
            if (t)
                fail(io, S2ERR_BAD_STATE, S2WERR_ALREADY_TRACKED);
            else if ((t = AllocVec(sizeof(*t), MEMF_PUBLIC | MEMF_CLEAR)) == NULL)
                fail(io, S2ERR_NO_RESOURCES, S2WERR_GENERIC_ERROR);
            else {
                t->type = io->ios2_PacketType;
                AddTail((struct List *)&pz->types, (struct Node *)t);
            }
        } else if (!t) {
            fail(io, S2ERR_BAD_STATE, S2WERR_NOT_TRACKED);
        } else if (io->ios2_Req.io_Command == S2_UNTRACKTYPE) {
            Remove((struct Node *)t);
            FreeVec(t);
        } else if (!io->ios2_StatData) {
            fail(io, S2ERR_BAD_ARGUMENT, S2WERR_NULL_POINTER);
        } else {
            pz_copy(&t->s, io->ios2_StatData, sizeof(t->s));
        }
        ReleaseSemaphore(&pz->lock);
        break;
    }
    default:
        return FALSE;
    }
    return TRUE;
}

static BOOL known(UWORD cmd)
{
    const UWORD *c;
    for (c = supported_cmds; *c; c++)
        if (*c == cmd)
            return TRUE;
    return FALSE;
}

void DevBeginIO(REGARG(struct IOSana2Req *io, a1), REGARG(struct PZBase *pz, a6))
{
    io->ios2_Req.io_Message.mn_Node.ln_Type = NT_MESSAGE;
    io->ios2_Req.io_Error = 0;
    if (io->ios2_Req.io_Command != S2_ONEVENT)
        io->ios2_WireError = 0;

    if (quick(pz, io)) {
        reply(io);
        return;
    }
    if (!known(io->ios2_Req.io_Command)) {
        fail(io, IOERR_NOCMD, 0);
        reply(io);
        return;
    }
    io->ios2_Req.io_Flags &= ~IOF_QUICK;
    PutMsg(pz->port, &io->ios2_Req.io_Message);
}

LONG DevAbortIO(REGARG(struct IOSana2Req *io, a1), REGARG(struct PZBase *pz, a6))
{
    struct Node *n;
    struct Opener *op, *nop;
    struct IOSana2Req *r, *nr;
    BOOL found = FALSE;

    /* Still waiting in the process's port? */
    Disable();
    if (io->ios2_Req.io_Message.mn_Node.ln_Type == NT_MESSAGE && pz->port) {
        for (n = pz->port->mp_MsgList.lh_Head; n->ln_Succ; n = n->ln_Succ) {
            if (n == (struct Node *)io) {
                Remove(n);
                found = TRUE;
                break;
            }
        }
    }
    Enable();
    if (found) {
        fail(io, IOERR_ABORTED, 0);
        ReplyMsg(&io->ios2_Req.io_Message);
        return 0;
    }

    /* Queued by the process: reads, orphan reads, events. */
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->openers, op, nop) {
        FOR_LIST(&op->reads, r, nr) if (r == io) found = TRUE;
        FOR_LIST(&op->orphans, r, nr) if (r == io) found = TRUE;
    }
    FOR_LIST(&pz->events, r, nr) if (r == io) found = TRUE;
    if (found) {
        Remove((struct Node *)io);
        fail(io, IOERR_ABORTED, 0);
        ReplyMsg(&io->ios2_Req.io_Message);
    }
    ReleaseSemaphore(&pz->lock);
    return 0;
}
