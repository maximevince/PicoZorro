/*
 * memcheck: walk exec's free memory lists and the task lists without
 * calling anything in exec that validates them (no AvailMem), to find what
 * trips AN_MemoryInsane (0100000C).
 *   memcheck [TASKADDR]     (hex; where that address lies)
 * Everything is copied into static tables under Forbid() and printed after
 * Permit(). Exit code 0 all OK, 5 warnings only, 20 on a violation.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/execbase.h>
#include <exec/tasks.h>
#include <dos/dos.h>
#include <dos/dosextens.h>
#include <proto/exec.h>
#include <proto/dos.h>

#define MAX_HDR    16
#define MAX_CHUNKS 4096
#define MAX_TASKS  128
#define DUMP_SPAN  64

enum {
    V_OK,
    V_RANGE,    /* chunk address outside [mh_Lower, mh_Upper) */
    V_ALIGN,    /* chunk address not a multiple of MEM_BLOCKSIZE */
    V_SIZE,     /* mc_Bytes zero, unaligned or past mh_Upper */
    V_ORDER,    /* mc_Next <= chunk: list does not ascend */
    V_OVERLAP,  /* mc_Next < chunk + mc_Bytes */
    V_LOOP,     /* more than MAX_CHUNKS chunks */
};

static const char *const vname[] = {
    "OK",
    "chunk outside [lower, upper)",
    "chunk not 8-byte aligned",
    "bad mc_Bytes (zero, unaligned or past upper)",
    "mc_Next <= chunk (not ascending)",
    "mc_Next overlaps chunk",
    "more than 4096 chunks (loop?)",
};

struct hdr {
    ULONG addr, name_ptr, lower, upper, free, first;
    LONG pri;
    UWORD attr;
    char name[32];
    ULONG nchunks, sum, largest;
    /* first violation */
    int viol;
    ULONG bad, bad_next, bad_bytes;
    int bad_read;               /* bad_next/bad_bytes valid */
    ULONG prev, prev_bytes;
    /* first adjacent-chunks warning */
    ULONG nadj, adj_chunk, adj_bytes;
    /* bytes around the offending chunk */
    ULONG dump_start, dump_len;
    UBYTE dump[2 * DUMP_SPAN];
    /* TASKADDR */
    int has_addr, addr_free;
    ULONG addr_chunk, addr_chunk_bytes;
};

struct tsk {
    ULONG addr, splower, spupper, spreg;
    LONG pri, tasknum;
    UBYTE type, state;
    char name[32], cmd[32];
    int running;
};

static struct hdr hdrs[MAX_HDR];
static struct tsk tasks[MAX_TASKS];
static int nhdr, nhdr_more, ntask, ntask_more;

static ULONG hex(const char *s)
{
    ULONG v = 0;
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X'))
        s += 2;
    else if (s[0] == '$')
        s++;
    for (; *s; s++) {
        char c = *s | 0x20;
        v = (v << 4) | (c >= 'a' ? c - 'a' + 10 : c - '0');
    }
    return v;
}

static void copystr(char *out, const char *s, int n)
{
    int i = 0;
    if (s)
        while (i < n - 1 && s[i]) {
            out[i] = s[i];
            i++;
        }
    out[i] = 0;
}

static void copybstr(char *out, BSTR b, int n)
{
    const UBYTE *s = (const UBYTE *)BADDR(b);
    int len = s ? s[0] : 0, i;
    if (len > n - 1)
        len = n - 1;
    for (i = 0; i < len; i++)
        out[i] = s[i + 1];
    out[i] = 0;
}

