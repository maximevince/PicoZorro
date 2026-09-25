/*
 * picozorro.device, PicoZorro backend: the register window of
 * docs/REGISTERS.md (window version 2 or later) on the board found through
 * expansion.library (manufacturer 2011, product $5A).
 * ENV:PZNET = "pz" (interrupt on /INT2, default) or "pz 6" (/INT6),
 * matching the jumper on the card. From window version 3, frames move
 * through the $C0-$FE data alias in movem.l bursts of 32 bytes.
 */
#include <exec/memory.h>
#include <hardware/intbits.h>
#include <libraries/configvars.h>
#include <libraries/expansionbase.h>
#include <proto/exec.h>
#include <proto/expansion.h>
#include "device.h"

struct ExpansionBase *ExpansionBase;

/* Byte offsets, docs/REGISTERS.md */
#define R_MAGIC       0x00
#define R_VERSION     0x02
#define R_INT         0x06
#define R_INT_ENABLE  0x10
#define R_CTRL        0x12
#define R_STATUS      0x14
#define R_MAC0        0x16
#define R_TX_LEN      0x20
#define R_TX_DATA     0x22
#define R_TX_COMMIT   0x24
#define R_RX_LEN      0x30
#define R_RX_DATA     0x32
#define R_RX_DONE     0x34
#define R_MCAST       0x60
#define R_MCAST_VALID 0x78
#define R_ALIAS       0xc0  /* $C0-$FE: RX_DATA / TX_DATA alias, version 3 */

#define INT_RX_AVAIL    0x0001
#define INT_TX_DONE     0x0002
#define INT_LINK_CHANGE 0x0004
#define INT_RX_OVERRUN  0x0008
#define INT_BUS_ERROR   0x0010
#define INT_LATCHED     (INT_TX_DONE | INT_LINK_CHANGE | INT_RX_OVERRUN | INT_BUS_ERROR)

#define CTRL_ONLINE        0x0001
#define CTRL_PROMISC       0x0002
#define CTRL_MULTICAST_ALL 0x0004
#define CTRL_RESET_FIFOS   0x8000

#define ST_SPEED_100 0x4000

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
#define PZ_MAGIC        0x505a

#define REG(d, off) (*(volatile UWORD *)((d)->base + (off)))

struct PzData {
    volatile UBYTE *base;
    struct Interrupt irq;
    BOOL irq_added;
    UBYTE irq_level;         /* INTB_PORTS or INTB_EXTER */
    struct Task *task;       /* the device process */
    ULONG sigmask;
    BYTE sigbit;
    volatile UWORD enabled;  /* shadow of INT_ENABLE, shared with the ISR */
    UWORD ctrl;
    ULONG rx_overruns_seen;
    BOOL alias;              /* window version 3: the data alias */
};

/* isr.s: calls pz_isr_c and sets the condition codes from D0 */
extern LONG pz_isr(void);

LONG pz_isr_c(REGARG(struct PzData *d, a1))
{
    UWORD v = REG(d, R_INT) & d->enabled;
    if (!v)
        return 0; /* not ours: next server in the chain */
    if (v & INT_RX_AVAIL) {
        d->enabled &= ~INT_RX_AVAIL; /* the process re-enables after draining */
        REG(d, R_INT_ENABLE) = d->enabled;
    }
    if (v & INT_LATCHED)
        REG(d, R_INT) = v & INT_LATCHED;
    Signal(d->task, d->sigmask);
    return 1;
}

static void set_enabled(struct PzData *d, UWORD v)
{
    Disable();
    d->enabled = v;
    REG(d, R_INT_ENABLE) = v;
    Enable();
}

static void pz_close(struct PZBase *pz)
{
    struct PzData *d = pz->be_data;
    if (!d)
        return;
    if (d->base) {
        REG(d, R_CTRL) = 0;
        set_enabled(d, 0);
    }
    if (d->irq_added)
        RemIntServer(d->irq_level, &d->irq);
    if (d->sigbit >= 0)
        FreeSignal(d->sigbit);
    FreeVec(d);
    pz->be_data = NULL;
    if (ExpansionBase)
        CloseLibrary((struct Library *)ExpansionBase);
    ExpansionBase = NULL;
}

static LONG pz_open(struct PZBase *pz, const char *args)
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
    if (!cd)
        goto fail;
    d->base = cd->cd_BoardAddr;
    /* A write first: ends the card's boot ROM mode should the boot
     * ROM not have been copied, so MAGIC is the register again. */
    REG(d, 0x04) = 0;   /* SCRATCH */
    if (REG(d, R_MAGIC) != PZ_MAGIC || REG(d, R_VERSION) < 2)
        goto fail;
    d->alias = REG(d, R_VERSION) >= 3;

    REG(d, R_CTRL) = CTRL_RESET_FIFOS; /* offline, queues empty */
    set_enabled(d, 0);
    REG(d, R_INT) = INT_LATCHED;

    d->sigbit = AllocSignal(-1);
    if (d->sigbit < 0)
        goto fail;
    d->sigmask = 1UL << d->sigbit;
    d->task = FindTask(NULL);
    pz->rx_signal = d->sigmask;

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
    d->irq.is_Code = (void (*)())pz_isr;
    AddIntServer(d->irq_level, &d->irq);
    d->irq_added = TRUE;
    return 0;

fail:
    pz_close(pz);
    return -1;
}

static void pz_get_mac(struct PZBase *pz, UBYTE mac[ETH_ALEN])
{
    struct PzData *d = pz->be_data;
    int i;
    for (i = 0; i < 3; i++) {
        UWORD w = REG(d, R_MAC0 + 2 * i);
        mac[2 * i] = (UBYTE)(w >> 8);
        mac[2 * i + 1] = (UBYTE)w;
    }
}

