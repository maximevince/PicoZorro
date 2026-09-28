//! PicoZorro firmware: the Zorro II bus slave (Autoconfig, register window of
//! `docs/REGISTERS.md`) and the network path, one image for every board.
//!
//! Core 1: the bus service loop (`zbus.rs`), alone, from RAM.
//! Core 0: set-up first (the slave stays silent until an Amiga reset), then
//! the Ethernet chip on SPI1 if one answers (`pz_hal::chip`, `nic_task.rs`; none:
//! bus slave only, link down, TX frames dropped), and housekeeping: a text log
//! on the UART (GPIO46, 115200 8N1, TX only) and, with `usb-log` instead of
//! `usb-host`, on USB CDC ACM (`/dev/serial/by-id/usb-PicoZorro_picozorro-if00`, with the
//! picotool reset interface), defmt over RTT when a probe is attached, the
//! activity LED on GPIO39 when no W5500 owns that pin.
//!
//! `--features bench-host`: core 1 plays the Amiga driver instead
//! (`bench_host.rs`), for network tests from a PC; bus pins untouched.
//! `--features uart-backplane`: no bus slave; register cycles arrive over
//! UART0 on GPIO0/1 (`backplane.rs`), e.g. from FS-UAE through a serial
//! relay; the text log travels on that link.
//!
//! Build: `cargo run --release --bin picozorro` (the PicoZorro One: XRDY and
//! /INT from the GPIOs, USB host); without `xrdy-direct` / `int-nfet` for a
//! board with an XRDY gate and an open-drain /INT buffer.
#![no_std]
#![no_main]

#[cfg(all(feature = "bench-host", feature = "uart-backplane"))]
compile_error!("bench-host and uart-backplane both replace the bus slave: pick one");
#[cfg(all(feature = "usb-host", feature = "usb-log"))]
compile_error!("usb-host and usb-log both want the USB port: pick one (--no-default-features)");

#[cfg(feature = "uart-backplane")]
#[path = "../backplane.rs"]
mod backplane;
#[cfg(feature = "bench-host")]
#[path = "../bench_host.rs"]
mod bench_host;
#[path = "../nic_task.rs"]
mod nic_task;
#[path = "../update_task.rs"]
mod update_task;
#[path = "../boot_task.rs"]
mod boot_task;
#[cfg(feature = "mpeg")]
#[path = "../minimp3.rs"]
mod minimp3;
#[cfg(feature = "mpeg")]
#[path = "../mpeg_task.rs"]
mod mpeg_task;
#[allow(dead_code)] // bench-host and backplane builds only read its STATS
#[path = "../zbus.rs"]
mod zbus;

use core::fmt::Write as _;
#[cfg(not(feature = "uart-backplane"))]
use core::ptr::addr_of_mut;
use core::sync::atomic::Ordering::Relaxed;

use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
#[cfg(not(feature = "uart-backplane"))]
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::peripherals::{PIO0, PIO1};
use embassy_rp::pio::InterruptHandler;
use embassy_rp::uart::Config as UartConfig;
#[cfg(not(feature = "uart-backplane"))]
use embassy_rp::uart::UartTx;
use embassy_time::Timer;
use pz_core::nic::Shared;
use pz_hal::{chip, chip_pins};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
});
#[cfg(feature = "usb-host")]
bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => embassy_rp::usb::host::InterruptHandler<embassy_rp::peripherals::USB>;
});
#[cfg(feature = "usb-log")]
bind_interrupts!(struct UsbIrqs {
    USBCTRL_IRQ => embassy_rp::usb::InterruptHandler<embassy_rp::peripherals::USB>;
});
#[cfg(feature = "uart-backplane")]
bind_interrupts!(struct UartIrqs {
    UART0_IRQ => embassy_rp::uart::BufferedInterruptHandler<embassy_rp::peripherals::UART0>;
});

