/*
 * picozorrousb.device, backend `uae`: the tunnel records as UDP datagrams
 * to a module on the bench, over bsdsocket.library. Used under FS-UAE,
 * where the emulator's bsdsocket emulation carries the datagrams to the PC
 * and the PC's network to the module. Works on a real Amiga with a TCP/IP
 * stack too.
 *
 * ENV:PZUSB = "uae [a.b.c.d [port]]", default 10.42.0.2 9995.
 */
#include <proto/exec.h>
#include <sys/ioctl.h>
#include <utility/tagitem.h>
#include <sys/socket.h>
#include <netinet/in.h>
#ifdef __VBCC__
#include <inline/bsdsocket_protos.h>
#else
#include <proto/bsdsocket.h>
#endif
#include "device.h"

struct Library *SocketBase;

struct Uae {
    LONG fd;
    struct sockaddr_in to;
    UBYTE rx[sizeof(struct PzuRep) + PZU_MAX_DATA];
    UBYTE tx[sizeof(struct PzuReq) + PZU_MAX_DATA];
};

static ULONG parse_ip(const char *s, const char **end)
{
    ULONG ip = 0;
    int part = 0, n = 0;
    while (*s) {
        if (*s >= '0' && *s <= '9') {
            n = n * 10 + (*s - '0');
        } else if (*s == '.') {
            ip = (ip << 8) | (n & 255);
            n = 0;
            part++;
        } else {
            break;
        }
        s++;
    }
    *end = s;
    return part == 3 ? ((ip << 8) | (n & 255)) : 0;
}

static LONG uae_open(struct PZUBase *pz, const char *args)
{
    struct Uae *u;
    const char *p = args;
    ULONG ip = 0, port = 0;
    LONG one = 1;

    SocketBase = OpenLibrary("bsdsocket.library", 4);
    if (!SocketBase) {
        D(("uae: no bsdsocket.library"));
        return -1;
    }
    u = AllocVec(sizeof(*u), MEMF_PUBLIC | MEMF_CLEAR);
    if (!u)
        return -1;
    pz->be_data = u;

    /* "uae 10.42.0.2 9995" */
    while (*p && *p != ' ')
        p++;
    while (*p == ' ')
        p++;
    if (*p)
        ip = parse_ip(p, &p);
    while (*p == ' ')
        p++;
    while (*p >= '0' && *p <= '9')
        port = port * 10 + (*p++ - '0');
    if (!ip)
        ip = (10UL << 24) | (42UL << 16) | 2;
    if (!port)
        port = 9995;

    u->fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (u->fd < 0) {
        D(("uae: socket() failed"));
        return -1;
    }
    IoctlSocket(u->fd, FIONBIO, (char *)&one);
    u->to.sin_len = sizeof(u->to);
    u->to.sin_family = AF_INET;
    u->to.sin_port = (UWORD)port;      /* 68000 is big-endian: network order */
    u->to.sin_addr.s_addr = ip;
    D(("uae: socket %ld to %lx:%ld", u->fd, ip, port));
    return 0;
}

static void uae_close(struct PZUBase *pz)
{
    struct Uae *u = pz->be_data;
    if (u) {
        if (u->fd >= 0)
            CloseSocket(u->fd);
        FreeVec(u);
    }
    pz->be_data = NULL;
    if (SocketBase)
        CloseLibrary(SocketBase);
    SocketBase = NULL;
}

static ULONG uae_wait(struct PZUBase *pz, ULONG sigs, ULONG ms)
{
    struct Uae *u = pz->be_data;
    fd_set rfds;
    struct timeval tv;
    ULONG mask = sigs;
    LONG n;

    FD_ZERO(&rfds);
    FD_SET(u->fd, &rfds);
    tv.tv_secs = ms / 1000;
    tv.tv_micro = (ms % 1000) * 1000;
    n = WaitSelect(u->fd + 1, &rfds, NULL, NULL, (struct __timeval *)&tv, &mask);
    pz->be_readable = (n > 0) && FD_ISSET(u->fd, &rfds);
    /* `mask` now holds the signals that arrived (0 when the socket or the
     * timeout ended the wait). */
    return mask;
}

static LONG uae_send(struct PZUBase *pz, const struct PzuReq *h, const UBYTE *data, UWORD len)
{
    struct Uae *u = pz->be_data;
    CopyMem((APTR)h, u->tx, sizeof(*h));
    if (len)
        CopyMem((APTR)data, u->tx + sizeof(*h), len);
    return sendto(u->fd, u->tx, sizeof(*h) + len, 0, (struct sockaddr *)&u->to, sizeof(u->to)) < 0 ? -1 : 0;
}

static LONG uae_recv(struct PZUBase *pz, struct PzuRep *rep, UBYTE *data, UWORD max)
{
    struct Uae *u = pz->be_data;
    struct sockaddr_in from;
    socklen_t fromlen = sizeof(from);
    LONG n = recvfrom(u->fd, u->rx, sizeof(u->rx), 0, (struct sockaddr *)&from, &fromlen);
    UWORD actual;

    if (n < (LONG)sizeof(*rep))
        return 0;
    CopyMem(u->rx, rep, sizeof(*rep));
    if (rep->magic != PZU_MAGIC)
        return 0;
    actual = rep->actual;
    if (actual > n - sizeof(*rep))
        actual = n - sizeof(*rep);
    if (actual > max)
        actual = max;
    if (actual)
        CopyMem(u->rx + sizeof(*rep), data, actual);
    rep->actual = actual;
    return 1;
}

const struct Backend backend_uae = {
    "uae", uae_open, uae_close, uae_wait, uae_send, uae_recv, NULL,
};