/* Under Forbid(): no allocation, no output. */
static void walk_header(struct MemHeader *mh, struct hdr *h, ULONG taddr, int want_addr)
{
    ULONG lower = (ULONG)mh->mh_Lower, upper = (ULONG)mh->mh_Upper;
    ULONG c = (ULONG)mh->mh_First, prev = 0, prev_bytes = 0;

    h->addr = (ULONG)mh;
    h->name_ptr = (ULONG)mh->mh_Node.ln_Name;
    copystr(h->name, mh->mh_Node.ln_Name, sizeof(h->name));
    h->pri = mh->mh_Node.ln_Pri;
    h->attr = mh->mh_Attributes;
    h->lower = lower;
    h->upper = upper;
    h->free = mh->mh_Free;
    h->first = c;
    h->has_addr = want_addr && taddr >= lower && taddr < upper;

    while (c) {
        ULONG next, bytes;

        if (h->nchunks >= MAX_CHUNKS) {
            h->viol = V_LOOP;
            break;
        }
        if (c < lower || c >= upper) {
            h->viol = V_RANGE;
            break;
        }
        if (c & 1) {            /* never read at an odd address */
            h->viol = V_ALIGN;
            break;
        }
        if (upper - c >= sizeof(struct MemChunk)) {
            h->bad_next = next = (ULONG)((struct MemChunk *)c)->mc_Next;
            h->bad_bytes = bytes = ((struct MemChunk *)c)->mc_Bytes;
            h->bad_read = 1;
        } else {
            h->viol = V_SIZE;   /* the chunk header itself sticks out */
            break;
        }
        if (c & MEM_BLOCKMASK) {
            h->viol = V_ALIGN;
            break;
        }
        if (bytes == 0 || (bytes & MEM_BLOCKMASK) || bytes > upper - c) {
            h->viol = V_SIZE;
            break;
        }
        if (next) {
            if (next <= c) {
                h->viol = V_ORDER;
                break;
            }
            if (next < c + bytes) {
                h->viol = V_OVERLAP;
                break;
            }
            if (next == c + bytes && h->nadj++ == 0) {
                h->adj_chunk = c;
                h->adj_bytes = bytes;
            }
        }
        h->nchunks++;
        h->sum += bytes;
        if (bytes > h->largest)
            h->largest = bytes;
        if (h->has_addr && taddr >= c && taddr - c < bytes) {
            h->addr_free = 1;
            h->addr_chunk = c;
            h->addr_chunk_bytes = bytes;
        }
        prev = c;
        prev_bytes = bytes;
        h->bad_read = 0;
        c = next;
    }

    if (h->viol) {
        ULONG s, e, i;
        h->bad = c;
        h->prev = prev;
        h->prev_bytes = prev_bytes;
        s = c - lower > DUMP_SPAN ? c - DUMP_SPAN : lower;
        e = upper - c > DUMP_SPAN ? c + DUMP_SPAN : upper;
        if (c < lower || c >= upper)
            s = e = 0;          /* wild pointer: nothing safe to show */
        h->dump_start = s;
        h->dump_len = e - s;
        for (i = 0; i < h->dump_len; i++)
            h->dump[i] = ((volatile UBYTE *)s)[i];
    }
}

static void grab_task(struct Task *t, int running)
{
    struct tsk *k;

    if (ntask >= MAX_TASKS) {
        ntask_more++;
        return;
    }
    k = &tasks[ntask++];
    k->addr = (ULONG)t;
    k->type = t->tc_Node.ln_Type;
    k->pri = t->tc_Node.ln_Pri;
    k->state = t->tc_State;
    copystr(k->name, t->tc_Node.ln_Name, sizeof(k->name));
    k->splower = (ULONG)t->tc_SPLower;
    k->spupper = (ULONG)t->tc_SPUpper;
    k->spreg = (ULONG)t->tc_SPReg;
    k->running = running;
    k->tasknum = -1;
    k->cmd[0] = 0;
    if (k->type == NT_PROCESS) {
        struct Process *p = (struct Process *)t;
        k->tasknum = p->pr_TaskNum;
        if (p->pr_CLI) {
            struct CommandLineInterface *cli = BADDR(p->pr_CLI);
            copybstr(k->cmd, cli->cli_CommandName, sizeof(k->cmd));
        }
    }
}

static void grab_list(struct List *l)
{
    struct Node *n;
    for (n = l->lh_Head; n->ln_Succ; n = n->ln_Succ)
        grab_task((struct Task *)n, 0);
}

static void dump(const struct hdr *h)
{
    ULONG off;
    char line[80];
    static const char hx[] = "0123456789abcdef";

    for (off = 0; off < h->dump_len; off += 16) {
        int i, p = 0;
        for (i = 0; i < 16; i++) {
            if (off + i < h->dump_len) {
                UBYTE b = h->dump[off + i];
                line[p++] = hx[b >> 4];
                line[p++] = hx[b & 15];
            } else {
                line[p++] = ' ';
                line[p++] = ' ';
            }
            line[p++] = ' ';
        }
        line[p++] = ' ';
        for (i = 0; i < 16 && off + i < h->dump_len; i++) {
            UBYTE b = h->dump[off + i];
            line[p++] = b >= 0x20 && b < 0x7f ? b : '.';
        }
        line[p] = 0;
        Printf("    %08lx: %s\n", h->dump_start + off, (ULONG)line);
    }
}