static SHARED: StaticCell<Shared> = StaticCell::new();
static UPDATE: StaticCell<pz_core::update::Shared> = StaticCell::new();
static BOOT: StaticCell<pz_core::boot::Shared> = StaticCell::new();
#[cfg(feature = "mpeg")]
static MPEG: StaticCell<pz_core::mpeg::Shared> = StaticCell::new();
#[cfg(feature = "usb-host")]
static USB_WIN: StaticCell<pz_core::usb::Shared> = StaticCell::new();

/// The USB device (`usb-log`): CDC ACM carrying the text log (each finished
/// `Log` line, through embassy-usb-logger) and the picotool reset interface.
#[cfg(feature = "usb-log")]
#[embassy_executor::task]
async fn usb_log_task(usb: embassy_rp::Peri<'static, embassy_rp::peripherals::USB>) -> ! {
    // SAFETY: update_task owns the watchdog for feeding and its scratch
    // words; the reset interface only uses it for `trigger_reset` on
    // picotool's RESET_REQUEST_FLASH, after which nothing runs.
    let wd = embassy_rp::watchdog::Watchdog::new(unsafe { embassy_rp::peripherals::WATCHDOG::steal() });
    let driver = embassy_rp::usb::Driver::new(usb, UsbIrqs);
    let (mut device, class) = pz_hal::usb::cdc_with_reset(driver, wd, "picozorro");
    let log_fut = embassy_usb_logger::with_class!(1024, log::LevelFilter::Info, class);
    embassy_futures::join::join(device.run(), log_fut).await;
    unreachable!()
}

// The latency-sensitive tasks (network, backplane) run on this executor,
// which preempts the thread-mode one; the MPEG decoder and the update task
// stay in thread mode. The decoder holds core 0 for ~3 ms per frame: next
// to it in thread mode the backplane would fall behind, the UART's RX buffer
// fill and the hardware FIFO overrun.
static EXECUTOR_HIGH: embassy_executor::InterruptExecutor = embassy_executor::InterruptExecutor::new();

use embassy_rp::interrupt;

#[interrupt]
unsafe fn SWI_IRQ_1() {
    // SAFETY: the executor's own interrupt, started once in main.
    unsafe { EXECUTOR_HIGH.on_interrupt() }
}

/// Whether the Ethernet chip answered (net_init to main).
static NET_UP: embassy_sync::signal::Signal<embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, bool> =
    embassy_sync::signal::Signal::new();

/// Runs on EXECUTOR_HIGH: gets that executor's own Spawner and hands the
/// bring-up to `net_init` there. The network driver's state is not Send
/// (embassy-net's channel uses a NoopRawMutex), so its tasks can only be
/// spawned from inside the executor they run on; this task's arguments are.
#[embassy_executor::task]
async fn net_start(pins: chip::Pins, nic: pz_core::nic::NicPort<'static>, mac: [u8; 6]) {
    // SAFETY: polled by the Embassy InterruptExecutor, as the method needs.
    let spawner = unsafe { Spawner::for_current_executor() }.await;
    spawner.spawn(unwrap!(net_init(spawner, pins, nic, mac)));
}

/// Runs on EXECUTOR_HIGH, next to the network: the USB host (workers,
/// root port, interrupt readers; pz_hal::usb_host) and the USB window's
/// host side. Its tasks are not Send either, so they start from here.
#[cfg(feature = "usb-host")]
#[embassy_executor::task]
async fn usb_start(usb: embassy_rp::Peri<'static, embassy_rp::peripherals::USB>, host: pz_core::usb::HostPort<'static>) {
    use pz_hal::usb_host;
    // SAFETY: polled by the Embassy InterruptExecutor, as the method needs.
    let spawner = unsafe { Spawner::for_current_executor() }.await;
    usb_host::start(spawner, embassy_rp::usb::host::Driver::new(usb, UsbIrqs), "picozorro");
    info!("usb: host up, USB window at A16 = 1");
    // The request queue is looked at every 250 us (nothing on the bus side
    // wakes this task) and at once when a transfer finishes.
    usb_host::WindowHost::new(host).run(embassy_time::Duration::from_micros(250)).await
}

