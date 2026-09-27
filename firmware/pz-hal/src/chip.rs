//! Ethernet chip bring-up on SPI, shared by the firmware images that use
//! the chip (`pz-app`: `picozorro`, `nic-demo`). Built with the `pz-hal`
//! feature `nic-w5500` or `nic-enc28j60`.
//!
//! Wiring on the PicoZorro boards (hardware/PINMAP.md),
//! [`chip_pins!`](crate::chip_pins)`(p)`:
//!
//!   GPIO40  MISO      GPIO41  /CS       GPIO42  SCK      GPIO43  MOSI   (SPI1)
//!   GPIO39  W5500 /INT (shared with the module LED; the ENC28J60 driver polls)
//!   GPIO47  never: on the Core2350B it is the module's PSRAM chip select
//!
//! Other boards name their own SPI instance and pins with the long form of
//! the macro. [`Pins`] is generic over
//! the SPI instance and its three pins; /CS and /INT are any GPIO.
//!
//! No reset pin: the boards tie the W5500 /RST to RUN and the devkits have
//! their own; the driver resets the chip in software (MR.RST) anyway.
//!
//! Chip: cargo feature `nic-w5500` or `nic-enc28j60` (`pz-app` forwards its
//! own features of the same names, `nic-w5500` by default).
//! `nic-poll` (W5500): no /INT, the runner polls the RX size every
//! `POLL_US` (500 us) instead (~6 % of core 0 idle, +0.2 ms per frame);
//! the /INT pin stays free.
//!
//! [`chip_pins!`](crate::chip_pins) takes the pins out of
//! `embassy_rp::Peripherals` and binds the DMA interrupt of the SPI's two
//! channels (DMA_IRQ_0) in the binary that calls it, so a firmware that
//! only links this crate keeps DMA_IRQ_0 for itself.

use defmt::{info, unwrap};
use embassy_executor::Spawner;
use embassy_rp::gpio::{AnyPin, Level, Output};
use embassy_rp::dma;
use embassy_rp::time::Hertz;
use embassy_rp::interrupt::typelevel::{Binding, DMA_IRQ_0};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1};
use embassy_rp::spi::{self, ClkPin, Config as SpiConfig, MisoPin, MosiPin, Spi};
use embassy_rp::Peri;
use embassy_time::Delay;
use embedded_hal_bus::spi::ExclusiveDevice;

#[cfg(all(feature = "nic-w5500", feature = "nic-enc28j60"))]
compile_error!("select one chip feature: nic-w5500 or nic-enc28j60");

/// The peripherals the chip uses, moved out of `embassy_rp::Peripherals`:
/// the SPI instance `S` with its SCK / MOSI / MISO pins, /CS and /INT as
/// any GPIO. The default parameters are the PicoZorro wiring (SPI1,
/// GPIO42 / 43 / 40), which exists on the RP2350B only.
#[cfg(feature = "rp235xb")]
pub struct Pins<
    S: spi::Instance + 'static = embassy_rp::peripherals::SPI1,
    Sck: ClkPin<S> + 'static = embassy_rp::peripherals::PIN_42,
    Mosi: MosiPin<S> + 'static = embassy_rp::peripherals::PIN_43,
    Miso: MisoPin<S> + 'static = embassy_rp::peripherals::PIN_40,
> {
    pub spi: Peri<'static, S>,
    pub miso: Peri<'static, Miso>,
    pub cs: Peri<'static, AnyPin>,
    pub sck: Peri<'static, Sck>,
    pub mosi: Peri<'static, Mosi>,
    pub int: Peri<'static, AnyPin>,
    pub dma0: Peri<'static, DMA_CH0>,
    pub dma1: Peri<'static, DMA_CH1>,
    /// Proof that DMA_IRQ_0 runs the two channels' handlers.
    pub irqs: Irqs,
    /// For the log: "SPI1", "SPI0".
    pub bus: &'static str,
}

/// As on the RP2350B, without the default wiring (the RP2350A has no GPIO40-43).
#[cfg(not(feature = "rp235xb"))]
pub struct Pins<S: spi::Instance + 'static, Sck: ClkPin<S> + 'static, Mosi: MosiPin<S> + 'static, Miso: MisoPin<S> + 'static> {
    pub spi: Peri<'static, S>,
    pub miso: Peri<'static, Miso>,
    pub cs: Peri<'static, AnyPin>,
    pub sck: Peri<'static, Sck>,
    pub mosi: Peri<'static, Mosi>,
    pub int: Peri<'static, AnyPin>,
    pub dma0: Peri<'static, DMA_CH0>,
    pub dma1: Peri<'static, DMA_CH1>,
    /// Proof that DMA_IRQ_0 runs the two channels' handlers.
    pub irqs: Irqs,
    /// For the log: "SPI1", "SPI0".
    pub bus: &'static str,
}

