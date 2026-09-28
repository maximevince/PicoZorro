/*
 * picozorrousb.device, backend `bp`: the card's USB register window
 * (docs/REGISTERS-USB.md, A16 = 1) reached over the UART backplane
 * (../common/bpclient.c) instead of the Zorro bus. A request record goes in
 * through REQ_LEN / REQ_DATA / REQ_COMMIT, completion records come out of
 * CPL_LEN / CPL_DATA / CPL_DONE: the same register accesses the backend
 * `pz` makes on the bus. /INT (CPL_AVAIL) arrives as a notification frame
 * and starts the reading.
 *
 * Firmware: picozorro with the `uart-backplane` and `usb-host` features.
 * ENV:PZUSB = "bp [baud [unit [device]]]".
 */
#include <exec/memory.h>
#include <proto/exec.h>
#include "../common/bpclient.h"
#include "device.h"

/* docs/REGISTERS.md (window A16 = 0) */
#define PZ_MAGIC   0x505a
#define R_MAGIC    0x00
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
#define CTRL_RESET_QUEUES 0x8000
#define REQ_FREE(st)     (((st) >> 8) & 0x1f)
#define REC_MAX          (sizeof(struct PzuRep) + PZU_MAX_DATA)

struct BpData {
    struct BpClient c;
    UWORD free;          /* request slots known free (STATUS minus what we sent since) */
    UWORD next_len;      /* CPL_LEN read with the previous record */
    BOOL more;           /* keep reading completions */
    UBYTE rec[REC_MAX + 4];
};

#define B(d) (&(d)->c)

static void bpu_close(struct PZUBase *pz)
{
    struct BpData *d = pz->be_data;
    if (!d)
        return;
    if (d->c.link) {
        b_begin(B(d));
        b_window(B(d), 1);
        b_write(B(d), U_INT_ENABLE, S_WORD, 0);
        b_write(B(d), U_CTRL, S_WORD, CTRL_RESET_QUEUES);
        b_run(B(d), NULL, 0);
        bpc_close(B(d));
    }
    FreeVec(d);
    pz->be_data = NULL;
}

static LONG bpu_open(struct PZUBase *pz, const char *args)
{
    struct BpData *d;
    UBYTE w[8];
    UWORD st;

    d = AllocVec(sizeof(*d), MEMF_PUBLIC | MEMF_CLEAR);
    if (!d)
        return -1;
    pz->be_data = d;
    while (*args && *args != ' ')   /* skip "bp" */
        args++;
    if (bpc_open(B(d), args) != 0)
        goto fail;
    bpc_attach_begin(B(d));
    b_read(B(d), R_MAGIC);
    b_window(B(d), 1);
    b_read(B(d), U_MAGIC);
    b_read(B(d), U_VERSION);
    b_write(B(d), U_CTRL, S_WORD, CTRL_RESET_QUEUES);
    b_write(B(d), U_INT, S_WORD, INT_REQ_FREE | INT_PORT_CHANGE | INT_BUS_ERROR);
    b_write(B(d), U_INT_ENABLE, S_WORD, INT_CPL_AVAIL);
    b_read(B(d), U_STATUS);
    if (bpc_attach_run(B(d), w, 8) != 8) {
        D(("bp: no answer from the backplane"));
        goto fail;
    }
    if (((UWORD)w[0] << 8 | w[1]) != PZ_MAGIC || ((UWORD)w[2] << 8 | w[3]) != PZU_MAGIC) {
        D(("bp: board %lx, usb window %lx", (ULONG)((UWORD)w[0] << 8 | w[1]), (ULONG)((UWORD)w[2] << 8 | w[3])));
        goto fail;
    }
    st = (UWORD)w[6] << 8 | w[7];
    d->free = REQ_FREE(st);
    D(("bp: usb window version %ld, status %04lx", (ULONG)((UWORD)w[4] << 8 | w[5]), (ULONG)st));
    return 0;

fail:
    bpu_close(pz);
    return -1;
}

