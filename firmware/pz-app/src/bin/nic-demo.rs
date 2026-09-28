//! Ethernet chip check: bring the chip up on SPI1, get a DHCP lease, answer
//! pings and echo TCP on port 7. The embassy-net stack is only a test tool
//! here; the product firmware moves raw frames between the Zorro side and
//! the chip.
//!
//! Wiring and chip features: `pz_hal::chip` (`pz-hal/src/chip.rs`).
//! Flash and watch: `cargo run --release --bin nic-demo` (probe-rs, defmt over RTT).
#![no_std]
#![no_main]

use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_net::tcp::{TcpListener, TcpSocket};
use embassy_net::{Stack, StackStorage};
use embassy_rp::clocks::RoscRng;
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use pz_hal::{chip, chip_pins};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};


#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static>) -> ! {
    runner.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!("PicoZorro nic-demo");

    let chip_id = match embassy_rp::otp::get_chipid() {
        Ok(id) => id,
        Err(e) => {
            warn!("OTP chip id unreadable ({:?}), using 0", e);
            0
        }
    };
    let mac = pz_core::mac::from_chip_id(chip_id);
    info!("MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    let Some(device) = chip::bring_up(&spawner, chip_pins!(p), mac).await else {
        defmt::panic!("no Ethernet chip on SPI1: check the wiring (hardware/PINMAP.md)");
    };

    static STORAGE: StaticCell<StackStorage> = StaticCell::new();
    let seed = RoscRng.next_u64();
    let (stack, runner) = Stack::new(STORAGE.init(StackStorage::new()), seed);
    static DEVICE: StaticCell<chip::NetDevice> = StaticCell::new();
    let iface = unwrap!(stack.add_iface_borrowed(DEVICE.init(device)).ok());
    unwrap!(iface.set_dhcpv4(Some(Default::default())).ok());
    spawner.spawn(unwrap!(net_task(runner)));

    info!("waiting for link + DHCP");
    iface.wait_link_up().await;
    info!("link up");
    iface.wait_config_up().await;
    let lease = unwrap!(iface.dhcpv4_lease());
    info!("IP {} (ping me), TCP echo on port 7", lease.address.address());

    echo_server(stack).await
}

/// TCP echo on port 7: `nc <ip> 7`.
async fn echo_server(stack: Stack<'static>) -> ! {
    let mut rx_buffer = [0u8; 2048];
    let mut tx_buffer = [0u8; 2048];
    let mut buf = [0u8; 1024];
    let mut listener = unwrap!(TcpListener::new(stack).ok());
    unwrap!(listener.listen(7).ok());
    loop {
        let token = match listener.accept().await {
            Ok(t) => t,
            Err(e) => {
                warn!("accept: {:?}", e);
                Timer::after_millis(100).await;
                continue;
            }
        };
        let mut socket = unwrap!(TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer).ok());
        socket.set_timeout(Some(Duration::from_secs(30)));
        if let Err(e) = socket.accept(token).await {
            warn!("accept: {:?}", e);
            continue;
        }
        info!("client {:?}", socket.remote_addr());
        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    warn!("read: {:?}", e);
                    break;
                }
            };
            if let Err(e) = socket.write_all(&buf[..n]).await {
                warn!("write: {:?}", e);
                break;
            }
        }
        // write_all only queues; dropping the socket now would abort the
        // connection with the tail still in the TX buffer. Drain, FIN, drain.
        if let Err(e) = socket.flush().await {
            warn!("flush: {:?}", e);
        }
        socket.close();
        let _ = socket.flush().await;
        info!("client gone");
    }
}
