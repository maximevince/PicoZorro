/*
 * stortest: check of a USB stick through Poseidon's
 * massstorage.class (usbscsi.device), i.e. bulk transfers through
 * picozorrousb.device. READ CAPACITY and INQUIRY via HD_SCSICMD, sector 0,
 * then the same range read twice with CMD_READ: CRC32 of both passes must
 * match; prints the time per pass.
 *
 *   stortest [kbytes [unit [device [chunk]]]]   default 64 0 usbscsi.device 4
 *   stortest kbytes unit device chunk write START [any]
 *
 * chunk: KiB per CMD_READ (1..64); kbytes is rounded down to whole chunks.
 *
 * write START: instead of the read passes, writes a pseudo-random pattern
 * to `kbytes` from block START on (TD_WRITE64, chunk KiB each), reads it
 * back (TD_READ64) and compares; prints time and bytes/s of both passes.
 * The area is read first and kept (in memory and in RAM:stortest-START.bin)
 * and written back after the read-back, then read again and compared with
 * what was kept; the file goes once that restore is verified. By default
 * it writes only to a dedicated test stick that answers INQUIRY with
 * 'General' / 'UDisk' and refuses any other, so a wrong unit number cannot
 * write elsewhere. With the word `any` at the end it writes to any unit,
 * but not in the first or the last MiB of the device (partition tables,
 * GPT and its backup).
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/io.h>
#include <devices/scsidisk.h>
#include <devices/trackdisk.h>
#include <dos/dos.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static ULONG crc_table[256];

static void crc_init(void)
{
    ULONG i, j, c;
    for (i = 0; i < 256; i++) {
        c = i;
        for (j = 0; j < 8; j++)
            c = (c & 1) ? 0xedb88320UL ^ (c >> 1) : c >> 1;
        crc_table[i] = c;
    }
}

static ULONG crc32(ULONG crc, const UBYTE *p, ULONG n)
{
    crc = ~crc;
    while (n--)
        crc = crc_table[(crc ^ *p++) & 0xff] ^ (crc >> 8);
    return ~crc;
}

static LONG scsi(struct IOStdReq *io, UBYTE *cmd, UWORD cmdlen, UBYTE *data, ULONG len)
{
    struct SCSICmd sc;
    UBYTE sense[18];
    memset(&sc, 0, sizeof(sc));
    sc.scsi_Data = (UWORD *)data;
    sc.scsi_Length = len;
    sc.scsi_Command = cmd;
    sc.scsi_CmdLength = cmdlen;
    sc.scsi_Flags = SCSIF_READ | SCSIF_AUTOSENSE;
    sc.scsi_SenseData = sense;
    sc.scsi_SenseLength = sizeof(sense);
    io->io_Command = HD_SCSICMD;
    io->io_Data = &sc;
    io->io_Length = sizeof(sc);
    if (DoIO((struct IORequest *)io) != 0)
        return -(LONG)io->io_Error - 1000;
    if (sc.scsi_Status)
        return -(LONG)sc.scsi_Status;
    return (LONG)sc.scsi_Actual;
}

/* The pattern's longword at byte offset `off` of the test area. */
static ULONG pattern(ULONG seed, ULONG off)
{
    ULONG v = seed ^ (off >> 2) * 0x9e3779b1UL;
    v ^= v >> 16;
    v *= 0x85ebca6bUL;
    v ^= v >> 13;
    return v;
}

/* TD_READ64 / TD_WRITE64 of `len` bytes at byte offset hi:lo. */
static LONG io64(struct IOStdReq *io, UWORD cmd, UBYTE *buf, ULONG len, ULONG hi, ULONG lo)
{
    io->io_Command = cmd;
    io->io_Data = buf;
    io->io_Length = len;
    io->io_Offset = lo;
    io->io_Actual = hi;
    if (DoIO((struct IORequest *)io) != 0 || io->io_Actual != len)
        return io->io_Error ? -(LONG)io->io_Error : -1;
    return 0;
}

static ULONG ticks(void)
{
    struct DateStamp ds;
    DateStamp(&ds);
    return (ULONG)ds.ds_Minute * 3000 + ds.ds_Tick;
}

