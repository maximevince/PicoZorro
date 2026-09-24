#![no_std]
#![allow(async_fn_in_trait)]
#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

// must go first!
mod fmt;

pub mod chip;
mod device;
pub mod trace;

use embassy_futures::select::{Either3, select3};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use embassy_net_driver_channel as ch;
use embassy_net_driver_channel::driver::{LinkState, PacketBuf};
use embassy_time::{Duration, Ticker, Timer};
use embedded_hal::digital::OutputPin;
use embedded_hal_async::digital::Wait;
use embedded_hal_async::spi::SpiDevice;

use crate::chip::Chip;
pub use crate::device::InitError;
use crate::device::WiznetDevice;

// If you change this update the docs of State
const MTU: usize = 1514;

/// Type alias for the embassy-net driver.
pub type Device<'d> = embassy_net_driver_channel::Device<'d>;

/// Internal state for the embassy-net integration.
///
/// The two generic arguments `N_RX` and `N_TX` set the size of the receive and
/// send packet queue, in packets. The packets themselves come from the global
/// packet pool, sized by xarxa's `packet-buf-count-N` feature, so these only
/// bound how many of them this driver can hold at a time. Setting both to 1 is
/// the minimum, but this might hurt performance as a packet can not be received
/// while processing another.
pub struct State<const N_RX: usize, const N_TX: usize> {
    ch_state: ch::State<N_RX, N_TX>,
}

impl<const N_RX: usize, const N_TX: usize> State<N_RX, N_TX> {
    /// Create a new `State`.
    pub const fn new() -> Self {
        Self {
            ch_state: ch::State::new(),
        }
    }
}

/// PicoZorro addition: run-time controls shared between the application and
/// [`Runner::run_with_control`]. Changes are applied on the runner's
/// 500 ms tick.
pub struct Control {
    mac_filter: AtomicBool,
    phy_cfg: AtomicU8,
    int_wakeups: AtomicU32,
}

impl Control {
    /// MAC filter on, PHY state unknown (0).
    pub const fn new() -> Self {
        Self {
            mac_filter: AtomicBool::new(true),
            phy_cfg: AtomicU8::new(0),
            int_wakeups: AtomicU32::new(0),
        }
    }

    /// Ask for the chip's MAC filter on (only own unicast and broadcast
    /// frames) or off (all frames, filter in software).
    pub fn set_mac_filter(&self, on: bool) {
        self.mac_filter.store(on, Ordering::Relaxed);
    }

    /// The raw PHY status as last read (W5500 PHYCFGR).
    pub fn phy_cfg(&self) -> u8 {
        self.phy_cfg.load(Ordering::Relaxed)
    }

    /// How often the runner read a frame after waiting for the interrupt
    /// line (not counting the frames it read back to back after one).
    pub fn int_wakeups(&self) -> u32 {
        self.int_wakeups.load(Ordering::Relaxed)
    }
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

/// Background runner for the driver.
///
/// You must call `.run()` in a background task for the driver to operate.
pub struct Runner<'d, C: Chip, SPI: SpiDevice, INT: Wait, RST: OutputPin> {
    mac: WiznetDevice<C, SPI>,
    ch: ch::Runner<'d>,
    int: INT,
    _reset: RST,
}

/// You must call this in a background task for the driver to operate.
impl<'d, C: Chip, SPI: SpiDevice, INT: Wait, RST: OutputPin> Runner<'d, C, SPI, INT, RST> {
    /// Run the driver.
    pub async fn run(self) -> ! {
        self.run_inner(None).await
    }

    /// Run the driver and apply the [`Control`] requests (PicoZorro addition).
    pub async fn run_with_control(self, ctl: &Control) -> ! {
        self.run_inner(Some(ctl)).await
    }

