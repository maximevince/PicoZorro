/*
 * bpclient: the Amiga end of the backplane, see bpclient.h.
 */
#include <proto/exec.h>
#include <proto/timer.h>
#include "bpclient.h"

#define AC_BASE_HI 0x48
#define AC_BASE_LO 0x4a

void b_begin(struct BpClient *c)
{
    c->len = 6;
    c->slow = FALSE;
}

void b_slow(struct BpClient *c)
{
    c->slow = TRUE;
}

void b_put(struct BpClient *c, UBYTE b)
{
    c->req[c->len++] = b;
}

void b_window(struct BpClient *c, UBYTE a16)
{
    b_put(c, BOP_WINDOW);
    b_put(c, a16);
}

void b_write(struct BpClient *c, UBYTE reg, UBYTE strobes, UWORD v)
{
    b_put(c, BOP_WRITE);
    b_put(c, reg);
    b_put(c, strobes);
    b_put(c, v >> 8);
    b_put(c, v);
}

void b_read(struct BpClient *c, UBYTE reg)
{
    b_put(c, BOP_READ);
    b_put(c, reg);
    b_put(c, S_WORD);
}

void b_write_n2(struct BpClient *c, UBYTE reg, const UBYTE *a, ULONG alen, const UBYTE *b, ULONG blen)
{
    ULONG len = alen + blen;
    UWORD n = (len + 1) / 2;
    b_put(c, BOP_WRITE_N);
    b_put(c, reg);
    b_put(c, n >> 8);
    b_put(c, n);
    if (alen)
        CopyMem((APTR)a, c->req + c->len, alen);
    c->len += alen;
    if (blen)
        CopyMem((APTR)b, c->req + c->len, blen);
    c->len += blen;
    if (len & 1)
        b_put(c, 0);
}

void b_read_n(struct BpClient *c, UBYTE reg, UWORD n)
{
    b_put(c, BOP_READ_N);
    b_put(c, reg);
    b_put(c, n >> 8);
    b_put(c, n);
}

void b_mem_write(struct BpClient *c, ULONG offset, const UBYTE *data, UWORD len)
{
    b_put(c, BOP_MEM_WRITE);
    b_put(c, offset >> 16);
    b_put(c, offset >> 8);
    b_put(c, offset);
    b_put(c, len >> 8);
    b_put(c, len);
    if (len)
        CopyMem((APTR)data, c->req + c->len, len);
    c->len += len;
}

void b_mem_read(struct BpClient *c, ULONG offset, UWORD len)
{
    b_put(c, BOP_MEM_READ);
    b_put(c, offset >> 16);
    b_put(c, offset >> 8);
    b_put(c, offset);
    b_put(c, len >> 8);
    b_put(c, len);
}

/* An /INT notification or a stray reply (serial: to a posted batch that
 * failed, or late, after a timeout; UDP: every batch is awaited, so a
 * stray reply is a duplicate, the late or repeated answer to a retry). */
static void note(struct BpClient *c, const UBYTE *dg, LONG n)
{
    if (n < 7 || dg[0] != (BP_MAGIC >> 8) || dg[1] != (BP_MAGIC & 0xff))
        return;
    if (dg[3] & BF_INT)
        c->irq = dg[6] != 0;
    else if ((dg[3] & BF_IS_REPLY) && c->lossy)
        c->duplicates++;
    else if ((dg[3] & BF_IS_REPLY) && dg[6] != BST_OK)
        c->stray_errors++;
}

void bpc_drain(struct BpClient *c)
{
    UBYTE *dg;
    LONG n;
    while ((n = c->ops->recv(c->link, &dg)) > 0)
        note(c, dg, n);
}

ULONG bpc_wait(struct BpClient *c, ULONG sigs, ULONG ms)
{
    return c->ops->wait(c->link, sigs, ms);
}

ULONG bpc_link_errors(struct BpClient *c)
{
    return c->link ? c->ops->errors(c->link) : 0;
}

static ULONG now_ms(void)
{
    struct timeval tv;
    GetSysTime(&tv);
    return tv.tv_secs * 1000 + tv.tv_micro / 1000;
}

