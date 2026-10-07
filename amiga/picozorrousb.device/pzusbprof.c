/*
 * pzusbprof: the interrupt IN timing of a picozorrousb.device built with
 * `make PROF=1` (struct PzuProf in device.h), per report:
 *
 *   recv_io   recv() of the completion record (T0..T1)
 *   handle    after recv .. just before our ReplyMsg (T1..T2)
 *   replymsg  ReplyMsg itself, when Poseidon did not resubmit inside it
 *   poseidon  our ReplyMsg .. the next DevBeginIO for that endpoint (T2..T3)
 *   wake      DevBeginIO .. our process dispatches it (T3..T4)
 *   send      dispatch .. the request is in the card's queue (T4..T5)
 *
 *   pzusbprof          print
 *   pzusbprof reset    zero the counters
 *
 * Built with -DPZU_PROF whatever PROF says (Makefile): the prof block is
 * in struct PZUBase only then. prof_magic tells a device without it.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/io.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <stdio.h>
#include <string.h>
#include "device.h"

static ULONG hz;

/* ticks -> tenths of a microsecond */
static ULONG us10(ULONG ticks, ULONG n)
{
    if (!n || !hz)
        return 0;
    return (ULONG)((unsigned long long)ticks * 10000000ULL / ((unsigned long long)hz * n));
}

static ULONG line(const char *name, ULONG n, ULONG ticks)
{
    ULONG a = us10(ticks, n);
    printf("  %-9s %8lu  avg %6lu.%lu us  total %8lu ms\n", name, n, a / 10, a % 10,
           (ULONG)((unsigned long long)ticks * 1000ULL / (hz ? hz : 1)));
    return a;
}

int main(int argc, char **argv)
{
    struct MsgPort *port;
    struct IOUsbHWReq *iou;
    struct PZUBase *pz;
    struct PzuProf p;
    int rc = 0;

    port = CreateMsgPort();
    iou = port ? (struct IOUsbHWReq *)CreateIORequest(port, sizeof(*iou)) : NULL;
    if (!iou || OpenDevice(DEVICE_NAME, 0, (struct IORequest *)iou, 0) != 0) {
        printf("pzusbprof: cannot open " DEVICE_NAME "\n");
        if (iou)
            DeleteIORequest((struct IORequest *)iou);
        if (port)
            DeleteMsgPort(port);
        return 20;
    }
    pz = (struct PZUBase *)iou->iouh_Req.io_Device;
    if (pz->lib.lib_PosSize < sizeof(struct PZUBase) || pz->prof.prof_magic != PROF_MAGIC) {
        printf("pzusbprof: " DEVICE_NAME " built without PROF=1 (make PROF=1)\n");
        rc = 10;
        goto out;
    }
    if (argc > 1 && strcmp(argv[1], "reset") == 0) {
        Forbid();
        memset((UBYTE *)&pz->prof + 2 * sizeof(ULONG), 0, sizeof(pz->prof) - 2 * sizeof(ULONG)); /* keeps magic, eclock_hz */
        Permit();
        printf("pzusbprof: reset\n");
        goto out;
    }
    Forbid();
    CopyMem(&pz->prof, &p, sizeof(p));
    Permit();
    hz = p.eclock_hz;
    printf("E-clock %lu Hz; proc loops %lu, of them with no message and no record %lu\n", hz, p.n_loops,
           p.n_wakes_empty);
    printf("interrupt IN reports %lu; Poseidon resubmits matched %lu (%lu inside our ReplyMsg)\n", p.n,
           p.n_match, p.n_preempt);
    {
        ULONG drv = 0, pos;
        drv += line("recv_io", p.n, p.t_recv_io);
        drv += line("handle", p.n, p.t_handle);
        drv += line("replymsg", p.n_replymsg, p.t_replymsg);
        pos = line("poseidon", p.n_match, p.t_poseidon);
        drv += line("wake", p.n_wake, p.t_wake);
        drv += line("send", p.n_send, p.t_send);
        printf("  per report: driver %lu.%lu us (wake includes the scheduler), Poseidon %lu.%lu us\n", drv / 10,
               drv % 10, pos / 10, pos % 10);
    }
out:
    CloseDevice((struct IORequest *)iou);
    DeleteIORequest((struct IORequest *)iou);
    DeleteMsgPort(port);
    return rc;
}
