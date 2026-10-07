/*
 * picozorrousb.device: Poseidon USB hardware driver for PicoZorro.
 *
 * Structure follows picozorro.device (the network driver): RomTag +
 * autoinit tables, one process that does the hardware work. What the
 * process does is what usbhardware.doc asks of a host-controller driver:
 * emulate the root hub at its own address, queue transfers per endpoint,
 * split them into packets (here: into tunnel requests of up to 1 KiB that
 * the module splits further), keep data toggles (here: the module keeps
 * them, per cached pipe), apply NAK timeouts, answer aborts. The card's
 * side is in docs/REGISTERS-USB.md.
 */
#include <stdarg.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/utility.h>
#include <proto/timer.h>
#include <exec/errors.h>
#include <exec/memory.h>
#include <dos/dostags.h>
#include <devices/usb_hub.h>
#include <string.h>
#include "device.h"

struct ExecBase *SysBase;
struct DosLibrary *DOSBase;
struct Library *UtilityBase;
struct Device *TimerBase;
struct PZUBase *PZU;

static void new_list(struct MinList *l)
{
    l->mlh_Head = (struct MinNode *)&l->mlh_Tail;
    l->mlh_Tail = NULL;
    l->mlh_TailPred = (struct MinNode *)&l->mlh_Head;
}

#define FOR_LIST(l, n, next) \
    for (n = (void *)(l)->mlh_Head; (next = (void *)((struct MinNode *)n)->mln_Succ) != NULL; n = next)

#ifdef PZU_DEBUG
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

/* Lines are formatted into `line`. With a backend that has a log hook they
 * queue in `ring` (any task may log) and the device process sends them;
 * otherwise they go straight out through RawPutChar. */
#define LOG_RING 4096
static UBYTE ring[LOG_RING];
static UWORD ring_head, ring_tail;

struct LineBuf {
    UWORD len;
    UBYTE text[160];
};

static void put_line(REGARG(UBYTE c, d0), REGARG(struct LineBuf *lb, a3))
{
    if (c && lb->len < sizeof(lb->text) - 1)
        lb->text[lb->len++] = c;
}

void pzu_log(const char *fmt, ...)
{
    struct PZUBase *pz = PZU;
    struct LineBuf lb;
    va_list ap;
    UWORD i;

    lb.len = 0;
    RawDoFmt("pzu: ", NULL, (void (*)())put_line, &lb);
    va_start(ap, fmt);  /* m68k: a pointer to the stacked arguments, as RawDoFmt wants */
    RawDoFmt((STRPTR)fmt, (APTR)ap, (void (*)())put_line, &lb);
    va_end(ap);
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
    if (FindTask(NULL) == (struct Task *)pz->proc)
        pzu_log_flush(pz);
}

/* Device process only. */
void pzu_log_flush(struct PZUBase *pz)
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

ULONG pzu_now(struct PZUBase *pz)
{
    struct timeval tv;
    if (!TimerBase)
        return 0;
    GetSysTime(&tv);
    return tv.tv_secs * 1000 + tv.tv_micro / 1000;
}

static void reply(struct IOUsbHWReq *iou, BYTE err)
{
    iou->iouh_Req.io_Error = err;
    ReplyMsg(&iou->iouh_Req.io_Message);
}

#ifdef PZU_PROF
/* make PROF=1 (device.h, struct PzuProf). */
static ULONG prof_now(void)
{
    struct EClockVal ev;
    if (!TimerBase)
        return 0;
    ReadEClock(&ev);
    return ev.ev_lo;
}

/* An interrupt IN to a real device (not root_int). */
static BOOL prof_int_in(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    return iou->iouh_Req.io_Command == UHCMD_INTXFER && iou->iouh_Dir == UHDIR_IN &&
           iou->iouh_DevAddr != pz->root_addr;
}

/* The slot of (addr, ep); NULL when another endpoint holds it. `claim`: take it. */
static struct PzuProfEp *prof_ep(struct PZUBase *pz, struct IOUsbHWReq *iou, BOOL claim)
{
    UBYTE ep = iou->iouh_Endpoint & 0x0f;
    struct PzuProfEp *e = &pz->prof.ep[(iou->iouh_DevAddr * 3 + ep) & (PROF_EPS - 1)];
    if (e->addr == iou->iouh_DevAddr && e->ep == ep)
        return e;
    if (!claim)
        return NULL;
    e->state = PS_NONE;
    e->addr = iou->iouh_DevAddr;
    e->ep = ep;
    return e;
}

/* finish_xfer's reply of an interrupt IN report: T1 (pz->prof.t1) .. T2. */
static void prof_reply(struct PZUBase *pz, struct IOUsbHWReq *iou, BYTE err)
{
    struct PzuProf *p = &pz->prof;
    struct PzuProfEp *e = prof_ep(pz, iou, TRUE);
    ULONG t2 = prof_now(), t;

    p->n++;
    p->t_recv_io += p->t1 - p->t0;
    p->t_handle += t2 - p->t1;
    e->t2pre = t2;
    e->state = PS_REPLYING;
    reply(iou, err);
    t = prof_now();
    Forbid();
    if (e->state == PS_REPLYING) {
        /* No DevBeginIO for it during ReplyMsg. */
        e->t2 = t;
        e->state = PS_REPLIED;
        p->n_replymsg++;
        p->t_replymsg += t - t2;
    }
    Permit();
}

/* start_chunk sent an interrupt IN: T5. */
static void prof_sent(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    struct PzuProfEp *e = prof_ep(pz, iou, FALSE);
    if (e && e->state == PS_DISP) {
        pz->prof.t_send += prof_now() - e->t4;
        pz->prof.n_send++;
        e->state = PS_NONE;
    }
}
#endif

/* ------------------------------------------------------------- root hub */

static const UBYTE root_dev_desc[18] = {
    18, UDT_DEVICE, 0x10, 0x01, 9, 0, 0, 8, 0, 0, 0, 0, 0x00, 0x01, 1, 2, 0, 1,
};
static const UBYTE root_cfg_desc[25] = {
    9, UDT_CONFIGURATION, 25, 0, 1, 1, 0, 0xe0, 0,
    9, UDT_INTERFACE, 0, 0, 1, 9, 0, 0, 0,
    7, UDT_ENDPOINT, 0x81, 3, 1, 0, 255,
};
static const UBYTE root_hub_desc[9] = {
    9, UDT_HUB, 1, 0x00, 0x00, 50, 0, 0x00, 0xff,
};
static const UBYTE root_str0[4] = {4, UDT_STRING, 0x09, 0x04};
static const UBYTE root_str1[20] = {20, UDT_STRING, 'P', 0, 'i', 0, 'c', 0, 'o', 0, 'Z', 0, 'o', 0, 'r', 0, 'r', 0, 'o', 0};
static const UBYTE root_str2[18] = {18, UDT_STRING, 'r', 0, 'o', 0, 'o', 0, 't', 0, ' ', 0, 'h', 0, 'u', 0, 'b', 0};

