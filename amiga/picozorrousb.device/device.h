/*
 * picozorrousb.device: Poseidon USB hardware driver for PicoZorro.
 *
 * Legacy IOUsbHWReq interface (Poseidon's usbhardware.doc). One process
 * per unit does everything: root-hub emulation, transfer scheduling, the
 * transport. Transports ("backends"): `pz` = the card's USB register window
 * on the Zorro bus (backend_pz.c); `uae` = the UDP tunnel to a module on
 * the bench over bsdsocket.library; `ser` = the same tunnel as COBS frames
 * over serial.device; `bp` = the register window over the UART backplane.
 */
#ifndef PZU_DEVICE_H
#define PZU_DEVICE_H

#include "../common/compiler.h"
#include <exec/types.h>
#include <exec/libraries.h>
#include <exec/devices.h>
#include <exec/io.h>
#include <exec/lists.h>
#include <exec/semaphores.h>
#include <dos/dos.h>
#include <devices/timer.h>
#include <devices/usbhardware.h>

#define DEVICE_NAME     "picozorrousb.device"
#define DEVICE_VERSION  1
#define DEVICE_REVISION 0
#define DEVICE_DATE     "25.9.2026"

/* ---- tunnel records (docs/REGISTERS-USB.md), 68000 layout = wire layout ---- */

#define PZU_MAGIC     0x5055
#define PZU_VERSION   1
#define PZU_MAX_DATA  1024

#define OP_PING         0
#define OP_PORT_STATUS  1
#define OP_PORT_RESET   2
#define OP_XFER         3
#define OP_ABORT        4
#define OP_FORGET       5
#define OP_RESET_TOGGLE 6
#define OP_XFER_DATA    7    /* the next record of a streamed bulk OUT */

#define XT_CONTROL   0
#define XT_ISO       1
#define XT_BULK      2
#define XT_INTERRUPT 3

#define XF_LOWSPEED  1
#define XF_PRE       2
#define XF_NOSHORT   4
#define XF_ALLOWRUNT 8
#define XF_STREAM    16      /* bulk IN / OUT: setup[0..3] = total, `length` per record */

#define RF_MORE      1       /* reply flags: another record of this request follows */
#define FEAT_STREAM_IN  1    /* PORT_STATUS payload byte 2 */
#define FEAT_STREAM_OUT 2
#define FEAT_INTERVAL   4    /* request bytes 26-27: poll interval of an interrupt pipe */

#define ST_ABORTED 15

struct PzuReq {              /* 28 bytes */
    UWORD magic;
    UBYTE version, op;
    UWORD seq;
    UBYTE addr, ep, kind, flags;
    UWORD mps, timeout_ms, length;
    UBYTE hub_addr, hub_port;
    UBYTE setup[8];
    UWORD interval;          /* interrupt: poll interval in ms (0 = 1) */
};

struct PzuRep {              /* 12 bytes */
    UWORD magic;
    UBYTE version, op;
    UWORD seq;
    UBYTE status, flags;
    UWORD actual, pad2;
};

/* ---- driver ---- */

struct PZUBase;

/* The transport. Called only from the device process. */
struct Backend {
    const char *name;
    LONG (*open)(struct PZUBase *pz, const char *args);   /* 0 = ok */
    void (*close)(struct PZUBase *pz);
    /* Sleep until a datagram arrives, a signal in `sigs` fires, or
     * `ms` pass. Returns the signals received; sets pz->be_readable. */
    ULONG (*wait)(struct PZUBase *pz, ULONG sigs, ULONG ms);
    LONG (*send)(struct PZUBase *pz, const struct PzuReq *h, const UBYTE *data, UWORD len);
    /* Next reply, 0 when none is waiting. `data` gets at most `max` bytes. */
    LONG (*recv)(struct PZUBase *pz, struct PzuRep *rep, UBYTE *data, UWORD max);
    /* Debug text, for a backend that owns the serial port (NULL: the log
     * goes out through exec's RawPutChar). */
    void (*log)(struct PZUBase *pz, const UBYTE *text, UWORD len);
    /* No record is ever lost (the register window): a bulk transfer may
     * be streamed, a record after the other without one request each. */
    BOOL lossless;
};

extern const struct Backend backend_uae;
extern const struct Backend backend_ser;
extern const struct Backend backend_bp;
extern const struct Backend backend_pz;

/* In-flight tunnel request. `iou` is NULL for the driver's own requests
 * (root port polling). */
#define SLOTS 16
struct Slot {
    struct IOUsbHWReq *iou;
    UBYTE op;                /* OP_* sent */
    UBYTE key_addr, key_ep;  /* endpoint the slot occupies (ep with dir bit) */
    UBYTE aborted;           /* reply to be ignored */
    UBYTE stream;            /* SS_*: a streamed bulk IN or OUT, records until one without RF_MORE */
    UBYTE out_unacked;       /* streamed OUT: records sent, not acknowledged yet (at most 2) */
    UWORD timeout_ms;        /* per record, as sent */
    UWORD seq;
    UWORD chunk;             /* bytes asked for in this request (streams: per record) */
    ULONG off;               /* offset of the chunk in iouh_Data */
    ULONG out_sent;          /* streamed OUT: offset of the next record to send */
    ULONG out_acked;         /* streamed OUT: bytes up to here written to the device */
    ULONG deadline;          /* ms clock: give up waiting for the reply */
};
#define SS_IN  1
#define SS_OUT 2