#[embassy_executor::task]
async fn net_init(spawner: Spawner, pins: chip::Pins, nic: pz_core::nic::NicPort<'static>, mac: [u8; 6]) {
    // The Ethernet chip, if one is fitted (~100 ms either way).
    match chip::bring_up(&spawner, pins, mac).await {
        Some(dev) => {
            spawner.spawn(unwrap!(nic_task::nic_task(dev, nic, mac)));
            NET_UP.signal(true);
        }
        None => {
            spawner.spawn(unwrap!(nic_task::drain_task(nic)));
            NET_UP.signal(false);
        }
    }
}
#[cfg(not(any(feature = "bench-host", feature = "uart-backplane")))]
static mut CORE1_STACK: Stack<8192> = Stack::new();
#[cfg(feature = "bench-host")]
static mut CORE1_STACK: Stack<32768> = Stack::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(clock_config());
    let high = {
        use embassy_rp::interrupt::{InterruptExt, Priority};
        interrupt::SWI_IRQ_1.set_priority(Priority::P2);
        // The GPIO and DMA interrupts wake tasks on this executor; embassy-rp
        // leaves them at P3, below it. embassy-sync's lockless AtomicWaker
        // (git main) sets WAKING, wakes, then clears WAKING: an executor that
        // preempts the interrupt in between finds WAKING when its task
        // registers again, is woken again at once, and never lets the
        // interrupt finish (seen with the W5500 /INT wait: every thread-mode
        // task starved). Above the executor, their short handlers always
        // finish first.
        interrupt::IO_IRQ_BANK0.set_priority(Priority::P1);
        interrupt::DMA_IRQ_0.set_priority(Priority::P1);
        EXECUTOR_HIGH.start(interrupt::SWI_IRQ_1)
    };

    let chip_id = embassy_rp::otp::get_chipid().unwrap_or(0);
    let mac = pz_core::mac::from_chip_id(chip_id);
    let shared = SHARED.init_with(|| Shared::new(mac));
    let (bus, nic) = shared.split();
    // Firmware update (docs/UPDATE.md): the registers in the window, the
    // flash side as a task. Every image carries it: one that boots on trial
    // after an update keeps the watchdog fed and takes CONFIRM here.
    let (upd_bus, upd_host) = UPDATE.init_with(pz_core::update::Shared::new).split();
    let layout = update_task::probe();
    let mut wd = embassy_rp::watchdog::Watchdog::new(p.WATCHDOG);
    let resume_base = update_task::take_stash(&mut wd);
    // Boot ROM: the ROM goes in before the bus side runs;
    // the stream is a task. No image in the build: no boot ROM.
    #[cfg_attr(feature = "bench-host", allow(unused_variables))]
    let boot_bus = {
        let (b, mut h) = BOOT.init_with(pz_core::boot::Shared::new).split();
        b.shared().set_enabled(!update_task::bootrom_off());
        if let Some(img) = boot_task::image() {
            h.set_rom(unwrap!(img.rom().ok()));
            spawner.spawn(unwrap!(boot_task::run(h, img)));
        }
        b
    };
    spawner.spawn(unwrap!(update_task::run(upd_host, layout, wd, boot_bus.shared())));
    // MPEG audio decoder (docs/REGISTERS-MPEG.md): registers at A16 = 1,
    // minimp3 as a task.
    #[cfg(feature = "mpeg")]
    #[cfg_attr(feature = "bench-host", allow(unused_variables))]
    let mpeg_bus = {
        let (b, h) = MPEG.init_with(pz_core::mpeg::Shared::new).split();
        spawner.spawn(unwrap!(mpeg_task::run(h)));
        b
    };
    // USB host (docs/REGISTERS-USB.md): the window at A16 = 1; its host
    // side starts on the high-priority executor below, after the bus.
    #[cfg(feature = "usb-host")]
    #[cfg_attr(feature = "bench-host", allow(unused_variables))]
    let (usb_bus, usb_host_port) = USB_WIN.init_with(pz_core::usb::Shared::new).split();
    // SAFETY: the only reference to the core-1 stack, taken once.
    #[cfg(not(feature = "uart-backplane"))]
    let stack = unsafe { &mut *addr_of_mut!(CORE1_STACK) };

    // Core 1 first: the bus pins are set up and silent before anything else;
    // the Ethernet chip comes after.
    #[cfg(not(any(feature = "bench-host", feature = "uart-backplane")))]
    {
        use embassy_rp::pio::Pio;
        macro_rules! any {
            ($($p:expr),* $(,)?) => { [$($p.into()),*] };
        }
        let window = pz_core::window::Window::new(bus).with_update(upd_bus).with_boot(boot_bus);
        #[cfg(feature = "mpeg")]
        let window = window.with_mpeg(mpeg_bus);
        #[cfg(feature = "usb-host")]
        let window = window.with_usb_window(usb_bus);
        let mut window = window;
        if let Some(base) = resume_base {
            // Rebooted into a new image by an update: `Zbus::new` answers at
            // this base again if the Amiga is not in reset.
            window.slave.state = pz_core::slave::State::Configured;
            window.slave.base = base;
        }
        let Pio { common: mut c0, sm0: data, .. } = Pio::new(p.PIO0, Irqs);
        let Pio { common: mut c1, sm0: decode, .. } = Pio::new(p.PIO1, Irqs);
        macro_rules! pio_pins {
            ($c:ident; $($p:expr),* $(,)?) => { [$($c.make_pio_pin($p)),*] };
        }
        let pins = zbus::BusPins {
            d: pio_pins![c0; p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3, p.PIN_4, p.PIN_5, p.PIN_6, p.PIN_7, p.PIN_8,
                p.PIN_9, p.PIN_10, p.PIN_11, p.PIN_12, p.PIN_13, p.PIN_14, p.PIN_15],
            strobes_a1_a7: any![
                p.PIN_16, p.PIN_17, p.PIN_18, p.PIN_19, p.PIN_20, p.PIN_21, p.PIN_22, p.PIN_23, p.PIN_24, p.PIN_25,
                p.PIN_26
            ],
            match_field: pio_pins![c1; p.PIN_28, p.PIN_29, p.PIN_30, p.PIN_31, p.PIN_32, p.PIN_33, p.PIN_34,
                p.PIN_35, p.PIN_36],
            #[cfg(not(feature = "xrdy-direct"))]
            xrdy: p.PIN_27.into(),
            #[cfg(not(feature = "xrdy-direct"))]
            arm: c1.make_pio_pin(p.PIN_44),
            #[cfg(feature = "xrdy-direct")]
            xrdy: c1.make_pio_pin(p.PIN_27),
            #[cfg(feature = "xrdy-direct")]
            arm: p.PIN_44.into(),
            cfgout: p.PIN_37.into(),
            busrst: p.PIN_38.into(),
            int: p.PIN_45.into(),
        };
        let slave = zbus::Zbus::new(&mut c0, data, &mut c1, decode, pins, window);
        // The programs stay loaded: the PIO blocks are never handed back.
        core::mem::forget(c0);
        core::mem::forget(c1);
        spawn_core1(p.CORE1, stack, move || slave.serve_forever());
    }
    #[cfg(feature = "bench-host")]
    {
        let _ = (resume_base, upd_bus);
        spawn_core1(p.CORE1, stack, move || bench_host::run(bus, mac));
    }

    // Backplane: the window lives on core 0, served from the UART; core 1
    // stays off.
    #[cfg(feature = "uart-backplane")]
    let mut log = {
        static TX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
        static RX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
        let mut cfg = UartConfig::default();
        cfg.baudrate = backplane::BAUD;
        let uart = embassy_rp::uart::BufferedUart::new(
            p.UART0,
            p.PIN_0,
            p.PIN_1,
            UartIrqs,
            TX_BUF.init([0; 4096]),
            RX_BUF.init([0; 4096]),
            cfg,
        );
        let window = pz_core::window::Window::new(bus).with_update(upd_bus).with_boot(boot_bus);
        #[cfg(feature = "mpeg")]
        let window = window.with_mpeg(mpeg_bus);
        #[cfg(feature = "usb-host")]
        let window = window.with_usb_window(usb_bus);
        let mut window = window;
        if let Some(base) = resume_base {
            // Rebooted into a new image by an update: the Amiga still has
            // the board at this base, so carry on configured there.
            window.slave.state = pz_core::slave::State::Configured;
            window.slave.base = base;
            info!("update: resuming configured at base {:x}", base);
        }
        high.spawn(unwrap!(backplane::run(window, uart)));
        Log(core::marker::PhantomData)
    };
    #[cfg(not(feature = "uart-backplane"))]
    let mut log = Log { uart: UartTx::new_blocking(p.UART0, p.PIN_46, UartConfig::default()), line: [0; 200], len: 0 };
    banner(&mut log, &mac);

    // The Ethernet chip, if one is fitted (~100 ms either way), brought up
    // on the high-priority executor, where the network tasks run (above
    // the MPEG decoder).
    // The USB host, next to the network (no USB host behind bench-host).
    #[cfg(all(feature = "usb-host", not(feature = "bench-host")))]
    high.spawn(unwrap!(usb_start(p.USB, usb_host_port)));
    #[cfg(feature = "usb-log")]
    spawner.spawn(unwrap!(usb_log_task(p.USB)));
    high.spawn(unwrap!(net_start(chip_pins!(p), nic, mac)));
    let led = match NET_UP.wait().await {
        true => {
            let _ = writeln!(log, "{} up, SCK {} kHz, network registers live", chip::NAME, chip::spi_sck_hz() / 1000);
            None // GPIO39 is the chip's /INT now; the chip drives the LED
        }
        false => {
            let _ = writeln!(log, "no {}: bus slave only (link down, TX frames dropped)", chip::NAME);
            info!("no {}: bus slave only", chip::NAME);
            // SAFETY: bring_up has dropped its GPIO39 input on the way out;
            // nothing else owns the pin.
            Some(Output::new(unsafe { embassy_rp::peripherals::PIN_39::steal() }, Level::Low))
        }
    };
    #[cfg(feature = "bench-host")]
    let _ = writeln!(log, "bench host on core 1, answering at 10.42.0.2 (not for use in an Amiga)");
    #[cfg(feature = "uart-backplane")]
    let _ = writeln!(log, "UART backplane on GPIO0/1, {} baud (not for use in an Amiga)", backplane::BAUD);

    housekeeping(log, led).await
}

