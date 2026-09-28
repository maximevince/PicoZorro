//! Zorro II bus slave: pads, the two PIO blocks, and the service loop that
//! answers matched cycles through [`pz_core::window::Window`].
//!
//! PIO1 (GPIOBASE 16) runs the address decode and holds XRDY; PIO0
//! (GPIOBASE 0) captures the cycle and drives read data (`src/zbus.pio`).
//! The service loop runs on one core with nothing else, polls the data SM's
//! RX FIFO and /BUSRST, and drives /INT from the window's interrupt state.
//! Its code sits in RAM (`.data.zbus`), so an XIP cache miss never delays a
//! bus cycle.
//!
//! `wait gpio` indices are relative to the block's GPIOBASE on the RP2350
//! (datasheet, GPIOBASE register) and embassy-rp does not relocate them, so
//! the programs use relative indices. The jump pin and the input-sync bypass
//! are given as GPIO numbers: embassy-rp's `set_config` shifts them by
//! GPIOBASE (embassy git main).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};

#[cfg(feature = "xrdy-direct")]
use embassy_rp::gpio::Output;
use embassy_rp::gpio::{AnyPin, Drive, Flex, Input, Level, Pull, SlewRate};
use embassy_rp::pac;
use embassy_rp::peripherals::{PIO0, PIO1};
use embassy_rp::pio::{
    Common, Config, Direction, ExecConfig, LoadedProgram, Pin as PioPin, PinConfig, ShiftConfig, ShiftDirection,
    StateMachine,
};
use embassy_rp::Peri;
use pio::{InstructionOperands, JmpCondition, MovDestination, MovOperation, MovSource};
use pz_core::pins::{self, cw, match_configured, MATCH_BASE_CONFIGURED, MATCH_BITS, MATCH_BITS_CONFIGURED, MATCH_CONFIG,
    MATCH_NEVER};
use pz_core::slave::{Event, State};
use pz_core::window::Window;

/// Settle time from "matched" to sampling a write's cycle word (reads take
/// none, `zbus.pio`). On a write the strobes and data become valid in S4,
/// up to ~175 ns after /AS (TRM table 3-1). Measured on the Denise with a
/// TF536: data valid by /AS + 28 ns, strobes at /AS + 110-152 ns.
/// "Matched" is ~34 ns after /AS at 240 MHz, so 170 ns samples at
/// ~/AS + 205 ns: the TRM's worst case plus ~30 ns.
const SETTLE_NS: u32 = 170;

/// XRDY path, must match the board: the on-board gate or the GPIO direct.
#[cfg(not(feature = "xrdy-direct"))]
pub const XRDY_MODE: &str = "gate";
#[cfg(feature = "xrdy-direct")]
pub const XRDY_MODE: &str = "GPIO";

/// Loose snapshot for the housekeeping core.
/// Flash writes on core 0 (firmware update, `update_task.rs`) turn XIP off;
/// any flash access from core 1 then bus-faults (datasheet 5.4.8.9). The
/// bus loop runs from RAM, but three of its paths reach flash (checked in
/// the disassembly, docs/UPDATE.md): the Autoconfig ROM table while
/// unconfigured, the reset / shut-up paths, and panics. Those two paths
/// take this handshake; the configured fast path does not.
pub static FLASH_BUSY: AtomicBool = AtomicBool::new(false);
static CORE1_IN_FLASH_PATH: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn enter_flash_path() {
    use core::sync::atomic::Ordering::SeqCst;
    loop {
        CORE1_IN_FLASH_PATH.store(true, SeqCst);
        if !FLASH_BUSY.load(SeqCst) {
            return;
        }
        CORE1_IN_FLASH_PATH.store(false, SeqCst);
        while FLASH_BUSY.load(SeqCst) {}
    }
}

#[inline(always)]
fn leave_flash_path() {
    CORE1_IN_FLASH_PATH.store(false, core::sync::atomic::Ordering::SeqCst);
}

/// Core 0: run `f` (a flash erase / program) while core 1 stays out of
/// flash. Core 1 waits at most as long as `f` takes.
pub fn flash_exclusive<R>(f: impl FnOnce() -> R) -> R {
    use core::sync::atomic::Ordering::SeqCst;
    FLASH_BUSY.store(true, SeqCst);
    while CORE1_IN_FLASH_PATH.load(SeqCst) {}
    let r = f();
    FLASH_BUSY.store(false, SeqCst);
    r
}