/// DMA_IRQ_0 bound to the handlers of DMA_CH0 and DMA_CH1: made only from
/// a type that `bind_interrupts!` bound so ([`Irqs::new`]; `chip_pins!`
/// does it).
#[derive(Clone, Copy)]
pub struct Irqs(());

impl Irqs {
    pub fn new<I>(_bound: I) -> Self
    where
        I: Binding<DMA_IRQ_0, dma::InterruptHandler<DMA_CH0>> + Binding<DMA_IRQ_0, dma::InterruptHandler<DMA_CH1>>,
    {
        Irqs(())
    }
}

// SAFETY: an `Irqs` exists only after `Irqs::new` saw a type with both
// bindings, which `bind_interrupts!` gives only with the handler installed.
unsafe impl Binding<DMA_IRQ_0, dma::InterruptHandler<DMA_CH0>> for Irqs {}
// SAFETY: as above.
unsafe impl Binding<DMA_IRQ_0, dma::InterruptHandler<DMA_CH1>> for Irqs {}

/// For `chip_pins!`: the HAL as the calling crate cannot name it.
#[doc(hidden)]
pub use embassy_rp as __rp;

/// Reset pin for drivers that insist on one: there is none (see above).
pub struct NoReset;

impl embedded_hal::digital::ErrorType for NoReset {
    type Error = core::convert::Infallible;
}

impl embedded_hal::digital::OutputPin for NoReset {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// The chip's [`Pins`] out of `embassy_rp::Peripherals` `$p`, with
/// DMA_IRQ_0 bound here (once per firmware image).
///
/// - `chip_pins!(p)`: the PicoZorro wiring, SPI1 GPIO42 SCK / 43 MOSI /
///   40 MISO, /CS GPIO41, /INT GPIO39 (RP2350B).
/// - `chip_pins!(p, SPI0, sck: PIN_22, mosi: PIN_23, miso: PIN_20, cs: PIN_9,
///   int: PIN_6)`: any other SPI instance and pins.
#[macro_export]
macro_rules! chip_pins {
    ($p:ident) => {
        $crate::chip_pins!($p, SPI1, sck: PIN_42, mosi: PIN_43, miso: PIN_40, cs: PIN_41, int: PIN_39)
    };
    ($p:ident, $spi:ident, sck: $sck:ident, mosi: $mosi:ident, miso: $miso:ident, cs: $cs:ident, int: $int:ident) => {{
        $crate::chip::__rp::bind_interrupts!(struct ChipDmaIrqs {
            DMA_IRQ_0 => $crate::chip::__rp::dma::InterruptHandler<$crate::chip::__rp::peripherals::DMA_CH0>,
                $crate::chip::__rp::dma::InterruptHandler<$crate::chip::__rp::peripherals::DMA_CH1>;
        });
        $crate::chip::Pins {
            spi: $p.$spi,
            miso: $p.$miso,
            cs: $p.$cs.into(),
            sck: $p.$sck,
            mosi: $p.$mosi,
            int: $p.$int.into(),
            dma0: $p.DMA_CH0,
            dma1: $p.DMA_CH1,
            irqs: $crate::chip::Irqs::new(ChipDmaIrqs),
            bus: stringify!($spi),
        }
    }};
}

/// SCK requested from embassy's SPI divider; `PZ_SPI_HZ` at build time.
const SPI_HZ: u32 = match option_env!("PZ_SPI_HZ") {
    Some(s) => match u32::from_str_radix(s, 10) {
        Ok(v) => v,
        Err(_) => panic!("PZ_SPI_HZ: not a number"),
    },
    None => 36_000_000,
};

/// The SCK the chip's SPI runs at (after `bring_up`'s divider).
pub fn spi_sck_hz() -> u32 {
    sck_hz(SPI_HZ)
}

/// The SCK embassy-rp's divider gives for `want` (its `calc_prescs`:
/// clk_peri / (2 * presc) / postdiv, rounded so it never exceeds `want`).
fn sck_hz(want: u32) -> u32 {
    let peri = embassy_rp::clocks::clk_peri_freq();
    let ratio = peri.div_ceil(want * 2);
    let presc = ratio.div_ceil(256);
    let postdiv = if presc == 1 { ratio } else { ratio.div_ceil(presc) };
    peri / (2 * presc * postdiv)
}

// --------------------------------------------------------------- W5500 path
#[cfg(feature = "nic-w5500")]
mod imp {
    use super::*;
    use defmt::warn;
    use embassy_net_wiznet::chip::W5500;
    use embassy_net_wiznet::{Control, Device, Runner, State};
    #[cfg(not(feature = "nic-poll"))]
    use embassy_rp::gpio::{Input, Pull};
    use embassy_rp::mode::Async;

