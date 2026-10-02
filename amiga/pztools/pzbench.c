/*
 * pzbench: Zorro II bus bandwidth to the PicoZorro register window.
 * AmigaOS 2.04+ (V37) CLI tool, plain 68000 code.
 *
 *   pzbench [kwords [test]]   accesses per test in units of 1024 (default
 *                             512); test = one name below (default all)
 *
 * Every test runs under Forbid() and is timed with ReadEClock. The card's
 * CYCLES counter ($0C) is read before and after each test: the difference
 * is the number of bus cycles the slave served, which must equal the
 * number of 16-bit accesses the test made (a 68030 long access to the
 * 16-bit Zorro II bus is two cycles).
 *
 *   read.w     move.w (MAGIC),d0: raw read cycle rate
 *   write.w    move.w d0,(SCRATCH): raw write cycle rate
 *   write.x    move.w d0,(SCRATCH) with d0 alternating $0000 / $FFFF:
 *              every data line toggles, for timing write data on a scope
 *   read.l     move.l (BOOT_US),d0: two word cycles per access
 *   rx-copy    move.w (MAGIC),(a1)+: read into fast RAM, as an RX copy
 *   empty.r    move.w ($E90000),d0: an empty Zorro II I/O slot (Gary
 *              ends the cycle, no card): the bus without our wait states
 *   custom.r   move.w ($DFF006),d0 (VHPOSR): a custom chip register
 *   tx-frame   TX_LEN 1514, 757 x move.w (a1)+,(TX_DATA), TX_COMMIT: the
 *              network driver's TX path; the data lands in the card's
 *              SRAM (no NIC needed: the commit is then a tx_error)
 *   tx-unroll  tx-frame with the copy loop unrolled 16 x
 *   tx-regs    tx-frame, 8 longs into registers, then 16 word writes
 *   tx-alias   tx-frame, 8 longs from registers to the $C0-$FE data alias
 *   tx-movem   tx-frame, movem.l (a1)+,d0-d7 / movem.l d0-d7,(alias)
 *   rx-movem   movem.l (alias),d0-d7 / movem.l d0-d7,(a1), 32 bytes a go
 */
#include <exec/types.h>
#include <exec/execbase.h>
#include <devices/timer.h>
#include <libraries/configvars.h>
#include <proto/exec.h>
#include <proto/expansion.h>
#include <proto/timer.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
#define PZ_MAGIC        0x505a

#define REG_MAGIC       0x00
#define REG_SCRATCH     0x04
#define REG_BOOT_US     0x08
#define REG_CYCLES      0x0c
#define REG_TX_LEN      0x20
#define REG_TX_DATA     0x22
#define REG_TX_COMMIT   0x24
#define REG_ALIAS       0xc0    /* $C0-$FE: RX_DATA / TX_DATA alias */

#define FRAME 1514

struct ExpansionBase *ExpansionBase;
struct Device *TimerBase;

static volatile UBYTE *win;
static UWORD buf[(FRAME + 1) / 2];

#define R16(off) (*(volatile UWORD *)(win + (off)))
#define R32(off) (*(volatile ULONG *)(win + (off)))
#define X16(s) s s s s s s s s s s s s s s s s

static ULONG cycles(void)
{
    ULONG hi = R16(REG_CYCLES), lo = R16(REG_CYCLES + 2);
    return (hi << 16) | lo;
}

/* Each test: n = number of inner blocks; returns 16-bit accesses made. */
static ULONG t_read_w(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)(win + REG_MAGIC);
    UWORD d;
    ULONG i;
    for (i = 0; i < n; i++) {
        X16(d = *p;)
    }
    (void)d;
    return n * 16;
}

static ULONG t_write_w(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)(win + REG_SCRATCH);
    ULONG i;
    for (i = 0; i < n; i++) {
        X16(*p = (UWORD)i;)
    }
    return n * 16;
}

static ULONG t_write_x(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)(win + REG_SCRATCH);
    ULONG i;
    for (i = 0; i < n; i++) {
        X16(*p = 0x0000; *p = 0xffff;)
    }
    return n * 32;
}