static LONG read_pass(struct IOStdReq *io, UBYTE *buf, ULONG chunk, ULONG total, ULONG *crc, ULONG *t)
{
    ULONG off, t0 = ticks();
    *crc = 0;
    for (off = 0; off < total; off += chunk) {
        io->io_Command = CMD_READ;
        io->io_Data = buf;
        io->io_Length = chunk;
        io->io_Offset = off;
        if (DoIO((struct IORequest *)io) != 0 || io->io_Actual != chunk) {
            printf("  CMD_READ at %lu: error %ld, actual %lu\n", off, (LONG)io->io_Error, io->io_Actual);
            return -1;
        }
        *crc = crc32(*crc, buf, chunk);
    }
    *t = ticks() - t0;
    return 0;
}

/* `total` bytes from byte offset `base` into `save` (TD_READ64, chunk each). */
static LONG read_area(struct IOStdReq *io, UBYTE *save, ULONG chunk, ULONG total, unsigned long long base)
{
    ULONG off;
    LONG e;
    for (off = 0; off < total; off += chunk) {
        unsigned long long pos = base + off;
        if ((e = io64(io, TD_READ64, save + off, chunk, (ULONG)(pos >> 32), (ULONG)pos)) != 0) {
            printf("  TD_READ64 at +%lu: error %ld, actual %lu\n", off, e, io->io_Actual);
            return e;
        }
    }
    return 0;
}

/* Write the pattern over [start, start + total), read it back, compare.
 * The area is saved before and written back after; returns 20 when the
 * restore could not be verified. */
static int write_test(struct IOStdReq *io, UBYTE *buf, ULONG chunk, ULONG total, ULONG start, ULONG bsize)
{
    ULONG seed = ticks() * 2654435761UL + 1, off, i, t0, tw = 0, tr = 0, bad = 0, first = 0;
    unsigned long long base = (unsigned long long)start * bsize, pos;
    UBYTE *save, *check;
    char name[40];
    BPTR fh;
    LONG e;
    int rc = 0, restored = 0;

    save = AllocVec(total, MEMF_ANY);
    check = AllocVec(chunk, MEMF_ANY);
    if (!save || !check) {
        printf("stortest: no memory to keep %lu KiB of the area: not writing\n", total / 1024);
        FreeVec(save);
        FreeVec(check);
        return 20;
    }
    if (read_area(io, save, chunk, total, base) != 0) {
        printf("stortest: could not read the area first: not writing\n");
        FreeVec(save);
        FreeVec(check);
        return 20;
    }
    sprintf(name, "RAM:stortest-%lu.bin", start);
    fh = Open((STRPTR)name, MODE_NEWFILE);
    if (!fh || Write(fh, save, total) != (LONG)total) {
        printf("stortest: could not write %s: not writing\n", name);
        if (fh)
            Close(fh);
        FreeVec(save);
        FreeVec(check);
        return 20;
    }
    Close(fh);
    printf("  saved: blocks %lu-%lu (%lu KiB) read and kept in memory and in %s\n", start, start + total / bsize - 1,
           total / 1024, name);

    printf("  write: blocks %lu-%lu, pattern seed %08lx\n", start, start + total / bsize - 1, seed);
    t0 = ticks();
    for (off = 0; off < total; off += chunk) {
        for (i = 0; i < chunk; i += 4)
            *(ULONG *)(buf + i) = pattern(seed, off + i);
        pos = base + off;
        if ((e = io64(io, TD_WRITE64, buf, chunk, (ULONG)(pos >> 32), (ULONG)pos)) != 0) {
            printf("  TD_WRITE64 at +%lu: error %ld, actual %lu\n", off, e, io->io_Actual);
            rc = 20;
            goto restore;
        }
    }
    tw = ticks() - t0;
    t0 = ticks();
    for (off = 0; off < total; off += chunk) {
        pos = base + off;
        memset(buf, 0, chunk);
        if ((e = io64(io, TD_READ64, buf, chunk, (ULONG)(pos >> 32), (ULONG)pos)) != 0) {
            printf("  TD_READ64 at +%lu: error %ld, actual %lu\n", off, e, io->io_Actual);
            rc = 20;
            goto restore;
        }
        for (i = 0; i < chunk; i += 4)
            if (*(ULONG *)(buf + i) != pattern(seed, off + i) && bad++ == 0)
                first = off + i;
    }
    tr = ticks() - t0;
    printf("  write pass: %lu.%02lu s", tw / 50, (tw % 50) * 2);
    if (tw)
        printf(", %lu bytes/s", total * 50 / tw);
    printf("\n  read pass:  %lu.%02lu s", tr / 50, (tr % 50) * 2);
    if (tr)
        printf(", %lu bytes/s", total * 50 / tr);
    printf("\n");
    if (bad) {
        printf("  %lu longwords differ, first at +%lu\n", bad, first);
        rc = 10;
    }

restore:
    /* Twice at most: a failed command is retried once as a whole. */
    for (i = 0; i < 2 && !restored; i++) {
        for (off = 0; off < total; off += chunk) {
            pos = base + off;
            if ((e = io64(io, TD_WRITE64, save + off, chunk, (ULONG)(pos >> 32), (ULONG)pos)) != 0) {
                printf("  restore: TD_WRITE64 at +%lu: error %ld\n", off, e);
                break;
            }
        }
        if (off < total)
            continue;
        for (off = 0; off < total; off += chunk) {
            pos = base + off;
            if (io64(io, TD_READ64, check, chunk, (ULONG)(pos >> 32), (ULONG)pos) != 0 ||
                memcmp(check, save + off, chunk) != 0)
                break;
        }
        restored = off >= total;
    }
    if (restored) {
        printf("  restore: blocks %lu-%lu written back, read back identical to what was saved\n", start,
               start + total / bsize - 1);
        DeleteFile((STRPTR)name);
    } else {
        printf("  restore: NOT VERIFIED; the original content is in %s\n", name);
        rc = 20;
    }
    FreeVec(save);
    FreeVec(check);
    if (rc == 0)
        printf("stortest: PASS (%lu KiB read back identical, area restored)\n", total / 1024);
    else if (rc == 10 && restored)
        printf("stortest: FAIL (read back differs; area restored)\n");
    else
        printf("stortest: FAIL%s\n", restored ? " (area restored)" : " (area NOT restored)");
    return rc;
}

