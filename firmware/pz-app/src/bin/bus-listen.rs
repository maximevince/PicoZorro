//! Passive bus listener for a card's first hours in an Amiga: nothing on
//! the Zorro bus is ever driven. Every bus GPIO (0-38) is a plain input with
//! no pull, the XRDY gate is held disarmed (/ARM, GPIO44, pulled up), /CFGOUT
//! is never driven (the slot's pull-down passes the config chain on, so the
//! Amiga boots as if the slot were empty), /INT (GPIO45) and GPIO47 are not
//! touched.
//!
//! What it shows, as text on the UART (GPIO46, 115200 8N1, TX only, as
//! `picozorro`) and on USB CDC (with the picotool reset interface so
//! `picotool load -f` re-flashes without BOOTSEL):
//! - a banner: stepping, chip ID (OTP unique ID), VREG voltage, clk_sys;
//! - once per second: the bus pins now, which of them moved during the
//!   second (bus alive, no open finger), and the /AS statistics below.
//!
//! /AS statistics come from PIO1 (GPIOBASE 16, so it sees GPIO16-36):
//! per /AS falling edge the state machine captures GPIO16-36 (strobes, R/W,
//! A1-A7, XRDY, A16-A23, /CFGIN) one clk_sys after the edge came through
//! the input synchroniser, then counts /AS low in a two-instruction loop.
//! Resolution: 2 clk_sys = 13.3 ns at 150 MHz; both edges pass the same
//! two-flop synchroniser, so its delay cancels, and the reported time is
//! (2 n + 3) clk_sys, within +-1 clk_sys of the truth. Core 1 drains the
//! joined 8-deep RX FIFO and samples SIO GPIO_IN in the same loop (a few
//! tens of ns per pass) for the "moved" masks. A full FIFO stalls the
//! state machine (`push block`): that second's numbers then miss cycles
//! and the line says so (stalls).
//!
//! Build matches the board: `--features xrdy-direct` when GPIO27 is on the
//! bus XRDY (the XRDY column is the bus line); the default keeps GPIO27
//! pulled down, as `picozorro` does on a board with the XRDY gate.
//!
//! Flash: `cargo run --release --bin bus-listen [--features xrdy-direct]`
//! (SWD probe) or `picotool load -f -x` on the ELF (USB).
#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{AnyPin, Input, Level, Output, Pull};
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::pac;
use embassy_rp::peripherals::{PIO1, USB};
use embassy_rp::pio::{Config, ExecConfig, FifoJoin, InterruptHandler, Pio, PinConfig, ShiftConfig, ShiftDirection};
use embassy_rp::uart::{Config as UartConfig, UartTx};
use embassy_rp::usb::{Driver, InterruptHandler as UsbInterruptHandler};
use embassy_rp::watchdog::Watchdog;
use embassy_rp::Peri;
use embassy_time::{Instant, Timer};
use pz_core::pins;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

/// Bits of the word the PIO captures at /AS falling: GPIO16-36, bit 0 = GPIO16.
mod snap {
    pub const READ: u32 = 1 << 3;
    pub const CFGIN: u32 = 1 << 20;
    pub fn a1_a7(w: u32) -> u32 {
        (w >> 4) & 0x7f
    }
    pub fn a16_a23(w: u32) -> u32 {
        (w >> 12) & 0xff
    }
}