static ULONG bpu_wait(struct PZUBase *pz, ULONG sigs, ULONG ms)
{
    struct BpData *d = pz->be_data;
    ULONG got = serlink_wait(d->c.link, sigs, ms);
    bpc_drain(B(d));
    pz->be_readable = d->c.irq || d->more;
    return got;
}

static LONG bpu_send(struct PZUBase *pz, const struct PzuReq *h, const UBYTE *data, UWORD len)
{
    struct BpData *d = pz->be_data;
    UBYTE w[2];
    ULONG tries = 0;

    /* The module takes requests off the queue as it gets them; the credit
     * runs out only when the Amiga outruns it. */
    while (d->free == 0) {
        b_begin(B(d));
        b_window(B(d), 1);
        b_read(B(d), U_STATUS);
        if (b_run(B(d), w, 2) == 2)
            d->free = REQ_FREE((UWORD)w[0] << 8 | w[1]);
        if (d->free == 0 && ++tries > 20)
            return -1;
    }
    b_begin(B(d));
    b_window(B(d), 1);
    b_write(B(d), U_REQ_LEN, S_WORD, (UWORD)(sizeof(*h) + len));
    b_write_n2(B(d), U_REQ_DATA, (const UBYTE *)h, sizeof(*h), data, len);
    b_write(B(d), U_REQ_COMMIT, S_WORD, 0);
    if (b_run(B(d), NULL, 0) != 0)
        return -1;
    d->free--;
    return 0;
}

/* One completion record per call. The first read after /INT asks for
 * CPL_LEN; after that every read of a record also brings the next
 * CPL_LEN and STATUS, so a queue of n records costs n round trips. */
static LONG bpu_recv(struct PZUBase *pz, struct PzuRep *rep, UBYTE *data, UWORD max)
{
    struct BpData *d = pz->be_data;
    UBYTE w[4];
    UWORD len, st, actual;
    LONG n;

    bpc_drain(B(d));
    if (!d->more) {
        if (!d->c.irq)
            return 0;
        d->c.irq = FALSE;   /* repeated every 100 ms while CPL_AVAIL stays */
        b_begin(B(d));
        b_window(B(d), 1);
        b_read(B(d), U_CPL_LEN);
        b_read(B(d), U_STATUS);
        if (b_run(B(d), w, 4) != 4)
            return 0;
        d->next_len = (UWORD)w[0] << 8 | w[1];
        d->free = REQ_FREE((UWORD)w[2] << 8 | w[3]);
        d->more = TRUE;
    }
    len = d->next_len;
    if (len == 0 || len > REC_MAX) {
        if (len > REC_MAX)
            bpc_wr1(B(d), 1, U_CPL_DONE, 0);
        d->more = FALSE;
        return 0;
    }
    b_begin(B(d));
    b_window(B(d), 1);
    b_read_n(B(d), U_CPL_DATA, (len + 1) / 2);
    b_write(B(d), U_CPL_DONE, S_WORD, 0);
    b_read(B(d), U_CPL_LEN);
    b_read(B(d), U_STATUS);
    n = b_run(B(d), d->rec, sizeof(d->rec));
    if (n != (LONG)(((len + 1) & ~1UL) + 4)) {
        /* Lost on the line; the record is gone with it if CPL_DONE ran.
         * The driver's timeout resends the request. */
        d->more = FALSE;
        return 0;
    }
    st = (UWORD)d->rec[n - 2] << 8 | d->rec[n - 1];
    d->next_len = (UWORD)d->rec[n - 4] << 8 | d->rec[n - 3];
    d->free = REQ_FREE(st);
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

static void bpu_log(struct PZUBase *pz, const UBYTE *text, UWORD len)
{
    struct BpData *d = pz->be_data;
    if (d)
        bpc_log(B(d), text, len);
}

const struct Backend backend_bp = {
    "bp", bpu_open, bpu_close, bpu_wait, bpu_send, bpu_recv, bpu_log, TRUE,
};
