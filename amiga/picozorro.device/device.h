/*
 * picozorro.device: SANA-II driver for the PicoZorro network card.
 *
 * Structure follows ZZ9000Net.device (MNT Research GmbH / Henryk Richter /
 * Dimitris Panokostas, https://github.com/BlitterStudio/zz9000-drivers):
 * RomTag + autoinit tables, one process that does the hardware work, an
 * interrupt server that only signals it, RX staged through a RAM buffer.
 * The code is a new implementation.
 */
#ifndef PZ_DEVICE_H
#define PZ_DEVICE_H

#include "../common/compiler.h"
#include <exec/types.h>
#include <exec/libraries.h>
#include <exec/devices.h>
#include <exec/io.h>
#include <exec/lists.h>
#include <exec/semaphores.h>
#include <exec/interrupts.h>
#include <dos/dos.h>
#include <devices/sana2.h>
#include <devices/timer.h>

#define DEVICE_NAME     "picozorro.device"
#define DEVICE_VERSION  1
#define DEVICE_REVISION 0
#define DEVICE_DATE     "23.9.2026"

#define ETH_ALEN      6
#define ETH_HLEN      14
#define ETH_MTU       1500
#define ETH_FRAME_MAX (ETH_HLEN + ETH_MTU) /* 1514, no FCS */
#define BUF_SIZE      1536

/* Buffer management hooks as the protocol stack hands them in (SANA-II
 * copybuff.doc): register arguments, callable from interrupts. */
typedef BOOL (*CopyFn)(REGPROTO(APTR to, a0), REGPROTO(APTR from, a1), REGPROTO(ULONG n, d0));

/* One OpenDevice() of unit 0: ios2_BufferManagement points here afterwards. */
struct Opener {
    struct MinNode node;
    CopyFn copy_to;        /* S2_CopyToBuff: into the stack's buffer */
    CopyFn copy_from;      /* S2_CopyFromBuff: out of the stack's buffer */
    struct Hook *filter;   /* S2_PacketFilter, may be NULL */
    struct MinList reads;  /* CMD_READ */
    struct MinList orphans;/* S2_READORPHAN */
    UBYTE flags;           /* SANA2OPF_MINE / SANA2OPF_PROM at open */
};

struct TypeStats {
    struct MinNode node;
    ULONG type;
    struct Sana2PacketTypeStats s;
};

#define MCAST_MAX 32
struct McastEntry {
    UBYTE addr[ETH_ALEN];
    UWORD refs;
};

struct PZBase;

/* The hardware, or what stands in for it. Called only from the device
 * process, except bps(), which S2_DEVICEQUERY calls in the caller's task
 * (quick command): it must not wait for anything. */
struct Backend {
    const char *name;
    LONG (*open)(struct PZBase *pz, const char *args);   /* 0 = ok */
    void (*close)(struct PZBase *pz);
    void (*get_mac)(struct PZBase *pz, UBYTE mac[ETH_ALEN]);
    LONG (*online)(struct PZBase *pz);
    void (*offline)(struct PZBase *pz);
    /* Send one raw frame (with Ethernet header, no FCS). 0 = ok. */
    LONG (*send)(struct PZBase *pz, const UBYTE *frame, ULONG len);
    /* Copy the next received raw frame into buf; returns its length,
     * 0 when none is waiting. */
    ULONG (*poll_rx)(struct PZBase *pz, UBYTE *buf);
    /* Receive filter: promiscuous, all multicast, or the table. */
    void (*set_filter)(struct PZBase *pz, BOOL promisc, BOOL all_multi,
                       const struct McastEntry *tab, UWORD n);
    ULONG (*bps)(struct PZBase *pz);
    /* Debug text, for a backend that owns the serial port (NULL: the log
     * goes out through exec's RawPutChar). */
    void (*log)(struct PZBase *pz, const UBYTE *text, UWORD len);
};
/* A backend that wants to wake the process when frames arrive sets
 * pz->rx_signal (a signal mask of the process) in open(); the process
 * calls poll_rx() until it returns 0 after every wake-up while online. */

extern const struct Backend backend_uae, backend_loop, backend_pz, backend_bp;

struct PZBase {
    struct Library lib;
    UWORD pad;
    BPTR seglist;
    struct Unit unit;             /* io_Unit of every opener */

    struct SignalSemaphore lock;  /* opener list, open/close */
    struct MinList openers;
    UWORD open_count;
    BOOL exclusive;               /* an opener holds SANA2OPF_MINE */

    BOOL filter_dirty;            /* PROM openers changed: process re-applies */
    struct Process *proc;
    struct MsgPort *port;         /* requests for the process */
    struct Task *opener_task;     /* waiting in Open for process startup */
    BYTE startup_error;
    ULONG rx_signal;              /* backend: frames may be waiting */

    /* unit 0 state, owned by the process */
    const struct Backend *be;
    char be_args[80];             /* ENV:PZNET */
    APTR be_data;
    BOOL be_open;                 /* backend opened (lazily, first command) */
    BOOL configured;
    BOOL online;
    UBYTE mac[ETH_ALEN];          /* current station address */
    UBYTE hw_mac[ETH_ALEN];       /* default (hardware) address */
    struct Sana2DeviceStats stats;
    struct MinList types;         /* struct TypeStats */
    struct MinList events;        /* S2_ONEVENT */
    struct McastEntry mcast[MCAST_MAX];
    UWORD mcast_n;
    UWORD prom_openers;

    struct Device *timer_base;
    struct timerequest *treq;

    UBYTE *rxbuf;                 /* BUF_SIZE, staging for RX */
    UBYTE *txbuf;                 /* BUF_SIZE, staging for TX */
};

extern struct ExecBase *SysBase;
extern struct DosLibrary *DOSBase;
extern struct Library *UtilityBase;
extern struct Device *TimerBase;
extern struct PZBase *PZ;

/* Debug build (make DEBUG=1): printf-style lines to the serial port via
 * RawDoFmt/RawPutChar, safe in any context. Use %ld/%lx (32-bit args).
 * With the backplane backend they queue and go out in LOG frames. */
#ifdef PZ_DEBUG
void pz_log(const char *fmt, ...);
void pz_log_flush(struct PZBase *pz);
#define D(x) pz_log x
#else
#define D(x)
#endif

/* device.c */
void pz_frame_received(struct PZBase *pz, UBYTE *frame, ULONG len);
void pz_event(struct PZBase *pz, ULONG events);
void pz_copy(const void *from, void *to, ULONG n);

#endif
