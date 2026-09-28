//! UART backplane (`--features uart-backplane`): the
//! Zorro bus replaced by a serial link. The serve loop is
//! `pz_hal::backplane::serve`; this task runs it against the same `Window`
//! the bus loop serves (Autoconfig slave, v0 registers, the network model
//! and behind it the W5500) and keeps the configured base for an update
//! reboot. Bench only: UART0 sits on GPIO0/1, which are Zorro data lines on
//! a card; no PIO bus slave in this build.

use core::sync::atomic::Ordering::Relaxed;

use embassy_rp::uart::BufferedUart;
pub use pz_hal::backplane::{BAUD, LOG};
use pz_core::slave::State;
use pz_core::window::Window;

#[embassy_executor::task]
pub async fn run(mut window: Window<'static>, uart: BufferedUart<'static>) -> ! {
    pz_hal::backplane::serve(&mut window, uart, |w| {
        // For an update reboot: the base to come back at.
        let base = if w.slave.state == State::Configured { w.slave.base as u16 } else { 0xffff };
        crate::update_task::BASE.store(base, Relaxed);
    })
    .await
}