static BYTE append(struct IOUsbHWReq *iou, UWORD *left, const void *src, UWORD n)
{
    if (n > *left)
        n = *left;
    CopyMem((APTR)src, (UBYTE *)iou->iouh_Data + iou->iouh_Actual, n);
    iou->iouh_Actual += n;
    *left -= n;
    return 0;
}

/* Answer requests to the root hub's port-change interrupt endpoint. */
static void root_wake_int(struct PZUBase *pz)
{
    struct IOUsbHWReq *iou, *next;
    UBYTE bitmap = 0x02;
    if (!pz->port_change)
        return;
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->root_int, iou, next) {
        Remove((struct Node *)iou);
        iou->iouh_Actual = 0;
        if (iou->iouh_Length) {
            ((UBYTE *)iou->iouh_Data)[0] = bitmap;
            iou->iouh_Actual = 1;
        }
        reply(iou, 0);
    }
    ReleaseSemaphore(&pz->lock);
}

static BYTE root_control(struct PZUBase *pz, struct IOUsbHWReq *iou);
static BOOL send_op(struct PZUBase *pz, UBYTE op, struct IOUsbHWReq *iou, UBYTE addr, UBYTE ep, UWORD arg);

#define CTLREQ(type, req) (((type) << 8) | (req))

static BYTE root_control(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    struct UsbSetupData *s = &iou->iouh_SetupData;
    UWORD value = (s->wValue << 8) | (s->wValue >> 8);   /* little-endian on the wire */
    UWORD index = (s->wIndex << 8) | (s->wIndex >> 8);
    UWORD left = (s->wLength << 8) | (s->wLength >> 8);
    UBYTE buf[4];

    iou->iouh_Actual = 0;
    if (left > iou->iouh_Length)
        left = iou->iouh_Length;
    D(("root: req %02lx/%02lx value %04lx index %04lx len %ld", (ULONG)s->bmRequestType, (ULONG)s->bRequest,
       (ULONG)value, (ULONG)index, (ULONG)left));

    switch (CTLREQ(s->bmRequestType, s->bRequest)) {
    case CTLREQ(URTF_OUT | URTF_STANDARD | URTF_DEVICE, USR_SET_ADDRESS):
        pz->root_addr = value & 0x7f;
        return 0;
    case CTLREQ(URTF_IN | URTF_STANDARD | URTF_DEVICE, USR_GET_DESCRIPTOR):
        switch (value >> 8) {
        case UDT_DEVICE:
            return append(iou, &left, root_dev_desc, sizeof(root_dev_desc));
        case UDT_CONFIGURATION:
            return append(iou, &left, root_cfg_desc, sizeof(root_cfg_desc));
        case UDT_STRING:
            switch (value & 0xff) {
            case 0: return append(iou, &left, root_str0, sizeof(root_str0));
            case 1: return append(iou, &left, root_str1, sizeof(root_str1));
            case 2: return append(iou, &left, root_str2, sizeof(root_str2));
            }
            return UHIOERR_STALL;
        }
        return UHIOERR_STALL;
    case CTLREQ(URTF_IN | URTF_STANDARD | URTF_DEVICE, USR_GET_CONFIGURATION):
        buf[0] = pz->root_config;
        return append(iou, &left, buf, 1);
    case CTLREQ(URTF_OUT | URTF_STANDARD | URTF_DEVICE, USR_SET_CONFIGURATION):
        pz->root_config = value & 0xff;
        return 0;
    case CTLREQ(URTF_IN | URTF_STANDARD | URTF_DEVICE, USR_GET_STATUS):
        buf[0] = 1; /* self powered */
        buf[1] = 0;
        return append(iou, &left, buf, 2);
    case CTLREQ(URTF_IN | URTF_STANDARD | URTF_INTERFACE, USR_GET_STATUS):
    case CTLREQ(URTF_IN | URTF_STANDARD | URTF_ENDPOINT, USR_GET_STATUS):
        buf[0] = buf[1] = 0;
        return append(iou, &left, buf, 2);
    case CTLREQ(URTF_OUT | URTF_STANDARD | URTF_ENDPOINT, USR_CLEAR_FEATURE):
    case CTLREQ(URTF_OUT | URTF_STANDARD | URTF_INTERFACE, USR_SET_INTERFACE):
        return 0;

    /* hub class */
    case CTLREQ(URTF_IN | URTF_CLASS | URTF_DEVICE, USR_GET_DESCRIPTOR):
        return append(iou, &left, root_hub_desc, sizeof(root_hub_desc));
    case CTLREQ(URTF_IN | URTF_CLASS | URTF_DEVICE, USR_GET_STATUS):
        buf[0] = buf[1] = buf[2] = buf[3] = 0;
        return append(iou, &left, buf, 4);
    case CTLREQ(URTF_OUT | URTF_CLASS | URTF_DEVICE, USR_CLEAR_FEATURE):
    case CTLREQ(URTF_OUT | URTF_CLASS | URTF_DEVICE, USR_SET_FEATURE):
        return 0;
    case CTLREQ(URTF_IN | URTF_CLASS | URTF_OTHER, USR_GET_STATUS):
        if ((index & 0xff) != 1)
            return UHIOERR_STALL;
        buf[0] = pz->port_status & 0xff;
        buf[1] = pz->port_status >> 8;
        buf[2] = pz->port_change & 0xff;
        buf[3] = pz->port_change >> 8;
        return append(iou, &left, buf, 4);
    case CTLREQ(URTF_OUT | URTF_CLASS | URTF_OTHER, USR_SET_FEATURE):
        if ((index & 0xff) != 1)
            return UHIOERR_STALL;
        switch (value) {
        case UFS_PORT_POWER:
            pz->port_status |= UPSF_PORT_POWER;
            pz->next_poll = pzu_now(pz); /* look at the port now */
            return 0;
        case UFS_PORT_RESET:
            if (!pz->port_connected)
                return UHIOERR_STALL;
            if (pz->reset_iou)
                return UHIOERR_NAK;
            pz->port_status |= UPSF_PORT_RESET;
            if (!send_op(pz, OP_PORT_RESET, iou, 0, 0, 0))
                return UHIOERR_HOSTERROR;
            pz->reset_iou = iou;
            return -1; /* replied when the module answers */
        case UFS_PORT_SUSPEND:
            pz->port_status |= UPSF_PORT_SUSPEND;
            return 0;
        case UFS_PORT_ENABLE:
            pz->port_status |= UPSF_PORT_ENABLE;
            return 0;
        }
        return 0;
    case CTLREQ(URTF_OUT | URTF_CLASS | URTF_OTHER, USR_CLEAR_FEATURE):
        if ((index & 0xff) != 1)
            return UHIOERR_STALL;
        switch (value) {
        case UFS_PORT_ENABLE:
            pz->port_status &= ~UPSF_PORT_ENABLE;
            return 0;
        case UFS_PORT_SUSPEND:
            pz->port_status &= ~UPSF_PORT_SUSPEND;
            return 0;
        case UFS_PORT_POWER:
            pz->port_status &= ~UPSF_PORT_POWER;
            return 0;
        case UFS_C_PORT_CONNECTION: pz->port_change &= ~UPSF_PORT_CONNECTION; return 0;
        case UFS_C_PORT_ENABLE:     pz->port_change &= ~UPSF_PORT_ENABLE; return 0;
        case UFS_C_PORT_SUSPEND:    pz->port_change &= ~UPSF_PORT_SUSPEND; return 0;
        case UFS_C_PORT_OVER_CURRENT: pz->port_change &= ~UPSF_PORT_OVER_CURRENT; return 0;
        case UFS_C_PORT_RESET:      pz->port_change &= ~UPSF_PORT_RESET; return 0;
        }
        return 0;
    }
    D(("root: unsupported request"));
    return UHIOERR_STALL;
}