#ifdef PZU_PROF
/* make PROF=1: where the time per interrupt IN report goes (pzusbprof).
 * E-clock ticks (ev_lo), sums modulo 2^32. Per (addr, ep), direct-mapped:
 * T2 = our ReplyMsg, T3 = the next DevBeginIO, T4 = dispatch, T5 = sent. */
#define PROF_MAGIC 0x50524f46        /* 'PROF' */
#define PROF_EPS   8
#define PS_NONE     0
#define PS_REPLYING 1                /* in ReplyMsg; t2pre = just before it */
#define PS_REPLIED  2                /* t2 = after ReplyMsg */
#define PS_BEGUN    3                /* t3 valid */
#define PS_DISP     4                /* t4 valid */
struct PzuProfEp {
    UBYTE addr, ep;
    volatile UBYTE state;            /* PS_*; DevBeginIO runs in the caller's task */
    UBYTE pad;
    ULONG t2pre, t2, t3, t4;
};
struct PzuProf {
    ULONG prof_magic;
    ULONG eclock_hz;
    ULONG n;                         /* interrupt IN reports replied */
    ULONG t_recv_io;                 /* recv() of the completion record */
    ULONG t_handle;                  /* after recv .. just before ReplyMsg */
    ULONG n_replymsg, t_replymsg;    /* ReplyMsg itself, when not preempted in it */
    ULONG n_match, t_poseidon;       /* our ReplyMsg .. next DevBeginIO, same (addr, ep) */
    ULONG n_preempt;                 /* of n_match: DevBeginIO came during our ReplyMsg */
    ULONG n_wake, t_wake;            /* DevBeginIO .. top of dispatch */
    ULONG n_send, t_send;            /* dispatch .. after be->send in start_chunk */
    ULONG n_loops, n_wakes_empty;    /* proc_main loop; rounds with no message and no record */
    ULONG t0, t1;                    /* process: around the recv() of the current record */
    struct PzuProfEp ep[PROF_EPS];
};
#endif

struct PZUBase {
    struct Library lib;
    UWORD pad;
    BPTR seglist;
    struct Unit unit;

    struct SignalSemaphore lock;   /* open/close, abort marks */
    UWORD open_count;
    struct Process *proc;
    struct MsgPort *port;          /* requests for the process */
    struct Task *opener_task;
    BYTE startup_error;

    /* process-owned from here on */
    const struct Backend *be;
    char be_args[80];              /* ENV:PZUSB */
    APTR be_data;
    BOOL be_open;
    BOOL be_readable;
    BOOL be_auto;                  /* no ENV:PZUSB: pz, else uae */
    struct Device *timer_base;
    struct timerequest *treq;
    UWORD state;                   /* UHSF_* */

    /* root hub emulation: one port = the module's root port */
    UBYTE root_addr;
    UBYTE root_config;
    UWORD port_status;             /* UPSF_* */
    UWORD port_change;             /* UPSF_* of the changed bits */
    BOOL port_connected;           /* last PORT_STATUS from the module */
    UBYTE port_speed;              /* 0 none, 1 LS, 2 FS */
    UBYTE features;                /* FEAT_* of the last PORT_STATUS reply */
    BOOL stream_ok;                /* the backend may stream (lossless, or "stream" in ENV:PZUSB) */
    BOOL stream_out_ok;            /* bulk OUT too ("nostreamout" in ENV:PZUSB: not) */
    BOOL poll1;                    /* "poll1" in ENV:PZUSB: every interrupt pipe polled every ms */
    UWORD minpoll;                 /* floor for the interrupt poll interval, ms ("minpoll=N", 10) */
    struct MinList root_int;       /* pending UHCMD_INTXFER on root ep 1 */
    ULONG next_poll;               /* ms clock of the next PORT_STATUS */
    struct IOUsbHWReq *reset_iou;  /* SetPortFeature(PORT_RESET) waiting for the module */

    struct MinList waiting;        /* transfer requests not yet in flight, in order */
    struct Slot slots[SLOTS];
    UWORD seq;
    UBYTE *buf;                    /* PZU_MAX_DATA staging */

    ULONG n_xfers, n_errors, n_retries;
#ifdef PZU_PROF
    struct PzuProf prof;           /* last: pzusbprof reads it (built with -DPZU_PROF too) */
#endif
};

extern struct ExecBase *SysBase;
extern struct DosLibrary *DOSBase;
extern struct Library *UtilityBase;
extern struct Device *TimerBase;
extern struct PZUBase *PZU;

/* Debug build (make DEBUG=1): lines on the serial port, or in LOG frames
 * when the tunnel itself runs over the serial port. %ld/%lx only. */
#ifdef PZU_DEBUG
void pzu_log(const char *fmt, ...);
void pzu_log_flush(struct PZUBase *pz);
#define D(x) pzu_log x
#else
#define D(x)
#endif

ULONG pzu_now(struct PZUBase *pz);   /* ms clock */

#endif
