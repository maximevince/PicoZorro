//! The backplane, for developer builds: the Zorro bus replaced by a link to
//! the Amiga side (FS-UAE through a serial relay, or a PC tool). That side
//! sends batches of bus cycles; [`serve_link`] runs them
//! (`pz_core::backplane::execute`) against a board, a [`BusTarget`], and
//! sends the replies back. /INT becomes a notification datagram.
//!
//! The link is behind [`Link`]; [`UartLink`] is UART0 on GPIO0/1 with the
//! framing of `pz_core::frame` (COBS + CRC). Not for a card in an Amiga:
//! those pins are Zorro data lines.
//!
//! The serve loop keeps a one-entry reply cache: a request that repeats the
//! last one's seq (same peer, same length, within [`CACHE_TTL`]) is a retry
//! after a lost reply; it gets the cached reply and is not executed again,
//! so a retried FIFO write does not happen twice.

use core::future::pending;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_futures::select::{select3, Either3};
use embassy_rp::uart::{BufferedUart, BufferedUartRx, BufferedUartTx};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex, RawMutex};
use embassy_sync::mutex::Mutex;
use embassy_sync::pipe::Pipe;
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::Write;
use pz_core::backplane::{execute, int_note, BusTarget, DGRAM_MAX, MAGIC, VERSION};
use pz_core::frame::{self, Deframer};

/// Line rate of the UART link.
pub const BAUD: u32 = 1_000_000;

/// Text log of a UART backplane build: the UART is the link, so log lines
/// travel in LOG frames (`LO` + text), which the relay writes to its log.
/// Writers use `try_write` and drop what does not fit, or write through
/// [`log`].
pub static LOG: Pipe<CriticalSectionRawMutex, 2048> = Pipe::new();

/// Log text to [`LOG`]. Text that does not fit as a whole is dropped as a
/// whole and counted in [`LOG_DROPPED`]. One `try_write` stores only up to
/// the end of the pipe's ring and nothing past its wrap, which would cut a
/// line's tail and run it into the next line.
pub fn log(text: &[u8]) {
    put_whole(&LOG, text);
}

/// Log texts dropped for want of room, per pipe ([`LOG`]; index 1 unused).
pub static LOG_DROPPED: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

#[inline(never)]
fn put_whole(p: &'static Pipe<CriticalSectionRawMutex, 2048>, text: &[u8]) {
    let k = usize::from(!core::ptr::eq(p, &LOG));
    // Writers on other executors may fill the pipe between the check and
    // the write; then the tail is cut as before, rarely.
    if p.free_capacity() < text.len() {
        LOG_DROPPED[k].fetch_add(1, Relaxed);
        return;
    }
    let _ = p.try_write_all(text);
}

/// Requests answered from the reply cache (all links).
pub static DUPLICATES: AtomicU32 = AtomicU32::new(0);

/// While /INT stays asserted the level is repeated, so a notification lost
/// on the way costs at most this long.
const INT_REPEAT: Duration = Duration::from_millis(100);

/// How long a reply stays in the cache. A client retries within 3 x 200 ms;
/// the limit keeps a new client that starts its
/// seq where an old one stopped from being answered from the cache.
pub const CACHE_TTL: Duration = Duration::from_secs(2);

/// A transport for backplane datagrams.
#[allow(async_fn_in_trait)] // used with concrete types in this workspace only
pub trait Link {
    /// Where a request came from, and where its reply goes.
    type Peer: Copy + PartialEq;
    /// Largest datagram, request or reply, on this link.
    const MAX: usize;
    /// The next datagram (a request, or anything else the serve loop
    /// ignores) into `buf`. Cancel-safe: dropped before completion, it
    /// loses nothing.
    async fn recv(&mut self, buf: &mut [u8]) -> (usize, Self::Peer);
    /// A reply to `to`.
    async fn send(&mut self, data: &[u8], to: Self::Peer);
    /// A notification (/INT): to the peer of the last request.
    async fn note(&mut self, data: &[u8]);
    /// Log text as a LOG datagram (`LO` + text), where this link sends it.
    async fn log(&mut self, text: &[u8]);
    /// The pipe this link drains into [`Link::log`]; `None`: no log.
    fn log_source(&self) -> Option<&'static Pipe<CriticalSectionRawMutex, 2048>>;
}

