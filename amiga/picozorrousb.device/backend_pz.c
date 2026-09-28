/*
 * picozorrousb.device, backend `pz`: the card's USB register window
 * (docs/REGISTERS-USB.md) on the Zorro bus, at board + $10000 (A16 = 1)
 * of the board found through expansion.library (manufacturer 2011,
 * product $5A). The same register sequence as the `bp` backend, as plain
 * bus cycles. /INT: CPL_AVAIL through an interrupt server on /INT2, or
 * /INT6 with "pz 6", matching the card's jumper (docs/REGISTERS.md).
 *
 * ENV:PZUSB = "pz" or "pz 6". Firmware: picozorro with the usb-host
 * feature.
 */
#include <exec/memory.h>
#include <hardware/intbits.h>
#include <devices/timer.h>
#include <libraries/configvars.h>
#include <libraries/expansionbase.h>
#include <proto/exec.h>
#include <proto/expansion.h>
#include "device.h"

struct ExpansionBase *ExpansionBase;

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
#define PZ_MAGIC        0x505a
#define USB_WINDOW      0x10000UL

/* docs/REGISTERS.md (window A16 = 0) */
#define R_MAGIC    0x00
#define R_SCRATCH  0x04
/* docs/REGISTERS-USB.md (window A16 = 1) */
#define U_MAGIC      0x00
#define U_VERSION    0x02
#define U_INT        0x04
#define U_INT_ENABLE 0x06
#define U_CTRL       0x08
#define U_STATUS     0x0a
#define U_REQ_LEN    0x10
#define U_REQ_DATA   0x12
#define U_REQ_COMMIT 0x14
#define U_CPL_LEN    0x20
#define U_CPL_DATA   0x22
#define U_CPL_DONE   0x24

#define INT_CPL_AVAIL    0x0001
#define INT_REQ_FREE     0x0002
#define INT_PORT_CHANGE  0x0004
#define INT_BUS_ERROR    0x0010
#define INT_LATCHED      (INT_REQ_FREE | INT_PORT_CHANGE | INT_BUS_ERROR)
#define CTRL_RESET_QUEUES 0x8000
#define REQ_FREE(st)     (((st) >> 8) & 0x1f)
#define REC_MAX          (sizeof(struct PzuRep) + PZU_MAX_DATA)

#define W(d, off) (*(volatile UWORD *)((d)->win + (off)))

struct PzData {
    volatile UBYTE *base;    /* A16 = 0 */
    volatile UBYTE *win;     /* A16 = 1 */
    struct Interrupt irq;
    BOOL irq_added;
    UBYTE irq_level;         /* INTB_PORTS or INTB_EXTER */
    struct Task *task;       /* the device process */
    ULONG sigmask;
    BYTE sigbit;
    volatile UWORD enabled;  /* shadow of INT_ENABLE, shared with the ISR */
    volatile BOOL pending;   /* the ISR saw CPL_AVAIL */
    UWORD next_len;          /* CPL_LEN read with the previous record */
    BOOL more;               /* keep reading completions */
    struct MsgPort *tport;
    struct timerequest *treq;
    BOOL timer_open;
    UBYTE rec[REC_MAX + 2];
};

/* isr.s: calls pzu_isr_c and sets the condition codes from D0 */
extern LONG pzu_isr(void);

LONG pzu_isr_c(REGARG(struct PzData *d, a1))
{
    UWORD v = W(d, U_INT) & d->enabled;
    if (!v)
        return 0; /* not ours (the network window, another board) */
    if (v & INT_CPL_AVAIL) {
        /* Level: off until the process has drained the queue. */
        d->enabled &= ~INT_CPL_AVAIL;
        W(d, U_INT_ENABLE) = d->enabled;
        d->pending = TRUE;
    }
    if (v & INT_LATCHED)
        W(d, U_INT) = v & INT_LATCHED;
    Signal(d->task, d->sigmask);
    return 1;
}

static void set_enabled(struct PzData *d, UWORD v)
{
    Disable();
    d->enabled = v;
    W(d, U_INT_ENABLE) = v;
    Enable();
}