    pub type SpiDev = ExclusiveDevice<Spi<'static, Async>, Output<'static>, Delay>;
    pub type NetDevice = Device<'static>;
    #[allow(dead_code)]
    pub const NAME: &str = "W5500";

    static CONTROL: Control = Control::new();

    #[cfg(not(feature = "nic-poll"))]
    type IntPin = Input<'static>;
    #[cfg(feature = "nic-poll")]
    type IntPin = poll::PollInt;

    #[embassy_executor::task]
    async fn ethernet_task(runner: Runner<'static, W5500, SpiDev, IntPin, NoReset>) -> ! {
        runner.run_with_control(&CONTROL).await
    }

    /// /INT replaced by a timer. The runner waits for /INT only when
    /// the chip said its RX buffer is empty (vendored lib.rs, run_inner);
    /// each "wake-up" then costs one RX-size read over SPI. The time from
    /// one wait's end to the next wait's start is that cost (plus any TX in
    /// between: an upper bound), counted here.
    #[cfg(feature = "nic-poll")]
    pub mod poll {
        use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
        use embassy_time::{Duration, Instant, Timer};

        /// Poll period; `PZ_POLL_US` at build time overrides it (bench).
        pub const POLL_US: u64 = match option_env!("PZ_POLL_US") {
            Some(v) => parse(v),
            None => 500,
        };

        const fn parse(s: &str) -> u64 {
            let b = s.as_bytes();
            let mut v = 0;
            let mut i = 0;
            while i < b.len() {
                v = v * 10 + (b[i] - b'0') as u64;
                i += 1;
            }
            v
        }
        pub static POLLS: AtomicU32 = AtomicU32::new(0);
        pub static BUSY_US: AtomicU32 = AtomicU32::new(0);

        #[derive(Default)]
        pub struct PollInt {
            last: Option<Instant>,
        }

        impl PollInt {
            pub fn new() -> Self {
                PollInt { last: None }
            }

            async fn tick(&mut self) {
                if let Some(t) = self.last {
                    BUSY_US.fetch_add(t.elapsed().as_micros() as u32, Relaxed);
                    POLLS.fetch_add(1, Relaxed);
                }
                Timer::after(Duration::from_micros(POLL_US)).await;
                self.last = Some(Instant::now());
            }
        }

        impl embedded_hal::digital::ErrorType for PollInt {
            type Error = core::convert::Infallible;
        }

        impl embedded_hal_async::digital::Wait for PollInt {
            async fn wait_for_high(&mut self) -> Result<(), Self::Error> {
                self.tick().await;
                Ok(())
            }
            async fn wait_for_low(&mut self) -> Result<(), Self::Error> {
                self.tick().await;
                Ok(())
            }
            async fn wait_for_rising_edge(&mut self) -> Result<(), Self::Error> {
                self.tick().await;
                Ok(())
            }
            async fn wait_for_falling_edge(&mut self) -> Result<(), Self::Error> {
                self.tick().await;
                Ok(())
            }
            async fn wait_for_any_edge(&mut self) -> Result<(), Self::Error> {
                self.tick().await;
                Ok(())
            }
        }
    }

    /// (polls, busy microseconds) so far; (0, 0) with /INT.
    #[allow(dead_code)]
    pub fn poll_stats() -> (u32, u32) {
        #[cfg(feature = "nic-poll")]
        {
            use core::sync::atomic::Ordering::Relaxed;
            (poll::POLLS.load(Relaxed), poll::BUSY_US.load(Relaxed))
        }
        #[cfg(not(feature = "nic-poll"))]
        (0, 0)
    }

