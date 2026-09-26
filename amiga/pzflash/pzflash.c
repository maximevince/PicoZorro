/*
 * pzflash: update the PicoZorro firmware from the Amiga (docs/UPDATE.md).
 *
 *   pzflash [BP [baud [unit [device]]]] INFO
 *   pzflash [BP ...] FLASH <file.pzf>     write into the other slot, verify
 *   pzflash [BP ...] ACTIVATE             reboot the module into it, on trial
 *   pzflash [BP ...] CONFIRM              keep the image running on trial
 *   pzflash [BP ...] UPDATE <file.pzf>    all of the above
 *   pzflash [BP ...] BOOTROM ON|OFF       the card's boot ROM, from the next
 *                                         Amiga reset
 *
 * Without BP the card is found with FindConfigDev and its update registers
 * ($C0-$FE of the A16 = 1 window, board base + $10000) are accessed on the
 * bus. With BP the same register accesses go over the UART backplane
 * (../common/bpclient.c).
 *
 * An image written with FLASH is only booted by ACTIVATE, and only kept by
 * CONFIRM: without it, the module goes back to the old image when its trial
 * ends (about two minutes). Nothing partial is ever booted.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <devices/timer.h>
#include <libraries/configvars.h>
#include <libraries/expansionbase.h>
#include <dos/dos.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/expansion.h>
#include <proto/timer.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include "../common/bpclient.h"

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
#define PZ_MAGIC        0x505a

/* docs/UPDATE.md, A16 = 1 window */
#define U_CTRL    0xc0
#define U_STATUS  0xc2
#define U_SIZE    0xc4
#define U_CRC     0xc8
#define U_DATA    0xcc
#define U_DONE    0xd0
#define U_RESULT  0xd4
#define U_SLOT_KB 0xd8
#define U_ERROR   0xda
#define U_VERSION 0xe0

#define C_BEGIN    1
#define C_ABORT    2
#define C_COMMIT   3
#define C_ACTIVATE 4
#define C_CONFIRM  5
#define C_BOOTROM_OFF 6
#define C_BOOTROM_ON  7

#define S_SPACE       0x0010
#define S_BUSY        0x0020
#define S_REFUSED     0x0040
#define S_SLOT_B      0x0100
#define S_BUY_PENDING 0x0200
#define S_PARTITIONED 0x0400
#define S_BOOTROM     0x0800

#define ST_RECEIVING 1
#define ST_READY     3
#define ST_ERROR     5

#define SECTOR 4096
#define CHUNK  1024       /* bytes per posted batch (datagrams hold 2048) */

struct Device *TimerBase;
struct ExpansionBase *ExpansionBase;

static const char *states[] = { "idle", "receiving", "verifying", "ready", "rebooting", "error" };
static const char *errors[] = { "", "not partitioned", "bad size", "flash error", "CRC mismatch",
                                "sequence", "not a firmware image", "confirm failed", "short image" };

/* ---- the two ways to the registers ---- */

static struct BpClient *bp;         /* BP: backplane */
static volatile UWORD *win1;        /* bus: board base + $10000 */

/* A register read; 0xffff when the backplane did not answer (retried: a
 * poll can fall into a flash erase on the module, when its UART loses
 * bytes). */
static UWORD rd(UBYTE reg)
{
    int i;
    UWORD v = 0xffff;
    if (!bp)
        return win1[reg / 2];
    for (i = 0; i < 10 && v == 0xffff; i++)
        v = bpc_rd1(bp, 1, reg);
    return v;
}

static void wr(UBYTE reg, UWORD v)
{
    if (!bp)
        win1[reg / 2] = v;
    else
        bpc_wr1(bp, 1, reg, v);
}

static ULONG rd32(UBYTE reg)
{
    UWORD hi = rd(reg);
    return (ULONG)hi << 16 | rd(reg + 2);
}

static void wr32(UBYTE reg, ULONG v)
{
    wr(reg, v >> 16);
    wr(reg + 2, v & 0xffff);
}