static ULONG t_read_l(ULONG n)
{
    volatile ULONG *p = (volatile ULONG *)(win + REG_BOOT_US);
    ULONG d, i;
    for (i = 0; i < n / 2; i++) {
        X16(d = *p;)
    }
    (void)d;
    return (n / 2) * 32;
}

static ULONG t_empty_r(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)0xe90000;
    UWORD d;
    ULONG i;
    for (i = 0; i < n; i++) {
        X16(d = *p;)
    }
    (void)d;
    return n * 16;
}

static ULONG t_custom_r(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)0xdff006;
    UWORD d;
    ULONG i;
    for (i = 0; i < n; i++) {
        X16(d = *p;)
    }
    (void)d;
    return n * 16;
}

static ULONG t_rx_copy(ULONG n)
{
    volatile UWORD *p = (volatile UWORD *)(win + REG_MAGIC);
    ULONG i, made = 0;
    while (made < n * 16) {
        UWORD *q = buf;
        for (i = 0; i < (FRAME + 1) / 2; i++)
            *q++ = *p;
        made += (FRAME + 1) / 2;
    }
    return made;
}

static ULONG t_tx_frame(ULONG n)
{
    volatile UWORD *len = (volatile UWORD *)(win + REG_TX_LEN);
    volatile UWORD *data = (volatile UWORD *)(win + REG_TX_DATA);
    volatile UWORD *commit = (volatile UWORD *)(win + REG_TX_COMMIT);
    ULONG i, made = 0;
    while (made < n * 16) {
        const UWORD *q = buf;
        *len = FRAME;
        for (i = 0; i < (FRAME + 1) / 2; i++)
            *data = *q++;
        *commit = 0;
        made += (FRAME + 1) / 2 + 2;
    }
    return made;
}

/* tx-frame with the copy loop unrolled 16 x: what a tuned driver does. */
static ULONG t_tx_unroll(ULONG n)
{
    volatile UWORD *len = (volatile UWORD *)(win + REG_TX_LEN);
    volatile UWORD *data = (volatile UWORD *)(win + REG_TX_DATA);
    volatile UWORD *commit = (volatile UWORD *)(win + REG_TX_COMMIT);
    ULONG i, made = 0;
    while (made < n * 16) {
        const UWORD *q = buf;
        *len = FRAME;
        for (i = 0; i < 47; i++) {
            X16(*data = *q++;)
        }
        for (i = 0; i < (FRAME + 1) / 2 - 47 * 16; i++)
            *data = *q++;
        *commit = 0;
        made += (FRAME + 1) / 2 + 2;
    }
    return made;
}

/* tx-frame from registers: 8 longs loaded from fast RAM, then 16 word
 * writes back to back (does the TF536 stall fast RAM reads during a
 * motherboard cycle?). */
static ULONG t_tx_regs(ULONG n)
{
    volatile UWORD *len = (volatile UWORD *)(win + REG_TX_LEN);
    volatile UWORD *data = (volatile UWORD *)(win + REG_TX_DATA);
    volatile UWORD *commit = (volatile UWORD *)(win + REG_TX_COMMIT);
    ULONG i, made = 0;
    while (made < n * 16) {
        const ULONG *q = (const ULONG *)buf;
        *len = FRAME;
        for (i = 0; i < 47; i++) {
            register ULONG a = q[0], b = q[1], c = q[2], d = q[3], e = q[4], f = q[5], g = q[6], h = q[7];
            q += 8;
            *data = a >> 16; *data = a; *data = b >> 16; *data = b;
            *data = c >> 16; *data = c; *data = d >> 16; *data = d;
            *data = e >> 16; *data = e; *data = f >> 16; *data = f;
            *data = g >> 16; *data = g; *data = h >> 16; *data = h;
        }
        {
            const UWORD *r = (const UWORD *)q;
            for (i = 0; i < (FRAME + 1) / 2 - 47 * 16; i++)
                *data = *r++;
        }
        *commit = 0;
        made += (FRAME + 1) / 2 + 2;
    }
    return made;
}

/* TX through the data alias with long writes from registers: each long is
 * two back-to-back word cycles. 47 x 32 bytes, then 5 words to TX_DATA. */
