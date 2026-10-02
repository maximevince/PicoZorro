/*
 * pzload: CPU load meter. AmigaOS 2.04+ (V37), plain 68000 code.
 *
 *   pzload [LOGFILE]      e.g. pzload RAM:pzload.log
 *
 * An idle task at priority -128 counts loop passes; it only runs when
 * nothing else wants the CPU. The count rate of an unloaded CPU is
 * calibrated at startup (the same loop under Forbid(), timed with
 * ReadEClock), so load = 1 - idle rate / calibrated rate. Start pzload
 * before the work you want to measure: interrupts during the calibration
 * inflate it. A later idle rate above the calibration raises it.
 *
 * A window shows the last 120 samples (one every 0.5 s) as a scrolling
 * bar graph, with the current load, mean and peak in the title. With a
 * LOGFILE, each sample is appended as "<ms> <load in 0.1 %>". Close
 * the window or Ctrl-C to quit.
 */
#include <exec/types.h>
#include <exec/memory.h>
#include <exec/tasks.h>
#include <devices/timer.h>
#include <dos/dos.h>
#include <intuition/intuition.h>
#include <graphics/rastport.h>
#include <proto/exec.h>
#include <proto/dos.h>
#include <proto/intuition.h>
#include <proto/graphics.h>
#include <proto/timer.h>
#include <clib/alib_protos.h>
#include <stdio.h>

#define SAMPLES 120
#define BAR     2
#define GRAPH_H 64
#define STACK   1024
#define CAL_N   100000

struct Device *TimerBase;
struct IntuitionBase *IntuitionBase;
struct GfxBase *GfxBase;

static volatile ULONG idle_count;

/* The idle loop and the calibration run the same code. */
static void __attribute__((noinline)) spin(ULONG n)
{
    while (n--)
        idle_count++;
}

static void idle_task(void)
{
    for (;;)
        spin(0x7fffffff);
}

static ULONG ediff(struct EClockVal *a, struct EClockVal *b)
{
    return b->ev_lo - a->ev_lo;
}