int main(int argc, char **argv)
{
    struct ExecBase *eb = SysBase;
    struct Node *n;
    ULONG taddr = 0;
    int want_addr = argc > 1, i, rc = RETURN_OK;

    if (want_addr)
        taddr = hex(argv[1]);

    Forbid();
    for (n = eb->MemList.lh_Head; n->ln_Succ; n = n->ln_Succ) {
        if (nhdr >= MAX_HDR) {
            nhdr_more++;
            continue;
        }
        walk_header((struct MemHeader *)n, &hdrs[nhdr++], taddr, want_addr);
    }
    grab_task(eb->ThisTask, 1);
    grab_list(&eb->TaskReady);
    grab_list(&eb->TaskWait);
    Permit();

    PutStr("Memory headers\n");
    for (i = 0; i < nhdr; i++) {
        struct hdr *h = &hdrs[i];
        Printf("header %08lx name %08lx \"%s\" pri %ld attr %04lx\n",
               h->addr, h->name_ptr, (ULONG)h->name, h->pri, (ULONG)h->attr);
        Printf("  lower %08lx upper %08lx free %08lx first %08lx\n",
               h->lower, h->upper, h->free, h->first);
        Printf("  chunks %ld sum %08lx largest %08lx\n", h->nchunks, h->sum, h->largest);
        if (h->nadj) {
            Printf("  WARNING %ld adjacent free chunk pairs (not merged), first %08lx size %08lx\n",
                   h->nadj, h->adj_chunk, h->adj_bytes);
            if (rc < RETURN_WARN)
                rc = RETURN_WARN;
        }
        if (h->viol) {
            Printf("  VIOLATION after %ld chunks: %s\n", h->nchunks, (ULONG)vname[h->viol]);
            if (h->bad_read)
                Printf("  chunk %08lx mc_Next %08lx mc_Bytes %08lx\n",
                       h->bad, h->bad_next, h->bad_bytes);
            else
                Printf("  chunk %08lx (not read)\n", h->bad);
            Printf("  previous chunk %08lx size %08lx\n", h->prev, h->prev_bytes);
            if (h->dump_len)
                dump(h);
            rc = RETURN_FAIL;
        } else if (h->sum != h->free) {
            Printf("  MISMATCH sum %08lx mh_Free %08lx difference %ld\n",
                   h->sum, h->free, (LONG)(h->free - h->sum));
            rc = RETURN_FAIL;
        } else {
            PutStr("  OK\n");
        }
    }
    if (nhdr_more)
        Printf("(%ld more headers not checked)\n", (LONG)nhdr_more);

    PutStr("Tasks\n");
    for (i = 0; i < ntask; i++) {
        struct tsk *k = &tasks[i];
        int stack = !k->running && (k->spreg < k->splower || k->spreg >= k->spupper);
        Printf("%08lx type %ld pri %ld state %ld sp %08lx [%08lx %08lx) cli %ld \"%s\" cmd \"%s\"%s%s\n",
               k->addr, (LONG)k->type, k->pri, (LONG)k->state, k->spreg, k->splower,
               k->spupper, k->tasknum, (ULONG)k->name, (ULONG)k->cmd,
               (ULONG)(k->running ? " RUNNING" : ""), (ULONG)(stack ? " STACK?" : ""));
    }
    if (ntask_more)
        Printf("(%ld more tasks not listed)\n", (LONG)ntask_more);

    if (want_addr) {
        int found = 0;
        for (i = 0; i < ntask; i++)
            if (tasks[i].addr == taddr)
                found = 1;
        if (found) {
            Printf("%08lx is a listed task\n", taddr);
        } else {
            Printf("%08lx is not a listed task\n", taddr);
            for (i = 0; i < nhdr; i++) {
                struct hdr *h = &hdrs[i];
                if (!h->has_addr)
                    continue;
                found = 1;
                Printf("  inside header %08lx \"%s\"\n", h->addr, (ULONG)h->name);
                if (h->addr_free)
                    Printf("  inside FREE chunk %08lx size %08lx\n",
                           h->addr_chunk, h->addr_chunk_bytes);
                else if (h->viol)
                    PutStr("  not in a free chunk seen before the violation\n");
                else
                    PutStr("  not in a free chunk (allocated)\n");
            }
            if (!found)
                PutStr("  not inside any memory header\n");
        }
    }
    return rc;
}