pub struct Stats {
    pub reads: AtomicU32,
    pub writes: AtomicU32,
    pub resets: AtomicU32,
    pub boot_us: AtomicU32,
    pub armed: AtomicBool,
    /// 0 unconfigured, 1 configured, 2 shut up
    pub state: AtomicU8,
    pub base: AtomicU8,
}

/// `bus-prof`: core 1 cycle counts (DWT CYCCNT, clk_sys), published every
/// 2^16 bus cycles: [read sum, read n, read max, write sum, write n, write
/// max, idle pass max, idle pass sum, idle passes]. An idle pass is the
/// longest a cycle can wait before core 1 sees it.
#[cfg(feature = "bus-prof")]
pub static PROF: [AtomicU32; 9] = [const { AtomicU32::new(0) }; 9];

pub static STATS: Stats = Stats {
    reads: AtomicU32::new(0),
    writes: AtomicU32::new(0),
    resets: AtomicU32::new(0),
    boot_us: AtomicU32::new(0),
    armed: AtomicBool::new(false),
    state: AtomicU8::new(0),
    base: AtomicU8::new(0),
};

fn bump(a: &AtomicU32) {
    a.store(a.load(Relaxed).wrapping_add(1), Relaxed);
}

/// Every pin the slave uses. PIO pins are made by the caller
/// (`Common::make_pio_pin` needs the concrete pin types).
pub struct BusPins {
    /// D0-D15, PIO0
    pub d: [PioPin<'static, PIO0>; 16],
    /// /AS /UDS /LDS READ A1-A7 (GPIO16-26): plain inputs, both blocks read them
    pub strobes_a1_a7: [Peri<'static, AnyPin>; 11],
    /// A16-A23, /CFGIN (GPIO28-36), PIO1
    pub match_field: [PioPin<'static, PIO1>; 9],
    #[cfg(not(feature = "xrdy-direct"))]
    pub xrdy: Peri<'static, AnyPin>,
    #[cfg(not(feature = "xrdy-direct"))]
    pub arm: PioPin<'static, PIO1>,
    #[cfg(feature = "xrdy-direct")]
    pub xrdy: PioPin<'static, PIO1>,
    #[cfg(feature = "xrdy-direct")]
    pub arm: Peri<'static, AnyPin>,
    pub cfgout: Peri<'static, AnyPin>,
    pub busrst: Peri<'static, AnyPin>,
    pub int: Peri<'static, AnyPin>,
}

/// The bus slave, built on core 0 and then moved to the core that serves it.
pub struct Zbus {
    data: StateMachine<'static, PIO0, 0>,
    decode: StateMachine<'static, PIO1, 0>,
    data_origin: u8,
    decode_origin: u8,
    settle_cycles: u32,
    cfgout: Flex<'static>,
    int: Flex<'static>,
    int_asserted: bool,
    window: Window<'static>,
    #[cfg(feature = "xrdy-direct")]
    xrdy: PioPin<'static, PIO1>,
    _keep: KeepAlive,
}

/// Pins that only need to stay configured (never touched after init).
struct KeepAlive {
    _d: [PioPin<'static, PIO0>; 16],
    _inputs: [Input<'static>; 12],
    _match: [PioPin<'static, PIO1>; 9],
    #[cfg(not(feature = "xrdy-direct"))]
    _arm: PioPin<'static, PIO1>,
    #[cfg(feature = "xrdy-direct")]
    _arm: Output<'static>,
}

/// Instructions executed on a state machine from the CPU, encoded at compile
/// time (the service loop must not call into flash).
const INSTR_PULL_NOBLOCK: u16 = InstructionOperands::PULL { if_empty: false, block: false }.encode();
const INSTR_MOV_Y_OSR: u16 =
    InstructionOperands::MOV { destination: MovDestination::Y, op: MovOperation::None, source: MovSource::OSR }
        .encode();
const INSTR_JMP_ALWAYS: u16 = InstructionOperands::JMP { condition: JmpCondition::Always, address: 0 }.encode();

#[inline(always)]
fn exec<P: embassy_rp::pio::Instance, const SM: usize>(sm: &mut StateMachine<'static, P, SM>, instr: u16) {
    // SAFETY: plain PULL / MOV / JMP instructions on our own state machine.
    unsafe { sm.exec_instr(instr) }
}

/// Put `value` into Y (through the TX FIFO and OSR).
/// Inlined: it also runs inside the configuring bus cycle.
#[inline(always)]
fn load_y<P: embassy_rp::pio::Instance, const SM: usize>(sm: &mut StateMachine<'static, P, SM>, value: u32) {
    while !sm.tx().try_push(value) {}
    exec(sm, INSTR_PULL_NOBLOCK);
    exec(sm, INSTR_MOV_Y_OSR);
}

fn jmp<P: embassy_rp::pio::Instance, const SM: usize>(sm: &mut StateMachine<'static, P, SM>, to: u8) {
    exec(sm, INSTR_JMP_ALWAYS | u16::from(to & 0x1f));
}

#[inline(always)]
fn gpio_high(pin: u8) -> bool {
    pac::SIO.gpio_in((pin / 32) as usize).read() & (1 << (pin % 32)) != 0
}

impl Zbus {
    /// Pads and PIO with the slave silent (match = never). Nothing slow before
    /// this: it is the cold-start path.
    pub fn new(
        pio0: &mut Common<'static, PIO0>,
        mut data: StateMachine<'static, PIO0, 0>,
        pio1: &mut Common<'static, PIO1>,
        mut decode: StateMachine<'static, PIO1, 0>,
        p: BusPins,
        window: Window<'static>,
    ) -> Self {
        // --- pads. Every bus pin a plain input with no pull (the pad reset
        // state has the pull-down on, which must not load the Amiga bus).
        let [s0, s1, s2, s3, s4, s5, s6, s7, s8, s9, s10] = p.strobes_a1_a7;
        let mut inputs = [
            Input::new(s0, Pull::None),
            Input::new(s1, Pull::None),
            Input::new(s2, Pull::None),
            Input::new(s3, Pull::None),
            Input::new(s4, Pull::None),
            Input::new(s5, Pull::None),
            Input::new(s6, Pull::None),
            Input::new(s7, Pull::None),
            Input::new(s8, Pull::None),
            Input::new(s9, Pull::None),
            Input::new(s10, Pull::None),
            Input::new(p.busrst, Pull::None),
        ];
        for i in inputs.iter_mut() {
            i.set_schmitt(true);
        }

        let mut d = p.d;
        for pin in d.iter_mut() {
            pin.set_pull(Pull::None);
            pin.set_schmitt(true);
            pin.set_drive_strength(Drive::_8mA);
            pin.set_slew_rate(SlewRate::Slow);
        }
        let mut mf = p.match_field;
        for pin in mf.iter_mut() {
            pin.set_pull(Pull::None);
            pin.set_schmitt(true);
        }

        // /CFGOUT: high-Z until the first time /BUSRST reads high, i.e. the
        // Amiga is powered (a USB-powered card in a dead machine must not
        // drive its bus). From then on push-pull: negated (high)
        // until configured or shut up.
        let mut cfgout = Flex::new(p.cfgout);
        cfgout.set_pull(Pull::None);
        cfgout.set_high();
        cfgout.set_as_input();
        // /INT: open drain by direction, released. Value 0, input for now.
        // With `int-nfet` an N-MOSFET pulls /INT: push-pull,
        // high asserts, low (the pad's reset pull-down too) releases.
        let mut int = Flex::new(p.int);
        int.set_pull(Pull::None);
        int.set_low();
        if cfg!(feature = "int-nfet") {
            int.set_as_output();
        } else {
            int.set_as_input();
        }

        // --- programs
        let prg_data = pio::pio_file!("src/zbus.pio", select_program("pz_data"));
        let data_prog: LoadedProgram<'static, PIO0> = pio0.load_program(&prg_data.program);

        let mut dc = Config::default();
        dc.use_program(&data_prog, &[]);
        dc.set_out_pins(&d.each_ref());
        // in_base D0 without claiming GPIO16-26 for PIO0 (both blocks read
        // them); `in pins, 29` has its own count, IN_COUNT stays 0 = all.
        let mut pc: PinConfig = dc.get_pins();
        pc.in_base = pins::D0;
        // SAFETY: base only; the pins are inputs.
        unsafe { dc.set_pins(pc) };
        let mut ec: ExecConfig = dc.get_exec();
        ec.jmp_pin = Some(pins::READ);
        unsafe { dc.set_exec(ec) };
        dc.shift_in = ShiftConfig { threshold: cw::BITS as u8, direction: ShiftDirection::Left, auto_fill: true };
        dc.shift_out = ShiftConfig { threshold: 32, direction: ShiftDirection::Right, auto_fill: true };
        data.set_config(&dc);
        data.set_pin_dirs(Direction::In, &d.each_ref());

        #[cfg(not(feature = "xrdy-direct"))]
        let (decode_prog, arm, _no_xrdy) = {
            // GPIO27 is not on the bus XRDY here: keep it from floating.
            let x = Flex::new(p.xrdy);
            let mut x = x;
            x.set_pull(Pull::Down);
            x.set_as_input();
            core::mem::forget(x);

            let prg = pio::pio_file!("src/zbus.pio", select_program("pz_decode_gate"));
            let prog = pio1.load_program(&prg.program);
            let mut arm = p.arm;
            arm.set_pull(Pull::Up);
            let mut c = Config::default();
            c.use_program(&prog, &[&arm]);
            c.set_in_pins(&mf.each_ref());
            // The match field is only sampled after /AS came through its own
            // synchroniser, so it is stable by then; the bypass makes it two
            // cycles fresher (matters on a PiStorm). /AS stays synchronised.
            c.set_input_sync_bypass(&mf.each_ref());
            let mut ec = c.get_exec();
            ec.jmp_pin = Some(pins::AS);
            unsafe { c.set_exec(ec) };
            decode.set_config(&c);
            // /ARM idles high (disarmed); the board also pulls it up.
            decode.set_pins(Level::High, &[&arm]);
            decode.set_pin_dirs(Direction::Out, &[&arm]);
            (prog, arm, ())
        };
        #[cfg(feature = "xrdy-direct")]
        let (decode_prog, arm, xrdy) = {
            let prg = pio::pio_file!("src/zbus.pio", select_program("pz_decode"));
            let prog = pio1.load_program(&prg.program);
            let mut x = p.xrdy;
            // Denise pulls XRDY up with 470 R to 5 V: 10.6 mA to sink.
            x.set_drive_strength(Drive::_12mA);
            x.set_slew_rate(SlewRate::Fast);
            x.set_pull(Pull::None);
            let mut c = Config::default();
            c.use_program(&prog, &[]);
            c.set_set_pins(&[&x]);
            c.set_in_pins(&mf.each_ref());
            // The match field is only sampled after /AS came through its own
            // synchroniser, so it is stable by then; the bypass makes it two
            // cycles fresher (matters on a PiStorm). /AS stays synchronised.
            c.set_input_sync_bypass(&mf.each_ref());
            decode.set_config(&c);
            // value 0, direction in: `set pindirs` toggles pull-low / float
            decode.set_pins(Level::Low, &[&x]);
            decode.set_pin_dirs(Direction::In, &[&x]);
            let arm = Output::new(p.arm, Level::High);
            (prog, arm, x)
        };

        let hz = embassy_rp::clocks::clk_sys_freq();
        let settle_cycles = ((SETTLE_NS as u64 * hz as u64).div_ceil(1_000_000_000)) as u32;

        let mut z = Zbus {
            data,
            decode,
            data_origin: data_prog.origin,
            decode_origin: decode_prog.origin,
            settle_cycles,
            cfgout,
            int,
            int_asserted: false,
            window,
            #[cfg(feature = "xrdy-direct")]
            xrdy,
            _keep: KeepAlive { _d: d, _inputs: inputs, _match: mf, _arm: arm },
        };
        if z.window.slave.state == State::Configured && gpio_high(pins::BUSRST) {
            // Rebooted into a new image by an update (docs/UPDATE.md): the
            // Amiga still has the board at this base, so carry on there.
            z.cfgout.set_low();
            z.cfgout.set_as_output();
            z.rewind(match_configured(z.window.slave.base), true);
            z.publish_state();
            z.window.slave.boot_us = embassy_time::Instant::now().as_micros() as u32;
            STATS.boot_us.store(z.window.slave.boot_us, Relaxed);
            STATS.armed.store(true, Relaxed);
            return z;
        }
        z.rewind(MATCH_NEVER, false);
        // BOOT_US in the window (REGISTERS.md).
        z.window.slave.boot_us = embassy_time::Instant::now().as_micros() as u32;
        STATS.boot_us.store(z.window.slave.boot_us, Relaxed);
        STATS.armed.store(false, Relaxed);
        z
    }

    fn data_load_settle(&mut self) {
        load_y(&mut self.data, self.settle_cycles);
        // load_y left a full OSR behind; restart empties it so the first `out`
        // autopulls a real reply. X and Y survive a restart.
        self.data.restart();
        let o = self.data_origin;
        jmp(&mut self.data, o);
    }

    /// Which address lines the decode compares: {/CFGIN, A23..A16} while
    /// unconfigured, {/CFGIN, A23..A17} once configured (128 KiB; A16 goes
    /// to the data phase). PINCTRL.IN_BASE is relative to GPIOBASE 16.
    #[inline(always)]
    fn decode_field(configured: bool) {
        let (base, count) = if configured {
            (MATCH_BASE_CONFIGURED, MATCH_BITS_CONFIGURED)
        } else {
            (pins::A16, MATCH_BITS)
        };
        let sm = pac::PIO1.sm(0);
        sm.pinctrl().modify(|w| w.set_in_base(base - 16));
        sm.shiftctrl().modify(|w| w.set_in_count(count as u8));
    }

    /// Both state machines back to their entry points, bus released.
    fn rewind(&mut self, match_value: u32, configured: bool) {
        self.decode.set_enable(false);
        self.data.set_enable(false);

        // data pins released
        self.data.set_pin_dirs(Direction::In, &self._keep._d.each_ref());
        #[cfg(feature = "xrdy-direct")]
        self.decode.set_pin_dirs(Direction::In, &[&self.xrdy]);
        self.data.clear_fifos();
        self.decode.clear_fifos();
        pac::PIO0.irq().write_value(pac::pio::regs::Irq(0xff));
        pac::PIO1.irq().write_value(pac::pio::regs::Irq(0xff));

        self.decode.restart();
        let o = self.decode_origin;
        jmp(&mut self.decode, o);
        Self::decode_field(configured);
        load_y(&mut self.decode, match_value);
        self.data_load_settle();

        self.data.set_enable(true);
        self.decode.set_enable(true);
    }

    fn publish_state(&self) {
        let s = match self.window.slave.state {
            State::Unconfigured => 0,
            State::Configured => 1,
            State::ShutUp => 2,
        };
        STATS.state.store(s, Relaxed);
        STATS.base.store(self.window.slave.base, Relaxed);
        // For an update reboot: the base to come back at.
        let base = if s == 1 { u16::from(self.window.slave.base) } else { 0xffff };
        crate::update_task::BASE.store(base, Relaxed);
    }

    fn handle_reset(&mut self) {
        self.cfgout.set_high();
        enter_flash_path();
        self.rewind(MATCH_NEVER, false);
        self.window.reset();
        leave_flash_path();
        self.set_int(false);
        bump(&STATS.resets);
        self.publish_state();
        while !gpio_high(pins::BUSRST) {}
        // The Amiga is up: /CFGOUT is driven from here on (no-op after the
        // first reset).
        self.cfgout.set_as_output();
        enter_flash_path();
        self.rewind(MATCH_CONFIG, false);
        leave_flash_path();
        STATS.armed.store(true, Relaxed);
    }

    /// /CFGOUT goes active only after the configuring cycle has ended (TRM).
    #[inline(always)]
    fn cfgout_after_cycle(&mut self) {
        let mut guard = 0u32;
        while !gpio_high(pins::AS) && guard < 1_000_000 {
            guard += 1;
        }
        self.cfgout.set_low();
    }

    #[inline(always)]
    fn set_int(&mut self, on: bool) {
        if on != self.int_asserted {
            self.int_asserted = on;
            if cfg!(feature = "int-nfet") {
                self.int.set_level(if on { Level::High } else { Level::Low });
            } else if on {
                self.int.set_as_output(); // value 0: pulls /INT low
            } else {
                self.int.set_as_input();
            }
        }
    }

    #[inline(always)]
    fn serve_cycle(&mut self, w: u32) {
        let reg = cw::reg(w);
        let uds = w & cw::UDS == 0;
        // Sampled but without effect: pz-core decides on /UDS alone (the One
        // TH has no /LDS; slave::Slave::write).
        let lds = w & cw::LDS == 0;
        // Unconfigured, the window reads the Autoconfig ROM table and a
        // write may shut the board up (reset paths): both in flash.
        let slow = self.window.slave.state != State::Configured;
        if slow {
            enter_flash_path();
        }
        let r = self.serve_cycle_inner(w, reg, uds, lds);
        if slow {
            leave_flash_path();
        }
        r
    }

    #[inline(always)]
    fn serve_cycle_inner(&mut self, w: u32, reg: u8, uds: bool, lds: bool) {
        let a16 = cw::a16(w);
        // Hot registers first, past the generic dispatch.
        if !a16 && uds {
            if w & cw::READ != 0 {
                if let Some(v) = self.window.read_fast(reg) {
                    pac::PIO0.txf(0).write_value(0xffff_0000 | u32::from(v));
                    bump(&STATS.reads);
                    return;
                }
            } else if Window::is_fast_write_reg(reg) && self.window.fast_ok() {
                // Finish the cycle first, store after: these writes change
                // nothing the next cycle depends on, and the next cycle is
                // only served once this one is done (measured: TX_DATA
                // released XRDY ~30 ns after one of Gary's 7 MHz edges).
                pac::PIO0.txf(0).write_value(0); // ack: finish the cycle
                self.window.write_fast(reg, cw::data(w));
                bump(&STATS.writes);
                return;
            }
        }
        if w & cw::READ != 0 {
            let v = self.window.read_at(a16, reg, uds, lds);
            pac::PIO0.txf(0).write_value(0xffff_0000 | u32::from(v));
            bump(&STATS.reads);
            return;
        }
        let ev = self.window.write_at(a16, reg, cw::data(w), uds, lds);
        match ev {
            Event::Configured => {
                // The decode SM waits for PZ_IRQ_DONE: neither change can
                // meet a half-done compare.
                Self::decode_field(true);
                load_y(&mut self.decode, match_configured(self.window.slave.base));
            }
            Event::ShutUp => load_y(&mut self.decode, MATCH_NEVER),
            Event::None => {}
        }
        pac::PIO0.txf(0).write_value(0); // ack: finish the cycle
        bump(&STATS.writes);
        if ev != Event::None {
            self.cfgout_after_cycle();
            self.publish_state();
        }
    }

    /// Never returns. Answers matched cycles, tracks /BUSRST, drives /INT.
    #[inline(never)]
    #[link_section = ".data.zbus"]
    pub fn serve_forever(mut self) -> ! {
        let mut spin = 0u32;
        #[cfg(feature = "bus-prof")]
        let mut prof = [0u32; 9];
        #[cfg(feature = "bus-prof")]
        // SAFETY: core 1's own DWT (DEMCR.TRCENA, DWT_CTRL.CYCCNTENA).
        unsafe {
            core::ptr::write_volatile(0xe000_edfc as *mut u32, core::ptr::read_volatile(0xe000_edfc as *const u32) | 1 << 24);
            core::ptr::write_volatile(0xe000_1000 as *mut u32, core::ptr::read_volatile(0xe000_1000 as *const u32) | 1);
        }
        loop {
            #[cfg(feature = "bus-prof")]
            let t0 = cyccnt();
            #[cfg(feature = "bus-prof")]
            let mut idle = false;
            if pac::PIO0.fstat().read().rxempty() & 1 == 0 {
                let w = pac::PIO0.rxf(0).read();
                #[cfg(feature = "bus-prof")]
                let t1 = cyccnt();
                self.serve_cycle(w);
                #[cfg(feature = "bus-prof")]
                {
                    let d = cyccnt().wrapping_sub(t1);
                    let k = if w & cw::READ != 0 { 0 } else { 3 };
                    prof[k] = prof[k].wrapping_add(d);
                    prof[k + 1] += 1;
                    prof[k + 2] = prof[k + 2].max(d);
                    if prof[1] + prof[4] >= 1 << 16 {
                        for (a, v) in PROF.iter().zip(prof.iter_mut()) {
                            a.store(*v, Relaxed);
                            *v = 0;
                        }
                    }
                }
            } else {
                #[cfg(feature = "bus-prof")]
                {
                    // An idle pass, ended below: the worst-case wait for a
                    // cycle that lands just after the FIFO check.
                    prof[8] = prof[8].wrapping_add(1);
                    idle = true;
                }
            }
            // /BUSRST and /INT every 32nd pass only: they cost ~78 cycles,
            // and a cycle landing just after the FIFO check waited all of
            // that. A reset lasts ms, an interrupt can wait ~1 us.
            spin = spin.wrapping_add(1);
            if spin & 31 == 0 {
                if !gpio_high(pins::BUSRST) {
                    self.handle_reset();
                }
                let irq = self.window.irq_asserted();
                self.set_int(irq);
            }
            #[cfg(feature = "bus-prof")]
            {
                if idle {
                    let d = cyccnt().wrapping_sub(t0);
                    prof[6] = prof[6].max(d);
                    prof[7] = prof[7].wrapping_add(d);
                }
            }
        }
    }
}

#[cfg(feature = "bus-prof")]
#[inline(always)]
fn cyccnt() -> u32 {
    // SAFETY: DWT CYCCNT, read only.
    unsafe { core::ptr::read_volatile(0xe000_1004 as *const u32) }
}