/* The module told us what the root port looks like. */
static void port_update(struct PZUBase *pz, BOOL connected, UBYTE speed)
{
    if (connected != pz->port_connected) {
        pz->port_connected = connected;
        pz->port_change |= UPSF_PORT_CONNECTION;
        if (connected) {
            pz->port_status |= UPSF_PORT_CONNECTION;
        } else {
            pz->port_status &= ~(UPSF_PORT_CONNECTION | UPSF_PORT_ENABLE | UPSF_PORT_LOW_SPEED);
            if (pz->port_status & UPSF_PORT_ENABLE)
                pz->port_change |= UPSF_PORT_ENABLE;
        }
        D(("root port: %s", connected ? (ULONG)"connected" : (ULONG)"disconnected"));
    }
    pz->port_speed = speed;
    if (speed == 1)
        pz->port_status |= UPSF_PORT_LOW_SPEED;
    else
        pz->port_status &= ~UPSF_PORT_LOW_SPEED;
    root_wake_int(pz);
}

/* --------------------------------------------------------------- transfers */

static struct Slot *free_slot(struct PZUBase *pz)
{
    int i;
    for (i = 0; i < SLOTS; i++)
        if (pz->slots[i].op == 0xff)
            return &pz->slots[i];
    return NULL;
}

static void slot_free(struct Slot *s)
{
    s->op = 0xff;
    s->iou = NULL;
    s->aborted = 0;
    s->stream = 0;
    s->out_unacked = 0;
}

/* A word in ENV:PZUSB after the backend's name. "stream": stream over a
 * backend that can lose records too (tests over the UDP tunnel; a lost
 * record fails the read). "nostream": never stream. */
/* The number after "w=" in the args, else `def`. */
static UWORD args_num(const char *a, const char *w, UWORD def)
{
    for (; *a; a++) {
        if (*a == ' ') {
            const char *p = a + 1, *q = w;
            while (*q && *p == *q) {
                p++;
                q++;
            }
            if (!*q && *p == '=') {
                UWORD v = 0;
                for (p++; *p >= '0' && *p <= '9'; p++)
                    v = v * 10 + (*p - '0');
                return v;
            }
        }
    }
    return def;
}

static BOOL args_word(const char *a, const char *w)
{
    for (; *a; a++) {
        if (*a == ' ') {
            const char *p = a + 1, *q = w;
            while (*q && *p == *q) {
                p++;
                q++;
            }
            if (!*q && (*p == 0 || *p == ' ' || *p == '\n'))
                return TRUE;
        }
    }
    return FALSE;
}

static BOOL ep_busy(struct PZUBase *pz, UBYTE addr, UBYTE ep)
{
    int i;
    for (i = 0; i < SLOTS; i++)
        if (pz->slots[i].op == OP_XFER && pz->slots[i].key_addr == addr && pz->slots[i].key_ep == ep)
            return TRUE;
    return FALSE;
}

static BOOL ensure_backend(struct PZUBase *pz)
{
    if (pz->be_open)
        return TRUE;
    if (pz->be->open(pz, pz->be_args) != 0) {
        pz->be->close(pz);
        /* No ENV:PZUSB: the card on the bus if there is one, else the
         * UDP tunnel. */
        if (pz->be_auto && pz->be == &backend_pz) {
            pz->be = &backend_uae;
            return ensure_backend(pz);
        }
        return FALSE;
    }
    pz->be_open = TRUE;
    /* Not 0: half of the time the clock is "before" 0 (times wrap). */
    pz->next_poll = pzu_now(pz);
    pz->features = 0;
    pz->stream_ok = (pz->be->lossless || args_word(pz->be_args, "stream")) && !args_word(pz->be_args, "nostream");
    /* Bulk OUT too where IN streams by itself; over the tunnel only when
     * asked ("streamout"). "nostreamout": one record per request. */
    pz->stream_out_ok = pz->stream_ok && (pz->be->lossless || args_word(pz->be_args, "streamout")) &&
                        !args_word(pz->be_args, "nostreamout");
    pz->poll1 = args_word(pz->be_args, "poll1");
    /* A report costs the 68030 about 3.5 ms (hid.class, input.device,
     * Intuition); a mouse polled every 2 ms as it asks keeps the CPU
     * busy for good. Motion accumulates in the device, so 10 ms loses
     * nothing but latency. "minpoll=N": another floor, 0 none. */
    pz->minpoll = args_num(pz->be_args, "minpoll", 10);
    return TRUE;
}

/* A request of the driver's own (root port) or a cache op. */
static BOOL send_op(struct PZUBase *pz, UBYTE op, struct IOUsbHWReq *iou, UBYTE addr, UBYTE ep, UWORD arg)
{
    struct PzuReq h;
    struct Slot *s;
    if (!ensure_backend(pz))
        return FALSE;
    s = free_slot(pz);
    if (!s)
        return FALSE;
    memset(&h, 0, sizeof(h));
    h.magic = PZU_MAGIC;
    h.version = PZU_VERSION;
    h.op = op;
    h.seq = ++pz->seq;
    h.addr = addr;
    h.ep = ep;
    h.length = arg;
    if (pz->be->send(pz, &h, NULL, 0) != 0)
        return FALSE;
    s->op = op;
    s->iou = iou;
    s->seq = h.seq;
    s->key_addr = 0xff;
    s->deadline = pzu_now(pz) + 5000;
    return TRUE;
}

static UBYTE xfer_kind(UWORD cmd)
{
    switch (cmd) {
    case UHCMD_CONTROLXFER: return XT_CONTROL;
    case UHCMD_BULKXFER: return XT_BULK;
    case UHCMD_INTXFER: return XT_INTERRUPT;
    default: return XT_ISO;
    }
}

