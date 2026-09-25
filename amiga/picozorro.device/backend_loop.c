/*
 * picozorro.device, loop backend: every sent frame comes back with the
 * MAC addresses swapped, so it is addressed to us again. For command-level
 * tests with no network at all (ENV:PZNET = "loop").
 */
#include <exec/memory.h>
#include <proto/exec.h>
#include "device.h"

#define LOOP_SLOTS 8

struct LoopData {
    UWORD head, count;
    UWORD len[LOOP_SLOTS];
    UBYTE buf[LOOP_SLOTS][ETH_FRAME_MAX];
};

static const UBYTE loop_mac[ETH_ALEN] = {0x52, 0x5a, 0x00, 0x00, 0x00, 0x01};

static LONG loop_open(struct PZBase *pz, const char *args)
{

    pz->be_data = AllocVec(sizeof(struct LoopData), MEMF_PUBLIC | MEMF_CLEAR);
    return pz->be_data ? 0 : -1;
}

static void loop_close(struct PZBase *pz)
{
    FreeVec(pz->be_data);
    pz->be_data = NULL;
}

static void loop_get_mac(struct PZBase *pz, UBYTE mac[ETH_ALEN])
{

    pz_copy(loop_mac, mac, ETH_ALEN);
}

static LONG loop_online(struct PZBase *pz)
{

    return 0;
}

static void loop_offline(struct PZBase *pz)
{
    ((struct LoopData *)pz->be_data)->count = 0;
}

static LONG loop_send(struct PZBase *pz, const UBYTE *f, ULONG len)
{
    struct LoopData *d = pz->be_data;
    UWORD slot;
    UBYTE *b;
    if (d->count == LOOP_SLOTS)
        return 0; /* dropped, like a full wire */
    slot = (d->head + d->count) % LOOP_SLOTS;
    b = d->buf[slot];
    pz_copy(f, b, len);
    pz_copy(f + ETH_ALEN, b, ETH_ALEN); /* dst = old src (us) */
    pz_copy(f, b + ETH_ALEN, ETH_ALEN); /* src = old dst */
    d->len[slot] = (UWORD)len;
    d->count++;
    return 0;
}

static ULONG loop_poll_rx(struct PZBase *pz, UBYTE *buf)
{
    struct LoopData *d = pz->be_data;
    ULONG n;
    if (!d->count)
        return 0;
    n = d->len[d->head];
    pz_copy(d->buf[d->head], buf, n);
    d->head = (d->head + 1) % LOOP_SLOTS;
    d->count--;
    return n;
}

static void loop_set_filter(struct PZBase *pz, BOOL p, BOOL a, const struct McastEntry *t, UWORD n)
{

}

static ULONG loop_bps(struct PZBase *pz)
{

    return 10000000;
}

const struct Backend backend_loop = {
    "loop", loop_open, loop_close, loop_get_mac, loop_online, loop_offline,
    loop_send, loop_poll_rx, loop_set_filter, loop_bps, NULL,
};
