/*
 * picozorro.device, backplane backend: the register window of
 * docs/REGISTERS.md reached over a serial link instead of the Zorro bus.
 * The firmware (`picozorro --features uart-backplane`) runs every cycle
 * against the same window code the bus slave serves, so this backend does
 * what backend_pz does, register for register, only batched: writes are
 * posted, reads come back in one reply per batch.
 * /INT arrives as a notification frame; the interrupt server's work (read
 * INT, mask RX_AVAIL, acknowledge latched bits) runs in the device process.
 *
 * ENV:PZNET = "bp [baud [unit [device]]]", default 115200 0 serial.device.
 */
#include <exec/memory.h>
#include <proto/exec.h>
#include <proto/timer.h>
#include "../common/bpclient.h"
#include "device.h"

/* Byte offsets, docs/REGISTERS.md (as backend_pz.c) */
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
#define ST_SPEED_100       0x4000
#define PZ_MAGIC           0x505a

struct BpData {
    struct BpClient c;
    UWORD enabled;           /* shadow of INT_ENABLE */
    UWORD ctrl;
    BOOL draining;           /* between an interrupt and RX_LEN == 0 */
    WORD next_len;           /* RX_LEN read with the previous frame, -1 = unknown */
    UWORD status;            /* STATUS at open and after LINK_CHANGE, for bps() */
};

#define B(d) (&(d)->c)

static UWORD rd1(struct BpData *d, UBYTE reg)
{
    return bpc_rd1(B(d), 0, reg);
}

static void wr1(struct BpData *d, UBYTE reg, UWORD v)
{
    bpc_wr1(B(d), 0, reg, v);
}

/* ---- backend ---- */

static void bp_close(struct PZBase *pz)
{
    struct BpData *d = pz->be_data;
    if (!d)
        return;
    if (d->c.link) {
        b_begin(B(d));
        b_write(B(d), R_CTRL, S_WORD, 0);
        b_write(B(d), R_INT_ENABLE, S_WORD, 0);
        b_run(B(d), NULL, 0);
        bpc_close(B(d));
    }
    FreeVec(d);
    pz->be_data = NULL;
    pz->rx_signal = 0;
}

static LONG bp_open(struct PZBase *pz, const char *args)
{
    struct BpData *d;
    UBYTE w[6];

    d = AllocVec(sizeof(*d), MEMF_PUBLIC | MEMF_CLEAR);
    if (!d)
        return -1;
    pz->be_data = d;
    d->next_len = -1;
    while (*args && *args != ' ')   /* skip "bp" */
        args++;
    if (bpc_open(B(d), args) != 0)
        goto fail;

    /* Reset and Autoconfig, then the driver reads MAGIC and VERSION. */
    bpc_attach_begin(B(d));
    b_read(B(d), R_MAGIC);
    b_read(B(d), R_VERSION);
    b_read(B(d), R_STATUS);
    if (bpc_attach_run(B(d), w, 6) != 6) {
        D(("bp: no answer from the backplane"));
        goto fail;
    }
    if (((UWORD)w[0] << 8 | w[1]) != PZ_MAGIC || ((UWORD)w[2] << 8 | w[3]) < 2) {
        D(("bp: magic %lx version %lx", (ULONG)((UWORD)w[0] << 8 | w[1]), (ULONG)((UWORD)w[2] << 8 | w[3])));
        goto fail;
    }
    d->status = (UWORD)w[4] << 8 | w[5];
    b_begin(B(d));
    b_write(B(d), R_CTRL, S_WORD, CTRL_RESET_FIFOS); /* offline, queues empty */
    b_write(B(d), R_INT_ENABLE, S_WORD, 0);
    b_write(B(d), R_INT, S_WORD, INT_LATCHED);
    b_run(B(d), NULL, 0);
    d->enabled = 0;
    pz->rx_signal = serlink_sigmask(d->c.link);
    return 0;

fail:
    bp_close(pz);
    return -1;
}

static void bp_get_mac(struct PZBase *pz, UBYTE mac[ETH_ALEN])
{
    struct BpData *d = pz->be_data;
    int i;
    b_begin(B(d));
    for (i = 0; i < 3; i++)
        b_read(B(d), R_MAC0 + 2 * i);
    if (b_run(B(d), mac, ETH_ALEN) != ETH_ALEN)
        for (i = 0; i < ETH_ALEN; i++)
            mac[i] = 0;
}

static LONG bp_online(struct PZBase *pz)
{
    struct BpData *d = pz->be_data;
    d->ctrl |= CTRL_ONLINE;
    d->enabled = INT_RX_AVAIL | INT_RX_OVERRUN | INT_LINK_CHANGE;
    d->draining = FALSE;
    d->next_len = -1;
    b_begin(B(d));
    b_write(B(d), R_CTRL, S_WORD, d->ctrl);
    b_write(B(d), R_INT_ENABLE, S_WORD, d->enabled);
    b_run(B(d), NULL, 0);
    return 0;
}

static void bp_offline(struct PZBase *pz)
{
    struct BpData *d = pz->be_data;
    d->enabled = 0;
    d->ctrl &= ~CTRL_ONLINE;
    b_begin(B(d));
    b_write(B(d), R_INT_ENABLE, S_WORD, 0);
    b_write(B(d), R_CTRL, S_WORD, d->ctrl | CTRL_RESET_FIFOS);
    b_run(B(d), NULL, 0);
}