/* `len` bytes (memory order) into UPD_DATA. */
static void put_data(const UBYTE *p, ULONG len)
{
    ULONG i;
    if (!bp) {
        for (i = 0; i + 1 < len; i += 2)
            win1[U_DATA / 2] = (UWORD)p[i] << 8 | p[i + 1];
        if (len & 1)
            win1[U_DATA / 2] = (UWORD)p[len - 1] << 8;
        return;
    }
    for (i = 0; i < len; i += CHUNK) {
        ULONG n = len - i > CHUNK ? CHUNK : len - i;
        b_begin(bp);
        b_window(bp, 1);
        b_write_n2(bp, U_DATA, p + i, n, NULL, 0);
        b_run(bp, NULL, 0);
    }
}

/* ---- helpers ---- */

static int cmp_nocase(const char *a, const char *b)
{
    for (;; a++, b++) {
        int ca = (*a >= 'a' && *a <= 'z') ? *a - 32 : *a;
        int cb = (*b >= 'a' && *b <= 'z') ? *b - 32 : *b;
        if (ca != cb || !ca)
            return ca - cb;
    }
}

static void describe(UWORD st)
{
    UWORD err = rd(U_ERROR);
    printf("%s", (st & 0xf) < 6 ? states[st & 0xf] : "?");
    if ((st & 0xf) == ST_ERROR)
        printf(" (%s)", err < 9 ? errors[err] : "?");
    if (st & S_PARTITIONED)
        printf(", running from slot %s", (st & S_SLOT_B) ? "B" : "A");
    else
        printf(", not partitioned (no updates)");
    if (st & S_BUY_PENDING)
        printf(", ON TRIAL (CONFIRM keeps it)");
    printf(", boot ROM %s", (st & S_BOOTROM) ? "on" : "off");
    if (st & S_REFUSED)
        printf(", a write was refused");
    printf("\n");
}

static void version(char *out)
{
    int k;
    for (k = 0; k < 8; k++) {
        UWORD w = rd(U_VERSION + 2 * k);
        out[2 * k] = w >> 8;
        out[2 * k + 1] = w;
    }
    out[16] = 0;
}

static UWORD command(UWORD c)
{
    UWORD st;
    int i;
    wr(U_CTRL, c);
    for (i = 0; i < 1500; i++) {    /* 30 s */
        st = rd(U_STATUS);
        if (st != 0xffff && !(st & S_BUSY))
            return st;
        Delay(1);
    }
    return 0xffff;
}

static void info(void)
{
    char v[17];
    UWORD st = rd(U_STATUS);
    if (st == 0xffff) {
        printf("pzflash: no answer from the update registers\n");
        return;
    }
    version(v);
    printf("firmware %s, slots of %ld KiB\n", v, (LONG)rd(U_SLOT_KB));
    describe(st);
}

/* .pzf: 64-byte header, then the image (tools/mkpzf.py). */
struct Pzf {
    ULONG magic, len, crc, flags;
    char version[16];
    UBYTE reserved[32];
};

static ULONG crc_table[256];

static ULONG crc32(ULONG crc, const UBYTE *p, ULONG n)
{
    crc = ~crc;
    while (n--)
        crc = crc_table[(crc ^ *p++) & 0xff] ^ (crc >> 8);
    return ~crc;
}

