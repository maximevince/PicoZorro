/*
 * picozorrousb.device, backend `ser`: the tunnel records as frames over
 * serial.device (../common/serlink.c: payload + CRC-16, COBS). Under FS-UAE
 * the emulated serial port is a TCP socket that a relay on the PC bridges
 * to the module's UART; on a real Amiga a level shifter to the module would
 * do.
 *
 * ENV:PZUSB = "ser [baud [unit [device]]]", default 115200 0 serial.device.
 * Debug text (make DEBUG=1) goes out in frames starting with "LO"; the
 * relay writes those to its log instead of passing them on.
 */
#include <proto/exec.h>
#include "../common/serlink.h"
#include "device.h"

static void ser_close(struct PZUBase *pz)
{
    serlink_close(pz->be_data);
    pz->be_data = NULL;
}

static LONG ser_open(struct PZUBase *pz, const char *args)
{
    while (*args && *args != ' ')   /* skip "ser" */
        args++;
    pz->be_data = serlink_open(args);
    return pz->be_data ? 0 : -1;
}

static ULONG ser_wait(struct PZUBase *pz, ULONG sigs, ULONG ms)
{
    ULONG got = serlink_wait(pz->be_data, sigs, ms);
    pz->be_readable = serlink_pending(pz->be_data);
    return got;
}

static LONG ser_send(struct PZUBase *pz, const struct PzuReq *h, const UBYTE *data, UWORD len)
{
    return serlink_send(pz->be_data, (const UBYTE *)h, sizeof(*h), data, len);
}

static void ser_log(struct PZUBase *pz, const UBYTE *text, UWORD len)
{
    serlink_send(pz->be_data, (const UBYTE *)"LO", 2, text, len);
}

static LONG ser_recv(struct PZUBase *pz, struct PzuRep *rep, UBYTE *data, UWORD max)
{
    UBYTE *dg;
    LONG n;
    UWORD actual;

    while ((n = serlink_recv(pz->be_data, &dg)) > 0) {
        if (n < (LONG)sizeof(*rep))
            continue;
        CopyMem(dg, rep, sizeof(*rep));
        if (rep->magic != PZU_MAGIC)
            continue;
        actual = rep->actual;
        if (actual > n - sizeof(*rep))
            actual = n - sizeof(*rep);
        if (actual > max)
            actual = max;
        if (actual)
            CopyMem(dg + sizeof(*rep), data, actual);
        rep->actual = actual;
        return 1;
    }
    return 0;
}

const struct Backend backend_ser = {
    "ser", ser_open, ser_close, ser_wait, ser_send, ser_recv, ser_log,
};