LONG b_run(struct BpClient *c, UBYTE *words, ULONG bytes)
{
    UWORD seq = ++c->seq;
    ULONG start, timeout, tries, left;
    BOOL posted = !words;
    UBYTE *dg;
    LONG n;

    c->req[0] = BP_MAGIC >> 8;
    c->req[1] = BP_MAGIC & 0xff;
    c->req[2] = BP_VERSION;
    c->req[3] = (words || c->lossy) ? BF_REPLY : 0;
    c->req[4] = seq >> 8;
    c->req[5] = seq;
    c->status = BST_NONE;
    if (c->len > c->max)
        return -1;
    /* Serial link: one try, 2 s. UDP: every batch awaited, up to
     * BP_UDP_RETRIES more tries of BP_UDP_TIMEOUT_MS with the same seq (the
     * firmware answers a repeated seq from its reply cache). */
    timeout = !c->lossy ? BP_REPLY_TIMEOUT_MS : c->slow ? BP_UDP_SLOW_MS : BP_UDP_TIMEOUT_MS;
    for (tries = 0;; tries++) {
        if (c->ops->send(c->link, c->req, c->len, NULL, 0) != 0)
            return -1;
        if (posted && !c->lossy) {
            c->status = BST_OK;
            return 0;
        }
        /* Wall clock, not wake-ups: under FS-UAE a reply trickles in a
         * byte per scanline and every byte ends a wait. */
        start = now_ms();
        for (;;) {
            while ((n = c->ops->recv(c->link, &dg)) > 0) {
                if (n >= 8 && dg[0] == (BP_MAGIC >> 8) && dg[1] == (BP_MAGIC & 0xff) && (dg[3] & BF_IS_REPLY) &&
                    ((UWORD)dg[4] << 8 | dg[5]) == seq) {
                    if (posted) {
                        /* UDP, a posted batch: as on the UART, where the
                         * firmware answers only an error */
                        c->status = BST_OK;
                        if (dg[6] != BST_OK)
                            c->stray_errors++;
                        return 0;
                    }
                    c->status = dg[6];
                    if (dg[6] != BST_OK)
                        return -1;
                    n -= 8;
                    if (n > bytes)
                        n = bytes;
                    if (n)
                        CopyMem(dg + 8, words, n);
                    return n;
                }
                note(c, dg, n);
            }
            left = now_ms() - start;
            if (left >= timeout)
                break;
            left = timeout - left;
            c->ops->wait(c->link, 0, left < 100 ? left : 100);
        }
        if (!c->lossy || tries >= BP_UDP_RETRIES) {
            c->timeouts++;
            return -1;
        }
        c->retries++;
    }
}

UWORD bpc_rd1(struct BpClient *c, UBYTE a16, UBYTE reg)
{
    UBYTE w[2];
    b_begin(c);
    if (a16)
        b_window(c, 1);
    b_read(c, reg);
    if (b_run(c, w, 2) != 2)
        return 0xffff;
    return (UWORD)w[0] << 8 | w[1];
}

void bpc_wr1(struct BpClient *c, UBYTE a16, UBYTE reg, UWORD v)
{
    b_begin(c);
    if (a16)
        b_window(c, 1);
    b_write(c, reg, S_WORD, v);
    b_run(c, NULL, 0);
}

LONG bpc_mem_write_all(struct BpClient *c, ULONG offset, const UBYTE *data, ULONG len)
{
    UBYTE none;
    ULONG k = 0;
    while (len) {
        UWORD n = len > c->mem_chunk ? c->mem_chunk : len;
        /* Every BP_MEM_SYNC-th batch and the last one wait for their
         * reply: FS-UAE hands serial bytes over faster than the 1 Mbaud
         * line takes them, and an unbounded backlog of posted batches in
         * the relay delays the next reply past BP_REPLY_TIMEOUT_MS. */
        BOOL sync = ++k % BP_MEM_SYNC == 0 || len == n;
        b_begin(c);
        b_mem_write(c, offset, data, n);
        if (b_run(c, sync ? &none : NULL, 0) != 0)
            return -1;
        offset += n;
        data += n;
        len -= n;
    }
    return 0;
}

LONG bpc_mem_read_all(struct BpClient *c, ULONG offset, UBYTE *buf, ULONG len)
{
    while (len) {
        UWORD n = len > c->mem_chunk ? c->mem_chunk : len;
        b_begin(c);
        b_mem_read(c, offset, n);
        if (b_run(c, buf, n) != n)
            return -1;
        offset += n;
        buf += n;
        len -= n;
    }
    return 0;
}

LONG bpc_open_link(struct BpClient *c, const struct BpLinkOps *ops, const char *args)
{
    c->ops = ops;
    c->max = ops->max_dgram < SERLINK_MAX ? ops->max_dgram : SERLINK_MAX;
    c->mem_chunk = ops->mem_chunk;
    c->lossy = ops->lossy;
    /* Over UDP the firmware answers a repeated seq from its cache, so a
     * new session must not start where an earlier one left off: start
     * from the clock. */
    if (ops->lossy)
        c->seq = (UWORD)now_ms();
    c->link = ops->open(args);
    return c->link ? 0 : -1;
}

LONG bpc_open(struct BpClient *c, const char *args)
{
    return bpc_open_link(c, &serlink_ops, args);
}

void bpc_close(struct BpClient *c)
{
    if (c->link)
        c->ops->close(c->link);
    c->link = NULL;
}

/* What Kickstart does to the card: /BUSRST, then Autoconfig at $E90000
 * (byte writes, /UDS only, low nibble first). Then a write to SCRATCH:
 * with a boot image the card serves its boot ROM after configuration
 * until Kickstart has copied it or anything is written, and there is no
 * Kickstart copy on the backplane. */
void bpc_attach_begin(struct BpClient *c)
{
    b_begin(c);
    b_put(c, BOP_RESET);
    b_write(c, AC_BASE_LO, S_UDS, 0x9000);
    b_write(c, AC_BASE_HI, S_UDS, 0xe000);
    b_write(c, 0x04, S_WORD, 0);
}

LONG bpc_attach_run(struct BpClient *c, UBYTE *words, ULONG bytes)
{
    UWORD len = c->len;
    ULONG tries;
    LONG n;
    /* The batch starts with a reset, so it can simply go again. */
    for (tries = 0; tries < 3; tries++) {
        c->len = len;
        if ((n = b_run(c, words, bytes)) == (LONG)bytes)
            return n;
    }
    return -1;
}

void bpc_log(struct BpClient *c, const UBYTE *text, UWORD len)
{
    if (c->link)
        c->ops->log(c->link, text, len);
}
