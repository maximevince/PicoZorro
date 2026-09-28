//! First light for a board without a debug probe: `log` over USB CDC
//! (embassy-usb-logger), a picotool-compatible reset interface so
//! `picotool load -f` can re-flash without touching BOOTSEL, a heartbeat on
//! the LED, and the chip identity (SYSINFO stepping and package, OTP chip ID,
//! the MAC derived from it).
//!
//! Flash: `picotool load -f -x` on the ELF (USB; the first time from
//! BOOTSEL, afterwards `-f` reboots the running image into BOOTSEL itself).
//! Watch: `picocom /dev/ttyACM0` or `cat /dev/ttyACM0`.
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_rp::watchdog::Watchdog;
use embassy_time::{Duration, Timer};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

#[embassy_executor::task]
async fn usb_task(driver: Driver<'static, USB>, watchdog: Watchdog<'static>) -> ! {
    let (mut device, class) = pz_hal::usb::cdc_with_reset(driver, watchdog, "hello");
    let log_fut = embassy_usb_logger::with_class!(1024, log::LevelFilter::Info, class);
    join(device.run(), log_fut).await;
    unreachable!()
}

/// The stepping, from the bootrom version byte: pico-sdk `rp2350_rom_version()`
/// documents 2 = A2, 3 = A3, 4 = A4. CHIP_ID.REVISION is not enough: A4 is A3
/// silicon with a new bootrom and reads 0x3 there (observed on an A4-marked
/// part; the datasheet's "A4 = 0x8" does not match, the SDK's "3 for A3/A4"
/// does).
fn stepping(rom_version: u8) -> &'static str {
    match rom_version {
        2 => "A2",
        3 => "A3",
        4 => "A4",
        _ => "?",
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let watchdog = Watchdog::new(p.WATCHDOG);
    spawner.spawn(defmt::unwrap!(usb_task(Driver::new(p.USB, Irqs), watchdog)));

    // The activity LED on GPIO39 (on the Core2350B the module's LED).
    let mut led = Output::new(p.PIN_39, Level::Low);

    let sysinfo = embassy_rp::pac::SYSINFO;
    let chip = sysinfo.chip_id().read();
    let qfn60 = sysinfo.package_sel().read().package_sel();
    let gitref = sysinfo.gitref_rp2350().read();
    // second opinion on the stepping: the bootrom version byte (A2 = 2, A3 = 3, A4 = 4)
    let rom_ver = embassy_rp::rom_data::rom_version_number();
    let rom_git = embassy_rp::rom_data::git_revision();
    let chip_id = embassy_rp::otp::get_chipid().ok();
    let mac = chip_id.map(pz_core::mac::from_chip_id);

    let mut n: u32 = 0;
    loop {
        led.toggle();
        if n.is_multiple_of(4) {
            log::info!(
                "PicoZorro hello #{}: RP2350{} stepping {} (rev 0x{:x}, part 0x{:04x}, gitref {:08x}, \
                 bootrom v{} git {:08x}); chip id {:?}; MAC {:?}",
                n / 4,
                if qfn60 { "A" } else { "B" },
                stepping(rom_ver),
                chip.revision(),
                chip.part(),
                gitref,
                rom_ver,
                rom_git,
                chip_id,
                mac,
            );
        }
        n += 1;
        Timer::after(Duration::from_millis(500)).await;
    }
}
