/*
 * pztest: finds the PicoZorro board and hammers its scratch register.
 * AmigaOS 2.04+ (V37) CLI tool, plain 68000 code.
 *
 *   pztest [seconds]      default 10
 */
#include <exec/types.h>
#include <libraries/configvars.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/expansion.h>
#include <devices/timer.h>
#include <proto/timer.h>
#include <stdio.h>
#include <stdlib.h>

#define PZ_MANUFACTURER 2011
#define PZ_PRODUCT      0x5a
#define PZ_MAGIC        0x505a

#define REG_MAGIC       0x00
#define REG_VERSION     0x02
#define REG_SCRATCH     0x04
#define REG_BOOT_US_HI  0x08
#define REG_BOOT_US_LO  0x0a

struct ExpansionBase *ExpansionBase;
struct Device *TimerBase;

static ULONG errors, iterations;

static void check_word(volatile UWORD *reg, UWORD value)
{
    UWORD got;

    *reg = value;
    got = *reg;
    iterations++;
    if (got != value) {
        if (errors < 10)
            printf("  word: wrote %04lx read %04lx\n", (ULONG)value, (ULONG)got);
        errors++;
    }
}

static void check_bytes(volatile UBYTE *reg, UBYTE hi, UBYTE lo)
{
    UBYTE ghi, glo;

    /* The card ignores /LDS (the One TH does not wire it): the even byte
     * is taken as a word write, the odd one as the low byte. Only this
     * order reads back what was written. */
    reg[0] = hi;                /* even address: /UDS, D15-D8 */
    reg[1] = lo;                /* odd address:  /LDS, D7-D0  */
    ghi = reg[0];
    glo = reg[1];
    iterations++;
    if (ghi != hi || glo != lo) {
        if (errors < 10)
            printf("  byte: wrote %02lx %02lx read %02lx %02lx\n",
                   (ULONG)hi, (ULONG)lo, (ULONG)ghi, (ULONG)glo);
        errors++;
    }
}

static ULONG now_seconds(void)
{
    struct timeval tv;

    GetSysTime(&tv);
    return tv.tv_secs;
}

int main(int argc, char **argv)
{
    struct ConfigDev *cd;
    struct MsgPort *port = NULL;
    struct timerequest *tr = NULL;
    volatile UWORD *base;
    ULONG seconds = 10, start, seed = 1, boot_us;
    int bit, rc = 20, timer_open = 0;

    if (argc > 1)
        seconds = (ULONG)atol(argv[1]);

    ExpansionBase = (struct ExpansionBase *)OpenLibrary((CONST_STRPTR)"expansion.library", 0);
    if (!ExpansionBase) {
        printf("cannot open expansion.library\n");
        return 20;
    }

    cd = FindConfigDev(NULL, PZ_MANUFACTURER, PZ_PRODUCT);
    if (!cd) {
        printf("no PicoZorro board (manufacturer %ld product $%lx) in the config list\n",
               (LONG)PZ_MANUFACTURER, (ULONG)PZ_PRODUCT);
        goto out;
    }
    base = (volatile UWORD *)cd->cd_BoardAddr;
    printf("PicoZorro at $%08lx, size $%lx, serial $%08lx\n",
           (ULONG)cd->cd_BoardAddr, (ULONG)cd->cd_BoardSize,
           (ULONG)cd->cd_Rom.er_SerialNumber);

    /* 2011 is a shared hacker ID: make sure it really is our board. */
    if (base[REG_MAGIC / 2] != PZ_MAGIC) {
        printf("magic register reads $%04lx, expected $%04lx\n",
               (ULONG)base[REG_MAGIC / 2], (ULONG)PZ_MAGIC);
        goto out;
    }
    boot_us = ((ULONG)base[REG_BOOT_US_HI / 2] << 16) | base[REG_BOOT_US_LO / 2];
    printf("firmware version $%04lx, cold start to slave ready %ld us\n",
           (ULONG)base[REG_VERSION / 2], (LONG)boot_us);

    port = CreateMsgPort();
    tr = port ? (struct timerequest *)CreateIORequest(port, sizeof *tr) : NULL;
    if (!tr || OpenDevice((CONST_STRPTR)TIMERNAME, UNIT_VBLANK, (struct IORequest *)tr, 0)) {
        printf("cannot open timer.device\n");
        goto out;
    }
    timer_open = 1;
    TimerBase = tr->tr_node.io_Device;

    printf("walking bits\n");
    for (bit = 0; bit < 16; bit++) {
        check_word(&base[REG_SCRATCH / 2], (UWORD)(1u << bit));
        check_word(&base[REG_SCRATCH / 2], (UWORD)~(1u << bit));
    }

    printf("random words and bytes for %ld s (Ctrl-C stops)\n", (LONG)seconds);
    start = now_seconds();
    iterations = 0;
    while (now_seconds() - start < seconds) {
        int n;
        for (n = 0; n < 256; n++) {
            seed = seed * 1103515245UL + 12345UL;
            check_word(&base[REG_SCRATCH / 2], (UWORD)(seed >> 16));
            check_bytes((volatile UBYTE *)&base[REG_SCRATCH / 2],
                        (UBYTE)(seed >> 8), (UBYTE)seed);
        }
        if (SetSignal(0L, SIGBREAKF_CTRL_C) & SIGBREAKF_CTRL_C)
            break;
    }
    seconds = now_seconds() - start;
    printf("%ld iterations, %ld errors, %ld iterations/s\n",
           (LONG)iterations, (LONG)errors, (LONG)(seconds ? iterations / seconds : 0));
    rc = errors ? 5 : 0;

out:
    if (timer_open) CloseDevice((struct IORequest *)tr);
    if (tr) DeleteIORequest((struct IORequest *)tr);
    if (port) DeleteMsgPort(port);
    CloseLibrary((struct Library *)ExpansionBase);
    return rc;
}