/* Tell the module to stop request `seq` (its reply comes with status 15). */
static void send_abort(struct PZUBase *pz, UWORD seq)
{
    struct PzuReq h;
    memset(&h, 0, sizeof(h));
    h.magic = PZU_MAGIC;
    h.version = PZU_VERSION;
    h.op = OP_ABORT;
    h.seq = ++pz->seq;
    h.length = seq;
    pz->be->send(pz, &h, NULL, 0);
}

/* The next record of the streamed bulk OUT in slot `s`: an XFER_DATA
 * with the stream's seq and the data from out_sent on. */
static BOOL send_data(struct PZUBase *pz, struct Slot *s)
{
    struct IOUsbHWReq *iou = s->iou;
    struct PzuReq h;
    ULONG left = iou->iouh_Length - s->out_sent;
    UWORD n = left > s->chunk ? s->chunk : (UWORD)left;

    memset(&h, 0, sizeof(h));
    h.magic = PZU_MAGIC;
    h.version = PZU_VERSION;
    h.op = OP_XFER_DATA;
    h.seq = s->seq;
    h.addr = s->key_addr;
    h.ep = s->key_ep;
    h.kind = XT_BULK;
    h.mps = iou->iouh_MaxPktSize;
    h.timeout_ms = s->timeout_ms;
    h.length = n;
    if (pz->be->send(pz, &h, (UBYTE *)iou->iouh_Data + s->out_sent, n) != 0)
        return FALSE;
    D(("xfer seq %ld: data %ld at %ld of %ld", (ULONG)h.seq, (ULONG)n, s->out_sent, iou->iouh_Length));
    s->out_sent += n;
    s->out_unacked++;
    return TRUE;
}

/* A streamed bulk OUT that cannot go on (a continuation could not be
 * sent): stop the module, fail the request with what was written. */
static void fail_stream_out(struct PZUBase *pz, struct Slot *s)
{
    struct IOUsbHWReq *iou = s->iou;
    send_abort(pz, s->seq);
    iou->iouh_Actual = s->out_acked;
    slot_free(s);
    pz->n_errors++;
    reply(iou, UHIOERR_HOSTERROR);
}

/* Put the next chunk of `iou` on the wire. */
static BOOL start_chunk(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    struct PzuReq h;
    struct Slot *s = free_slot(pz);
    ULONG remaining = iou->iouh_Length - iou->iouh_Actual;
    UWORD chunk;
    UBYTE kind = xfer_kind(iou->iouh_Req.io_Command);
    BOOL in;
    UBYTE stream = 0;

    if (!s)
        return FALSE;
    memset(&h, 0, sizeof(h));
    h.magic = PZU_MAGIC;
    h.version = PZU_VERSION;
    h.op = OP_XFER;
    h.seq = ++pz->seq;
    h.addr = iou->iouh_DevAddr;
    h.kind = kind;
    h.mps = iou->iouh_MaxPktSize;
    if (kind == XT_CONTROL) {
        in = (iou->iouh_SetupData.bmRequestType & URTF_IN) != 0;
        h.ep = iou->iouh_Endpoint & 0x0f;
        CopyMem(&iou->iouh_SetupData, h.setup, 8);
        chunk = remaining > PZU_MAX_DATA ? PZU_MAX_DATA : remaining;
    } else {
        in = iou->iouh_Dir == UHDIR_IN;
        h.ep = (iou->iouh_Endpoint & 0x0f) | (in ? 0x80 : 0);
        if (remaining > PZU_MAX_DATA) {
            chunk = PZU_MAX_DATA - (PZU_MAX_DATA % (h.mps ? h.mps : 8));
            h.flags |= XF_NOSHORT; /* more follows: never a ZLP here */
            if (kind == XT_BULK && (in ? pz->stream_ok : pz->stream_out_ok) &&
                (pz->features & (in ? FEAT_STREAM_IN : FEAT_STREAM_OUT))) {
                /* One request for all of it. IN: the module sends a
                 * record per chunk without waiting for us in between.
                 * OUT: this record and XFER_DATA records with the rest,
                 * two ahead of the acknowledgements; NOSHORT is about the
                 * end of the whole transfer then. */
                h.flags |= XF_STREAM;
                h.setup[0] = remaining >> 24;
                h.setup[1] = remaining >> 16;
                h.setup[2] = remaining >> 8;
                h.setup[3] = remaining;
                stream = in ? SS_IN : SS_OUT;
                if (!in && !(iou->iouh_Flags & UHFF_NOSHORTPKT))
                    h.flags &= ~XF_NOSHORT;
            }
        } else {
            chunk = remaining;
            if (iou->iouh_Flags & UHFF_NOSHORTPKT)
                h.flags |= XF_NOSHORT;
        }
    }
    if (iou->iouh_Flags & UHFF_LOWSPEED) {
        h.flags |= XF_LOWSPEED;
        if (pz->port_speed == 2)
            h.flags |= XF_PRE; /* low speed behind our full-speed root port = behind a hub */
        h.hub_addr = iou->iouh_SplitHubAddr;
        h.hub_port = iou->iouh_SplitHubPort;
    }
    h.timeout_ms = (iou->iouh_Flags & UHFF_NAKTIMEOUT) ? (iou->iouh_NakTimeout > 60000 ? 60000 : iou->iouh_NakTimeout) : 5000;
    h.length = chunk;
    /* Poseidon's interval is in ms; the module clamps it to 1..255 (0 = 1). */
    if (kind == XT_INTERRUPT && !pz->poll1)
        h.interval = iou->iouh_Interval > pz->minpoll ? iou->iouh_Interval : pz->minpoll;

    if (pz->be->send(pz, &h, in ? NULL : (UBYTE *)iou->iouh_Data + iou->iouh_Actual, in ? 0 : chunk) != 0)
        return FALSE;
#ifdef PZU_PROF
    if (kind == XT_INTERRUPT && in && iou->iouh_DevAddr != pz->root_addr)
        prof_sent(pz, iou);
#endif
    D(("xfer seq %ld: addr %ld ep %02lx kind %ld flags %02lx mps %ld chunk %ld off %ld of %ld, iouh_Flags %04lx",
       (ULONG)h.seq, (ULONG)h.addr, (ULONG)h.ep, (ULONG)kind, (ULONG)h.flags, (ULONG)h.mps, (ULONG)chunk,
       iou->iouh_Actual, iou->iouh_Length, (ULONG)iou->iouh_Flags));
    s->op = OP_XFER;
    s->iou = iou;
    s->seq = h.seq;
    s->key_addr = h.addr;
    s->key_ep = h.ep;
    s->chunk = chunk;
    s->off = iou->iouh_Actual;
    s->timeout_ms = h.timeout_ms;
    s->deadline = pzu_now(pz) + h.timeout_ms + 3000;
    s->aborted = 0;
    s->stream = stream;
    if (stream == SS_OUT) {
        s->out_acked = iou->iouh_Actual;
        s->out_sent = iou->iouh_Actual + chunk;
        s->out_unacked = 1;
        /* The second record goes now; the others one per acknowledgement. */
        if (!send_data(pz, s)) {
            send_abort(pz, s->seq);
            slot_free(s);
            return FALSE;
        }
    }
    return TRUE;
}