static ULONG t_tx_alias(ULONG n)
{
    volatile UWORD *len = (volatile UWORD *)(win + REG_TX_LEN);
    volatile UWORD *data = (volatile UWORD *)(win + REG_TX_DATA);
    volatile ULONG *al = (volatile ULONG *)(win + REG_ALIAS);
    volatile UWORD *commit = (volatile UWORD *)(win + REG_TX_COMMIT);
    ULONG i, made = 0;
    while (made < n * 16) {
        const ULONG *q = (const ULONG *)buf;
        *len = FRAME;
        for (i = 0; i < 47; i++) {
            register ULONG a = q[0], b = q[1], c = q[2], d = q[3], e = q[4], f = q[5], g = q[6], h = q[7];
            q += 8;
            al[0] = a; al[1] = b; al[2] = c; al[3] = d; al[4] = e; al[5] = f; al[6] = g; al[7] = h;
        }
        {
            const UWORD *r = (const UWORD *)q;
            for (i = 0; i < 5; i++)
                *data = *r++;
        }
        *commit = 0;
        made += (FRAME + 1) / 2 + 2;
    }
    return made;
}

/* TX with movem.l: 8 longs from fast RAM into d0-d7, then into the alias. */
static ULONG t_tx_movem(ULONG n)
{
    volatile UWORD *len = (volatile UWORD *)(win + REG_TX_LEN);
    volatile UWORD *data = (volatile UWORD *)(win + REG_TX_DATA);
    volatile UWORD *commit = (volatile UWORD *)(win + REG_TX_COMMIT);
    UBYTE *al = (UBYTE *)(win + REG_ALIAS);
    ULONG i, made = 0;
    while (made < n * 16) {
        const UBYTE *q = (const UBYTE *)buf;
        *len = FRAME;
        for (i = 0; i < 47; i++) {
            __asm__ volatile("movem.l (%0)+,%%d0-%%d7\n\tmovem.l %%d0-%%d7,(%1)"
                             : "+a"(q) : "a"(al) : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "memory");
        }
        {
            const UWORD *r = (const UWORD *)q;
            for (i = 0; i < 5; i++)
                *data = *r++;
        }
        *commit = 0;
        made += (FRAME + 1) / 2 + 2;
    }
    return made;
}

/* RX side with movem.l: 8 longs from the alias into d0-d7, then into fast
 * RAM (no frame queued: the reads return 0, the timing is the same). */