/// One second of statistics, published by core 1 under a sequence count
/// (odd while it writes), read by core 0.
struct Published {
    seq: AtomicU32,
    edges: AtomicU32,
    reads: AtomicU32,
    cfgin_low: AtomicU32,
    e8: AtomicU32,
    e8_cfgin_low: AtomicU32,
    low_min: AtomicU32,
    low_max: AtomicU32,
    low_sum_lo: AtomicU32,
    last_e8: AtomicU32,
    stalls: AtomicU32,
    seen_low0: AtomicU32,
    seen_high0: AtomicU32,
    seen_low1: AtomicU32,
    seen_high1: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const Z: AtomicU32 = AtomicU32::new(0);
static PUB: Published = Published {
    seq: Z,
    edges: Z,
    reads: Z,
    cfgin_low: Z,
    e8: Z,
    e8_cfgin_low: Z,
    low_min: Z,
    low_max: Z,
    low_sum_lo: Z,
    last_e8: Z,
    stalls: Z,
    seen_low0: Z,
    seen_high0: Z,
    seen_low1: Z,
    seen_high1: Z,
};

/// Core 1's running second.
#[derive(Clone, Copy)]
struct Acc {
    edges: u32,
    reads: u32,
    cfgin_low: u32,
    e8: u32,
    e8_cfgin_low: u32,
    /// /AS low, in loop passes of the PIO counter (2 clk_sys each)
    low_min: u32,
    low_max: u32,
    low_sum: u64,
    last_e8: u32,
    seen_low: [u32; 2],
    seen_high: [u32; 2],
}

impl Acc {
    const fn new() -> Self {
        Acc {
            edges: 0,
            reads: 0,
            cfgin_low: 0,
            e8: 0,
            e8_cfgin_low: 0,
            low_min: u32::MAX,
            low_max: 0,
            low_sum: 0,
            last_e8: u32::MAX,
            seen_low: [0; 2],
            seen_high: [0; 2],
        }
    }

    #[inline(always)]
    fn cycle(&mut self, w: u32, passes: u32) {
        self.edges += 1;
        if w & snap::READ != 0 {
            self.reads += 1;
        }
        let cfg = w & snap::CFGIN == 0;
        if cfg {
            self.cfgin_low += 1;
        }
        if snap::a16_a23(w) == 0xe8 {
            self.e8 += 1;
            if cfg {
                self.e8_cfgin_low += 1;
            }
            self.last_e8 = w;
        }
        self.low_min = self.low_min.min(passes);
        self.low_max = self.low_max.max(passes);
        self.low_sum += u64::from(passes);
    }

    fn publish(&self, stalls: u32) {
        use Ordering::{Relaxed, Release};
        let p = &PUB;
        let s = p.seq.load(Relaxed);
        p.seq.store(s.wrapping_add(1), Release);
        p.edges.store(self.edges, Relaxed);
        p.reads.store(self.reads, Relaxed);
        p.cfgin_low.store(self.cfgin_low, Relaxed);
        p.e8.store(self.e8, Relaxed);
        p.e8_cfgin_low.store(self.e8_cfgin_low, Relaxed);
        p.low_min.store(self.low_min, Relaxed);
        p.low_max.store(self.low_max, Relaxed);
        // a second holds < 2^24 cycles of < 2^8 passes typical: the low
        // 32 bits of the sum are enough for the average
        p.low_sum_lo.store(self.low_sum as u32, Relaxed);
        p.last_e8.store(self.last_e8, Relaxed);
        p.stalls.store(stalls, Relaxed);
        p.seen_low0.store(self.seen_low[0], Relaxed);
        p.seen_high0.store(self.seen_high[0], Relaxed);
        p.seen_low1.store(self.seen_low[1], Relaxed);
        p.seen_high1.store(self.seen_high[1], Relaxed);
        p.seq.store(s.wrapping_add(2), Release);
    }
}

/// Core 1: drains PIO1 SM0 and samples the pins, nothing else (no
/// interrupts are enabled on this core).
fn listen() -> ! {
    let pio = pac::PIO1;
    let mut acc = Acc::new();
    let mut pending: Option<u32> = None;
    let mut stalls = 0u32;
    let mut next = Instant::now().as_micros() + 1_000_000;
    let mut n: u32 = 0;
    loop {
        if pio.fstat().read().rxempty() & 1 == 0 {
            let w = pio.rxf(0).read();
            // words come in pairs: the capture at /AS falling, then the count
            match pending.take() {
                None => pending = Some(w),
                Some(s) => acc.cycle(s, w),
            }
        }
        let lo = pac::SIO.gpio_in(0).read();
        let hi = pac::SIO.gpio_in(1).read();
        acc.seen_low[0] |= !lo;
        acc.seen_high[0] |= lo;
        acc.seen_low[1] |= !hi;
        acc.seen_high[1] |= hi;
        n = n.wrapping_add(1);
        if n & 0xff == 0 {
            if pio.fdebug().read().rxstall() & 1 != 0 {
                pio.fdebug().write(|w| w.set_rxstall(1));
                stalls += 1;
            }
            let now = Instant::now().as_micros();
            if now >= next {
                acc.publish(stalls);
                acc = Acc::new();
                stalls = 0;
                next += 1_000_000;
            }
        }
    }
}

static mut CORE1_STACK: Stack<4096> = Stack::new();

/// Text to the UART, each finished line also to the USB log.
struct Log<'d> {
    uart: UartTx<'d, embassy_rp::mode::Blocking>,
    line: [u8; 200],
    len: usize,
}