static LONG pz_online(struct PZBase *pz)
{
    struct PzData *d = pz->be_data;
    d->ctrl = (d->ctrl & ~CTRL_ONLINE) | CTRL_ONLINE;
    REG(d, R_CTRL) = d->ctrl;
    set_enabled(d, INT_RX_AVAIL | INT_RX_OVERRUN | INT_LINK_CHANGE);
    return 0;
}

static void pz_offline(struct PZBase *pz)
{
    struct PzData *d = pz->be_data;
    set_enabled(d, 0);
    d->ctrl &= ~CTRL_ONLINE;
    REG(d, R_CTRL) = d->ctrl | CTRL_RESET_FIFOS;
}

/* Words to the alias, 16 per movem.l burst (fast RAM to d0-d7 to the
 * card), the rest one by one; f is word aligned (AllocVec). */
static void tx_words(struct PzData *d, const UBYTE *f, ULONG words)
{
    volatile UBYTE *al = d->base + R_ALIAS;
    const UWORD *w;
    ULONG n;
    for (n = words >> 4; n; n--)
        __asm__ volatile("movem.l (%0)+,%%d0-%%d7\n\tmovem.l %%d0-%%d7,(%1)"
                         : "+a"(f) : "a"(al) : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "memory");
    w = (const UWORD *)f;
    for (n = words & 15; n; n--)
        REG(d, R_TX_DATA) = *w++;
}

/* The same from the card into buf (word aligned). */
static void rx_words(struct PzData *d, UBYTE *buf, ULONG words)
{
    volatile UBYTE *al = d->base + R_ALIAS;
    UWORD *w;
    ULONG n;
    for (n = words >> 4; n; n--)
        __asm__ volatile("movem.l (%1),%%d0-%%d7\n\tmovem.l %%d0-%%d7,(%0)\n\tlea 32(%0),%0"
                         : "+a"(buf) : "a"(al) : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "memory");
    w = (UWORD *)buf;
    for (n = words & 15; n; n--)
        *w++ = REG(d, R_RX_DATA);
}

static LONG pz_send(struct PZBase *pz, const UBYTE *f, ULONG len)
{
    struct PzData *d = pz->be_data;
    ULONG i, spins = 0;

    /* Wait for a TX slot; the firmware frees one per frame it hands to the
     * chip (about 0.1 ms at 25 MHz SPI for a full frame). */
    while (((REG(d, R_STATUS) >> 8) & 0xf) == 0)
        if (++spins > 200000)
            return -1;
    REG(d, R_TX_LEN) = (UWORD)len;
    if (d->alias) {
        tx_words(d, f, (len + 1) / 2); /* an odd length's pad byte is ignored */
        REG(d, R_TX_COMMIT) = 0;
        return 0;
    }
    for (i = 0; i + 1 < len; i += 2)
        REG(d, R_TX_DATA) = ((UWORD)f[i] << 8) | f[i + 1];
    if (len & 1)
        REG(d, R_TX_DATA) = (UWORD)f[len - 1] << 8;
    REG(d, R_TX_COMMIT) = 0;
    return 0;
}

static ULONG pz_poll_rx(struct PZBase *pz, UBYTE *buf)
{
    struct PzData *d = pz->be_data;
    ULONG len = REG(d, R_RX_LEN), i;
    if (len == 0) {
        /* Drained: take RX interrupts again. A frame that arrived in
         * between raises /INT at once (RX_AVAIL is a level). */
        if (!(d->enabled & INT_RX_AVAIL))
            set_enabled(d, d->enabled | INT_RX_AVAIL);
        return 0;
    }
    if (len > ETH_FRAME_MAX) {
        REG(d, R_RX_DONE) = 0;
        pz->stats.BadData++;
        return 0;
    }
    if (d->alias) {
        rx_words(d, buf, (len + 1) / 2); /* buf has room for the pad byte */
        REG(d, R_RX_DONE) = 0;
        return len;
    }
    for (i = 0; i < len; i += 2) {
        UWORD w = REG(d, R_RX_DATA);
        buf[i] = (UBYTE)(w >> 8);
        buf[i + 1] = (UBYTE)w; /* buf has room for the odd pad byte */
    }
    REG(d, R_RX_DONE) = 0;
    return len;
}

static void pz_set_filter(struct PZBase *pz, BOOL promisc, BOOL all_multi, const struct McastEntry *t, UWORD n)
{
    struct PzData *d = pz->be_data;
    UWORD valid = 0, i, k;

    if (n > 4)
        all_multi = TRUE; /* four entries in the firmware table */
    REG(d, R_MCAST_VALID) = 0;
    if (!all_multi) {
        for (i = 0; i < n; i++) {
            for (k = 0; k < 3; k++)
                REG(d, R_MCAST + 6 * i + 2 * k) = ((UWORD)t[i].addr[2 * k] << 8) | t[i].addr[2 * k + 1];
            valid |= 1 << i;
        }
        REG(d, R_MCAST_VALID) = valid;
    }
    d->ctrl = (d->ctrl & CTRL_ONLINE) | (promisc ? CTRL_PROMISC : 0) | (all_multi ? CTRL_MULTICAST_ALL : 0);
    REG(d, R_CTRL) = d->ctrl;
}

static ULONG pz_bps(struct PZBase *pz)
{
    struct PzData *d = pz->be_data;
    return (REG(d, R_STATUS) & ST_SPEED_100) ? 100000000 : 10000000;
}

const struct Backend backend_pz = {
    "pz", pz_open, pz_close, pz_get_mac, pz_online, pz_offline,
    pz_send, pz_poll_rx, pz_set_filter, pz_bps, NULL,
};