static void pzu_close(struct PZUBase *pz)
{
    struct PzData *d = pz->be_data;
    if (!d)
        return;
    if (d->win) {
        set_enabled(d, 0);
        W(d, U_CTRL) = CTRL_RESET_QUEUES;
    }
    if (d->irq_added)
        RemIntServer(d->irq_level, &d->irq);
    if (d->timer_open)
        CloseDevice((struct IORequest *)d->treq);
    if (d->treq)
        DeleteIORequest((struct IORequest *)d->treq);
    if (d->tport)
        DeleteMsgPort(d->tport);
    if (d->sigbit >= 0)
        FreeSignal(d->sigbit);
    FreeVec(d);
    pz->be_data = NULL;
    if (ExpansionBase)
        CloseLibrary((struct Library *)ExpansionBase);
    ExpansionBase = NULL;
}

static LONG pzu_open(struct PZUBase *pz, const char *args)
{
    struct PzData *d;
    struct ConfigDev *cd;

    d = AllocVec(sizeof(*d), MEMF_PUBLIC | MEMF_CLEAR);
    if (!d)
        return -1;
    d->sigbit = -1;
    pz->be_data = d;
    ExpansionBase = (struct ExpansionBase *)OpenLibrary("expansion.library", 37);
    if (!ExpansionBase)
        goto fail;
    cd = FindConfigDev(NULL, PZ_MANUFACTURER, PZ_PRODUCT);
    if (!cd) {
        D(("pz: no PicoZorro board"));
        goto fail;
    }
    if (cd->cd_BoardSize < 2 * USB_WINDOW) {
        D(("pz: board of %ld bytes, no USB window (firmware before 128 KiB)", cd->cd_BoardSize));
        goto fail;
    }
    d->base = cd->cd_BoardAddr;
    d->win = d->base + USB_WINDOW;
    /* A write first: ends the card's boot ROM mode should the boot
     * ROM not have been copied, so MAGIC is the register again. */
    *(volatile UWORD *)(d->base + R_SCRATCH) = 0;
    if (*(volatile UWORD *)(d->base + R_MAGIC) != PZ_MAGIC || W(d, U_MAGIC) != PZU_MAGIC) {
        D(("pz: board magic %lx, usb window %lx", (ULONG)*(volatile UWORD *)(d->base + R_MAGIC),
           (ULONG)W(d, U_MAGIC)));
        goto fail;
    }

    d->tport = CreateMsgPort();
    if (!d->tport)
        goto fail;
    d->treq = (struct timerequest *)CreateIORequest(d->tport, sizeof(struct timerequest));
    if (!d->treq || OpenDevice(TIMERNAME, UNIT_MICROHZ, (struct IORequest *)d->treq, 0) != 0)
        goto fail;
    d->timer_open = TRUE;
    d->sigbit = AllocSignal(-1);
    if (d->sigbit < 0)
        goto fail;
    d->sigmask = 1UL << d->sigbit;
    d->task = FindTask(NULL);

    set_enabled(d, 0);
    W(d, U_CTRL) = CTRL_RESET_QUEUES;
    W(d, U_INT) = INT_LATCHED;

    /* "pz 6": /INT6 jumpered, else /INT2 */
    d->irq_level = INTB_PORTS;
    while (*args && *args != ' ')
        args++;
    while (*args == ' ')
        args++;
    if (*args == '6')
        d->irq_level = INTB_EXTER;
    d->irq.is_Node.ln_Type = NT_INTERRUPT;
    d->irq.is_Node.ln_Pri = 0;
    d->irq.is_Node.ln_Name = DEVICE_NAME;
    d->irq.is_Data = d;
    d->irq.is_Code = (void (*)())pzu_isr;
    AddIntServer(d->irq_level, &d->irq);
    d->irq_added = TRUE;
    set_enabled(d, INT_CPL_AVAIL);

    D(("pz: board at %lx, usb window version %ld, status %04lx, /INT%ld", (ULONG)d->base,
       (ULONG)W(d, U_VERSION), (ULONG)W(d, U_STATUS), (ULONG)(d->irq_level == INTB_EXTER ? 6 : 2)));
    return 0;

fail:
    pzu_close(pz);
    return -1;
}