impl core::fmt::Write for Log<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                let _ = self.uart.blocking_write(b"\r\n");
                if let Ok(t) = core::str::from_utf8(&self.line[..self.len]) {
                    log::info!("{}", t);
                }
                self.len = 0;
            } else {
                let _ = self.uart.blocking_write(&[b]);
                if self.len < self.line.len() {
                    self.line[self.len] = b;
                    self.len += 1;
                }
            }
        }
        Ok(())
    }
}

#[embassy_executor::task]
async fn usb_task(driver: Driver<'static, USB>, watchdog: Watchdog<'static>) -> ! {
    let (mut device, class) = pz_hal::usb::cdc_with_reset(driver, watchdog, "bus-listen");
    let log_fut = embassy_usb_logger::with_class!(1024, log::LevelFilter::Info, class);
    join(device.run(), log_fut).await;
    unreachable!()
}

/// The stepping from the bootrom version byte: 2 = A2, 3 = A3, 4 = A4.
fn stepping() -> &'static str {
    match embassy_rp::rom_data::rom_version_number() {
        2 => "A2 (erratum E9: not for the bus)",
        3 => "A3",
        4 => "A4",
        _ => "unknown",
    }
}

fn ns(passes: u32, hz: u32) -> u32 {
    ((u64::from(passes) * 2 + 3) * 1_000_000_000 / u64::from(hz.max(1))) as u32
}

fn bit(w: u32, n: u8) -> u8 {
    ((w >> n) & 1) as u8
}

/// Bus pins packed as the groups the log names: D0-15, A1-A7, A16-A23, and
/// the control bits (/AS /UDS /LDS R/W XRDY /CFGIN /CFGOUT /BUSRST).
fn groups(lo: u32, hi: u32) -> (u32, u32, u32, u32) {
    let d = lo & 0xffff;
    let a_lo = (lo >> pins::A1) & 0x7f;
    let a_hi = ((lo >> pins::A16) | (hi << (32 - pins::A16))) & 0xff;
    let ctl = ((lo >> pins::AS) & 0xf)
        | (u32::from(bit(lo, pins::XRDY)) << 4)
        | (u32::from(bit(hi, pins::CFGIN - 32)) << 5)
        | (u32::from(bit(hi, pins::CFGOUT - 32)) << 6)
        | (u32::from(bit(hi, pins::BUSRST - 32)) << 7);
    (d, a_lo, a_hi, ctl)
}

const CTL_NAMES: [&str; 8] = ["/AS", "/UDS", "/LDS", "R/W", "XRDY", "/CFGIN", "/CFGOUT", "/BUSRST"];