static ULONG t_rx_movem(ULONG n)
{
    UBYTE *al = (UBYTE *)(win + REG_ALIAS);
    ULONG i, made = 0;
    while (made < n * 16) {
        UBYTE *q = (UBYTE *)buf;
        for (i = 0; i < 47; i++) {
            __asm__ volatile("movem.l (%1),%%d0-%%d7\n\tmovem.l %%d0-%%d7,(%0)\n\tlea 32(%0),%0"
                             : "+a"(q) : "a"(al) : "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "memory");
        }
        made += 47 * 16;
    }
    return made;
}

struct test {
    const char *name;
    ULONG (*fn)(ULONG);
    const char *what;
};

static const struct test tests[] = {
    { "empty.r",  t_empty_r,  "word reads, empty slot $E90000 (baseline)" },
    { "custom.r", t_custom_r, "word reads, VHPOSR (baseline)" },
    { "read.w",   t_read_w,   "word reads" },
    { "write.w",  t_write_w,  "word writes" },
    { "write.x",  t_write_x,  "word writes, alternating $0000/$FFFF" },
    { "read.l",   t_read_l,   "long reads (2 cycles each)" },
    { "rx-copy",  t_rx_copy,  "word reads into fast RAM" },
    { "tx-frame", t_tx_frame, "1514-byte TX frames" },
    { "tx-regs",  t_tx_regs,  "1514-byte TX frames, 16 words from registers per burst" },
    { "tx-alias", t_tx_alias, "1514-byte TX frames, move.l from registers to $C0 alias" },
    { "tx-movem", t_tx_movem, "1514-byte TX frames, movem.l 8 longs to $C0 alias" },
    { "rx-movem", t_rx_movem, "movem.l 8 longs from $C0 alias into fast RAM" },
    { "tx-unroll", t_tx_unroll, "1514-byte TX frames, copy loop unrolled 16 x" },
};

int main(int argc, char **argv)
{
    struct ConfigDev *cd;
    struct MsgPort *port = NULL;
    struct timerequest *tr = NULL;
    ULONG kwords = 512, n, efreq, i;
    const char *only = argc > 2 ? argv[2] : NULL;
    int rc = 20, timer_open = 0;

    if (argc > 1)
        kwords = (ULONG)atol(argv[1]);
    if (kwords < 16)
        kwords = 16;
    n = kwords * 1024 / 16;

    ExpansionBase = (struct ExpansionBase *)OpenLibrary((CONST_STRPTR)"expansion.library", 0);
    if (!ExpansionBase) {
        printf("cannot open expansion.library\n");
        return 20;
    }
    cd = FindConfigDev(NULL, PZ_MANUFACTURER, PZ_PRODUCT);
    if (!cd) {
        printf("no PicoZorro board in the config list\n");
        goto out;
    }
    win = (volatile UBYTE *)cd->cd_BoardAddr;
    if (R16(REG_MAGIC) != PZ_MAGIC) {
        printf("magic register reads $%04lx, expected $%04lx\n", (ULONG)R16(REG_MAGIC), (ULONG)PZ_MAGIC);
        goto out;
    }

    port = CreateMsgPort();
    tr = port ? (struct timerequest *)CreateIORequest(port, sizeof *tr) : NULL;
    if (!tr || OpenDevice((CONST_STRPTR)TIMERNAME, UNIT_ECLOCK, (struct IORequest *)tr, 0)) {
        printf("cannot open timer.device\n");
        goto out;
    }
    timer_open = 1;
    TimerBase = tr->tr_node.io_Device;

    for (i = 0; i < (FRAME + 1) / 2; i++)
        buf[i] = (UWORD)(i * 0x0101);

    {
        struct EClockVal e;
        efreq = ReadEClock(&e);
    }
    printf("PicoZorro at $%08lx, CPU %s, EClock %ld Hz, %ld K accesses per test\n",
           (ULONG)win,
           (SysBase->AttnFlags & AFF_68040) ? "68040+" :
           (SysBase->AttnFlags & AFF_68030) ? "68030" :
           (SysBase->AttnFlags & AFF_68020) ? "68020" :
           (SysBase->AttnFlags & AFF_68010) ? "68010" : "68000",
           (LONG)efreq, (LONG)kwords);
    printf("%-9s %10s %10s %9s %8s %9s  %s\n",
           "test", "accesses", "served", "ns/cycle", "KB/s", "kcycle/s", "what");

    for (i = 0; i < sizeof tests / sizeof tests[0]; i++) {
        struct EClockVal a, b;
        ULONG c0, c1, made, ticks, ns, kbs, kcs;
        if (only && strcmp(only, tests[i].name))
            continue;
        unsigned long long t;

        Forbid();
        c0 = cycles();
        ReadEClock(&a);
        made = tests[i].fn(n);
        ReadEClock(&b);
        c1 = cycles();
        Permit();

        /* the two CYCLES reads after c0 are counted in c1 - c0 */
        ticks = b.ev_lo - a.ev_lo;
        if (!ticks)
            ticks = 1;
        t = ticks;
        ns = (ULONG)(t * 1000000000ULL / efreq / made);
        kbs = (ULONG)((unsigned long long)made * 2 * efreq / 1024 / t);
        kcs = (ULONG)((unsigned long long)made * efreq / 1000 / t);
        printf("%-9s %10ld %10ld %9ld %8ld %9ld  %s\n",
               tests[i].name, (LONG)made, (LONG)(c1 - c0 - 2), (LONG)ns, (LONG)kbs, (LONG)kcs,
               tests[i].what);
    }
    rc = 0;
out:
    if (timer_open)
        CloseDevice((struct IORequest *)tr);
    if (tr)
        DeleteIORequest((struct IORequest *)tr);
    if (port)
        DeleteMsgPort(port);
    CloseLibrary((struct Library *)ExpansionBase);
    return rc;
}
