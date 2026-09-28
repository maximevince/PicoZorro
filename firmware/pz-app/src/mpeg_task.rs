//! MPEG audio decoder (docs/REGISTERS-MPEG.md): takes the
//! compressed chunks the Amiga's mpega.library writes, decodes them with
//! minimp3 (MPEG-1/2/2.5, layers I-III) and posts one record per frame,
//! shaped as the session asked (rate division, mono, gain).
//!
//! One frame is ~2.7-3.6 ms of core 0 at 150 MHz (measured); the task
//! yields after each, so the network and backplane tasks wait at most
//! that long. minimp3 keeps ~16 KiB of scratch on the stack while it
//! decodes (core 0's main stack, the top of RAM).

use defmt::{info, warn};
use embassy_futures::yield_now;
use embassy_time::Timer;
use pz_core::mpeg::{self, flags, Frame, HostPort, Session, IN_BYTES, OUT_BYTES};
use static_cell::StaticCell;

use crate::minimp3::{Decoder, MAX_SAMPLES};

/// The decoder's linear input: minimp3 wants a frame and the next header
/// contiguous. Chunks are appended, used bytes compacted away.
const BUF: usize = 16 * 1024;
/// Decode only with this much input (or at the end of the stream): enough
/// for minimp3 to see a frame and the header after it, even at 320 kbps /
/// 32 kHz or a free-format frame.
const LOW_WATER: usize = 4096;

struct State {
    dec: Decoder,
    buf: [u8; BUF],
    len: usize,
    chunk: [u8; IN_BYTES],
    pcm: [i16; MAX_SAMPLES],
    rec: [u8; OUT_BYTES],
    /// A record built but not posted yet (the queue was full).
    pending: Option<usize>,
    /// The end marker came in.
    eof: bool,
    /// The END record is built.
    ended: bool,
}

impl State {
    fn restart(&mut self) {
        self.dec.reset();
        self.len = 0;
        self.pending = None;
        self.eof = false;
        self.ended = false;
    }

    /// Chunks into the linear buffer while they fit. True if any came.
    fn take_input(&mut self, host: &mut HostPort<'static>) -> bool {
        let mut got = false;
        while !self.eof && self.len + IN_BYTES <= BUF {
            match host.take_chunk(&mut self.chunk) {
                Some(0) => self.eof = true,
                Some(n) => {
                    self.buf[self.len..self.len + n].copy_from_slice(&self.chunk[..n]);
                    self.len += n;
                }
                None => break,
            }
            got = true;
        }
        got
    }

    /// Decode one frame into `rec` (or the END record). True if it did
    /// anything.
    fn step(&mut self, s: &Session, scale: u16) -> bool {
        if self.pending.is_some() || self.ended || (!self.eof && self.len < LOW_WATER) {
            return false;
        }
        let (n, info) = self.dec.decode(&self.buf[..self.len], &mut self.pcm);
        let used = info.frame_bytes.max(0) as usize;
        if info.hz == 0 {
            // No complete frame: `used` bytes of junk to skip, or nothing
            // (needs more input).
            if used > 0 {
                self.consume(used);
                return true;
            }
            if self.eof {
                self.pending = Some(mpeg::end_record(&mut self.rec));
                self.ended = true;
                self.len = 0;
                return true;
            }
            if self.len == BUF {
                // 16 KiB without a frame: not MPEG audio. Drop it so the
                // input keeps moving.
                warn!("mpeg: no frame in {} bytes, dropped", BUF);
                self.len = 0;
                return true;
            }
            return false;
        }
        let off = info.frame_offset.max(0) as usize;
        let mut header = [0u8; 4];
        header.copy_from_slice(&self.buf[off..off + 4]);
        let f = Frame {
            flags: if off > 0 { flags::LOST } else { 0 },
            frame_bytes: (used - off) as u16,
            header,
        };
        let ch = info.channels.clamp(1, 2) as usize;
        let cfg = mpeg::Config { scale, ..s.config };
        self.pending = Some(mpeg::record(&mut self.rec, &f, &self.pcm, n, ch, &cfg));
        self.consume(used);
        true
    }

    fn consume(&mut self, used: usize) {
        let used = used.min(self.len);
        self.buf.copy_within(used..self.len, 0);
        self.len -= used;
    }
}

static STATE: StaticCell<State> = StaticCell::new();

#[embassy_executor::task]
pub async fn run(mut host: HostPort<'static>) -> ! {
    let st = STATE.init_with(|| State {
        dec: Decoder::new(),
        buf: [0; BUF],
        len: 0,
        chunk: [0; IN_BYTES],
        pcm: [0; MAX_SAMPLES],
        rec: [0; OUT_BYTES],
        pending: None,
        eof: false,
        ended: false,
    });
    let mut session: Option<Session> = None;
    loop {
        if let Some(s) = host.take_session() {
            st.restart();
            info!(
                "mpeg: session {} {}, freq_div {} mono {} scale {}",
                s.epoch, if s.running { "start" } else { "stop" }, s.config.freq_div, s.config.mono, s.config.scale
            );
            session = s.running.then_some(s);
        }
        let Some(s) = session else {
            Timer::after_millis(2).await;
            continue;
        };
        let mut worked = st.take_input(&mut host);
        if let Some(n) = st.pending {
            match host.post(s.epoch, &mut st.rec[..n]) {
                Ok(true) | Err(()) => {
                    st.pending = None;
                    worked = true;
                }
                Ok(false) => {}
            }
        }
        worked |= st.step(&s, host.scale());
        if worked {
            yield_now().await;
        } else {
            Timer::after_micros(500).await;
        }
    }
}