/* A reply of a streamed bulk OUT: with MORE, the acknowledgement of the
 * oldest record not acknowledged yet (no payload; the backends clamp
 * `actual` to the payload, so the record sizes count, not rep->actual),
 * which makes room for the next one; without MORE the end of the stream. */
static void finish_stream_out(struct PZUBase *pz, struct Slot *s, struct PzuRep *rep)
{
    struct IOUsbHWReq *iou = s->iou;
    BYTE err = rep->status;

    if (err == 0 && (rep->flags & RF_MORE)) {
        ULONG n = iou->iouh_Length - s->out_acked;
        if (n > s->chunk)
            n = s->chunk;
        if (s->out_unacked) {
            s->out_acked += n;
            s->out_unacked--;
        }
        iou->iouh_Actual = s->out_acked;
        s->deadline = pzu_now(pz) + s->timeout_ms + 3000;
        while (s->out_unacked < 2 && s->out_sent < iou->iouh_Length) {
            if (!send_data(pz, s)) {
                fail_stream_out(pz, s);
                return;
            }
        }
        return;
    }
    if (err == 0 && s->out_sent == iou->iouh_Length) {
        iou->iouh_Actual = iou->iouh_Length;
    } else {
        /* An error, or an end before everything was sent. */
        iou->iouh_Actual = s->out_acked;
        if (err == 0)
            err = UHIOERR_HOSTERROR;
        pz->n_errors++;
        D(("xfer seq %ld: stream OUT ended, status %ld, %ld of %ld written", (ULONG)s->seq, (ULONG)rep->status,
           s->out_acked, iou->iouh_Length));
    }
    if (err == ST_ABORTED)
        err = IOERR_ABORTED;
    pz->n_xfers++;
    slot_free(s);
    reply(iou, err);
}

/* Start what can start: free slot, endpoint idle, arrival order. */
static void schedule(struct PZUBase *pz)
{
    struct IOUsbHWReq *iou, *next;
    if (!pz->be_open)
        return;
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->waiting, iou, next) {
        UBYTE ep = (iou->iouh_Endpoint & 0x0f);
        if (iou->iouh_Req.io_Command != UHCMD_CONTROLXFER && iou->iouh_Dir == UHDIR_IN)
            ep |= 0x80;
        if (!free_slot(pz))
            break;
        if (ep_busy(pz, iou->iouh_DevAddr, ep))
            continue;
        Remove((struct Node *)iou);
        if (!start_chunk(pz, iou)) {
            pz->n_errors++;
            reply(iou, UHIOERR_HOSTERROR);
        }
    }
    ReleaseSemaphore(&pz->lock);
}

/* After a control request that changes what the module caches. */
static void after_control(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    struct UsbSetupData *s = &iou->iouh_SetupData;
    UWORD value = (s->wValue << 8) | (s->wValue >> 8);
    UWORD index = (s->wIndex << 8) | (s->wIndex >> 8);
    if (s->bmRequestType == (URTF_OUT | URTF_STANDARD | URTF_DEVICE) && s->bRequest == USR_SET_ADDRESS) {
        send_op(pz, OP_FORGET, NULL, iou->iouh_DevAddr, 0, 0);
        send_op(pz, OP_FORGET, NULL, value & 0x7f, 0, 0);
    } else if (s->bmRequestType == (URTF_OUT | URTF_STANDARD | URTF_ENDPOINT) && s->bRequest == USR_CLEAR_FEATURE &&
               value == 0) {
        send_op(pz, OP_RESET_TOGGLE, NULL, iou->iouh_DevAddr, (UBYTE)index, 0);
    }
}

static void finish_xfer(struct PZUBase *pz, struct Slot *s, struct PzuRep *rep)
{
    struct IOUsbHWReq *iou = s->iou;
    BYTE err = rep->status;
    BOOL in = (s->key_ep & 0x80) != 0 || (xfer_kind(iou->iouh_Req.io_Command) == XT_CONTROL &&
                                          (iou->iouh_SetupData.bmRequestType & URTF_IN));

    if (err == UHIOERR_NAKTIMEOUT && !(iou->iouh_Flags & UHFF_NAKTIMEOUT)) {
        /* No NAK timeout wanted: the 5 s were only ours. Ask again; a
         * streamed OUT from what the device has acknowledged (the module
         * ended the stream with this reply). */
        if (s->stream == SS_OUT)
            iou->iouh_Actual = s->out_acked;
        slot_free(s);
        if (!start_chunk(pz, iou))
            reply(iou, UHIOERR_HOSTERROR);
        return;
    }
    if (s->stream == SS_OUT) {
        finish_stream_out(pz, s, rep);
        return;
    }
    if (err == 0) {
        if (s->off + rep->actual > iou->iouh_Length)
            rep->actual = iou->iouh_Length - s->off;
        if (in)
            CopyMem(pz->buf, (UBYTE *)iou->iouh_Data + s->off, rep->actual);
        iou->iouh_Actual = s->off + rep->actual;
        if (s->stream == SS_IN && (rep->flags & RF_MORE)) {
            /* The next record is on its way already. */
            s->off = iou->iouh_Actual;
            s->deadline = pzu_now(pz) + s->timeout_ms + 3000;
            return;
        }
        if (!s->stream && iou->iouh_Actual < iou->iouh_Length && rep->actual == s->chunk) {
            /* More to do, same endpoint, keep the slot's turn. */
            slot_free(s);
            if (!start_chunk(pz, iou))
                reply(iou, UHIOERR_HOSTERROR);
            return;
        }
        if (in && iou->iouh_Actual < iou->iouh_Length && !(iou->iouh_Flags & UHFF_ALLOWRUNTPKTS))
            err = UHIOERR_RUNTPACKET;
        if (xfer_kind(iou->iouh_Req.io_Command) == XT_CONTROL)
            after_control(pz, iou);
    } else {
        pz->n_errors++;
    }
    if (err == ST_ABORTED)
        err = IOERR_ABORTED;
    pz->n_xfers++;
    slot_free(s);
#ifdef PZU_PROF
    if (prof_int_in(pz, iou)) {
        prof_reply(pz, iou, err);
        return;
    }
#endif
    reply(iou, err);
}

