//! Core-0 side of the network path (`#[path = "../nic_task.rs"] mod nic_task;`,
//! next to `use pz_hal::chip;`): moves frames between the Ethernet chip and the rings
//! of `pz_core::nic`, whose other side is the bus loop on core 1 (or the
//! bench host). The chip is driven through the `embassy_net::driver::Driver`
//! trait (xarxa's frame buffers), with no IP stack: the Amiga runs its own.

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering::Relaxed};

use defmt::info;
use embassy_net::driver::{Driver, LinkState, PacketBuf};
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use pz_core::nic::{NicPort, RxResult};

use crate::chip;

/// Frames the firmware filter dropped (MAC filter off, not for us).
pub static FILTERED: AtomicU32 = AtomicU32::new(0);
/// Frames dropped because the Amiga side is offline (CTRL.ONLINE clear).
pub static OFFLINE: AtomicU32 = AtomicU32::new(0);
/// Link as last seen: bit 0 up, bit 1 100 Mbit/s, bit 2 full duplex; 0xff
/// before the first check. For the housekeeping log.
pub static LINK: AtomicU8 = AtomicU8::new(0xff);

/// Bench only: the fake host asks for one unpadded 42-byte frame sent
/// directly (bypassing the model's padding), to see whether the chip pads.
#[cfg(feature = "bench-host")]
pub static SEND_RUNT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[embassy_executor::task]
pub async fn nic_task(mut dev: chip::NetDevice, mut n: NicPort<'static>, mac: [u8; 6]) -> ! {
    let _ = mac; // only the bench runt frame needs it
    let mut filter_on = true;
    let mut next_poll = Instant::now();
    let mut next_log = Instant::now() + Duration::from_secs(10);
    let mut last_poll = (0u32, 0u32);
    loop {
        // Take a received frame, or wait 50 us, then look at TX and the rest.
        // (Nothing wakes this task: 50 us is well below what one frame takes
        // to cross the Zorro bus.)
        if let Some(frame) = Driver::receive(&mut dev) {
            match n.push_rx(&frame) {
                RxResult::Filtered => bump(&FILTERED),
                RxResult::Offline => bump(&OFFLINE),
                _ => {}
            }
            // Let the other high-priority tasks in between frames.
            yield_now().await;
        } else {
            Timer::after_micros(50).await;
        }

        // Everything the Amiga committed.
        loop {
            let Some(frame) = n.peek_tx() else { break };
            let mut buf = tx_buf(&mut dev).await;
            buf.set_len(frame.len());
            buf.copy_from_slice(frame);
            // can_transmit said yes and nothing ran in between: taken.
            let _ = Driver::transmit(&mut dev, buf);
            n.release_tx(true);
        }

        #[cfg(feature = "bench-host")]
        if SEND_RUNT.swap(false, Relaxed) {
            let mut buf = tx_buf(&mut dev).await;
            buf.set_len(42);
            buf.fill(0);
            buf[..6].fill(0xff);
            buf[6..12].copy_from_slice(&mac);
            buf[12..14].copy_from_slice(&0x88b5u16.to_be_bytes()); // IEEE local experimental
            buf[14..20].copy_from_slice(b"PZRUNT");
            let _ = Driver::transmit(&mut dev, buf);
            info!("runt frame (42 bytes, unpadded) sent");
        }

        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + Duration::from_millis(100);
            let up = Driver::link_state(&mut dev) == LinkState::Up;
            let (s100, full) = chip::phy_speed_duplex();
            n.set_link(up, s100, full);
            LINK.store(u8::from(up) | u8::from(s100) << 1 | u8::from(full) << 2, Relaxed);
            let want = !n.needs_all_frames();
            if want != filter_on {
                filter_on = want;
                chip::set_mac_filter(want);
                info!("chip MAC filter {}", if want { "on" } else { "off (firmware filters)" });
            }
        }
        if now >= next_log {
            next_log = now + Duration::from_secs(10);
            info!(
                "nic: filtered {} offline-dropped {} int-wakeups {} link {} rx_free {}",
                FILTERED.load(Relaxed),
                OFFLINE.load(Relaxed),
                chip::int_wakeups(),
                chip::phy_speed_duplex(),
                n.rx_free()
            );
            // nic-poll: what the polling costs core 0 over the last period.
            let (polls, busy) = chip::poll_stats();
            let (dp, db) = (polls.wrapping_sub(last_poll.0), busy.wrapping_sub(last_poll.1));
            last_poll = (polls, busy);
            if dp > 0 {
                info!(
                    "nic-poll: {} polls in 10 s, {} us each, {} permille of core 0",
                    dp,
                    db / dp,
                    db / 10_000
                );
            }
        }
    }
}

/// No Ethernet chip: link stays down, nothing is received, and every frame
/// the Amiga commits is dropped and counted in tx_errors, so a driver never
/// waits on TX_FREE = 0.
#[embassy_executor::task]
pub async fn drain_task(mut n: NicPort<'static>) -> ! {
    n.set_link(false, false, false);
    LINK.store(0, Relaxed);
    loop {
        while n.peek_tx().is_some() {
            n.release_tx(false);
        }
        Timer::after_millis(1).await;
    }
}

/// Wait until the driver can take a frame and a buffer is free in the
/// packet pool; the buffer comes back empty. Polled on the task's 50 us
/// beat like the rest of the loop.
async fn tx_buf(dev: &mut chip::NetDevice) -> PacketBuf {
    loop {
        // The pool is shared with the RX queue: wait for the runner or this
        // task to drop one.
        if Driver::can_transmit(dev) {
            if let Some(b) = PacketBuf::try_new() {
                return b;
            }
        }
        Timer::after_micros(50).await;
    }
}

fn bump(a: &AtomicU32) {
    a.store(a.load(Relaxed).wrapping_add(1), Relaxed);
}
