//! Boot stream: the Amiga modules from the boot image built into the
//! firmware (tools/mkboot.py), relocated
//! to the memory the boot ROM's loader reports and published for it to
//! read (`pz_core::boot`). Without an image there is no boot ROM.

use defmt::{info, warn};
use embassy_time::Timer;
use pz_core::boot::{HostPort, Image, Streamer};

static IMG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/boot.img"));

/// The built-in boot image, if there is a valid one.
pub fn image() -> Option<Image<'static>> {
    if IMG.is_empty() {
        info!("boot: no boot image in this build");
        return None;
    }
    match Image::new(IMG) {
        Ok(i) => {
            info!("boot: image of {} bytes, {} modules", IMG.len(), i.modules());
            Some(i)
        }
        Err(e) => {
            warn!("boot: bad boot image ({})", defmt::Debug2Format(&e));
            None
        }
    }
}

#[embassy_executor::task]
pub async fn run(mut host: HostPort<'static>, img: Image<'static>) -> ! {
    let mut st: Option<Streamer> = None;
    loop {
        if let Some(s) = host.take_start() {
            st = match Streamer::start(&mut host, &img, s) {
                Ok(x) => {
                    info!("boot: loader started (session {})", s);
                    Some(x)
                }
                Err(e) => {
                    warn!("boot: {}", defmt::Debug2Format(&e));
                    None
                }
            };
        }
        if let Some(x) = st.as_mut() {
            match x.step(&mut host, &img) {
                Ok(true) if x.done() => {
                    info!("boot: all modules streamed");
                    st = None;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("boot: {}", defmt::Debug2Format(&e));
                    st = None;
                }
            }
        }
        Timer::after_micros(200).await;
    }
}