static void on_reply(struct PZUBase *pz, struct PzuRep *rep)
{
    struct Slot *s = NULL;
    int i;
    for (i = 0; i < SLOTS; i++)
        if (pz->slots[i].op != 0xff && pz->slots[i].seq == rep->seq)
            s = &pz->slots[i];
    if (!s) {
        D(("reply seq %ld: no slot (late or aborted)", (ULONG)rep->seq));
        return;
    }
    D(("reply seq %ld: op %ld status %ld actual %ld", (ULONG)rep->seq, (ULONG)s->op, (ULONG)rep->status,
       (ULONG)rep->actual));
    switch (s->op) {
    case OP_PORT_STATUS:
    case OP_PORT_RESET:
        if (rep->status == 0 && rep->actual >= 2)
            port_update(pz, pz->buf[0] != 0, pz->buf[1]);
        if (rep->status == 0)
            pz->features = rep->actual >= 3 ? pz->buf[2] : 0;
        if (s->op == OP_PORT_RESET) {
            struct IOUsbHWReq *iou = s->iou;
            pz->port_status &= ~UPSF_PORT_RESET;
            pz->port_status |= UPSF_PORT_ENABLE;
            pz->port_change |= UPSF_PORT_RESET;
            pz->reset_iou = NULL;
            slot_free(s);
            if (iou)
                reply(iou, 0);
            root_wake_int(pz);
            break;
        }
        slot_free(s);
        break;
    case OP_XFER:
        if (s->aborted) {
            slot_free(s);
            break;
        }
        finish_xfer(pz, s, rep);
        break;
    default:
        slot_free(s);
        break;
    }
}

/* Lost datagrams: ask again; the module answers within the timeout it was given. */
static void expire(struct PZUBase *pz, ULONG now)
{
    int i;
    for (i = 0; i < SLOTS; i++) {
        struct Slot *s = &pz->slots[i];
        struct IOUsbHWReq *iou;
        if (s->op == 0xff || (LONG)(now - s->deadline) < 0)
            continue;
        iou = s->iou;
        D(("slot %ld op %ld seq %ld: no reply, retry", (ULONG)i, (ULONG)s->op, (ULONG)s->seq));
        pz->n_retries++;
        if (s->op == OP_XFER && iou && !s->aborted && s->stream) {
            /* A record of a stream went missing: what follows would land
             * at the wrong offset (IN), or the acknowledgements stopped
             * (OUT). Stop the module and fail the transfer. */
            send_abort(pz, s->seq);
            if (s->stream == SS_OUT)
                iou->iouh_Actual = s->out_acked;
            slot_free(s);
            pz->n_errors++;
            reply(iou, UHIOERR_HOSTERROR);
        } else if (s->op == OP_XFER && iou && !s->aborted) {
            slot_free(s);
            if (!start_chunk(pz, iou))
                reply(iou, UHIOERR_HOSTERROR);
        } else if (s->op == OP_PORT_RESET) {
            slot_free(s);
            pz->reset_iou = NULL;
            pz->port_status &= ~UPSF_PORT_RESET;
            if (iou)
                reply(iou, UHIOERR_HOSTERROR);
        } else {
            slot_free(s);
        }
    }
}

/* AbortIO marked requests; only the process may touch the socket. */
static void do_aborts(struct PZUBase *pz)
{
    int i;
    ObtainSemaphore(&pz->lock);
    for (i = 0; i < SLOTS; i++) {
        struct Slot *s = &pz->slots[i];
        if (s->op == OP_XFER && s->aborted && s->iou) {
            struct IOUsbHWReq *iou = s->iou;
            send_abort(pz, s->seq);
            if (s->stream == SS_OUT)
                iou->iouh_Actual = s->out_acked;
            slot_free(s);
            reply(iou, IOERR_ABORTED);
        }
    }
    ReleaseSemaphore(&pz->lock);
}

static void flush_all(struct PZUBase *pz)
{
    struct IOUsbHWReq *iou, *next;
    int i;
    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->waiting, iou, next) {
        Remove((struct Node *)iou);
        reply(iou, IOERR_ABORTED);
    }
    FOR_LIST(&pz->root_int, iou, next) {
        Remove((struct Node *)iou);
        reply(iou, IOERR_ABORTED);
    }
    for (i = 0; i < SLOTS; i++)
        if (pz->slots[i].op == OP_XFER && pz->slots[i].iou)
            pz->slots[i].aborted = 1;
    ReleaseSemaphore(&pz->lock);
    do_aborts(pz);
}

/* One request from BeginIO, in the process. */
static void dispatch(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    UWORD cmd = iou->iouh_Req.io_Command;
    BYTE err;
#ifdef PZU_PROF
    ULONG t4 = prof_now();
    if (prof_int_in(pz, iou)) {
        struct PzuProfEp *e = prof_ep(pz, iou, FALSE);
        if (e && e->state == PS_BEGUN) {
            pz->prof.t_wake += t4 - e->t3;
            pz->prof.n_wake++;
            e->t4 = t4;
            e->state = PS_DISP;
        }
    }
#endif

    D(("cmd %ld addr %ld ep %ld len %ld flags %lx", (ULONG)cmd, (ULONG)iou->iouh_DevAddr, (ULONG)iou->iouh_Endpoint,
       iou->iouh_Length, (ULONG)iou->iouh_Flags));
    switch (cmd) {
    case CMD_FLUSH:
        flush_all(pz);
        reply(iou, 0);
        return;
    case CMD_RESET:
    case UHCMD_USBRESET:
    case UHCMD_USBOPER:
    case UHCMD_USBRESUME:
        if (!ensure_backend(pz)) {
            pz->state = 0;
            iou->iouh_State = 0;
            reply(iou, UHIOERR_HOSTERROR);
            return;
        }
        pz->state = UHSF_OPERATIONAL;
        iou->iouh_State = pz->state;
        pz->next_poll = pzu_now(pz);
        reply(iou, 0);
        return;
    case UHCMD_USBSUSPEND:
        pz->state = UHSF_SUSPENDED;
        iou->iouh_State = pz->state;
        reply(iou, 0);
        return;
    case UHCMD_CONTROLXFER:
    case UHCMD_BULKXFER:
    case UHCMD_INTXFER:
    case UHCMD_ISOXFER:
        break;
    default:
        reply(iou, IOERR_NOCMD);
        return;
    }

    if (!(pz->state & UHSF_OPERATIONAL) || !ensure_backend(pz)) {
        reply(iou, UHIOERR_USBOFFLINE);
        return;
    }
    iou->iouh_Actual = 0;
    if (iou->iouh_DevAddr == pz->root_addr) {
        if (cmd == UHCMD_CONTROLXFER) {
            err = root_control(pz, iou);
            if (err != -1)
                reply(iou, err);
        } else if (cmd == UHCMD_INTXFER && iou->iouh_Endpoint == 1) {
            ObtainSemaphore(&pz->lock);
            AddTail((struct List *)&pz->root_int, (struct Node *)iou);
            ReleaseSemaphore(&pz->lock);
            root_wake_int(pz);
        } else {
            reply(iou, UHIOERR_STALL);
        }
        return;
    }
    if (cmd == UHCMD_ISOXFER || iou->iouh_MaxPktSize == 0 || iou->iouh_MaxPktSize > PZU_MAX_DATA ||
        (cmd == UHCMD_CONTROLXFER && iou->iouh_Length > PZU_MAX_DATA)) {
        reply(iou, UHIOERR_BADPARAMS);
        return;
    }
    if (iou->iouh_Length && !iou->iouh_Data) {
        reply(iou, UHIOERR_BADPARAMS);
        return;
    }
    ObtainSemaphore(&pz->lock);
    AddTail((struct List *)&pz->waiting, (struct Node *)iou);
    ReleaseSemaphore(&pz->lock);
}

