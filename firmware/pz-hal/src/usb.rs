//! A USB device for a board without a debug probe, or next to one: CDC ACM
//! (a serial port on the PC, `/dev/ttyACM*`) plus the pico-sdk reset
//! interface, so `picotool load -f` re-flashes a running image without
//! BOOTSEL.

use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_rp::watchdog::Watchdog;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::control::{OutResponse, Recipient, Request, RequestType};
use embassy_usb::{Builder, Config, Handler, UsbDevice};
use static_cell::StaticCell;

/// The pico-sdk reset interface (`pico/usb_reset_interface.h`): a vendor
/// interface, subclass 0, protocol 1, no endpoints. picotool sends
/// `RESET_REQUEST_BOOTSEL` (1, wValue bits 0-1 = bootrom disable_interface_mask)
/// or `RESET_REQUEST_FLASH` (2) as an interface request.
struct PicotoolReset {
    iface: u8,
    watchdog: Watchdog<'static>,
}

impl Handler for PicotoolReset {
    fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
        if req.recipient != Recipient::Interface
            || req.index != u16::from(self.iface)
            || !matches!(req.request_type, RequestType::Class | RequestType::Vendor)
        {
            return None;
        }
        match req.request {
            1 => {
                // A running watchdog (a firmware's boot watchdog) must not
                // reset the chip out of BOOTSEL.
                embassy_rp::pac::WATCHDOG.ctrl().modify(|w| w.set_enable(false));
                embassy_rp::rom_data::reset_to_usb_boot(0, u32::from(req.value & 3)) // no return
            }
            2 => self.watchdog.trigger_reset(),
            _ => return Some(OutResponse::Rejected),
        }
        Some(OutResponse::Accepted)
    }
}

/// The composite device: CDC ACM (two interfaces, 64-byte packets) and the
/// reset interface. `product` is the USB product string, and so part of the
/// `/dev/serial/by-id/usb-PicoZorro_<product>_...` name. Run the returned
/// device (`UsbDevice::run`) in a task; use the class in another. Call once
/// (the descriptors live in statics).
pub fn cdc_with_reset(
    driver: Driver<'static, USB>,
    watchdog: Watchdog<'static>,
    product: &'static str,
) -> (UsbDevice<'static, Driver<'static, USB>>, CdcAcmClass<'static, Driver<'static, USB>>) {
    static CDC_STATE: StaticCell<State> = StaticCell::new();
    let mut builder = builder(driver, product);
    let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(State::new()), 64);
    (finish(builder, watchdog), class)
}

/// A CDC ACM port of this device.
pub type Cdc = CdcAcmClass<'static, Driver<'static, USB>>;

/// [`cdc_with_reset`] with two CDC ACM ports (interfaces 0/1 and 2/3, so
/// `..._<product>_...-if00` and `-if02` on the PC), e.g. one for a binary
/// protocol and one for a text log. Call once, and not together with
/// [`cdc_with_reset`] (one USB peripheral).
pub fn cdc2_with_reset(
    driver: Driver<'static, USB>,
    watchdog: Watchdog<'static>,
    product: &'static str,
) -> (UsbDevice<'static, Driver<'static, USB>>, Cdc, Cdc) {
    static CDC_STATE: StaticCell<[State; 2]> = StaticCell::new();
    let mut builder = builder(driver, product);
    let [s0, s1] = CDC_STATE.init([State::new(), State::new()]);
    let c0 = CdcAcmClass::new(&mut builder, s0, 64);
    let c1 = CdcAcmClass::new(&mut builder, s1, 64);
    (finish(builder, watchdog), c0, c1)
}

/// The builder with the device configuration, before the classes.
fn builder(driver: Driver<'static, USB>, product: &'static str) -> Builder<'static, Driver<'static, USB>> {
    // 256 bytes of configuration descriptor: two CDC ports (66 bytes each
    // with their IAD) and the reset interface (17) fit with room to spare.
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    // Same VID/PID/class triple as a pico-sdk stdio_usb application, which is
    // what picotool looks for. Composite device: CDC (2 interfaces each) + reset.
    let mut config = Config::new(0x2e8a, 0x0009);
    config.manufacturer = Some("PicoZorro");
    config.product = Some(product);
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.device_class = 0xef;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    )
}

/// The reset interface after the classes, then the device.
fn finish(mut builder: Builder<'static, Driver<'static, USB>>, watchdog: Watchdog<'static>) -> UsbDevice<'static, Driver<'static, USB>> {
    static RESET: StaticCell<PicotoolReset> = StaticCell::new();
    let iface = {
        let mut func = builder.function(0xff, 0x00, 0x01);
        let mut iface = func.interface();
        let num = iface.interface_number();
        iface.alt_setting(0xff, 0x00, 0x01, None);
        num
    };
    builder.handler(RESET.init(PicotoolReset { iface: iface.into(), watchdog }));
    builder.build()
}