static int flash(const char *path)
{
    struct Pzf h;
    BPTR f;
    UBYTE *buf;
    ULONG done = 0, crc = 0, t0, t1, got;
    struct timeval tv;
    UWORD st;
    LONG n;
    int rc = 20, i;
    char ver[17];

    for (i = 0; i < 256; i++) {
        ULONG c = i, j;
        for (j = 0; j < 8; j++)
            c = (c & 1) ? 0xedb88320UL ^ (c >> 1) : c >> 1;
        crc_table[i] = c;
    }
    buf = AllocVec(SECTOR, MEMF_PUBLIC);
    f = Open((STRPTR)path, MODE_OLDFILE);
    if (!buf || !f || Read(f, &h, sizeof(h)) != sizeof(h) || h.magic != 0x505a4631) {
        printf("pzflash: %s is not a firmware file (.pzf)\n", path);
        goto out;
    }
    /* Check the file before touching the module. */
    while ((n = Read(f, buf, SECTOR)) > 0) {
        crc = crc32(crc, buf, n);
        done += n;
    }
    if (done != h.len || crc != h.crc) {
        printf("pzflash: %s is damaged (%lu of %lu bytes, crc %08lx, want %08lx)\n", path, done, h.len, crc, h.crc);
        goto out;
    }
    memcpy(ver, h.version, 16);
    ver[16] = 0;
    printf("pzflash: %s: firmware %s, %lu bytes\n", path, ver, h.len);
    st = rd(U_STATUS);
    if (st == 0xffff || !(st & S_PARTITIONED)) {
        printf("pzflash: the module cannot take updates: ");
        describe(st);
        goto out;
    }
    Seek(f, sizeof(h), OFFSET_BEGINNING);
    wr32(U_SIZE, h.len);
    wr32(U_CRC, h.crc);
    st = command(C_BEGIN);
    if ((st & 0xf) != ST_RECEIVING) {
        printf("pzflash: BEGIN: ");
        describe(st);
        goto out;
    }
    GetSysTime(&tv);
    t0 = tv.tv_secs * 1000 + tv.tv_micro / 1000;
    for (done = 0; done < h.len; done += n) {
        n = Read(f, buf, SECTOR);
        if (n <= 0)
            break;
        for (i = 0;; i++) {
            st = rd(U_STATUS);
            if (st != 0xffff && (st & S_SPACE))
                break;
            if (st != 0xffff && (st & 0xf) == ST_ERROR) {
                printf("\npzflash: ");
                describe(st);
                goto out;
            }
            if (i > 1000) {
                printf("\npzflash: the module stopped taking data\n");
                command(C_ABORT);
                goto out;
            }
        }
        put_data(buf, n);
        printf("\r  %lu / %lu", done + n, h.len);
        fflush(stdout);
    }
    printf("\n");
    st = command(C_COMMIT);
    got = rd32(U_RESULT);
    GetSysTime(&tv);
    t1 = tv.tv_secs * 1000 + tv.tv_micro / 1000;
    if ((st & 0xf) != ST_READY) {
        printf("pzflash: ");
        describe(st);
        goto out;
    }
    printf("pzflash: written and read back, crc %08lx, %lu.%lu s\n", got, (t1 - t0) / 1000, (t1 - t0) % 1000 / 100);
    rc = 0;
out:
    if (f)
        Close(f);
    FreeVec(buf);
    return rc;
}

static int activate(void)
{
    char before[17], after[17];
    UWORD st = rd(U_STATUS);
    int i;
    if ((st & 0xf) != ST_READY) {
        printf("pzflash: nothing to activate: ");
        describe(st);
        return 10;
    }
    version(before);
    wr(U_CTRL, C_ACTIVATE);
    printf("pzflash: %s: rebooting the module into the new image ...\n", before);
    /* On the bus the module is gone for a moment; on the backplane replies
     * just stop. Either way: wait, then ask until it answers. */
    Delay(100);
    for (i = 0; i < 20; i++) {
        st = rd(U_STATUS);
        if (st != 0xffff)
            break;
        Delay(50);
    }
    if (st == 0xffff) {
        printf("pzflash: no answer after the reboot\n");
        return 20;
    }
    version(after);
    printf("pzflash: now running %s: ", after);
    describe(st);
    return (st & S_BUY_PENDING) ? 0 : 10;
}

static int confirm(void)
{
    UWORD st = command(C_CONFIRM);
    printf("pzflash: CONFIRM: ");
    describe(st);
    return (st != 0xffff && !(st & S_BUY_PENDING) && (st & 0xf) != ST_ERROR) ? 0 : 10;
}

static void usage(void)
{
    printf("usage: pzflash [BP [baud [unit [device]]]] INFO | FLASH <file> | ACTIVATE | CONFIRM | UPDATE <file> | BOOTROM ON|OFF\n");
}

