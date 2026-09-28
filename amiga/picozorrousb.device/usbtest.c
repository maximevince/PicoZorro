/*
 * usbtest: exercise picozorrousb.device without Poseidon.
 *
 * Opens the device, queries it, resets the bus, reads the root hub's
 * descriptors, powers its port, waits for a device, resets the port,
 * enumerates whatever sits there (address 2) and prints its device
 * descriptor. Same requests Poseidon's hub.class sends, spelled out.
 *
 *   usbtest            (ENV:PZUSB selects the transport; unset: the card)
 */
#include <proto/exec.h>
#include <proto/dos.h>
#include <exec/errors.h>
#include <exec/memory.h>
#include <devices/usbhardware.h>
#include <devices/usb_hub.h>
#include <utility/tagitem.h>
#include <stdio.h>
#include <string.h>

static struct MsgPort *port;
static struct IOUsbHWReq *iou;
static UBYTE *buf;

static const char *errname(BYTE e)
{
    switch (e) {
    case 0: return "ok";
    case UHIOERR_USBOFFLINE: return "USB offline";
    case UHIOERR_NAK: return "NAK";
    case UHIOERR_HOSTERROR: return "host error";
    case UHIOERR_STALL: return "stall";
    case UHIOERR_TIMEOUT: return "timeout";
    case UHIOERR_OVERFLOW: return "overflow";
    case UHIOERR_CRCERROR: return "CRC";
    case UHIOERR_RUNTPACKET: return "runt packet";
    case UHIOERR_NAKTIMEOUT: return "NAK timeout";
    case UHIOERR_BADPARAMS: return "bad params";
    case UHIOERR_BABBLE: return "babble";
    case IOERR_ABORTED: return "aborted";
    case IOERR_NOCMD: return "no such command";
    }
    return "?";
}

static void clear(void)
{
    memset((UBYTE *)iou + sizeof(struct Message), 0, sizeof(*iou) - sizeof(struct Message));
    iou->iouh_Req.io_Message.mn_ReplyPort = port;
    iou->iouh_Req.io_Device = iou->iouh_Req.io_Device; /* kept */
}

static BYTE ctrl(UWORD addr, UBYTE bm, UBYTE br, UWORD wv, UWORD wi, UWORD wl, UWORD mps, UWORD flags)
{
    struct Device *dev = iou->iouh_Req.io_Device;
    struct Unit *unit = iou->iouh_Req.io_Unit;
    clear();
    iou->iouh_Req.io_Device = dev;
    iou->iouh_Req.io_Unit = unit;
    iou->iouh_Req.io_Command = UHCMD_CONTROLXFER;
    iou->iouh_DevAddr = addr;
    iou->iouh_Endpoint = 0;
    iou->iouh_MaxPktSize = mps;
    iou->iouh_Flags = flags | UHFF_NAKTIMEOUT;
    iou->iouh_NakTimeout = 1000;
    iou->iouh_Data = buf;
    iou->iouh_Length = wl;
    iou->iouh_SetupData.bmRequestType = bm;
    iou->iouh_SetupData.bRequest = br;
    iou->iouh_SetupData.wValue = (wv << 8) | (wv >> 8);
    iou->iouh_SetupData.wIndex = (wi << 8) | (wi >> 8);
    iou->iouh_SetupData.wLength = (wl << 8) | (wl >> 8);
    DoIO((struct IORequest *)iou);
    return iou->iouh_Req.io_Error;
}

static void hex(const UBYTE *p, ULONG n)
{
    ULONG i;
    for (i = 0; i < n; i++)
        printf("%02x", p[i]);
}

static int step(const char *what, BYTE err)
{
    printf("  %-44s %s", what, errname(err));
    if (err == 0 && iou->iouh_Actual)
        printf("  "), hex(buf, iou->iouh_Actual > 32 ? 32 : iou->iouh_Actual);
    printf("\n");
    return err == 0;
}