    async fn run_inner(mut self, ctl: Option<&Control>) -> ! {
        let mut mac_filter = true; // as set up by WiznetDevice::new
        let (state_chan, mut rx_chan, mut tx_chan) = self.ch.split();
        let mut tick = Ticker::every(Duration::from_millis(500));

        // Signals that there are more RX frames to read
        let mut rx_frames_remaining = false;
        loop {
            let waits_for_int = !rx_frames_remaining;
            match select3(
                async {
                    if !rx_frames_remaining {
                        self.int.wait_for_low().await.ok();
                    }
                    rx_chan.rx_ready().await
                },
                tx_chan.tx(),
                tick.next(),
            )
            .await
            {
                Either3::First(()) => {
                    let Some(mut p) = PacketBuf::try_new() else {
                        warn!("packet pool empty, can't receive");
                        // Back off a little, so we don't spin until the stack frees a buffer.
                        Timer::after_millis(1).await;
                        rx_frames_remaining = false;
                        continue;
                    };
                    p.set_len(MTU);
                    match self.mac.read_frame(&mut p).await {
                        Ok(n @ 1..) => {
                            if let (true, Some(ctl)) = (waits_for_int, ctl) {
                                ctl.int_wakeups.fetch_add(1, Ordering::Relaxed);
                            }
                            trace::STATS.rx_frames.fetch_add(1, Ordering::Relaxed);
                            p.set_len(n);
                            rx_chan.rx(p).await;
                            rx_frames_remaining = true;
                        }
                        // Empty RX buffer, or a read error: no more frames to read
                        Ok(0) | Err(_) => {
                            rx_frames_remaining = false;
                        }
                    };
                }
                Either3::Second(p) => {
                    if self.mac.write_frame(&p).await.is_ok() {
                        trace::STATS.tx_frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Either3::Third(()) => {
                    // PicoZorro addition: a chip that lost its set-up (reset
                    // by a glitch on RSTn or its supply, or by a stray MR
                    // write: registers back to their defaults, the socket
                    // closed, no /INT) would otherwise leave the network dead
                    // for good while the rest of the firmware runs on.
                    if let Some(sr) = self.mac.socket_state().await
                        && sr != device::SOCK_MACRAW
                    {
                        trace::freeze(sr);
                        warn!("wiznet: socket status {=u8:#04x}, not MACRAW: setting the chip up again", sr);
                        match self.mac.recover(mac_filter).await {
                            Ok(true) => {
                                trace::STATS.chip_reset.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(false) => {}
                            Err(_) => {
                                trace::STATS.recover_failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        rx_frames_remaining = false;
                    }
                    if let Some(ctl) = ctl {
                        let want = ctl.mac_filter.load(Ordering::Relaxed);
                        if want != mac_filter && self.mac.set_mac_filter(want).await.is_ok() {
                            mac_filter = want;
                            rx_frames_remaining = false;
                        }
                        ctl.phy_cfg.store(self.mac.phy_cfg().await, Ordering::Relaxed);
                    }
                    if self.mac.is_link_up().await {
                        state_chan.set_link_state(LinkState::Up);
                    } else {
                        state_chan.set_link_state(LinkState::Down);
                    }
                }
            }
        }
    }
}

/// Create a Wiznet ethernet chip driver for [`embassy-net`](https://crates.io/crates/embassy-net).
///
/// This returns two structs:
/// - a `Device` that you must pass to the `embassy-net` stack.
/// - a `Runner`. You must call `.run()` on it in a background task.
pub async fn new<'a, const N_RX: usize, const N_TX: usize, C: Chip, SPI: SpiDevice, INT: Wait, RST: OutputPin>(
    mac_addr: [u8; 6],
    state: &'a mut State<N_RX, N_TX>,
    spi_dev: SPI,
    int: INT,
    mut reset: RST,
) -> Result<(Device<'a>, Runner<'a, C, SPI, INT, RST>), InitError<SPI::Error>> {
    // Reset the chip.
    reset.set_low().ok();
    // Ensure the reset is registered.
    Timer::after_millis(1).await;
    reset.set_high().ok();

    // Wait for PLL lock. Some chips are slower than others.
    // Slowest is w5100s which is 100ms, so let's just wait that.
    Timer::after_millis(100).await;

    let mac = WiznetDevice::new(spi_dev, mac_addr).await?;

    let (runner, device) = ch::new(
        &mut state.ch_state,
        ch::driver::HardwareAddress::Ethernet(mac_addr),
        MTU,
    );

    Ok((
        device,
        Runner {
            ch: runner,
            mac,
            int,
            _reset: reset,
        },
    ))
}