int main(int argc, char **argv)
{
    struct timerequest *tr;
    struct MsgPort *port;
    struct Task *task = NULL;
    APTR stack = NULL;
    struct Window *win = NULL;
    BPTR log = 0;
    struct EClockVal e0, e1, t_start;
    ULONG efreq, cal, last_count, sum = 0, n = 0, peak = 0;
    UBYTE hist[SAMPLES] = {0};
    char title[64];
    int rc = RETURN_FAIL;

    port = CreateMsgPort();
    tr = port ? (struct timerequest *)CreateIORequest(port, sizeof(*tr)) : NULL;
    if (!tr || OpenDevice((CONST_STRPTR)TIMERNAME, UNIT_ECLOCK, (struct IORequest *)tr, 0)) {
        PutStr("pzload: no timer.device\n");
        goto out_port;
    }
    TimerBase = tr->tr_node.io_Device;
    IntuitionBase = (struct IntuitionBase *)OpenLibrary("intuition.library", 37);
    GfxBase = (struct GfxBase *)OpenLibrary("graphics.library", 37);
    if (!IntuitionBase || !GfxBase)
        goto out_libs;

    /* Calibrate: passes per E-clock tick with the CPU to ourselves. */
    Forbid();
    efreq = ReadEClock(&e0);
    spin(CAL_N);
    ReadEClock(&e1);
    Permit();
    /* passes per second */
    cal = (ULONG)((unsigned long long)CAL_N * efreq / (ediff(&e0, &e1) ? ediff(&e0, &e1) : 1));

    if (argc > 1) {
        log = Open((CONST_STRPTR)argv[1], MODE_READWRITE);
        if (log)
            Seek(log, 0, OFFSET_END);
    }

    win = OpenWindowTags(NULL,
                         WA_Title, (ULONG)"pzload",
                         WA_InnerWidth, SAMPLES * BAR,
                         WA_InnerHeight, GRAPH_H,
                         WA_DragBar, TRUE, WA_DepthGadget, TRUE, WA_CloseGadget, TRUE,
                         WA_IDCMP, IDCMP_CLOSEWINDOW,
                         WA_RMBTrap, TRUE, WA_Activate, FALSE,
                         TAG_DONE);
    if (!win) {
        PutStr("pzload: no window\n");
        goto out_libs;
    }

    task = AllocMem(sizeof(struct Task), MEMF_PUBLIC | MEMF_CLEAR);
    stack = AllocMem(STACK, MEMF_CLEAR);
    if (!task || !stack)
        goto out_win;
    task->tc_Node.ln_Type = NT_TASK;
    task->tc_Node.ln_Pri = -128;
    task->tc_Node.ln_Name = "pzload idle";
    task->tc_SPLower = stack;
    task->tc_SPUpper = (UBYTE *)stack + STACK;
    task->tc_SPReg = task->tc_SPUpper;
    NewList(&task->tc_MemEntry);   /* RemTask frees what is on it */
    last_count = idle_count;
    ReadEClock(&e0);
    t_start = e0;
    AddTask(task, (APTR)idle_task, NULL);

    for (;;) {
        struct IntuiMessage *m;
        struct RastPort *rp = win->RPort;
        WORD x0 = win->BorderLeft, y0 = win->BorderTop;
        ULONG count, dt, idle, load, i;
        BOOL quit = FALSE;

        Delay(25);
        while ((m = (struct IntuiMessage *)GetMsg(win->UserPort))) {
            if (m->Class == IDCMP_CLOSEWINDOW)
                quit = TRUE;
            ReplyMsg((struct Message *)m);
        }
        if (quit || (SetSignal(0, 0) & SIGBREAKF_CTRL_C))
            break;

        count = idle_count;
        ReadEClock(&e1);
        dt = ediff(&e0, &e1);
        idle = (ULONG)((unsigned long long)(count - last_count) * efreq / (dt ? dt : 1));
        last_count = count;
        e0 = e1;
        if (idle > cal)
            cal = idle;
        load = 1000 - (ULONG)((unsigned long long)idle * 1000 / cal);   /* 0.1 % */
        sum += load;
        n++;
        if (load > peak)
            peak = load;

        for (i = 0; i < SAMPLES - 1; i++)
            hist[i] = hist[i + 1];
        hist[SAMPLES - 1] = (UBYTE)((load * GRAPH_H + 500) / 1000);
        SetAPen(rp, 0);
        RectFill(rp, x0, y0, x0 + SAMPLES * BAR - 1, y0 + GRAPH_H - 1);
        SetAPen(rp, 1);
        for (i = 1; i < 4; i++) {   /* 25 / 50 / 75 % */
            WORD y = y0 + GRAPH_H - i * GRAPH_H / 4;
            WORD x;
            for (x = 0; x < SAMPLES * BAR; x += 4)
                WritePixel(rp, x0 + x, y);
        }
        SetAPen(rp, 3);
        for (i = 0; i < SAMPLES; i++)
            if (hist[i])
                RectFill(rp, x0 + i * BAR, y0 + GRAPH_H - hist[i], x0 + i * BAR + BAR - 1, y0 + GRAPH_H - 1);

        sprintf(title, "pzload %lu%%  mean %lu%%  peak %lu%%",
                (load + 5) / 10, (sum / n + 5) / 10, (peak + 5) / 10);
        SetWindowTitles(win, (UBYTE *)title, (UBYTE *)~0);

        if (log) {
            char line[32];
            ULONG ms = (ULONG)((unsigned long long)ediff(&t_start, &e1) * 1000 / efreq);
            int len = sprintf(line, "%lu %lu\n", ms, load);
            Write(log, line, len);
        }
    }
    rc = RETURN_OK;

    Forbid();
    RemTask(task);
    Permit();
out_win:
    if (stack)
        FreeMem(stack, STACK);
    if (task)
        FreeMem(task, sizeof(struct Task));
    CloseWindow(win);
out_libs:
    if (log)
        Close(log);
    if (GfxBase)
        CloseLibrary((struct Library *)GfxBase);
    if (IntuitionBase)
        CloseLibrary((struct Library *)IntuitionBase);
    if (TimerBase)
        CloseDevice((struct IORequest *)tr);
out_port:
    if (tr)
        DeleteIORequest((struct IORequest *)tr);
    if (port)
        DeleteMsgPort(port);
    return rc;
}