    /// `None` when no W5500 answers on the SPI (VERSIONR is not 0x04: with
    /// no chip, MISO reads all 0s or all 1s). Takes ~100 ms either way.
    pub async fn bring_up<S: spi::Instance, Sck: ClkPin<S>, Mosi: MosiPin<S>, Miso: MisoPin<S>>(
        spawner: &Spawner,
        p: Pins<S, Sck, Mosi, Miso>,
        mac: [u8; 6],
    ) -> Option<NetDevice> {
        let mut cfg = SpiConfig::default();
        // datasheet 5.5.4: 33.3 MHz guaranteed, 80 MHz "theoretical". 36 MHz
        // (clk_peri 144 / 4, picozorro overclocked; 150 / 6 = 25 at stock
        // clocks) ran 131k frames clean over flying leads; at 48 and 72 MHz
        // VERSIONR did not read back. PZ_SPI_HZ=25000000 turns it down; the
        // divider rounds to clk_peri / 2 / n at most.
        cfg.frequency = Hertz(SPI_HZ);
        let hz = sck_hz(cfg.frequency.0);
        let spi = unwrap!(Spi::new(p.spi, p.sck, p.mosi, p.miso, p.dma0, p.dma1, p.irqs, cfg).ok());
        let cs = Output::new(p.cs, Level::High);
        // /INT (GPIO39 on the PicoZorro wiring). INTn is push-pull, so
        // there it also drives the module LED; the pull-up only matters
        // without a chip.
        #[cfg(not(feature = "nic-poll"))]
        let int = Input::new(p.int, Pull::Up);
        #[cfg(feature = "nic-poll")]
        let int = {
            let _ = p.int; // not used: GPIO39 stays free for other uses
            poll::PollInt::new()
        };

        static STATE: static_cell::StaticCell<State<8, 8>> = static_cell::StaticCell::new();
        let state = STATE.init(State::<8, 8>::new());
        let bus = unwrap!(ExclusiveDevice::new(spi, cs, Delay));
        let (device, runner) = match embassy_net_wiznet::new(mac, state, bus, int, NoReset).await {
            Ok(x) => x,
            Err(embassy_net_wiznet::InitError::InvalidChipVersion { actual, .. }) => {
                info!("no W5500 on {} (VERSIONR reads {=u8:#04x})", p.bus, actual);
                return None;
            }
            Err(embassy_net_wiznet::InitError::SpiError(_)) => {
                warn!("W5500: SPI error during init");
                return None;
            }
        };
        spawner.spawn(unwrap!(ethernet_task(runner)));
        info!("W5500 up on {} at {} Hz", p.bus, hz);
        Some(device)
    }

    /// Chip MAC filter (W5500 Sn_MR.MFEN): on = own unicast + broadcast only.
    /// Applied by the runner within 500 ms.
    #[allow(dead_code)]
    pub fn set_mac_filter(on: bool) {
        CONTROL.set_mac_filter(on);
    }

    /// (100 Mbit/s, full duplex) from the last PHYCFGR read.
    #[allow(dead_code)]
    pub fn phy_speed_duplex() -> (bool, bool) {
        let v = CONTROL.phy_cfg();
        (v & 0b010 != 0, v & 0b100 != 0)
    }

    /// Frames read after a wake-up by /INT.
    #[allow(dead_code)]
    pub fn int_wakeups() -> u32 {
        CONTROL.int_wakeups()
    }
}

// ------------------------------------------------------------ ENC28J60 path
#[cfg(feature = "nic-enc28j60")]
mod imp {
    use super::*;
    use embassy_net_enc28j60::Enc28j60;
    use embassy_rp::mode::Blocking;

    pub type SpiDev = ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>;
    pub type NetDevice = Enc28j60<SpiDev, NoReset>;
    #[allow(dead_code)]
    pub const NAME: &str = "ENC28J60";

    /// Always `Some`: the ENC28J60 driver cannot tell a missing chip (it waits
    /// for CLKRDY forever or accepts EREVID 0xff). Bench devkit only.
    pub async fn bring_up<S: spi::Instance, Sck: ClkPin<S>, Mosi: MosiPin<S>, Miso: MisoPin<S>>(
        _spawner: &Spawner,
        p: Pins<S, Sck, Mosi, Miso>,
        mac: [u8; 6],
    ) -> Option<NetDevice> {
        let mut cfg = SpiConfig::default();
        // ENC28J60: 20 MHz max (its errata want >= 8 MHz on some revisions)
        cfg.frequency = Hertz(10_000_000);
        let hz = cfg.frequency.0;
        let spi = unwrap!(Spi::new_blocking(p.spi, p.sck, p.mosi, p.miso, cfg).ok());
        let cs = Output::new(p.cs, Level::High);
        // The driver polls the chip; no interrupt line needed. No reset pin:
        // the driver then uses the SPI soft reset.
        let _ = (p.int, p.dma0, p.dma1, p.irqs);
        let bus = unwrap!(ExclusiveDevice::new(spi, cs, Delay));
        let device = Enc28j60::new(bus, None::<NoReset>, mac);
        info!("ENC28J60 up on {} at {} Hz", p.bus, hz);
        Some(device)
    }

    /// Not switchable in embassy-net-enc28j60 0.3.0: its receive filter stays
    /// as the driver sets it up.
    #[allow(dead_code)]
    pub fn set_mac_filter(_on: bool) {}

    /// The ENC28J60 is 10BASE-T; duplex is not read back here.
    #[allow(dead_code)]
    pub fn phy_speed_duplex() -> (bool, bool) {
        (false, false)
    }

    /// Its own polling is inside the driver, not counted here.
    #[allow(dead_code)]
    pub fn poll_stats() -> (u32, u32) {
        (0, 0)
    }

    /// The ENC28J60 driver polls; no interrupt line.
    #[allow(dead_code)]
    pub fn int_wakeups() -> u32 {
        0
    }
}

pub use imp::*;