static void proc_main(void)
{
    struct PZUBase *pz = PZU;
    struct MsgPort *tport = NULL;
    ULONG portsig, sigs;
    struct IOUsbHWReq *iou;
    int i;

    pz->port = CreateMsgPort();
    tport = CreateMsgPort();
    pz->buf = AllocVec(PZU_MAX_DATA, MEMF_PUBLIC);
    if (tport && (pz->treq = (struct timerequest *)CreateIORequest(tport, sizeof(struct timerequest))) != NULL &&
        OpenDevice(TIMERNAME, UNIT_VBLANK, (struct IORequest *)pz->treq, 0) == 0)
        TimerBase = pz->treq->tr_node.io_Device;
#ifdef PZU_PROF
    if (TimerBase) {
        struct EClockVal ev;
        pz->prof.eclock_hz = ReadEClock(&ev);
    }
#endif

    pz->startup_error = IOERR_OPENFAIL;
    if (GetVar("PZUSB", pz->be_args, sizeof(pz->be_args), GVF_GLOBAL_ONLY) <= 0)
        pz->be_args[0] = 0;
    /* ENV:PZUSB: "uae ...", "ser ...", "bp ..." or "pz [6]"; none: `pz`
     * when the card is on the bus, else `uae` (ensure_backend). */
    pz->be_auto = pz->be_args[0] == 0;
    if (pz->be_auto)
        pz->be = &backend_pz;
    else if (pz->be_args[0] == 's' && pz->be_args[1] == 'e' && pz->be_args[2] == 'r')
        pz->be = &backend_ser;
    else if (pz->be_args[0] == 'b' && pz->be_args[1] == 'p')
        pz->be = &backend_bp;
    else if (pz->be_args[0] == 'p' && pz->be_args[1] == 'z')
        pz->be = &backend_pz;
    else
        pz->be = &backend_uae;
    pz->be_open = FALSE;
    pz->state = 0;
    pz->root_addr = 0;
    pz->root_config = 0;
    pz->port_status = 0;
    pz->port_change = 0;
    pz->port_connected = FALSE;
    pz->port_speed = 0;
    pz->reset_iou = NULL;
    new_list(&pz->waiting);
    new_list(&pz->root_int);
    for (i = 0; i < SLOTS; i++)
        slot_free(&pz->slots[i]);
    D(("process up: backend %s, args '%s', timer %lx", (ULONG)pz->be->name, (ULONG)pz->be_args, (ULONG)TimerBase));
    if (pz->port && pz->buf && TimerBase)
        pz->startup_error = 0;

    if (pz->startup_error == 0) {
        Signal(pz->opener_task, SIGF_SINGLE);
        portsig = 1UL << pz->port->mp_SigBit;
        for (;;) {
            ULONG now;
#ifdef PZU_PROF
            BOOL busy = FALSE;
            pz->prof.n_loops++;
#endif
            if (pz->be_open) {
                sigs = pz->be->wait(pz, portsig | SIGBREAKF_CTRL_C | SIGBREAKF_CTRL_F, 100);
            } else {
                sigs = Wait(portsig | SIGBREAKF_CTRL_C | SIGBREAKF_CTRL_F);
                pz->be_readable = FALSE;
            }
            if (sigs & SIGBREAKF_CTRL_C)
                break;
#ifdef PZU_PROF
            while ((iou = (struct IOUsbHWReq *)GetMsg(pz->port)) != NULL) {
                busy = TRUE;
                dispatch(pz, iou);
            }
#else
            while ((iou = (struct IOUsbHWReq *)GetMsg(pz->port)) != NULL)
                dispatch(pz, iou);
#endif
            /* Drain the socket every time round, not only when the wait
             * ended on it: a wait that a new request interrupted reports
             * no readiness, and Poseidon sends a new request after every
             * reply, so replies would otherwise sit in the socket until
             * something else woke us (keys stuck until the mouse moved). */
            if (pz->be_open) {
                struct PzuRep rep;
#ifdef PZU_PROF
                /* T0 before the recv() that returns the record, T1 after it. */
                for (;;) {
                    pz->prof.t0 = prof_now();
                    if (pz->be->recv(pz, &rep, pz->buf, PZU_MAX_DATA) <= 0)
                        break;
                    pz->prof.t1 = prof_now();
                    busy = TRUE;
                    on_reply(pz, &rep);
                }
#else
                while (pz->be->recv(pz, &rep, pz->buf, PZU_MAX_DATA) > 0)
                    on_reply(pz, &rep);
#endif
            }
#ifdef PZU_PROF
            if (!busy)
                pz->prof.n_wakes_empty++;
#endif
            do_aborts(pz);
#ifdef PZU_DEBUG
            pzu_log_flush(pz);
#endif
            now = pzu_now(pz);
            expire(pz, now);
            if (pz->be_open && (pz->state & UHSF_OPERATIONAL) && (LONG)(now - pz->next_poll) >= 0) {
                if (send_op(pz, OP_PORT_STATUS, NULL, 0, 0, 0))
                    pz->next_poll = now + (pz->root_int.mlh_Head->mln_Succ ? 250 : 1000);
                else
                    pz->next_poll = now + 1000;
            }
            schedule(pz);
        }
        flush_all(pz);
        while ((iou = (struct IOUsbHWReq *)GetMsg(pz->port)) != NULL)
            reply(iou, IOERR_ABORTED);
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
    FreeVec(pz->buf);
    pz->buf = NULL;
    if (pz->port)
        DeleteMsgPort(pz->port);
    pz->port = NULL;

    Forbid();
    pz->proc = NULL;
    Signal(pz->opener_task, SIGF_SINGLE);
}

static BOOL start_process(struct PZUBase *pz)
{
    static const struct TagItem tags[] = {
        {NP_Entry, (ULONG)proc_main},
        {NP_Name, (ULONG)DEVICE_NAME},
        {NP_Priority, 5},
        {NP_StackSize, 16384},
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

static void stop_process(struct PZUBase *pz)
{
    pz->opener_task = FindTask(NULL);
    SetSignal(0, SIGF_SINGLE);
    Signal(&pz->proc->pr_Task, SIGBREAKF_CTRL_C);
    Wait(SIGF_SINGLE);
}

/* ------------------------------------------------------------ entry points */

struct PZUBase *DevInit(REGARG(struct PZUBase *pz, d0), REGARG(BPTR seglist, a0), REGARG(struct ExecBase *sys, a6))
{
    SysBase = sys;
    PZU = pz;
    pz->seglist = seglist;
    pz->lib.lib_Node.ln_Type = NT_DEVICE;
    pz->lib.lib_Node.ln_Name = DEVICE_NAME;
    pz->lib.lib_Flags = LIBF_SUMUSED | LIBF_CHANGED;
    pz->lib.lib_Version = DEVICE_VERSION;
    pz->lib.lib_Revision = DEVICE_REVISION;
    pz->lib.lib_IdString = "picozorrousb.device 1.0 (" DEVICE_DATE ")";
    InitSemaphore(&pz->lock);
    new_list(&pz->waiting);
    new_list(&pz->root_int);
#ifdef PZU_PROF
    memset(&pz->prof, 0, sizeof(pz->prof));
    pz->prof.prof_magic = PROF_MAGIC;
#endif
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

BPTR DevExpunge(REGARG(struct PZUBase *pz, a6))
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

void DevOpen(REGARG(struct IOUsbHWReq *io, a1), REGARG(ULONG unit, d0), REGARG(ULONG flags, d1),
             REGARG(struct PZUBase *pz, a6))
{
    pz->lib.lib_OpenCnt++;
    io->iouh_Req.io_Error = IOERR_OPENFAIL;
    if (!DOSBase && !(DOSBase = (struct DosLibrary *)OpenLibrary("dos.library", 37)))
        goto out;
    D(("open unit %ld flags %lx len %ld", unit, flags, (ULONG)io->iouh_Req.io_Message.mn_Length));
    if (unit != 0)
        goto out;

    ObtainSemaphore(&pz->lock);
    if (pz->open_count == 0 && pz->proc) {
        ReleaseSemaphore(&pz->lock);
        goto out;
    }
    if (pz->open_count == 0 && !start_process(pz)) {
        ReleaseSemaphore(&pz->lock);
        goto out;
    }
    pz->open_count++;
    ReleaseSemaphore(&pz->lock);

    io->iouh_Req.io_Unit = &pz->unit;
    io->iouh_Req.io_Error = 0;
    pz->unit.unit_OpenCnt++;
    pz->lib.lib_Flags &= ~LIBF_DELEXP;
    return;
out:
    pz->lib.lib_OpenCnt--;
}

BPTR DevClose(REGARG(struct IOUsbHWReq *io, a1), REGARG(struct PZUBase *pz, a6))
{
    BOOL last;
    ObtainSemaphore(&pz->lock);
    pz->open_count--;
    last = pz->open_count == 0;
    ReleaseSemaphore(&pz->lock);
    if (last)
        stop_process(pz);

    io->iouh_Req.io_Device = (struct Device *)-1;
    io->iouh_Req.io_Unit = (struct Unit *)-1;
    pz->unit.unit_OpenCnt--;
    if (--pz->lib.lib_OpenCnt == 0 && (pz->lib.lib_Flags & LIBF_DELEXP))
        return DevExpunge(pz);
    return 0;
}

LONG DevNull(void)
{
    return 0;
}

static void query(struct PZUBase *pz, struct IOUsbHWReq *iou)
{
    struct TagItem *ti = iou->iouh_Data, *tag;
    ULONG n = 0;
    if (!ti) {
        iou->iouh_Req.io_Error = IOERR_BADADDRESS;
        return;
    }
    while ((tag = NextTagItem(&ti)) != NULL) {
        n++;
        switch (tag->ti_Tag) {
        case UHA_State:
            iou->iouh_State = pz->state;
            tag->ti_Data = pz->state;
            break;
        case UHA_Manufacturer: tag->ti_Data = (ULONG)"PicoZorro"; break;
        case UHA_ProductName: tag->ti_Data = (ULONG)"picozorrousb.device"; break;
        case UHA_Version: tag->ti_Data = DEVICE_VERSION; break;
        case UHA_Revision: tag->ti_Data = DEVICE_REVISION; break;
        case UHA_Description: tag->ti_Data = (ULONG)"USB 1.1 host, RP2350 on PicoZorro"; break;
        case UHA_Copyright: tag->ti_Data = (ULONG)"GPL-3.0-or-later, the PicoZorro contributors"; break;
        case UHA_DriverVersion: tag->ti_Data = 0x200; break;
        default: tag->ti_Data = 0; n--; break;
        }
    }
    iou->iouh_Actual = n;
    iou->iouh_Req.io_Error = 0;
}

void DevBeginIO(REGARG(struct IOUsbHWReq *io, a1), REGARG(struct PZUBase *pz, a6))
{
    io->iouh_Req.io_Message.mn_Node.ln_Type = NT_MESSAGE;
    io->iouh_Req.io_Error = 0;
    if (io->iouh_Req.io_Command == UHCMD_QUERYDEVICE) {
        query(pz, io);
        if (!(io->iouh_Req.io_Flags & IOF_QUICK))
            ReplyMsg(&io->iouh_Req.io_Message);
        return;
    }
    io->iouh_Req.io_Flags &= ~IOF_QUICK;
#ifdef PZU_PROF
    /* T3, in the caller's task; races with the process cost a sample at most. */
    if (prof_int_in(pz, io)) {
        struct PzuProfEp *e = prof_ep(pz, io, FALSE);
        ULONG t3 = prof_now();
        if (e) {
            UBYTE st = e->state;
            if (st == PS_REPLIED || st == PS_REPLYING) {
                pz->prof.t_poseidon += t3 - (st == PS_REPLIED ? e->t2 : e->t2pre);
                pz->prof.n_match++;
                if (st == PS_REPLYING)
                    pz->prof.n_preempt++;
            }
            e->t3 = t3;
            e->state = PS_BEGUN;
        }
    }
#endif
    PutMsg(pz->port, &io->iouh_Req.io_Message);
}

LONG DevAbortIO(REGARG(struct IOUsbHWReq *io, a1), REGARG(struct PZUBase *pz, a6))
{
    struct Node *n;
    struct IOUsbHWReq *r, *nr;
    BOOL found = FALSE;
    int i;

    /* Still in the process's port? */
    Disable();
    if (io->iouh_Req.io_Message.mn_Node.ln_Type == NT_MESSAGE && pz->port) {
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
        reply(io, IOERR_ABORTED);
        return 0;
    }

    ObtainSemaphore(&pz->lock);
    FOR_LIST(&pz->waiting, r, nr) if (r == io) found = TRUE;
    FOR_LIST(&pz->root_int, r, nr) if (r == io) found = TRUE;
    if (found) {
        Remove((struct Node *)io);
        reply(io, IOERR_ABORTED);
    } else {
        for (i = 0; i < SLOTS; i++) {
            if (pz->slots[i].op == OP_XFER && pz->slots[i].iou == io) {
                pz->slots[i].aborted = 1;
                found = TRUE;
            }
        }
        if (found && pz->proc)
            Signal(&pz->proc->pr_Task, SIGBREAKF_CTRL_F);
    }
    ReleaseSemaphore(&pz->lock);
    return 0;
}