int main(int argc, char **argv)
{
    struct MsgPort *tport = NULL;
    struct timerequest *treq = NULL;
    struct ConfigDev *cd;
    char bpargs[64];
    int a = 1, rc = 20;
    BOOL attach;
    const char *cmd;

    if (argc < 2) {
        usage();
        return 10;
    }
    tport = CreateMsgPort();
    treq = (struct timerequest *)CreateIORequest(tport, sizeof(*treq));
    if (!treq || OpenDevice((STRPTR)TIMERNAME, UNIT_VBLANK, (struct IORequest *)treq, 0) != 0) {
        printf("pzflash: no timer.device\n");
        goto out;
    }
    TimerBase = treq->tr_node.io_Device;

    bpargs[0] = 0;
    if (cmp_nocase(argv[a], "BP") == 0) {
        a++;
        /* numeric args after BP: baud unit device */
        while (a < argc && ((argv[a][0] >= '0' && argv[a][0] <= '9') || strchr(argv[a], '.'))) {
            strncat(bpargs, " ", sizeof(bpargs) - strlen(bpargs) - 1);
            strncat(bpargs, argv[a], sizeof(bpargs) - strlen(bpargs) - 1);
            a++;
        }
        bp = AllocVec(sizeof(*bp), MEMF_PUBLIC | MEMF_CLEAR);
        if (!bp || bpc_open(bp, bpargs) != 0) {
            printf("pzflash: cannot open the serial link\n");
            goto out;
        }
    }
    if (a >= argc) {
        usage();
        goto out;
    }
    cmd = argv[a];
    /* INFO / FLASH / UPDATE start as a booting Amiga would (on the
     * backplane: /BUSRST + Autoconfig); ACTIVATE / CONFIRM talk to the
     * board where it is, which survives the update reboot. */
    attach = cmp_nocase(cmd, "ACTIVATE") != 0 && cmp_nocase(cmd, "CONFIRM") != 0;
    if (bp) {
        if (attach) {
            UBYTE w[2];
            bpc_attach_begin(bp);
            b_read(bp, 0x00);
            if (bpc_attach_run(bp, w, 2) != 2 || ((UWORD)w[0] << 8 | w[1]) != PZ_MAGIC) {
                printf("pzflash: no PicoZorro on the backplane\n");
                goto out;
            }
        }
    } else {
        if (!(ExpansionBase = (struct ExpansionBase *)OpenLibrary((STRPTR)"expansion.library", 33)) ||
            !(cd = FindConfigDev(NULL, PZ_MANUFACTURER, PZ_PRODUCT))) {
            printf("pzflash: no PicoZorro board (manufacturer %ld, product %ld)\n", (LONG)PZ_MANUFACTURER, (LONG)PZ_PRODUCT);
            goto out;
        }
        if (cd->cd_BoardSize < 0x20000) {
            printf("pzflash: the board is %lu KiB: firmware without the update window (needs 128 KiB)\n",
                   cd->cd_BoardSize / 1024);
            goto out;
        }
        win1 = (volatile UWORD *)((UBYTE *)cd->cd_BoardAddr + 0x10000);
    }

    if (cmp_nocase(cmd, "INFO") == 0) {
        info();
        rc = 0;
    } else if (cmp_nocase(cmd, "FLASH") == 0 && a + 1 < argc) {
        info();
        rc = flash(argv[a + 1]);
    } else if (cmp_nocase(cmd, "ACTIVATE") == 0) {
        rc = activate();
    } else if (cmp_nocase(cmd, "CONFIRM") == 0) {
        rc = confirm();
    } else if (cmp_nocase(cmd, "UPDATE") == 0 && a + 1 < argc) {
        info();
        rc = flash(argv[a + 1]);
        if (rc == 0)
            rc = activate();
        if (rc == 0)
            rc = confirm();
        info();
    } else if (cmp_nocase(cmd, "BOOTROM") == 0 && a + 1 < argc) {
        BOOL on = cmp_nocase(argv[a + 1], "ON") == 0;
        UWORD st = command(on ? C_BOOTROM_ON : C_BOOTROM_OFF);
        printf("pzflash: boot ROM %s from the next reset: ", on ? "on" : "off");
        describe(st);
        rc = (st & 0xf) == ST_ERROR ? 10 : 0;
    } else {
        usage();
        rc = 10;
    }
out:
    if (bp) {
        bpc_close(bp);
        FreeVec(bp);
    }
    if (ExpansionBase)
        CloseLibrary((struct Library *)ExpansionBase);
    if (TimerBase)
        CloseDevice((struct IORequest *)treq);
    if (treq)
        DeleteIORequest((struct IORequest *)treq);
    if (tport)
        DeleteMsgPort(tport);
    return rc;
}