#[cfg(feature = "uart-backplane")]
struct Log<'d>(core::marker::PhantomData<&'d ()>);

#[cfg(feature = "uart-backplane")]
impl core::fmt::Write for Log<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let _ = backplane::LOG.try_write(s.as_bytes()); // full: dropped
        Ok(())
    }
}

/// Text to the UART; with `usb-log` each finished line also to USB CDC.
#[cfg(not(feature = "uart-backplane"))]
struct Log<'d> {
    uart: UartTx<'d, embassy_rp::mode::Blocking>,
    line: [u8; 200],
    len: usize,
}

#[cfg(not(feature = "uart-backplane"))]
impl core::fmt::Write for Log<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for chunk in s.as_bytes().split_inclusive(|&b| b == b'\n') {
            let (body, eol) = match chunk.split_last() {
                Some((&b'\n', body)) => (body, true),
                _ => (chunk, false),
            };
            let _ = self.uart.blocking_write(body);
            let n = body.len().min(self.line.len() - self.len);
            self.line[self.len..self.len + n].copy_from_slice(&body[..n]);
            self.len += n;
            if eol {
                let _ = self.uart.blocking_write(b"\r\n");
                #[cfg(feature = "usb-log")]
                if let Ok(t) = core::str::from_utf8(&self.line[..self.len]) {
                    log::info!("{}", t);
                }
                self.len = 0;
            }
        }
        Ok(())
    }
}