static LONG bp_send(struct PZBase *pz, const UBYTE *f, ULONG len)
{
    struct BpData *d = pz->be_data;
    ULONG tries = 0;

    /* A TX slot frees within ~0.1 ms of a commit; one round trip is
     * usually all this takes. */
    while (((rd1(d, R_STATUS) >> 8) & 0xf) == 0)
        if (++tries > 50)
            return -1;
    b_begin(B(d));
    b_write(B(d), R_TX_LEN, S_WORD, (UWORD)len);
    b_write_n2(B(d), R_TX_DATA, f, len, NULL, 0);
    b_write(B(d), R_TX_COMMIT, S_WORD, 0);
    return b_run(B(d), NULL, 0);
}

/* The interrupt server's work (backend_pz.c pz_isr_c), after /INT. */
static BOOL bp_service_int(struct BpData *d)
{
    UWORD v = rd1(d, R_INT) & d->enabled;
    if (!v)
        return FALSE;
    b_begin(B(d));
    if (v & INT_RX_AVAIL) {
        d->enabled &= ~INT_RX_AVAIL; /* re-enabled once RX_LEN reads 0 */
        b_write(B(d), R_INT_ENABLE, S_WORD, d->enabled);
    }
    if (v & INT_LATCHED)
        b_write(B(d), R_INT, S_WORD, v & INT_LATCHED);
    b_run(B(d), NULL, 0);
    if (v & INT_LINK_CHANGE)
        d->status = rd1(d, R_STATUS);
    return TRUE;
}

static ULONG bp_poll_rx(struct PZBase *pz, UBYTE *buf)
{
    struct BpData *d = pz->be_data;
    LONG n;
    ULONG len;

    bpc_drain(B(d));
    if (!d->draining) {
        if (!d->c.irq)
            return 0;
        d->c.irq = FALSE;             /* the next notification says otherwise */
        if (!bp_service_int(d))
            return 0;
        d->draining = TRUE;
        d->next_len = -1;
    }
    len = d->next_len >= 0 ? (ULONG)d->next_len : rd1(d, R_RX_LEN);
    d->next_len = -1;
    if (len == 0 || len == 0xffff) {
        /* Drained: take RX interrupts again. A frame that arrived in
         * between asserts /INT at once and a notification follows. */
        d->draining = FALSE;
        if (!(d->enabled & INT_RX_AVAIL)) {
            d->enabled |= INT_RX_AVAIL;
            wr1(d, R_INT_ENABLE, d->enabled);
        }
        return 0;
    }
    if (len > ETH_FRAME_MAX) {
        wr1(d, R_RX_DONE, 0);
        pz->stats.BadData++;
        return 0;
    }
    /* Frame, RX_DONE and the next RX_LEN in one round trip. The reply
     * carries the frame in memory order, the next length after it. */
    b_begin(B(d));
    b_read_n(B(d), R_RX_DATA, (UWORD)((len + 1) / 2));
    b_write(B(d), R_RX_DONE, S_WORD, 0);
    b_read(B(d), R_RX_LEN);
    n = b_run(B(d), buf, BUF_SIZE);
    if (n != (LONG)(((len + 1) & ~1UL) + 2)) {
        /* Lost on the line: start over from the interrupt, which RX_AVAIL
         * raises again as long as frames are queued. */
        d->draining = FALSE;
        d->enabled |= INT_RX_AVAIL;
        wr1(d, R_INT_ENABLE, d->enabled);
        return 0;
    }
    d->next_len = (WORD)((UWORD)buf[n - 2] << 8 | buf[n - 1]);
    return len;
}

static void bp_set_filter(struct PZBase *pz, BOOL promisc, BOOL all_multi, const struct McastEntry *t, UWORD n)
{
    struct BpData *d = pz->be_data;
    UWORD valid = 0, i, k;

    if (n > 4)
        all_multi = TRUE;
    b_begin(B(d));
    b_write(B(d), R_MCAST_VALID, S_WORD, 0);
    if (!all_multi) {
        for (i = 0; i < n; i++) {
            for (k = 0; k < 3; k++)
                b_write(B(d), R_MCAST + 6 * i + 2 * k, S_WORD, ((UWORD)t[i].addr[2 * k] << 8) | t[i].addr[2 * k + 1]);
            valid |= 1 << i;
        }
        b_write(B(d), R_MCAST_VALID, S_WORD, valid);
    }
    d->ctrl = (d->ctrl & CTRL_ONLINE) | (promisc ? CTRL_PROMISC : 0) | (all_multi ? CTRL_MULTICAST_ALL : 0);
    b_write(B(d), R_CTRL, S_WORD, d->ctrl);
    b_run(B(d), NULL, 0);
}

/* Called from the task that sent S2_DEVICEQUERY (device.c answers it in
 * the caller's context), which must not touch the link: the serial
 * requests and their signals belong to the device process. The value is
 * the one read at open or after the last LINK_CHANGE. */
static ULONG bp_bps(struct PZBase *pz)
{
    struct BpData *d = pz->be_data;
    return (d->status & ST_SPEED_100) ? 100000000 : 10000000;
}

static void bp_log(struct PZBase *pz, const UBYTE *text, UWORD len)
{
    struct BpData *d = pz->be_data;
    if (d)
        bpc_log(B(d), text, len);
}

const struct Backend backend_bp = {
    "bp", bp_open, bp_close, bp_get_mac, bp_online, bp_offline,
    bp_send, bp_poll_rx, bp_set_filter, bp_bps, bp_log,
};