/* Sleep until `sigs`, the ISR's signal or `ms` pass. */
static ULONG pzu_sleep(struct PzData *d, ULONG sigs, ULONG ms)
{
    ULONG tsig = 1UL << d->tport->mp_SigBit;
    ULONG got;

    d->treq->tr_node.io_Command = TR_ADDREQUEST;
    d->treq->tr_time.tv_secs = ms / 1000;
    d->treq->tr_time.tv_micro = (ms % 1000) * 1000;
    SendIO((struct IORequest *)d->treq);
    got = Wait(sigs | d->sigmask | tsig);
    if (!CheckIO((struct IORequest *)d->treq))
        AbortIO((struct IORequest *)d->treq);
    WaitIO((struct IORequest *)d->treq);
    SetSignal(0, tsig);
    return got;
}

static ULONG pzu_wait(struct PZUBase *pz, ULONG sigs, ULONG ms)
{
    struct PzData *d = pz->be_data;
    ULONG got = 0;

    if (!d->pending && !d->more)
        got = pzu_sleep(d, sigs, ms);
    /* Without /INT (wrong jumper) the queue is still read, once a wait. */
    if (!d->pending && (d->enabled & INT_CPL_AVAIL) && (W(d, U_INT) & INT_CPL_AVAIL)) {
        set_enabled(d, d->enabled & ~INT_CPL_AVAIL);
        d->pending = TRUE;
    }
    pz->be_readable = d->pending || d->more;
    return got & sigs;
}

static LONG pzu_send(struct PZUBase *pz, const struct PzuReq *h, const UBYTE *data, UWORD len)
{
    struct PzData *d = pz->be_data;
    const UWORD *hw = (const UWORD *)h;
    UWORD i, tries = 0;

    /* The firmware takes requests off the queue every 250 us; the queue
     * runs full only when the Amiga outruns the workers. */
    while (REQ_FREE(W(d, U_STATUS)) == 0) {
        if (++tries > 50)
            return -1;
        pzu_sleep(d, 0, 1);
    }
    W(d, U_REQ_LEN) = (UWORD)(sizeof(*h) + len);
    for (i = 0; i < sizeof(*h) / 2; i++)
        W(d, U_REQ_DATA) = hw[i];
    /* The data may start at an odd address: words from bytes. */
    for (i = 0; i + 1 < len; i += 2)
        W(d, U_REQ_DATA) = (UWORD)data[i] << 8 | data[i + 1];
    if (len & 1)
        W(d, U_REQ_DATA) = (UWORD)data[len - 1] << 8;
    W(d, U_REQ_COMMIT) = 0;
    return 0;
}

/* One completion record per call; when the queue is empty, CPL_AVAIL goes
 * back on so /INT reports the next one. */
static LONG pzu_recv(struct PZUBase *pz, struct PzuRep *rep, UBYTE *data, UWORD max)
{
    struct PzData *d = pz->be_data;
    UWORD *rw = (UWORD *)d->rec;
    UWORD len, i, actual;

    if (!d->more) {
        if (!d->pending)
            return 0;
        d->pending = FALSE;
        d->next_len = W(d, U_CPL_LEN);
        d->more = TRUE;
    }
    len = d->next_len;
    if (len == 0 || len > REC_MAX) {
        if (len > REC_MAX)
            W(d, U_CPL_DONE) = 0;
        d->more = FALSE;
        set_enabled(d, d->enabled | INT_CPL_AVAIL);
        return 0;
    }
    for (i = 0; i < (len + 1) / 2; i++)
        rw[i] = W(d, U_CPL_DATA);
    W(d, U_CPL_DONE) = 0;
    d->next_len = W(d, U_CPL_LEN);
    if (len < sizeof(*rep))
        return 0;
    CopyMem(d->rec, rep, sizeof(*rep));
    if (rep->magic != PZU_MAGIC)
        return 0;
    actual = rep->actual;
    if (actual > len - sizeof(*rep))
        actual = len - sizeof(*rep);
    if (actual > max)
        actual = max;
    if (actual)
        CopyMem(d->rec + sizeof(*rep), data, actual);
    rep->actual = actual;
    return 1;
}

const struct Backend backend_pz = {
    "pz", pzu_open, pzu_close, pzu_wait, pzu_send, pzu_recv, NULL, TRUE,
};