int main(int argc, char **argv)
{
    ULONG kb = argc > 1 ? strtoul(argv[1], NULL, 0) : 64;
    ULONG unit = argc > 2 ? strtoul(argv[2], NULL, 0) : 0;
    const char *dev = argc > 3 ? argv[3] : "usbscsi.device";
    struct MsgPort *port;
    struct IOStdReq *io;
    UBYTE *buf;
    UBYTE cmd[10];
    ULONG chunk = (argc > 4 ? strtoul(argv[4], NULL, 0) : 4) * 1024, total, crc1, crc2, t1, t2, blocks, bsize;
    ULONG start = 0;
    BOOL wr = argc > 5, any = FALSE;
    LONG n;
    int rc = 20, i;

    if (wr && argc == 8 && strcmp(argv[7], "any") == 0)
        any = TRUE;
    if (wr && ((argc != 7 && !any) || strcmp(argv[5], "write") != 0)) {
        printf("usage: stortest kbytes unit device chunk write START [any]\n");
        return 20;
    }
    if (wr)
        start = strtoul(argv[6], NULL, 0);

    if (chunk < 1024 || chunk > 65536)
        chunk = 4096;
    total = kb * 1024 / chunk * chunk;
    crc_init();
    port = CreateMsgPort();
    io = (struct IOStdReq *)CreateIORequest(port, sizeof(*io));
    buf = AllocVec(chunk, MEMF_PUBLIC);
    if (!port || !io || !buf || OpenDevice((STRPTR)dev, unit, (struct IORequest *)io, 0) != 0) {
        printf("stortest: cannot open %s unit %lu\n", dev, unit);
        goto out;
    }
    printf("stortest: %s unit %lu, %lu KiB in %lu KiB %s\n", dev, unit, total / 1024, chunk / 1024,
           wr ? "writes and reads" : "reads, read-only");

    memset(cmd, 0, sizeof(cmd));
    cmd[0] = 0x12; /* INQUIRY */
    cmd[4] = 36;
    n = scsi(io, cmd, 6, buf, 36);
    if (n >= 36)
        printf("  INQUIRY: type %lu '%.8s' '%.16s' '%.4s'\n", (ULONG)(buf[0] & 0x1f), buf + 8, buf + 16, buf + 32);
    else
        printf("  INQUIRY: %ld\n", n);
    /* 'General ' (padded to 8) and a product that starts with 'UDisk'. */
    if (wr && !any && (n < 36 || memcmp(buf + 8, "General ", 8) != 0 || memcmp(buf + 16, "UDisk", 5) != 0)) {
        printf("stortest: not the 'General UDisk' stick: refusing to write\n");
        rc = 10;
        goto close;
    }

    memset(cmd, 0, sizeof(cmd));
    cmd[0] = 0x25; /* READ CAPACITY(10) */
    n = scsi(io, cmd, 10, buf, 8);
    if (n != 8) {
        printf("  READ CAPACITY: %ld\n", n);
        goto close;
    }
    blocks = ((ULONG)buf[0] << 24 | (ULONG)buf[1] << 16 | (ULONG)buf[2] << 8 | buf[3]) + 1;
    bsize = (ULONG)buf[4] << 24 | (ULONG)buf[5] << 16 | (ULONG)buf[6] << 8 | buf[7];
    printf("  READ CAPACITY: %lu blocks of %lu bytes (%lu MiB)\n", blocks, bsize, blocks / (1048576 / (bsize ? bsize : 512)));

    io->io_Command = CMD_READ;
    io->io_Data = buf;
    io->io_Length = 512;
    io->io_Offset = 0;
    if (DoIO((struct IORequest *)io) != 0) {
        printf("  sector 0: error %ld\n", (LONG)io->io_Error);
        goto close;
    }
    printf("  sector 0:");
    for (i = 0; i < 16; i++)
        printf(" %02lx", (ULONG)buf[i]);
    printf(" ... signature %02lx%02lx\n", (ULONG)buf[510], (ULONG)buf[511]);
    if (buf[510] == 0x55 && buf[511] == 0xaa)
        for (i = 0; i < 4; i++) {
            UBYTE *e = buf + 446 + 16 * i;
            if (e[4])
                printf("  partition %ld: type %02lx, start %lu, %lu sectors\n", (LONG)i + 1, (ULONG)e[4],
                       (ULONG)e[8] | (ULONG)e[9] << 8 | (ULONG)e[10] << 16 | (ULONG)e[11] << 24,
                       (ULONG)e[12] | (ULONG)e[13] << 8 | (ULONG)e[14] << 16 | (ULONG)e[15] << 24);
        }

    if (wr) {
        if (bsize != 512 || start == 0 || start + total / bsize > blocks) {
            printf("stortest: blocks %lu+%lu outside the stick or block 0: refusing\n", start, total / 512);
            rc = 10;
        } else if (any && (start < 2048 || start + total / bsize > blocks - 2048)) {
            printf("stortest: blocks %lu+%lu in the first or the last MiB: refusing\n", start, total / 512);
            rc = 10;
        } else {
            rc = write_test(io, buf, chunk, total, start, bsize);
        }
        goto close;
    }
    if (read_pass(io, buf, chunk, total, &crc1, &t1) || read_pass(io, buf, chunk, total, &crc2, &t2))
        goto close;
    printf("  pass 1: crc32 %08lx, %lu.%02lu s\n", crc1, t1 / 50, (t1 % 50) * 2);
    printf("  pass 2: crc32 %08lx, %lu.%02lu s\n", crc2, t2 / 50, (t2 % 50) * 2);
    if (t1 + t2)
        printf("  %lu bytes/s\n", (2 * total * 50) / (t1 + t2));
    printf("stortest: %s\n", crc1 == crc2 ? "PASS" : "FAIL (passes differ)");
    rc = crc1 == crc2 ? 0 : 10;
close:
    CloseDevice((struct IORequest *)io);
out:
    FreeVec(buf);
    if (io)
        DeleteIORequest((struct IORequest *)io);
    if (port)
        DeleteMsgPort(port);
    return rc;
}