fn ctl_list(log: &mut Log<'_>, mask: u32) {
    if mask == 0 {
        let _ = write!(log, " -");
    }
    for (i, name) in CTL_NAMES.iter().enumerate() {
        if mask & (1 << i) != 0 {
            let _ = write!(log, " {}", name);
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Bus pins first: the pad reset state has the pull-down on, which must
    // not load the Amiga bus longer than needed. Kept for the life of the
    // image (dropping an Input would put the pull-down back).
    macro_rules! any {
        ($($p:expr),* $(,)?) => { [$($p.into()),*] };
    }
    let bus: [Peri<'static, AnyPin>; 38] = any![
        p.PIN_0, p.PIN_1, p.PIN_2, p.PIN_3, p.PIN_4, p.PIN_5, p.PIN_6, p.PIN_7, p.PIN_8, p.PIN_9, p.PIN_10,
        p.PIN_11, p.PIN_12, p.PIN_13, p.PIN_14, p.PIN_15, p.PIN_16, p.PIN_17, p.PIN_18, p.PIN_19, p.PIN_20,
        p.PIN_21, p.PIN_22, p.PIN_23, p.PIN_24, p.PIN_25, p.PIN_26, p.PIN_28, p.PIN_29, p.PIN_30, p.PIN_31,
        p.PIN_32, p.PIN_33, p.PIN_34, p.PIN_35, p.PIN_36, p.PIN_37, p.PIN_38,
    ];
    for pin in bus {
        let mut i = Input::new(pin, Pull::None);
        i.set_schmitt(true);
        core::mem::forget(i);
    }
    // XRDY: on the bus only with xrdy-direct; otherwise GPIO27 is kept from
    // floating, as picozorro does.
    let xrdy_pull = if cfg!(feature = "xrdy-direct") { Pull::None } else { Pull::Down };
    let mut x = Input::new(p.PIN_27, xrdy_pull);
    x.set_schmitt(true);
    core::mem::forget(x);
    // /ARM high: the XRDY gate, where fitted, never pulls XRDY. A pull-up
    // on the board does the same; this covers a missing one.
    core::mem::forget(Input::new(p.PIN_44, Pull::Up));

    let mut led = Output::new(p.PIN_39, Level::Low);
    let mut log = Log { uart: UartTx::new_blocking(p.UART0, p.PIN_46, UartConfig::default()), line: [0; 200], len: 0 };
    spawner.spawn(defmt::unwrap!(usb_task(Driver::new(p.USB, Irqs), Watchdog::new(p.WATCHDOG))));

    // PIO1 SM0: /AS capture and low-time counter (see the module comment).
    let Pio { mut common, sm0: mut sm, .. } = Pio::new(p.PIO1, Irqs);
    let prg = pio::pio_asm!(
        ".wrap_target",
        "    wait 1 pin 0", // /AS high first: a cycle under way at start is skipped
        "    wait 0 pin 0", // /AS falling
        "    in pins, 21",  // GPIO16-36 one clk_sys later
        "    push block",
        "    mov x, ~null",
        "low:",
        "    jmp pin high", // /AS (jmp_pin) high again
        "    jmp x-- low",
        "high:",
        "    mov isr, ~x", // passes of the loop
        "    push block",
        ".wrap",
    );
    let prog = common.load_program(&prg.program);
    // GPIOBASE 16 before set_config: no PIO pin of this block is >= 32 for
    // embassy-rp to infer it from, and /CFGIN (GPIO36) must be in the window.
    pac::PIO1.gpiobase().write(|w| w.set_gpiobase(true));
    let mut cfg = Config::default();
    cfg.use_program(&prog, &[]);
    let mut pc: PinConfig = cfg.get_pins();
    pc.in_base = pins::AS;
    // SAFETY: base only; every pin in the window is an input set up above.
    unsafe { cfg.set_pins(pc) };
    let mut ec: ExecConfig = cfg.get_exec();
    ec.jmp_pin = Some(pins::AS);
    // SAFETY: the jump pin is an input.
    unsafe { cfg.set_exec(ec) };
    cfg.shift_in = ShiftConfig { threshold: 32, direction: ShiftDirection::Left, auto_fill: false };
    cfg.fifo_join = FifoJoin::RxOnly;
    sm.set_config(&cfg);
    sm.set_enable(true);
    core::mem::forget(sm);
    core::mem::forget(common);

    let hz = embassy_rp::clocks::clk_sys_freq();
    let chip = pac::SYSINFO.chip_id().read();
    let qfn60 = pac::SYSINFO.package_sel().read().package_sel();
    let chip_id = embassy_rp::otp::get_chipid().ok();
    let vsel = (pac::POWMAN.vreg().read().0 >> 4) & 0x1f;
    let _ = writeln!(log, "\nPicoZorro bus-listen: passive, nothing on the bus is driven");
    let _ = writeln!(
        log,
        "RP2350{} stepping {} (CHIP_ID rev {:x}, part {:04x}), chip id {:016x}",
        if qfn60 { "A" } else { "B" },
        stepping(),
        chip.revision(),
        chip.part(),
        chip_id.unwrap_or(0)
    );
    if vsel <= 15 {
        let _ = writeln!(log, "VREG VSEL {} = {} mV, clk_sys {} Hz", vsel, 550 + 50 * vsel, hz);
    } else {
        let _ = writeln!(log, "VREG VSEL {} (above 1.30 V), clk_sys {} Hz", vsel, hz);
    }
    let _ = writeln!(
        log,
        "XRDY column: {}; /AS low time resolution {} ps (2 clk_sys), +-{} ps",
        if cfg!(feature = "xrdy-direct") { "the bus line" } else { "GPIO27 behind the gate (pulled down)" },
        2_000_000_000_000u64 / u64::from(hz),
        1_000_000_000_000u64 / u64::from(hz)
    );
    let _ = writeln!(log, "per second: /AS cycles, low time, pins now, pins that moved");

    // SAFETY: the only reference to the core-1 stack, taken once.
    let stack = unsafe { &mut *addr_of_mut!(CORE1_STACK) };
    spawn_core1(p.CORE1, stack, listen);

    let mut last_seq = 0u32;
    let mut t: u32 = 0;
    loop {
        Timer::after_millis(500).await;
        led.toggle();
        let seq = PUB.seq.load(Ordering::Acquire);
        if seq == last_seq || seq & 1 != 0 {
            continue;
        }
        last_seq = seq;
        t += 1;
        report(&mut log, t, hz);
    }
}

fn report(log: &mut Log<'_>, t: u32, hz: u32) {
    use Ordering::Relaxed;
    let p = &PUB;
    let edges = p.edges.load(Relaxed);
    let reads = p.reads.load(Relaxed);
    let _ = write!(
        log,
        "[{:5} s] /AS {}/s, rd {}, /CFGIN low {}, $E8xxxx {} (with /CFGIN low {})",
        t,
        edges,
        reads,
        p.cfgin_low.load(Relaxed),
        p.e8.load(Relaxed),
        p.e8_cfgin_low.load(Relaxed)
    );
    if let Some(avg) = p.low_sum_lo.load(Relaxed).checked_div(edges) {
        let _ = write!(
            log,
            ", /AS low min {} ns max {} ns avg {} ns",
            ns(p.low_min.load(Relaxed), hz),
            ns(p.low_max.load(Relaxed), hz),
            ns(avg, hz)
        );
    }
    let stalls = p.stalls.load(Relaxed);
    if stalls != 0 {
        let _ = write!(log, ", FIFO stalls {} (counts low)", stalls);
    }
    let _ = writeln!(log);
    let e8 = p.last_e8.load(Relaxed);
    if e8 != u32::MAX {
        let _ = writeln!(
            log,
            "  last $E8 cycle: offset ${:02x} {} /CFGIN {}",
            snap::a1_a7(e8) << 1,
            if e8 & snap::READ != 0 { "read" } else { "write" },
            bit(e8, 20)
        );
    }
    let lo = pac::SIO.gpio_in(0).read();
    let hi = pac::SIO.gpio_in(1).read();
    let (d, al, ah, ctl) = groups(lo, hi);
    let _ = write!(log, "  now: D {:04x} A7-A1 {:02x} A23-A16 {:02x} |", d, al, ah);
    for (i, name) in CTL_NAMES.iter().enumerate() {
        let _ = write!(log, " {} {}", name, (ctl >> i) & 1);
    }
    let _ = writeln!(log);
    let (sl, sh) = (
        groups(p.seen_low0.load(Relaxed), p.seen_low1.load(Relaxed)),
        groups(p.seen_high0.load(Relaxed), p.seen_high1.load(Relaxed)),
    );
    // seen low AND seen high = moved; seen only one level = static there
    let _ = write!(
        log,
        "  moved: D {:04x} A7-A1 {:02x} A23-A16 {:02x} |",
        sl.0 & sh.0,
        sl.1 & sh.1,
        sl.2 & sh.2
    );
    ctl_list(log, sl.3 & sh.3);
    let _ = write!(log, "; never low: D {:04x} A7-A1 {:02x} A23-A16 {:02x} |", !sl.0 & 0xffff, !sl.1 & 0x7f, !sl.2 & 0xff);
    ctl_list(log, !sl.3 & 0xff);
    let _ = writeln!(log);
    if edges == 0 && ctl & 1 == 0 {
        let _ = writeln!(log, "  /AS held low and no cycles: Amiga off or halted, or /AS open");
    }
}