/// clk_sys: embassy's 150 MHz, or pll_sys from the 12 MHz crystal with the
/// `oc240` / `oc300` features (VCO 1440 / 6 = 240, VCO 1500 / 5 = 300). The
/// bus loop's settle delay is computed from the clock at run time; the UART,
/// SPI and USB take their divisors from embassy's clock record. Overclocked,
/// clk_peri moves off clk_sys (`crystal()` puts it there; the datasheet rates
/// clk_peri at 150 MHz at most) onto pll_usb, raised to 144 MHz (VCO 1440 /
/// 5 / 2) with clk_usb and clk_adc at 144 / 3 = 48 MHz. SPI can then run at
/// 144 / 2 / n: 72, 36, 24 MHz (`PZ_SPI_HZ`, pz_hal::chip).
fn clock_config() -> embassy_rp::config::Config {
    #[allow(unused_mut)]
    let mut c = embassy_rp::config::Config::default();
    #[cfg(any(feature = "oc240", feature = "oc300"))]
    {
        use embassy_rp::clocks::{ClockConfig, CoreVoltage, PeriClkSrc, PllConfig};
        let mut clocks = ClockConfig::crystal(12_000_000);
        let (fbdiv, post_div1, mv) = if cfg!(feature = "oc300") {
            (125, 5, CoreVoltage::V1_25)
        } else {
            (120, 6, CoreVoltage::V1_15)
        };
        if let Some(x) = clocks.xosc.as_mut() {
            x.sys_pll = Some(PllConfig { refdiv: 1, fbdiv, post_div1, post_div2: 1 });
        }
        clocks.core_voltage = mv;
        if let Some(x) = clocks.xosc.as_mut() {
            x.usb_pll = Some(PllConfig { refdiv: 1, fbdiv: 120, post_div1: 5, post_div2: 2 });
        }
        if let Some(u) = clocks.usb_clk.as_mut() {
            u.div = 3;
        }
        if let Some(a) = clocks.adc_clk.as_mut() {
            a.div = 3;
        }
        clocks.peri_clk_src = Some(PeriClkSrc::PllUsb);
        c.clocks = clocks;
    }
    c
}