/// The last request and its reply (in the serve loop's reply buffer).
struct Cached<P> {
    peer: P,
    seq: u16,
    req_len: usize,
    reply_len: usize,
    at: Instant,
}

async fn read_log(p: Option<&'static Pipe<CriticalSectionRawMutex, 2048>>, buf: &mut [u8]) -> usize {
    match p {
        Some(p) => p.read(buf).await,
        None => pending().await,
    }
}

/// Serve the backplane on `link` against the board behind `board`,
/// forever. The mutex is taken per datagram (execute, then
/// `after_datagram`) and per wake-up (`on_tick`, the /INT level), never
/// across an await of the link, so several links can serve one board.
///
/// `on_tick` runs after every wake-up (a datagram, a log line, or at least
/// every millisecond), for what the build wants to do next to the
/// serving: note the configured base, flush a trace. `after_datagram` runs
/// after each datagram has been executed and before its reply is sent.
pub async fn serve_link<L: Link, M: RawMutex, T: BusTarget>(
    board: &Mutex<M, T>,
    link: &mut L,
    mut on_tick: impl FnMut(&mut T),
    mut after_datagram: impl FnMut(&mut T),
) -> ! {
    let mut req = [0u8; DGRAM_MAX];
    let mut reply = [0u8; DGRAM_MAX];
    let mut logbuf = [0u8; 200];
    let mut cache: Option<Cached<L::Peer>> = None;
    let mut level = false;
    let mut last_int = Instant::now();
    let max = L::MAX.min(DGRAM_MAX);
    let log_source = link.log_source();

    loop {
        match select3(link.recv(&mut req), read_log(log_source, &mut logbuf), Timer::after_millis(1)).await {
            Either3::First((n, peer)) => {
                let dg = &req[..n];
                // Only backplane requests; LOG text and anything else is not ours.
                if n >= 6 && u16::from_be_bytes([dg[0], dg[1]]) == MAGIC && dg[2] == VERSION {
                    let seq = u16::from_be_bytes([dg[4], dg[5]]);
                    let repeat = cache
                        .as_ref()
                        .is_some_and(|c| c.peer == peer && c.seq == seq && c.req_len == n && c.at.elapsed() < CACHE_TTL);
                    if repeat {
                        DUPLICATES.fetch_add(1, Relaxed);
                        defmt::debug!("backplane: seq {} again, answered from the cache", seq);
                    } else {
                        let m = {
                            let mut b = board.lock().await;
                            let m = execute(&mut *b, dg, &mut reply[..max]);
                            after_datagram(&mut b);
                            m
                        };
                        cache = Some(Cached { peer, seq, req_len: n, reply_len: m, at: Instant::now() });
                    }
                    if let Some(c) = cache.as_ref().filter(|c| c.reply_len > 0) {
                        link.send(&reply[..c.reply_len], peer).await;
                    }
                }
            }
            Either3::Second(n) => link.log(&logbuf[..n]).await,
            Either3::Third(()) => {}
        }
        let now = {
            let mut b = board.lock().await;
            on_tick(&mut b);
            b.irq_asserted()
        };
        if now != level || (now && last_int.elapsed() >= INT_REPEAT) {
            level = now;
            last_int = Instant::now();
            link.note(&int_note(now)).await;
        }
    }
}

/// A board served by one link only, without a mutex of its own.
struct Only<'a, T>(&'a mut T);