int main(void)
{
    struct Device *dev;
    struct Unit *unit;
    UWORD status = 0, change = 0, flags = 0, mps0 = 8;
    ULONG state = 0;
    STRPTR manu = NULL, prod = NULL;
    struct TagItem tags[] = {
        {UHA_State, (ULONG)&state}, {UHA_Manufacturer, (ULONG)&manu}, {UHA_ProductName, (ULONG)&prod}, {TAG_DONE, 0}};
    int i, rc = 20;

    port = CreateMsgPort();
    iou = (struct IOUsbHWReq *)CreateIORequest(port, sizeof(struct IOUsbHWReq));
    buf = AllocVec(1024, MEMF_PUBLIC | MEMF_CLEAR);
    if (!port || !iou || !buf)
        return 20;
    if (OpenDevice("picozorrousb.device", 0, (struct IORequest *)iou, 0) != 0) {
        printf("usbtest: cannot open picozorrousb.device\n");
        return 20;
    }
    dev = iou->iouh_Req.io_Device;
    unit = iou->iouh_Req.io_Unit;

    clear();
    iou->iouh_Req.io_Device = dev;
    iou->iouh_Req.io_Unit = unit;
    iou->iouh_Req.io_Command = UHCMD_QUERYDEVICE;
    iou->iouh_Data = tags;
    DoIO((struct IORequest *)iou);
    printf("query: %s, %ld tags, state %lx\n", errname(iou->iouh_Req.io_Error), iou->iouh_Actual, iou->iouh_State);
    for (i = 0; tags[i].ti_Tag != TAG_DONE; i++)
        printf("  tag %lx = %lx\n", tags[i].ti_Tag, tags[i].ti_Data);

    clear();
    iou->iouh_Req.io_Device = dev;
    iou->iouh_Req.io_Unit = unit;
    iou->iouh_Req.io_Command = UHCMD_USBRESET;
    DoIO((struct IORequest *)iou);
    printf("USBRESET: %s, state %lx\n", errname(iou->iouh_Req.io_Error), (ULONG)iou->iouh_State);
    if (iou->iouh_Req.io_Error)
        goto out;

    printf("root hub (address 0):\n");
    if (!step("GET_DESCRIPTOR(device)", ctrl(0, 0x80, 6, 0x0100, 0, 18, 8, 0)))
        goto out;
    step("SET_ADDRESS(1)", ctrl(0, 0x00, 5, 1, 0, 0, 8, 0));
    step("GET_DESCRIPTOR(config)", ctrl(1, 0x80, 6, 0x0200, 0, 25, 8, 0));
    step("SET_CONFIGURATION(1)", ctrl(1, 0x00, 9, 1, 0, 0, 8, 0));
    step("GET_HUB_DESCRIPTOR", ctrl(1, 0xa0, 6, 0x2900, 0, 9, 8, 0));
    step("SetPortFeature(PORT_POWER)", ctrl(1, 0x23, 3, UFS_PORT_POWER, 1, 0, 8, 0));
    for (i = 0; i < 40; i++) {
        if (ctrl(1, 0xa3, 0, 0, 1, 4, 8, 0) != 0)
            break;
        status = buf[0] | (buf[1] << 8);
        change = buf[2] | (buf[3] << 8);
        if (status & UPSF_PORT_CONNECTION)
            break;
        Delay(12);
    }
    printf("  port 1: status %04x change %04x%s\n", status, change,
           (status & UPSF_PORT_CONNECTION) ? "" : " (nothing plugged into the module)");
    if (!(status & UPSF_PORT_CONNECTION))
        goto out;
    step("ClearPortFeature(C_PORT_CONNECTION)", ctrl(1, 0x23, 1, UFS_C_PORT_CONNECTION, 1, 0, 8, 0));
    if (!step("SetPortFeature(PORT_RESET)", ctrl(1, 0x23, 3, UFS_PORT_RESET, 1, 0, 8, 0)))
        goto out;

    /* Port-change interrupt endpoint: one byte, bit 1 = port 1 changed. */
    clear();
    iou->iouh_Req.io_Device = dev;
    iou->iouh_Req.io_Unit = unit;
    iou->iouh_Req.io_Command = UHCMD_INTXFER;
    iou->iouh_DevAddr = 1;
    iou->iouh_Endpoint = 1;
    iou->iouh_Dir = UHDIR_IN;
    iou->iouh_MaxPktSize = 1;
    iou->iouh_Interval = 255;
    iou->iouh_Flags = UHFF_NAKTIMEOUT | UHFF_ALLOWRUNTPKTS;
    iou->iouh_NakTimeout = 3000;
    iou->iouh_Data = buf;
    iou->iouh_Length = 1;
    DoIO((struct IORequest *)iou);
    step("INT ep1 (port change bitmap)", iou->iouh_Req.io_Error);

    ctrl(1, 0xa3, 0, 0, 1, 4, 8, 0);
    status = buf[0] | (buf[1] << 8);
    change = buf[2] | (buf[3] << 8);
    printf("  port 1 after reset: status %04x change %04x -> %s\n", status, change,
           (status & UPSF_PORT_LOW_SPEED) ? "low speed" : "full speed");
    step("ClearPortFeature(C_PORT_RESET)", ctrl(1, 0x23, 1, UFS_C_PORT_RESET, 1, 0, 8, 0));
    if (status & UPSF_PORT_LOW_SPEED)
        flags = UHFF_LOWSPEED;

    printf("device on the root port:\n");
    if (!step("addr 0: GET_DESCRIPTOR(device, 8)", ctrl(0, 0x80, 6, 0x0100, 0, 8, 8, flags)))
        goto out;
    mps0 = buf[7];
    if (!step("addr 0: SET_ADDRESS(2)", ctrl(0, 0x00, 5, 2, 0, 0, mps0, flags)))
        goto out;
    Delay(2);
    if (!step("addr 2: GET_DESCRIPTOR(device, 18)", ctrl(2, 0x80, 6, 0x0100, 0, 18, mps0, flags)))
        goto out;
    printf("  VID:PID %02x%02x:%02x%02x class %02x\n", buf[9], buf[8], buf[11], buf[10], buf[4]);
    if (step("addr 2: GET_DESCRIPTOR(config, 9)", ctrl(2, 0x80, 6, 0x0200, 0, 9, mps0, flags))) {
        UWORD total = buf[2] | (buf[3] << 8);
        step("addr 2: GET_DESCRIPTOR(config, all)", ctrl(2, 0x80, 6, 0x0200, 0, total, mps0, flags));
    }
    if (buf[4] == 9 || iou->iouh_Actual > 5) {
        step("addr 2: SET_CONFIGURATION(1)", ctrl(2, 0x00, 9, 1, 0, 0, mps0, flags));
    }
    rc = 0;
out:
    CloseDevice((struct IORequest *)iou);
    DeleteIORequest((struct IORequest *)iou);
    DeleteMsgPort(port);
    FreeVec(buf);
    printf("usbtest: %s\n", rc == 0 ? "PASS" : "FAIL");
    return rc;
}