/// The stepping from the bootrom version byte: 2 = A2, 3 = A3, 4 = A4.
fn stepping() -> (&'static str, bool) {
    match embassy_rp::rom_data::rom_version_number() {
        2 => ("A2", true),
        3 => ("A3", false),
        4 => ("A4", false),
        _ => ("unknown", false),
    }
}

/// One line per PIO block for the log: GPIOBASE, program counter, jump pin,
/// wrap, pin bases, IN_COUNT, FIFO levels. What the state machines look like
/// on real silicon, e.g. both idle at their first instructions with no bus.
fn pio_report(log: &mut Log<'_>) {
    use embassy_rp::pac;
    for (name, pio) in [("PIO0 data  ", pac::PIO0), ("PIO1 decode", pac::PIO1)] {
        let sm = pio.sm(0);
        let e = sm.execctrl().read();
        let pc = sm.pinctrl().read();
        let sh = sm.shiftctrl().read();
        let base = if pio.gpiobase().read().gpiobase() { 16 } else { 0 };
        let pcv = sm.addr().read().addr();
        let en = pio.ctrl().read().sm_enable();
        let lvl = pio.flevel().read().0;
        let _ = write!(
            log,
            "{}: enabled {:x} gpiobase {} pc {} jmp_pin {} wrap {}..{} in_base {} in_count {} out_base {} set_base {} side_base {} flevel {:08x}\n",
            name,
            en,
            base,
            pcv,
            e.jmp_pin(),
            e.wrap_bottom(),
            e.wrap_top(),
            pc.in_base(),
            sh.in_count(),
            pc.out_base(),
            pc.set_base(),
            pc.sideset_base(),
            lvl
        );
        info!(
            "{}: en {:x} gpiobase {} pc {} jmp_pin {} wrap {}..{} in_base {} in_count {} out_base {} set_base {} side_base {} flevel {:08x}",
            name,
            en,
            base,
            pcv,
            e.jmp_pin(),
            e.wrap_bottom(),
            e.wrap_top(),
            pc.in_base(),
            sh.in_count(),
            pc.out_base(),
            pc.set_base(),
            pc.sideset_base(),
            lvl
        );
    }
}