impl<T: BusTarget> BusTarget for Only<'_, T> {
    fn read_at(&mut self, a16: bool, reg: u8, uds: bool, lds: bool) -> u16 {
        self.0.read_at(a16, reg, uds, lds)
    }
    fn write_at(&mut self, a16: bool, reg: u8, data: u16, uds: bool, lds: bool) {
        self.0.write_at(a16, reg, data, uds, lds)
    }
    fn reset(&mut self) {
        self.0.reset()
    }
    fn mem_write(&mut self, offset: u32, data: &[u8]) -> bool {
        self.0.mem_write(offset, data)
    }
    fn mem_read(&mut self, offset: u32, out: &mut [u8]) -> bool {
        self.0.mem_read(offset, out)
    }
    fn irq_asserted(&self) -> bool {
        self.0.irq_asserted()
    }
}

/// Serve the backplane on `uart` against `target`, forever. `on_tick` as
/// in [`serve_link`].
pub async fn serve<T: BusTarget>(target: &mut T, uart: BufferedUart<'static>, on_tick: impl FnMut(&mut T)) -> ! {
    serve_with(target, uart, on_tick, |_| {}).await
}

/// [`serve`], plus `after_datagram` as in [`serve_link`].
pub async fn serve_with<T: BusTarget>(
    target: &mut T,
    uart: BufferedUart<'static>,
    mut on_tick: impl FnMut(&mut T),
    mut after_datagram: impl FnMut(&mut T),
) -> ! {
    let board: Mutex<NoopRawMutex, Only<'_, T>> = Mutex::new(Only(target));
    let mut link = UartLink::new(uart);
    serve_link(&board, &mut link, |b| on_tick(b.0), |b| after_datagram(b.0)).await
}

// ---- UART --------------------------------------------------------------------

const ENC_MAX: usize = frame::encoded_max(DGRAM_MAX);

/// The UART link: datagram + CRC-16, COBS, 0x00 before and after; one peer.
pub struct UartLink {
    tx: BufferedUartTx<'static>,
    rx: BufferedUartRx<'static>,
    deframer: Deframer<ENC_MAX>,
    chunk: [u8; 256],
    /// Bytes of `chunk` not pushed into the deframer yet: `pos..n`.
    pos: usize,
    n: usize,
    enc: [u8; ENC_MAX],
}

impl UartLink {
    pub fn new(uart: BufferedUart<'static>) -> Self {
        let (tx, rx) = uart.split();
        UartLink { tx, rx, deframer: Deframer::new(), chunk: [0; 256], pos: 0, n: 0, enc: [0; ENC_MAX] }
    }

    async fn send_frame(&mut self, data: &[u8]) {
        let e = frame::encode(data, &mut self.enc);
        let _ = self.tx.write_all(&self.enc[..e]).await;
    }
}

impl Link for UartLink {
    type Peer = ();
    const MAX: usize = DGRAM_MAX;

    async fn recv(&mut self, buf: &mut [u8]) -> (usize, ()) {
        loop {
            while self.pos < self.n {
                let b = self.chunk[self.pos];
                self.pos += 1;
                if let Some(len) = self.deframer.push(b) {
                    let dg = self.deframer.datagram(len);
                    let k = dg.len().min(buf.len());
                    buf[..k].copy_from_slice(&dg[..k]);
                    return (k, ());
                }
            }
            // The only await: BufferedUartRx::read takes bytes out of the
            // ring buffer only when it completes.
            self.n = self.rx.read(&mut self.chunk).await.unwrap_or(0);
            self.pos = 0;
        }
    }

    async fn send(&mut self, data: &[u8], _to: ()) {
        self.send_frame(data).await
    }

    async fn note(&mut self, data: &[u8]) {
        self.send_frame(data).await
    }

    async fn log(&mut self, text: &[u8]) {
        let mut dg = [0u8; 2 + 256];
        let k = text.len().min(256);
        dg[..2].copy_from_slice(b"LO");
        dg[2..2 + k].copy_from_slice(&text[..k]);
        self.send_frame(&dg[..2 + k]).await
    }

    fn log_source(&self) -> Option<&'static Pipe<CriticalSectionRawMutex, 2048>> {
        Some(&LOG)
    }
}