fn state_name(s: u8) -> &'static str {
    match s {
        0 => "unconfigured",
        1 => "configured",
        _ => "shut up",
    }
}

fn banner(log: &mut Log<'_>, mac: &[u8; 6]) {
    let st = &zbus::STATS;
    let (step, a2) = stepping();
    let warn = if a2 { "  ** A2 silicon: erratum E9, do not use on a bus **" } else { "" };
    let boot_us = st.boot_us.load(Relaxed);
    let _ = writeln!(log, "\nPicoZorro (Rust), XRDY via {}, RP2350 {}{}", zbus::XRDY_MODE, step, warn);
    let _ = writeln!(log, "firmware {}", update_task::VERSION);
    let _ =
        writeln!(log, "MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    info!("PicoZorro, XRDY via {}, stepping {}, ready after {} us", zbus::XRDY_MODE, step, boot_us);
    if cfg!(any(feature = "bench-host", feature = "uart-backplane")) {
        return;
    }
    let _ = writeln!(log, "cold start to slave ready: {} us", boot_us);
    pio_report(log);
    let busrst =
        if embassy_rp::pac::SIO.gpio_in(1).read() & (1 << (pz_core::pins::BUSRST - 32)) != 0 { "high" } else { "low" };
    let _ = writeln!(log, "/BUSRST reads {}", busrst);
    info!("/BUSRST reads {}", busrst);
}

async fn housekeeping(mut log: Log<'static>, mut led: Option<Output<'static>>) -> ! {
    let st = &zbus::STATS;
    let mut last = (u32::MAX, u32::MAX, u32::MAX, false, u8::MAX);
    let mut last_link = 0xff;
    loop {
        Timer::after_millis(500).await;
        let now = (
            st.reads.load(Relaxed),
            st.writes.load(Relaxed),
            st.resets.load(Relaxed),
            st.armed.load(Relaxed),
            st.state.load(Relaxed),
        );
        if let Some(led) = &mut led {
            led.set_level(if now.0 != last.0 || now.1 != last.1 { Level::High } else { Level::Low });
        }
        if !cfg!(any(feature = "bench-host", feature = "uart-backplane")) && (now.2 != last.2 || now.3 != last.3 || now.4 != last.4) {
            let base = st.base.load(Relaxed);
            let _ = writeln!(
                log,
                "{}, {}, base ${:02x}0000, resets {}, rd {}, wr {}",
                if now.3 { "armed" } else { "waiting for /BUSRST" },
                state_name(now.4),
                base,
                now.2,
                now.0,
                now.1
            );
            info!("{} {} base {:02x} resets {} rd {} wr {}", now.3, state_name(now.4), base, now.2, now.0, now.1);
        }
        last = now;
        #[cfg(feature = "bus-prof")]
        {
            let p: [u32; 9] = core::array::from_fn(|i| zbus::PROF[i].load(Relaxed));
            if p[1] + p[4] != 0 {
                let _ = writeln!(
                    log,
                    "prof clk: read avg {} max {} (n {}), write avg {} max {} (n {}), idle pass max {} avg {}",
                    p[0] / p[1].max(1), p[2], p[1], p[3] / p[4].max(1), p[5], p[4], p[6],
                    p[7] / p[8].max(1)
                );
                for a in zbus::PROF.iter() {
                    a.store(0, Relaxed);
                }
            }
        }
        let link = nic_task::LINK.load(Relaxed);
        if link != last_link && link != 0xff {
            last_link = link;
            let _ = writeln!(
                log,
                "link {}{}",
                if link & 1 != 0 { "up" } else { "down" },
                match (link & 1 != 0, link >> 1) {
                    (false, _) => "",
                    (true, 0) => ", 10 half",
                    (true, 1) => ", 100 half",
                    (true, 2) => ", 10 full",
                    (true, _) => ", 100 full",
                }
            );
            info!("link {=u8:#x}", link);
        }
    }
}
