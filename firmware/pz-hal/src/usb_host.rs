//! The RP2350 as a USB host controller for Poseidon on the Amiga
//! (docs/REGISTERS-USB.md).
//!
//! The firmware enumerates nothing: it executes transfer records, keeps
//! the pipes (and so the data toggles) per endpoint, and follows the root
//! port. A record is a request (`REQ_HDR` + data), a reply its reply
//! record (`REP_HDR` + data); the USB window's queues carry these bytes
//! (`pz_core::usb`).
//!
//! - [`start`] spawns the root-port task and [`WORKERS`] workers on the
//!   calling executor. Workers run transfers concurrently; each endpoint's
//!   pipe is used by one transfer at a time.
//! - Every waker slot has one waiter: embassy-sync's `WakerRegistration`
//!   wakes the waker it replaces, so tasks waiting on the same channel end
//!   wake each other without end (on an interrupt executor that starves
//!   everything below it). Each worker has its own request and reply slot;
//!   the idle and ready queues have one receiver each ([`handle`] and
//!   [`next_reply`], called from one task).
//! - [`handle`] takes one request record: port operations, ABORT and the
//!   cache operations answer at once; a transfer goes to the workers and
//!   its reply comes out of [`next_reply`] later, tagged with the
//!   [`Origin`] it came from. The records of a streamed bulk OUT after
//!   the first (XFER_DATA) go to the worker that keeps that stream.
//! - [`WindowHost`] is the host side of the USB window: it takes request
//!   records from the window's queue and posts the replies.
//!
//! Interrupt IN endpoints are read by a task per endpoint that keeps the
//! endpoint armed while its pipe is cached (the driver's cancel path drops
//! a packet that has just landed); transfers take from its queue.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::task::Poll;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU8, Ordering::Relaxed};

use defmt::{debug, info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_rp::peripherals::USB;
use embassy_rp::usb::host::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embassy_usb_driver::host::{pipe, PipeError, SplitInfo, SplitSpeed, UsbHostAllocator, UsbHostController, UsbPipe};
use embassy_usb_driver::{Direction, EndpointAddress, EndpointInfo, EndpointType, Speed};
use embassy_usb_host::{BusController, BusHandle, BusState};
use pz_core::usb::{self as win, HostPort, Ticket};

// ---- protocol (docs/REGISTERS-USB.md) ------------------------------------------

pub const MAGIC: u16 = 0x5055;
pub const VERSION: u8 = 1;
pub const REQ_HDR: usize = 28;
pub const REP_HDR: usize = 12;
pub const MAX_DATA: usize = 1024;

pub const OP_PING: u8 = 0;
pub const OP_PORT_STATUS: u8 = 1;
pub const OP_PORT_RESET: u8 = 2;
pub const OP_XFER: u8 = 3;
pub const OP_ABORT: u8 = 4;
pub const OP_FORGET: u8 = 5;
pub const OP_RESET_TOGGLE: u8 = 6;
/// The next record of a streamed bulk OUT (docs/REGISTERS-USB.md).
pub const OP_XFER_DATA: u8 = 7;

const XT_CONTROL: u8 = 0;
const XT_BULK: u8 = 2;
const XT_INTERRUPT: u8 = 3;

const F_LOWSPEED: u8 = 1;
const F_PRE: u8 = 2;
const F_NOSHORT: u8 = 4;
/// Bulk IN: `total` bytes (bytes 18..22 of the request) in records of
/// `len` bytes, each but the last marked MORE. Bulk OUT: the same total,
/// the first record's data in this request, the rest in XFER_DATA records
/// (docs/REGISTERS-USB.md).
const F_STREAM: u8 = 16;

/// Reply flags (byte 7): another record of this request follows.
pub const RF_MORE: u8 = 1;
/// Feature bits, third byte of a PORT_STATUS payload.
pub const FEAT_STREAM_IN: u8 = 1;
pub const FEAT_STREAM_OUT: u8 = 2;
pub const FEAT_INTERVAL: u8 = 4;

/// A streamed bulk OUT ends with an error when its next record does not
/// come within this time (the sender stopped).
const STREAM_OUT_WAIT: Duration = Duration::from_secs(5);
/// How long `handle` waits for a stream's worker to take the record that
/// waits in its slot, in steps of `HANDOFF_STEP`. The worker only needs to
/// be polled once; longer means the sender broke the two-record rule.
const HANDOFF_STEP: Duration = Duration::from_micros(100);
const HANDOFF_STEPS: u32 = 50;

// Poseidon's UHIOERR_* values (devices/usbhcd_common.h).
pub const ST_OK: u8 = 0;
const ST_OFFLINE: u8 = 1;
const ST_HOSTERROR: u8 = 3;
const ST_STALL: u8 = 4;
const ST_TIMEOUT: u8 = 6;
const ST_OVERFLOW: u8 = 7;
const ST_CRC: u8 = 8;
const ST_NAKTIMEOUT: u8 = 10;
pub const ST_BADPARAMS: u8 = 11;
const ST_BABBLE: u8 = 13;
const ST_ABORTED: u8 = 15;

// The window's records, as `pz_core::usb` defines them.
const _: () = assert!(win::REQ_HDR == REQ_HDR && win::CPL_HDR == REP_HDR && win::MAX_DATA == MAX_DATA);

#[derive(Clone, Copy)]
struct Xfer {
    seq: u16,
    addr: u8,
    ep: u8,
    kind: u8,
    flags: u8,
    mps: u16,
    timeout_ms: u16,
    len: u16,
    hub_addr: u8,
    hub_port: u8,
    setup: [u8; 8],
    /// Poll interval of an interrupt pipe in ms, 1..=255 (bytes 26-27).
    interval: u8,
}

impl Xfer {
    /// A streamed bulk IN and its total length.
    fn stream(&self) -> Option<u32> {
        (self.flags & F_STREAM != 0 && self.kind == XT_BULK && self.ep & 0x80 != 0).then(|| self.total())
    }

    /// The first record of a streamed bulk OUT, and the total length.
    fn stream_out(&self) -> Option<u32> {
        (self.flags & F_STREAM != 0 && self.kind == XT_BULK && self.ep & 0x80 == 0).then(|| self.total())
    }

    fn total(&self) -> u32 {
        u32::from_be_bytes([self.setup[0], self.setup[1], self.setup[2], self.setup[3]])
    }
}

/// Where a request came from, so its reply goes back there.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A link of the firmware's own (UDP, UART).
    Link(u8),
    /// The USB window; the ticket's epoch voids it after a queue reset.
    Window(Ticket),
}

struct Request {
    x: Xfer,
    origin: Origin,
    data: [u8; MAX_DATA],
    /// `usb-prof`: time (us) when the window handed the request over.
    t_take: u32,
}

/// A finished transfer.
#[derive(Clone)]
pub struct Reply {
    pub seq: u16,
    pub origin: Origin,
    pub status: u8,
    pub actual: u16,
    /// Another record of this request follows (a streamed bulk IN or OUT).
    pub more: bool,
    /// The acknowledgement of a streamed bulk OUT record: `actual` counts
    /// the bytes written, the record carries no payload.
    pub bare: bool,
    pub data: [u8; MAX_DATA],
    /// `usb-prof`: times (us) at take, transfer start, transfer done; and
    /// whether it was a bulk transfer of 512 bytes or more.
    t: [u32; 3],
    big: bool,
}

/// `usb-prof`: per transfer class (0: bulk >= 512 bytes, 1: the rest), in
/// microseconds, read over SWD (`USB_PROF`): count, take -> start,
/// start -> done (the USB transfer), and for the window's transfers
/// done -> posted and posted -> next take (the Amiga's turnaround: /INT,
/// its task, reading the completion, the next request); last the maximum
/// of start -> done.
#[cfg(feature = "usb-prof")]
#[unsafe(no_mangle)]
pub static USB_PROF: [[AtomicU32; 6]; 2] = [const { [const { AtomicU32::new(0) }; 6] }; 2];
#[cfg(feature = "usb-prof")]
static LAST_POST: AtomicU32 = AtomicU32::new(0);

/// `usb-prof`: a flight recorder, read over SWD.
/// The last [`TRACE_N`] transfers and cache / port operations, six words
/// each: start time (us), duration (us, all ones while it runs),
/// seq << 16 | address << 8 | endpoint, flags << 27 | kind << 24 |
/// status << 16 | length (kind 4: an operation, its op in the status
/// field; status 0xff while a transfer runs; the length becomes the actual
/// one), and eight bytes of detail: the setup packet, the SCSI command of a
/// Bulk-Only CBW, else the first data bytes. `USB_TRACE_POS` counts entries.
/// Interrupt IN transfers are left out unless they fail (a NAK timeout is
/// not a failure there).
#[cfg(feature = "usb-prof")]
pub const TRACE_N: usize = 256;
#[cfg(feature = "usb-prof")]
#[unsafe(no_mangle)]
pub static USB_TRACE: [AtomicU32; 6 * TRACE_N] = [const { AtomicU32::new(0) }; 6 * TRACE_N];
#[cfg(feature = "usb-prof")]
#[unsafe(no_mangle)]
pub static USB_TRACE_POS: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "usb-prof")]
fn trace_start(x: &Xfer, data: &[u8; MAX_DATA]) -> usize {
    let i = USB_TRACE_POS.fetch_add(1, Relaxed) as usize % TRACE_N;
    let e = &USB_TRACE[6 * i..6 * i + 6];
    let detail: [u8; 8] = if x.kind == XT_CONTROL {
        x.setup
    } else if x.ep & 0x80 == 0 && x.len == 31 && data[..4] == *b"USBC" {
        unwrap!(data[15..23].try_into())
    } else if x.ep & 0x80 == 0 {
        unwrap!(data[..8].try_into())
    } else {
        [0; 8]
    };
    e[0].store(embassy_time::Instant::now().as_micros() as u32, Relaxed);
    e[1].store(u32::MAX, Relaxed);
    e[2].store((x.seq as u32) << 16 | (x.addr as u32) << 8 | x.ep as u32, Relaxed);
    e[3].store((x.flags as u32 & 0x1f) << 27 | (x.kind as u32 & 7) << 24 | 0xff << 16 | x.len as u32, Relaxed);
    e[4].store(u32::from_be_bytes(unwrap!(detail[..4].try_into())), Relaxed);
    e[5].store(u32::from_be_bytes(unwrap!(detail[4..].try_into())), Relaxed);
    i
}

#[cfg(feature = "usb-prof")]
fn trace_end(i: usize, x: &Xfer, r: Result<usize, u8>, data: &[u8; MAX_DATA]) {
    let e = &USB_TRACE[6 * i..6 * i + 6];
    // Overwritten meanwhile (more than TRACE_N entries since it started).
    if e[2].load(Relaxed) != (x.seq as u32) << 16 | (x.addr as u32) << 8 | x.ep as u32 {
        return;
    }
    let (status, n) = match r {
        Ok(n) => (0, n as u32),
        Err(st) => (st as u32, 0),
    };
    let now = embassy_time::Instant::now().as_micros() as u32;
    e[1].store(now.wrapping_sub(e[0].load(Relaxed)), Relaxed);
    e[3].store((x.flags as u32 & 0x1f) << 27 | (x.kind as u32 & 7) << 24 | status << 16 | n, Relaxed);
    if x.kind != XT_CONTROL && x.ep & 0x80 != 0 && n > 0 {
        e[4].store(u32::from_be_bytes(unwrap!(data[..4].try_into())), Relaxed);
        e[5].store(u32::from_be_bytes(unwrap!(data[4..8].try_into())), Relaxed);
    }
}

/// A port or cache operation in the flight recorder (op 0x10: the root
/// port connected, 0x11: disconnected; 0x12: a bulk or interrupt OUT pipe
/// evicted from the full cache, its toggle lost, 0x13: a control pipe
/// evicted; both with the evicted endpoint and its type as the length).
#[cfg(feature = "usb-prof")]
fn trace_op(op: u8, seq: u16, addr: u8, ep: u8, len: u16) {
    let i = USB_TRACE_POS.fetch_add(1, Relaxed) as usize % TRACE_N;
    let e = &USB_TRACE[6 * i..6 * i + 6];
    e[0].store(embassy_time::Instant::now().as_micros() as u32, Relaxed);
    e[1].store(0, Relaxed);
    e[2].store((seq as u32) << 16 | (addr as u32) << 8 | ep as u32, Relaxed);
    e[3].store(4 << 24 | (op as u32) << 16 | len as u32, Relaxed);
    e[4].store(0, Relaxed);
    e[5].store(0, Relaxed);
}

/// `usb-prof`: the time in us, never 0. (Not the DWT cycle counter: a
/// debug probe that attaches to core 0 stops and clears it.)
#[inline(always)]
fn cyc() -> u32 {
    #[cfg(feature = "usb-prof")]
    {
        (embassy_time::Instant::now().as_micros() as u32) | 1
    }
    #[cfg(not(feature = "usb-prof"))]
    {
        0
    }
}

impl Reply {
    /// The reply record, into `tx`; its length.
    pub fn pack(&self, tx: &mut [u8]) -> usize {
        let body = if self.bare { &[][..] } else { &self.data[..self.actual as usize] };
        let n = pack(tx, OP_XFER, self.seq, self.status, body);
        tx[7] = if self.more { RF_MORE } else { 0 };
        tx[8..10].copy_from_slice(&self.actual.to_be_bytes());
        n
    }
}

// ---- shared state ------------------------------------------------------------

/// Transfers in flight at once: the window's request queue depth, and
/// Poseidon's (picozorrousb.device `SLOTS`).
pub const WORKERS: usize = 16;
type Dev = Driver<'static, USB>;
type Alloc = <Dev as UsbHostController<'static>>::Allocator;
type Bus = BusHandle<'static, Alloc>;
type Pipe<T, D> = <Alloc as UsbHostAllocator<'static>>::Pipe<T, D>;

/// Per worker: its next request (one waiter: the worker).
static WORK: [Channel<CriticalSectionRawMutex, Request, 1>; WORKERS] = [const { Channel::new() }; WORKERS];
/// Idle workers, each at most once (one waiter: the task calling `handle`).
static IDLE: Channel<CriticalSectionRawMutex, u8, WORKERS> = Channel::new();
/// Per worker: its finished transfer, until `next_reply` takes it.
static DONE: [BlockingMutex<CriticalSectionRawMutex, RefCell<Option<Reply>>>; WORKERS] =
    [const { BlockingMutex::new(RefCell::new(None)) }; WORKERS];
/// Workers with a reply in `DONE`, each at most once (one waiter: the
/// task calling `next_reply`).
static READY: Channel<CriticalSectionRawMutex, u8, WORKERS> = Channel::new();
/// Per worker: its reply was taken (one waiter: the worker).
static TAKEN: [Signal<CriticalSectionRawMutex, ()>; WORKERS] = [const { Signal::new() }; WORKERS];
/// Per worker: the streamed bulk OUT it keeps, (seq, origin), so that
/// `handle` routes that stream's XFER_DATA records into its `WORK` slot.
/// Set by `handle` when it gives the first record to the worker (the next
/// record often follows before the worker has run), cleared by the worker
/// when the stream ends.
static STREAM_OUT: [BlockingMutex<CriticalSectionRawMutex, Cell<Option<(u16, Origin)>>>; WORKERS] =
    [const { BlockingMutex::new(Cell::new(None)) }; WORKERS];
/// XFER_DATA records dropped: no live stream with that seq (they follow an
/// error or an abort), or one more than the two-record rule allows.
static DATA_DROPPED: AtomicU32 = AtomicU32::new(0);
/// Pipes dropped from a full cache to make room (PING `evictions`), and of
/// them those that kept a data toggle: bulk and interrupt OUT
/// (`toggle-evictions`).
static EVICTIONS: AtomicU32 = AtomicU32::new(0);
static TOGGLE_EVICTIONS: AtomicU32 = AtomicU32::new(0);
/// Bumped by every root-port event and bus reset: a streamed bulk OUT that
/// sees it change ends (its pipe and its device are gone).
static PORT_GEN: AtomicU32 = AtomicU32::new(0);
static PORT_CMD: Channel<CriticalSectionRawMutex, (), 1> = Channel::new();
static PORT_DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

static NAME: Mutex<CriticalSectionRawMutex, &'static str> = Mutex::new("");
static ROOT_CONNECTED: AtomicBool = AtomicBool::new(false);
static ROOT_SPEED: AtomicU8 = AtomicU8::new(0); // 0 none, 1 LS, 2 FS
static XFERS: AtomicU32 = AtomicU32::new(0);
static ERRORS: AtomicU32 = AtomicU32::new(0);
/// Framing errors of a link, reported by PING.
pub static LINK_ERRORS: AtomicU32 = AtomicU32::new(0);

/// In-flight requests, for ABORT: slot i belongs to worker i.
struct Inflight {
    seq: AtomicU16,
    active: AtomicBool,
    /// Came through the USB window (aborted on a queue reset).
    window: AtomicBool,
    /// The status an abort ends the request with (ST_ABORTED but for a
    /// streamed bulk OUT that a root-port event or its sender broke).
    why: AtomicU8,
    abort: Signal<CriticalSectionRawMutex, ()>,
}
static INFLIGHT: [Inflight; WORKERS] = [const {
    Inflight {
        seq: AtomicU16::new(0),
        active: AtomicBool::new(false),
        window: AtomicBool::new(false),
        why: AtomicU8::new(ST_ABORTED),
        abort: Signal::new(),
    }
}; WORKERS];

impl Inflight {
    fn abort(&self, why: u8) {
        self.why.store(why, Relaxed);
        self.abort.signal(());
    }
}

enum AnyPipe {
    Ctrl(Pipe<pipe::Control, pipe::InOut>),
    BulkIn(Pipe<pipe::Bulk, pipe::In>),
    BulkOut(Pipe<pipe::Bulk, pipe::Out>),
    IntOut(Pipe<pipe::Interrupt, pipe::Out>),
}

#[derive(Clone, Copy)]
struct Report {
    len: u8,
    data: [u8; 64],
}

struct IntReader {
    key: AtomicU32,
    active: AtomicBool,
    err: AtomicU8,
    stop: Signal<CriticalSectionRawMutex, ()>,
    fifo: Channel<CriticalSectionRawMutex, Report, 8>,
}

const READERS: usize = 8;
static INT_READERS: [IntReader; READERS] = [const {
    IntReader {
        key: AtomicU32::new(Key::NONE),
        active: AtomicBool::new(false),
        err: AtomicU8::new(0),
        stop: Signal::new(),
        fifo: Channel::new(),
    }
}; READERS];

/// Root port: connected, low speed.
pub fn root() -> (bool, bool) {
    (ROOT_CONNECTED.load(Relaxed), ROOT_SPEED.load(Relaxed) == 1)
}

/// Transfers done, of them with an error.
pub fn stats() -> (u32, u32) {
    (XFERS.load(Relaxed), ERRORS.load(Relaxed))
}

/// Start the host: the root-port task and the workers, on the executor of
/// `spawner` (the interrupt readers start there later, on demand). `name`
/// goes into PING replies.
pub fn start(spawner: Spawner, driver: Dev, name: &'static str) {
    if let Ok(mut n) = NAME.try_lock() {
        *n = name;
    }
    static BUS_STATE: BusState = BusState::new();
    let (bus_ctrl, bus) = embassy_usb_host::bus(driver, &BUS_STATE);
    spawner.spawn(unwrap!(port_task(bus_ctrl)));
    for id in 0..WORKERS {
        spawner.spawn(unwrap!(worker(bus.clone(), id, spawner)));
    }
}

/// The next finished transfer. From one task only.
pub async fn next_reply() -> Reply {
    let id = READY.receive().await as usize;
    let rep = DONE[id].lock(|d| d.borrow_mut().take());
    TAKEN[id].signal(());
    unwrap!(rep)
}

#[embassy_executor::task(pool_size = READERS)]
async fn int_reader(idx: usize, mut p: Pipe<pipe::Interrupt, pipe::In>, mps: usize) {
    let r = &INT_READERS[idx];
    let mut buf = [0u8; 64];
    let mps = mps.min(64);
    loop {
        match select(p.request_in(&mut buf[..mps]), r.stop.wait()).await {
            Either::First(Ok(n)) => {
                let rep = Report { len: n as u8, data: buf };
                if r.fifo.try_send(rep).is_err() {
                    // Oldest report goes; the host is not keeping up.
                    let _ = r.fifo.try_receive();
                    let _ = r.fifo.try_send(rep);
                }
            }
            Either::First(Err(e)) => {
                warn!("int reader {}: {:?}", idx, e);
                r.err.store(map_err(e), Relaxed);
                break;
            }
            Either::Second(()) => break,
        }
    }
    drop(p);
    r.active.store(false, Relaxed);
}

/// Stop the readers selected like `drop_pipes`, and wait for them to let go
/// of their pipes.
async fn drop_readers(sel: Option<(u8, Option<u8>)>) {
    for r in INT_READERS.iter() {
        let hit = match (Key::unpack(r.key.load(Relaxed)), sel) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(k), Some((addr, None))) => k.addr == addr,
            (Some(k), Some((addr, Some(ep)))) => k.addr == addr && (k.ep & 0x8f) == (ep & 0x8f),
        };
        if hit {
            r.key.store(Key::NONE, Relaxed);
            r.stop.signal(());
            while r.active.load(Relaxed) {
                Timer::after_millis(1).await;
            }
            while r.fifo.try_receive().is_ok() {}
            r.err.store(0, Relaxed);
        }
    }
}

/// Interrupt IN through a reader: find or start one, then wait for a report.
async fn int_in(bus: &Bus, spawner: Spawner, x: &Xfer, out: &mut [u8; MAX_DATA]) -> Result<usize, u8> {
    let key = Key { addr: x.addr, ep: x.ep & 0x8f, kind: XT_INTERRUPT };
    let mut idx = INT_READERS.iter().position(|r| Key::unpack(r.key.load(Relaxed)) == Some(key));
    // A running reader keeps the interval of the request that started it.
    if idx.is_none() {
        let free = INT_READERS.iter().position(|r| !r.active.load(Relaxed) && r.key.load(Relaxed) == Key::NONE);
        let i = free.ok_or(ST_HOSTERROR)?;
        let split = if x.flags & F_PRE != 0 {
            Some(SplitInfo::new(x.hub_addr, x.hub_port, SplitSpeed::Low))
        } else {
            None
        };
        let info = EndpointInfo {
            addr: EndpointAddress::from_parts((x.ep & 0x0f) as usize, Direction::In),
            ep_type: EndpointType::Interrupt,
            max_packet_size: x.mps,
            interval_ms: x.interval,
        };
        let p = bus
            .alloc_pipe::<pipe::Interrupt, pipe::In>(x.addr, &info, split)
            .map_err(|_| ST_HOSTERROR)?;
        let r = &INT_READERS[i];
        r.key.store(key.pack(), Relaxed);
        r.active.store(true, Relaxed);
        r.err.store(0, Relaxed);
        r.stop.reset();
        while r.fifo.try_receive().is_ok() {}
        match int_reader(i, p, x.mps as usize) {
            Ok(token) => spawner.spawn(token),
            Err(_) => {
                r.key.store(Key::NONE, Relaxed);
                r.active.store(false, Relaxed);
                return Err(ST_HOSTERROR);
            }
        }
        idx = Some(i);
    }
    let r = &INT_READERS[idx.unwrap()];
    let len = (x.len as usize).min(64);
    let timeout = if x.timeout_ms == 0 { Duration::from_secs(3600) } else { Duration::from_millis(x.timeout_ms as u64) };
    let rep = match with_timeout(timeout, r.fifo.receive()).await {
        Ok(rep) => rep,
        Err(_) => {
            let e = r.err.load(Relaxed);
            if e != 0 {
                drop_readers(Some((x.addr, Some(x.ep)))).await;
                return Err(e);
            }
            return Err(ST_NAKTIMEOUT);
        }
    };
    let n = (rep.len as usize).min(len);
    out[..n].copy_from_slice(&rep.data[..n]);
    Ok(n)
}

/// (address, endpoint with direction bit, type); control pipes are keyed
/// with endpoint 0 and no direction bit.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    addr: u8,
    ep: u8,
    kind: u8,
}

impl Key {
    const NONE: u32 = u32::MAX;
    fn pack(self) -> u32 {
        (self.addr as u32) << 16 | (self.ep as u32) << 8 | self.kind as u32
    }
    fn unpack(v: u32) -> Option<Key> {
        (v != Self::NONE).then_some(Key {
            addr: (v >> 16) as u8,
            ep: (v >> 8) as u8,
            kind: v as u8,
        })
    }
}

struct Slot {
    key: AtomicU32,
    pipe: Mutex<CriticalSectionRawMutex, Option<AnyPipe>>,
    /// Milliseconds since boot at last use.
    last_use: AtomicU32,
}

impl Slot {
    fn key(&self) -> Option<Key> {
        Key::unpack(self.key.load(Relaxed))
    }
    fn set_key(&self, k: Option<Key>) {
        self.key.store(k.map_or(Key::NONE, Key::pack), Relaxed);
    }
}

/// As many as the driver has EPX pipes (`EPX_MAX_PIPES` in embassy-rp's
/// host driver): control and bulk pipes are EPX pipes, and only this cache
/// allocates them, so an allocation never fails for want of one.
/// (Interrupt OUT pipes take interrupt endpoints, shared with the readers.)
const SLOTS: usize = 16;
static PIPES: [Slot; SLOTS] = [const {
    Slot {
        key: AtomicU32::new(Key::NONE),
        pipe: Mutex::new(None),
        last_use: AtomicU32::new(0),
    }
}; SLOTS];

/// A pipe slot's lock, polled: `Mutex::lock` keeps one waker, and two
/// tasks waiting for the same slot would wake each other without end.
async fn lock_pipe(s: &'static Slot) -> embassy_sync::mutex::MutexGuard<'static, CriticalSectionRawMutex, Option<AnyPipe>> {
    loop {
        if let Ok(g) = s.pipe.try_lock() {
            return g;
        }
        Timer::after_micros(100).await;
    }
}

// ---- write guard (feature `write-guard`) --------------------------------------

/// Sticks nobody may write to, whoever sends the requests: the engine
/// learns which device sits at which address from the device descriptors
/// it fetches, and refuses a Bulk-Only command block (CBW) to a protected
/// device unless its SCSI opcode is on a list of commands that leave the
/// medium alone. A refused CBW never reaches the bus; the request ends
/// with STALL, as a device that rejects a command would. An address whose
/// device is not known (no descriptor seen since the last FORGET or bus
/// reset) counts as protected.
#[cfg(feature = "write-guard")]
mod guard {
    use super::*;

    /// (VID, PID): 346d:5678 'VendorCo ProductCode', 0781:5583 SanDisk
    /// 3.2Gen1. Edit the list for the sticks to protect.
    const PROTECTED: &[(u16, u16)] = &[(0x346d, 0x5678), (0x0781, 0x5583)];
    /// SCSI opcodes that pass: reads, status, capacity, mode sense, start /
    /// stop, medium lock, synchronize cache, verify, report LUNs.
    const HARMLESS: &[u8] = &[
        0x00, 0x03, 0x08, 0x12, 0x1a, 0x1b, 0x1e, 0x23, 0x25, 0x28, 0x2f, 0x35, 0x5a, 0x88, 0x91, 0x9e, 0xa0, 0xa8,
    ];
    const UNKNOWN: u32 = u32::MAX;

    /// Per device address: VID << 16 | PID.
    static IDS: [AtomicU32; 128] = [const { AtomicU32::new(UNKNOWN) }; 128];
    /// Refused CBWs, and the opcode of the last one (PING).
    pub static REFUSED: AtomicU32 = AtomicU32::new(0);
    pub static LAST_OP: AtomicU8 = AtomicU8::new(0);

    /// After a control IN: a GET_DESCRIPTOR(device) of 18 bytes names the
    /// device at `addr`.
    pub fn learn(addr: u8, setup: &[u8; 8], data: &[u8]) {
        if setup[0] == 0x80 && setup[1] == 0x06 && setup[3] == 0x01 && data.len() >= 18 && data[1] == 0x01 {
            let vid = u16::from_le_bytes([data[8], data[9]]);
            let pid = u16::from_le_bytes([data[10], data[11]]);
            IDS[addr as usize & 0x7f].store((vid as u32) << 16 | pid as u32, Relaxed);
            info!("guard: addr {} is {:04x}:{:04x}{}", addr, vid, pid, if protected(addr) { ", protected" } else { "" });
        }
    }

    /// FORGET of an address, or every address (bus reset, disconnect).
    pub fn forget(addr: Option<u8>) {
        match addr {
            Some(a) => IDS[a as usize & 0x7f].store(UNKNOWN, Relaxed),
            None => IDS.iter().for_each(|v| v.store(UNKNOWN, Relaxed)),
        }
    }

    fn protected(addr: u8) -> bool {
        let id = IDS[addr as usize & 0x7f].load(Relaxed);
        id == UNKNOWN || PROTECTED.iter().any(|&(v, p)| id == (v as u32) << 16 | p as u32)
    }

    /// Whether this bulk OUT payload must not go out: a CBW to a protected
    /// device with an opcode that is not known to be harmless.
    pub fn refuse(addr: u8, data: &[u8]) -> bool {
        if data.len() < 31 || &data[..4] != b"USBC" || !protected(addr) {
            return false;
        }
        let op = data[15];
        if HARMLESS.contains(&op) {
            return false;
        }
        REFUSED.fetch_add(1, Relaxed);
        LAST_OP.store(op, Relaxed);
        warn!("guard: refused SCSI opcode {:02x} to addr {}", op, addr);
        true
    }
}

// ---- root port -------------------------------------------------------------

/// Owns the bus controller: tracks connect / disconnect, executes resets.
#[embassy_executor::task]
async fn port_task(mut ctrl: BusController<'static, Dev>) {
    loop {
        match select(ctrl.wait_for_device_event(), PORT_CMD.receive()).await {
            Either::First(ev) => {
                use embassy_usb_driver::host::DeviceEvent::*;
                match ev {
                    Connected(s) => {
                        ROOT_SPEED.store(if s == Speed::Low { 1 } else { 2 }, Relaxed);
                        ROOT_CONNECTED.store(true, Relaxed);
                        info!("usb: root port connected, {}", if s == Speed::Low { "LS" } else { "FS" });
                        #[cfg(feature = "usb-prof")]
                        trace_op(0x10, 0, 0, 0, 0);
                    }
                    Disconnected => {
                        ROOT_CONNECTED.store(false, Relaxed);
                        ROOT_SPEED.store(0, Relaxed);
                        info!("usb: root port disconnected");
                        #[cfg(feature = "usb-prof")]
                        trace_op(0x11, 0, 0, 0, 0);
                    }
                    _ => warn!("usb: root port {:?}", ev),
                }
                end_out_streams();
                drop_pipes(None).await;
            }
            Either::Second(()) => {
                info!("usb: root port bus reset");
                end_out_streams();
                ctrl.controller_mut().bus_reset().await;
                drop_pipes(None).await;
                PORT_DONE.signal(());
            }
        }
    }
}

/// A root-port event or a bus reset: every streamed bulk OUT ends (the
/// pipe that keeps its data toggle is about to go, and with a bus reset
/// the device's state too). A disconnect ends them as offline.
fn end_out_streams() {
    PORT_GEN.fetch_add(1, Relaxed);
    let why = if ROOT_CONNECTED.load(Relaxed) { ST_ABORTED } else { ST_OFFLINE };
    for (s, f) in STREAM_OUT.iter().zip(INFLIGHT.iter()) {
        if s.lock(|c| c.get()).is_some() && f.active.load(Relaxed) {
            f.abort(why);
        }
    }
}

/// Drop cached pipes: all, or those of one (address, endpoint) / address.
async fn drop_pipes(sel: Option<(u8, Option<u8>)>) {
    #[cfg(feature = "write-guard")]
    match sel {
        None => guard::forget(None),
        Some((addr, None)) => guard::forget(Some(addr)),
        Some((_, Some(_))) => {}
    }
    drop_readers(sel).await;
    for s in PIPES.iter() {
        let hit = match (s.key(), sel) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(k), Some((addr, None))) => k.addr == addr,
            (Some(k), Some((addr, Some(ep)))) => k.addr == addr && (k.ep & 0x8f) == (ep & 0x8f),
        };
        if hit {
            // Waits for a transfer in progress on it to finish.
            let mut p = lock_pipe(s).await;
            *p = None;
            s.set_key(None);
        }
    }
}

// ---- workers -------------------------------------------------------------

#[embassy_executor::task(pool_size = WORKERS)]
async fn worker(bus: Bus, id: usize, spawner: Spawner) {
    loop {
        // Never full: every worker is in it at most once.
        let _ = IDLE.try_send(id as u8);
        // Requests and replies are passed by reference: a value moved into
        // an awaited future keeps its space here too (16 workers).
        let mut req = WORK[id].receive().await;
        let f = &INFLIGHT[id];
        f.seq.store(req.x.seq, Relaxed);
        f.abort.reset();
        f.why.store(ST_ABORTED, Relaxed);
        f.window.store(matches!(req.origin, Origin::Window(_)), Relaxed);
        f.active.store(true, Relaxed);
        let mut rep = reply_for(&req, [0; 3]);
        if req.x.stream_out().is_some() {
            stream_out(&bus, spawner, id, &mut req, &mut rep).await;
        } else {
            transfer(&bus, spawner, id, &mut req, &mut rep).await;
        }
    }
}

/// A reply of an empty slate: `req`'s seq and origin, status host error.
fn reply_for(req: &Request, t: [u32; 3]) -> Reply {
    Reply {
        seq: req.x.seq,
        origin: req.origin,
        status: ST_HOSTERROR,
        actual: 0,
        more: false,
        bare: false,
        data: [0; MAX_DATA],
        t,
        big: req.x.kind == XT_BULK && req.x.len >= 512,
    }
}

/// `rep` made ready for `req` without touching its data buffer.
fn reset_reply(rep: &mut Reply, req: &Request, t: [u32; 3]) {
    rep.seq = req.x.seq;
    rep.origin = req.origin;
    rep.status = ST_HOSTERROR;
    rep.actual = 0;
    rep.more = false;
    rep.bare = false;
    rep.t = t;
    rep.big = req.x.kind == XT_BULK && req.x.len >= 512;
}

/// One record of `req` on the bus into `rep`, ended early by an abort.
/// The reply gets its times; status and length are the caller's.
async fn record(bus: &Bus, spawner: Spawner, id: usize, req: &Request, rep: &mut Reply) -> Result<usize, u8> {
    reset_reply(rep, req, [req.t_take, cyc(), 0]);
    // Interrupt IN polls would push everything else out of the flight
    // recorder (a moving mouse: 200 a second): only their failures go in.
    #[cfg(feature = "usb-prof")]
    let ti = (req.x.kind != XT_INTERRUPT || req.x.ep & 0x80 == 0).then(|| trace_start(&req.x, &req.data));
    let r = match select(execute(bus, spawner, req, &mut rep.data), INFLIGHT[id].abort.wait()).await {
        Either::First(r) => r,
        Either::Second(()) => Err(INFLIGHT[id].why.load(Relaxed)),
    };
    rep.t[2] = cyc();
    #[cfg(feature = "usb-prof")]
    match (ti, r) {
        (Some(i), _) => trace_end(i, &req.x, r, &rep.data),
        (None, Err(st)) if st != ST_NAKTIMEOUT => trace_end(trace_start(&req.x, &req.data), &req.x, r, &rep.data),
        _ => {}
    }
    #[cfg(feature = "usb-prof")]
    {
        let b = &USB_PROF[!rep.big as usize];
        let usb = rep.t[2].wrapping_sub(rep.t[1]);
        b[0].fetch_add(1, Relaxed);
        b[1].fetch_add(rep.t[1].wrapping_sub(rep.t[0]), Relaxed);
        b[2].fetch_add(usb, Relaxed);
        b[5].fetch_max(usb, Relaxed);
    }
    if r.is_err() {
        ERRORS.fetch_add(1, Relaxed);
    }
    XFERS.fetch_add(1, Relaxed);
    r
}

/// Hand a reply to `next_reply` and wait until it is taken: the worker's
/// next job (or record) only starts after that.
async fn post(id: usize, rep: &Reply) {
    if !rep.more {
        INFLIGHT[id].active.store(false, Relaxed);
    }
    TAKEN[id].reset();
    DONE[id].lock(|d| *d.borrow_mut() = Some(rep.clone()));
    // Never full, as IDLE.
    let _ = READY.try_send(id as u8);
    TAKEN[id].wait().await;
}

/// A transfer of one record, or a streamed bulk IN: one record after the
/// other, without a round trip to the requester in between; a short or
/// failed one ends it.
async fn transfer(bus: &Bus, spawner: Spawner, id: usize, req: &mut Request, rep: &mut Reply) {
    let piece = req.x.len;
    let mut left = req.x.stream().unwrap_or(0);
    loop {
        if req.x.stream().is_some() {
            req.x.len = (piece as u32).min(left) as u16;
        }
        let r = record(bus, spawner, id, req, rep).await;
        match r {
            Ok(n) => {
                rep.status = ST_OK;
                rep.actual = n as u16;
                left = left.saturating_sub(n as u32);
                rep.more = left > 0 && n == req.x.len as usize && n > 0;
            }
            Err(st) => rep.status = st,
        }
        let more = rep.more;
        post(id, rep).await;
        if !more {
            break;
        }
        req.t_take = cyc();
    }
}

/// A streamed bulk OUT (docs/REGISTERS-USB.md): this worker keeps it until
/// the total is written or it fails. Each record is written, then
/// acknowledged with a reply without payload (MORE on all but the last);
/// the next record comes from `WORK[id]`, where `handle` puts the
/// stream's XFER_DATA records. The sender keeps at most two records
/// unacknowledged, so while one is written at most one waits there.
async fn stream_out(bus: &Bus, spawner: Spawner, id: usize, req: &mut Request, rep: &mut Reply) {
    let f = &INFLIGHT[id];
    // The first record's header holds for every record: endpoint, flags,
    // NAK timeout (per record), and the size of every record but the last.
    let first = req.x;
    let total = first.stream_out().unwrap_or(0);
    let piece = first.len as u32;
    let gen = PORT_GEN.load(Relaxed);
    let mut left = total;
    loop {
        let n = req.x.len as u32;
        // Every record but the last has the first one's size, a multiple
        // of the max packet size; the last brings the total.
        let fits = n > 0
            && n <= piece
            && (n == left || (n < left && n == piece))
            && first.mps > 0
            && piece % first.mps as u32 == 0
            && req.x.addr == first.addr
            && req.x.ep == first.ep;
        let last = n >= left;
        req.x = Xfer {
            len: req.x.len,
            // No zero-length packet between records; after the last one
            // as the first request asked (F_NOSHORT clear: a ZLP if the
            // total is a multiple of the max packet size).
            flags: if last { first.flags } else { first.flags | F_NOSHORT },
            ..first
        };
        let r = if !fits {
            ERRORS.fetch_add(1, Relaxed);
            reset_reply(rep, req, [0; 3]);
            Err(ST_BADPARAMS)
        } else if PORT_GEN.load(Relaxed) != gen {
            ERRORS.fetch_add(1, Relaxed);
            reset_reply(rep, req, [0; 3]);
            Err(if ROOT_CONNECTED.load(Relaxed) { ST_ABORTED } else { ST_OFFLINE })
        } else {
            record(bus, spawner, id, req, rep).await
        };
        rep.bare = true;
        match r {
            Ok(n) => {
                rep.status = ST_OK;
                rep.actual = n as u16;
                left = left.saturating_sub(n as u32);
                rep.more = left > 0;
            }
            Err(st) => rep.status = st,
        }
        let more = rep.more;
        if !more {
            // From here on a late XFER_DATA of this stream is dropped by
            // `handle`; what got into WORK before is drained below.
            STREAM_OUT[id].lock(|c| c.set(None));
        }
        post(id, rep).await;
        if !more {
            break;
        }
        // The next record. An abort (ABORT, queue reset, root port, a
        // sender that broke the two-record rule) comes first: a record may
        // wait in WORK already and must not be written then.
        let st = match select3(f.abort.wait(), WORK[id].receive(), Timer::after(STREAM_OUT_WAIT)).await {
            Either3::Second(r) => {
                // Same seq and origin (`handle` matched both); the header
                // is checked against the first one above.
                *req = r;
                continue;
            }
            Either3::First(()) => f.why.load(Relaxed),
            Either3::Third(()) => {
                warn!("usb: streamed OUT seq {}: no record for {} s", first.seq, STREAM_OUT_WAIT.as_secs());
                ST_TIMEOUT
            }
        };
        // Ended between two records.
        ERRORS.fetch_add(1, Relaxed);
        STREAM_OUT[id].lock(|c| c.set(None));
        reset_reply(rep, req, [0; 3]);
        rep.bare = true;
        rep.status = st;
        post(id, rep).await;
        break;
    }
    while WORK[id].try_receive().is_ok() {
        DATA_DROPPED.fetch_add(1, Relaxed);
    }
}

fn map_err(e: PipeError) -> u8 {
    match e {
        PipeError::Stall => ST_STALL,
        PipeError::Timeout => ST_TIMEOUT,
        PipeError::BufferOverflow => ST_OVERFLOW,
        PipeError::Babble => ST_BABBLE,
        PipeError::BadResponse | PipeError::DataToggleError => ST_CRC,
        PipeError::Disconnected => ST_OFFLINE,
        PipeError::Canceled => ST_ABORTED,
        _ => ST_HOSTERROR,
    }
}

/// Find or allocate the pipe for this transfer and run it. Returns the
/// number of bytes moved.
async fn execute(bus: &Bus, spawner: Spawner, req: &Request, out: &mut [u8; MAX_DATA]) -> Result<usize, u8> {
    let x = &req.x;
    if !ROOT_CONNECTED.load(Relaxed) {
        return Err(ST_OFFLINE);
    }
    let len = x.len as usize;
    if len > MAX_DATA || x.mps == 0 || x.addr > 127 {
        return Err(ST_BADPARAMS);
    }
    let is_in = x.ep & 0x80 != 0;
    if x.kind == XT_INTERRUPT && is_in {
        return int_in(bus, spawner, x, out).await;
    }
    #[cfg(feature = "write-guard")]
    if x.kind == XT_BULK && !is_in && guard::refuse(x.addr, &req.data[..len]) {
        return Err(ST_STALL);
    }
    let key = match x.kind {
        XT_CONTROL => Key { addr: x.addr, ep: x.ep & 0x0f, kind: XT_CONTROL },
        XT_BULK | XT_INTERRUPT => Key { addr: x.addr, ep: x.ep & 0x8f, kind: x.kind },
        _ => return Err(ST_BADPARAMS),
    };

    // Slot lookup: an existing pipe for this key, else a free slot.
    // Serialization on the same endpoint = the slot's lock.
    let mut idx = None;
    for (i, s) in PIPES.iter().enumerate() {
        if s.key() == Some(key) {
            idx = Some(i);
            break;
        }
    }
    let idx = match idx {
        Some(i) => i,
        None => {
            // An empty slot first; then the least recently used control
            // pipe (a control transfer starts with SETUP / DATA0: nothing
            // is lost); only then the least recently used of the rest, a
            // bulk or interrupt OUT pipe, whose endpoint's next transfer
            // then starts at DATA0 although the device may expect DATA1
            // (counted, PING `toggle-evictions`). Never a locked slot.
            let mut best: Option<(usize, u8, u32)> = None;
            for (i, s) in PIPES.iter().enumerate() {
                let Ok(p) = s.pipe.try_lock() else { continue };
                let rank = match (s.key(), p.is_some()) {
                    (Some(k), true) => (k.kind != XT_CONTROL) as u8 + 1,
                    _ => 0,
                };
                let t = s.last_use.load(Relaxed);
                if best.is_none_or(|(_, br, bt)| (rank, t) < (br, bt)) {
                    best = Some((i, rank, t));
                }
            }
            let (i, rank, _) = best.ok_or(ST_HOSTERROR)?;
            // The pipe in it belongs to the endpoint the slot had before:
            // its device address, PRE, max packet size and toggle are not
            // this one's. Free since the scan (no await in between).
            let mut p = PIPES[i].pipe.try_lock().map_err(|_| ST_HOSTERROR)?;
            if rank > 0 {
                let old = PIPES[i].key().unwrap_or(key);
                EVICTIONS.fetch_add(1, Relaxed);
                if rank == 2 {
                    TOGGLE_EVICTIONS.fetch_add(1, Relaxed);
                    warn!("usb: pipe cache full, addr {} ep {:02x} lost its data toggle", old.addr, old.ep);
                }
                #[cfg(feature = "usb-prof")]
                trace_op(if rank == 2 { 0x12 } else { 0x13 }, x.seq, old.addr, old.ep, old.kind as u16);
            }
            *p = None;
            PIPES[i].set_key(Some(key));
            i
        }
    };
    let slot = &PIPES[idx];
    let mut guard = lock_pipe(slot).await;
    // The slot was reassigned while we waited for it.
    if slot.key() != Some(key) {
        return Err(ST_HOSTERROR);
    }
    slot.last_use.store(Instant::now().as_millis() as u32, Relaxed);

    if guard.is_none() {
        let split = if x.flags & F_PRE != 0 {
            Some(SplitInfo::new(x.hub_addr, x.hub_port, SplitSpeed::Low))
        } else {
            None
        };
        let _ = x.flags & F_LOWSPEED; // speed of a direct LS device: the root port knows
        let ep_info = |ty: EndpointType, dir: Direction| EndpointInfo {
            addr: EndpointAddress::from_parts((x.ep & 0x0f) as usize, dir),
            ep_type: ty,
            max_packet_size: x.mps,
            // Interrupt OUT only; a cached pipe keeps its first interval.
            interval_ms: x.interval,
        };
        let p = match (x.kind, is_in) {
            (XT_CONTROL, _) => bus
                .alloc_pipe::<pipe::Control, pipe::InOut>(x.addr, &ep_info(EndpointType::Control, Direction::In), split)
                .map(AnyPipe::Ctrl),
            (XT_BULK, true) => bus
                .alloc_pipe::<pipe::Bulk, pipe::In>(x.addr, &ep_info(EndpointType::Bulk, Direction::In), split)
                .map(AnyPipe::BulkIn),
            (XT_BULK, false) => bus
                .alloc_pipe::<pipe::Bulk, pipe::Out>(x.addr, &ep_info(EndpointType::Bulk, Direction::Out), split)
                .map(AnyPipe::BulkOut),
            (XT_INTERRUPT, false) => bus
                .alloc_pipe::<pipe::Interrupt, pipe::Out>(x.addr, &ep_info(EndpointType::Interrupt, Direction::Out), split)
                .map(AnyPipe::IntOut),
            _ => return Err(ST_BADPARAMS),
        };
        match p {
            Ok(p) => *guard = Some(p),
            Err(e) => {
                warn!("usb: pipe alloc failed for addr {} ep {:02x}: {:?}", x.addr, x.ep, e);
                slot.set_key(None);
                return Err(ST_HOSTERROR);
            }
        }
    }
    let p = guard.as_mut().unwrap();

    let timeout = if x.timeout_ms == 0 { None } else { Some(Duration::from_millis(x.timeout_ms as u64)) };
    let ensure_end = x.flags & F_NOSHORT == 0;
    let t0 = Instant::now();
    let r = match p {
        AnyPipe::Ctrl(c) => {
            debug!("setup addr {} {=[u8]:02x}", x.addr, x.setup);
            if x.setup[0] & 0x80 != 0 {
                run(timeout, c.control_in(&x.setup, &mut out[..len])).await
            } else {
                run(timeout, c.control_out(&x.setup, &req.data[..len])).await.map(|_| len)
            }
        }
        AnyPipe::BulkIn(c) => run(timeout, c.request_in(&mut out[..len])).await,
        AnyPipe::BulkOut(c) => run(timeout, c.request_out(&req.data[..len], ensure_end)).await.map(|_| len),
        AnyPipe::IntOut(c) => run(timeout, c.request_out(&req.data[..len], ensure_end)).await.map(|_| len),
    };
    if x.kind == XT_BULK {
        debug!("bulk addr {} ep {:02x} len {} -> {} in {} ms", x.addr, x.ep, len, r, t0.elapsed().as_millis());
    }
    #[cfg(feature = "write-guard")]
    if let (XT_CONTROL, Ok(n)) = (x.kind, r) {
        guard::learn(x.addr, &x.setup, &out[..n]);
    }
    r
}

async fn run<T>(
    timeout: Option<Duration>,
    fut: impl core::future::Future<Output = Result<T, PipeError>>,
) -> Result<T, u8> {
    let Some(t) = timeout else {
        return fut.await.map_err(map_err);
    };
    // Not `with_timeout`: that polls its timer (the time queue, a critical
    // section) with every poll of `fut`, once per USB packet. The timer is
    // armed here, and again at most once per ms (another timer of this task
    // that fires takes the task's place in the time queue); in between only
    // the clock is compared.
    let deadline = Instant::now() + t;
    let mut fut = core::pin::pin!(fut);
    let mut armed = Instant::MIN;
    let mut first = true;
    core::future::poll_fn(|cx| {
        if let Poll::Ready(r) = fut.as_mut().poll(cx) {
            return Poll::Ready(r.map_err(map_err));
        }
        let now = Instant::now();
        if now >= deadline {
            return Poll::Ready(Err(ST_NAKTIMEOUT));
        }
        if first || now >= armed + Duration::from_millis(1) {
            first = false;
            armed = now;
            let _ = core::pin::Pin::new(&mut Timer::at(deadline)).poll(cx);
        }
        Poll::Pending
    })
    .await
}

// ---- request records -----------------------------------------------------------

/// One request record. Returns the length of an immediate reply in `tx`,
/// or 0 when the request went to the workers (its reply comes out of
/// [`next_reply`]) or was not a request.
pub async fn handle(rx: &[u8], tx: &mut [u8], origin: Origin) -> usize {
    let n = rx.len();
    if n < REQ_HDR || u16::from_be_bytes([rx[0], rx[1]]) != MAGIC || rx[2] != VERSION {
        return 0;
    }
    let op = rx[3];
    let seq = u16::from_be_bytes([rx[4], rx[5]]);
    let x = Xfer {
        seq,
        addr: rx[6],
        ep: rx[7],
        kind: rx[8],
        flags: rx[9],
        mps: u16::from_be_bytes([rx[10], rx[11]]),
        timeout_ms: u16::from_be_bytes([rx[12], rx[13]]),
        len: u16::from_be_bytes([rx[14], rx[15]]),
        hub_addr: rx[16],
        hub_port: rx[17],
        setup: unwrap!(rx[18..26].try_into()),
        interval: u16::from_be_bytes([rx[26], rx[27]]).clamp(1, 255) as u8,
    };
    match op {
        OP_PING => {
            // write-guard: room for " refused N last-op XX".
            let mut body = [0u8; if cfg!(feature = "write-guard") { 176 } else { 144 }];
            let name = *NAME.lock().await;
            let mut w = 0;
            for part in [&b"pzusb "[..], name.as_bytes(), b" "] {
                let k = part.len().min(body.len() - w);
                body[w..w + k].copy_from_slice(&part[..k]);
                w += k;
            }
            for (label, v) in [
                (&b"xfers "[..], XFERS.load(Relaxed)),
                (&b" errors "[..], ERRORS.load(Relaxed)),
                (&b" frame-errors "[..], LINK_ERRORS.load(Relaxed)),
                (&b" data-dropped "[..], DATA_DROPPED.load(Relaxed)),
                (&b" evictions "[..], EVICTIONS.load(Relaxed)),
                (&b" toggle-evictions "[..], TOGGLE_EVICTIONS.load(Relaxed)),
            ] {
                if w + label.len() + 10 > body.len() {
                    break;
                }
                body[w..w + label.len()].copy_from_slice(label);
                w += label.len();
                w += dec(&mut body[w..], v);
            }
            // write-guard: " refused N", and the last refused opcode.
            #[cfg(feature = "write-guard")]
            {
                let n = guard::REFUSED.load(Relaxed);
                body[w..w + 9].copy_from_slice(b" refused ");
                w += 9;
                w += dec(&mut body[w..], n);
                if n > 0 {
                    let o = guard::LAST_OP.load(Relaxed);
                    let hex = |d: u8| if d < 10 { b'0' + d } else { b'a' + d - 10 };
                    body[w..w + 11].copy_from_slice(&[b' ', b'l', b'a', b's', b't', b'-', b'o', b'p', b' ', hex(o >> 4), hex(o & 15)]);
                    w += 11;
                }
            }
            pack(tx, op, seq, ST_OK, &body[..w])
        }
        OP_PORT_STATUS | OP_PORT_RESET => {
            if op == OP_PORT_RESET {
                #[cfg(feature = "usb-prof")]
                trace_op(op, seq, 0, 0, 0);
                PORT_DONE.reset();
                PORT_CMD.send(()).await;
                PORT_DONE.wait().await;
            }
            let body = [ROOT_CONNECTED.load(Relaxed) as u8, ROOT_SPEED.load(Relaxed), FEAT_STREAM_IN | FEAT_STREAM_OUT | FEAT_INTERVAL];
            pack(tx, op, seq, ST_OK, &body)
        }
        OP_XFER_DATA => {
            // Built in place: a Request moved into the awaited future
            // would keep its space in this one too.
            let mut slot = Some(Request { x, origin, data: [0; MAX_DATA], t_take: cyc() });
            if let Some(req) = slot.as_mut() {
                let have = (n - REQ_HDR).min((x.len as usize).min(MAX_DATA));
                req.data[..have].copy_from_slice(&rx[REQ_HDR..REQ_HDR + have]);
            }
            to_stream(&mut slot).await;
            0
        }
        OP_XFER => {
            let dlen = (x.len as usize).min(MAX_DATA);
            let mut req = Request { x, origin, data: [0; MAX_DATA], t_take: cyc() };
            let have = (n - REQ_HDR).min(dlen);
            req.data[..have].copy_from_slice(&rx[REQ_HDR..REQ_HDR + have]);
            // Back-pressure: wait for an idle worker; the sender keeps at
            // most WORKERS in flight.
            let id = IDLE.receive().await as usize;
            if x.stream_out().is_some() {
                // Before the worker runs: the stream's next record may be
                // the very next request.
                STREAM_OUT[id].lock(|c| c.set(Some((seq, origin))));
            }
            let _ = WORK[id].try_send(req);
            0
        }
        OP_ABORT => {
            #[cfg(feature = "usb-prof")]
            trace_op(op, seq, x.addr, x.ep, x.len);
            let target = x.len;
            for f in INFLIGHT.iter() {
                if f.active.load(Relaxed) && f.seq.load(Relaxed) == target {
                    f.abort(ST_ABORTED);
                }
            }
            pack(tx, op, seq, ST_OK, &[])
        }
        OP_FORGET | OP_RESET_TOGGLE => {
            #[cfg(feature = "usb-prof")]
            trace_op(op, seq, x.addr, x.ep, 0);
            let ep = if op == OP_RESET_TOGGLE { Some(x.ep) } else { None };
            drop_pipes(Some((x.addr, ep))).await;
            pack(tx, op, seq, ST_OK, &[])
        }
        _ => pack(tx, op, seq, ST_BADPARAMS, &[]),
    }
}

/// An XFER_DATA record into the `WORK` slot of the worker that keeps its
/// stream (same seq, same origin), or dropped when there is none.
///
/// Why waiting here cannot deadlock: the worker takes the next record
/// from its slot only after its previous reply was taken, and replies are
/// taken by `next_reply`, which runs in the same loop as this function
/// (`WindowHost::run`) and so not while it waits.
/// A sender that keeps the two-record rule sends record k+2 only after it
/// has the reply to record k; that reply was taken, so the worker has its
/// TAKEN signal, and record k+1, which still fills the slot, goes as soon
/// as the worker is polled once. The same holds for the first XFER_DATA:
/// the slot still holds the first record when the worker has not run yet.
/// The waits below let the executor poll it. If the slot stays full the
/// worker is waiting for a reply that this loop has not taken: the sender
/// sent too much. Then the stream ends (status bad parameters) and the
/// record is dropped; the wait is bounded, so this loop goes on taking
/// replies and the worker gets to see its abort.
async fn to_stream(slot: &mut Option<Request>) {
    let Some(seq) = slot.as_ref().map(|r| r.x.seq) else { return };
    let key = slot.as_ref().map(|r| (seq, r.origin));
    let Some(id) = STREAM_OUT.iter().position(|s| s.lock(|c| c.get()) == key) else {
        DATA_DROPPED.fetch_add(1, Relaxed);
        debug!("usb: XFER_DATA seq {}: no stream, dropped", seq);
        return;
    };
    for _ in 0..HANDOFF_STEPS {
        // The stream may have ended meanwhile (error, abort): its worker
        // drains the slot once and must not get another record after that.
        if STREAM_OUT[id].lock(|c| c.get()) != key {
            DATA_DROPPED.fetch_add(1, Relaxed);
            return;
        }
        let Some(req) = slot.take() else { return };
        match WORK[id].try_send(req) {
            Ok(()) => return,
            Err(embassy_sync::channel::TrySendError::Full(r)) => *slot = Some(r),
        }
        Timer::after(HANDOFF_STEP).await;
    }
    DATA_DROPPED.fetch_add(1, Relaxed);
    if STREAM_OUT[id].lock(|c| c.get()) == key {
        warn!("usb: streamed OUT seq {}: more than two records ahead, ending it", seq);
        INFLIGHT[id].abort(ST_BADPARAMS);
    }
}

/// A reply record: header + `body`, into `tx`; its length.
pub fn pack(tx: &mut [u8], op: u8, seq: u16, status: u8, body: &[u8]) -> usize {
    tx[0..2].copy_from_slice(&MAGIC.to_be_bytes());
    tx[2] = VERSION;
    tx[3] = op;
    tx[4..6].copy_from_slice(&seq.to_be_bytes());
    tx[6] = status;
    tx[7] = 0;
    tx[8..10].copy_from_slice(&(body.len() as u16).to_be_bytes());
    tx[10..12].copy_from_slice(&[0, 0]);
    tx[REP_HDR..REP_HDR + body.len()].copy_from_slice(body);
    REP_HDR + body.len()
}

fn dec(out: &mut [u8], mut v: u32) -> usize {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let n = tmp.len() - i;
    out[..n].copy_from_slice(&tmp[i..]);
    n
}

// ---- the USB window ------------------------------------------------------------

/// The host side of the USB window (`pz_core::usb`): request records in,
/// replies out through the completion queue, the root port into STATUS.
pub struct WindowHost {
    host: HostPort<'static>,
    /// A reply that did not fit into the completion queue yet.
    held: Option<Reply>,
    rx: [u8; win::SLOT_BYTES],
    tx: [u8; win::SLOT_BYTES],
}

impl WindowHost {
    pub fn new(host: HostPort<'static>) -> Self {
        WindowHost { host, held: None, rx: [0; win::SLOT_BYTES], tx: [0; win::SLOT_BYTES] }
    }

    /// Whether a reply waits for room in the completion queue; while one
    /// does, take no other replies (their order stays).
    pub fn holding(&self) -> bool {
        self.held.is_some()
    }

    /// Post a finished transfer that came through the window (others are
    /// not ours and are dropped).
    pub fn deliver(&mut self, rep: Reply) {
        #[cfg(feature = "usb-prof")]
        if matches!(rep.origin, Origin::Window(_)) && rep.t[0] != 0 {
            let now = cyc();
            USB_PROF[!rep.big as usize][3].fetch_add(now.wrapping_sub(rep.t[2]), Relaxed);
            LAST_POST.store(now, Relaxed);
        }
        if let Origin::Window(t) = rep.origin {
            let n = rep.pack(&mut self.tx);
            if self.host.post_cpl(t, &self.tx[..n]) == Ok(false) {
                self.held = Some(rep);
            }
        }
    }

    /// Everything the host side has to do after bus cycles or a worker
    /// reply: follow the root port, drop work voided by a queue reset,
    /// retry a held reply, and take new requests while their replies are
    /// sure to fit.
    pub async fn service(&mut self) {
        let (connected, low) = root();
        self.host.set_port(connected, low);
        if self.host.take_reset() {
            // Streamed bulk OUTs among them: their next record would come
            // with the new epoch, from a driver that knows nothing of them.
            for f in INFLIGHT.iter() {
                if f.active.load(Relaxed) && f.window.load(Relaxed) {
                    f.abort(ST_ABORTED);
                }
            }
            self.held = None;
        }
        if let Some(rep) = self.held.take() {
            self.deliver(rep);
            if self.held.is_some() {
                return;
            }
        }
        while self.host.cpl_free() {
            let Some((n, t)) = self.host.take_req(&mut self.rx) else { break };
            #[cfg(feature = "usb-prof")]
            {
                // Posted -> this take, charged to the class of the new request.
                let last = LAST_POST.swap(0, Relaxed);
                let rx = &self.rx[..n];
                if last != 0 && n >= REQ_HDR && (rx[3] == OP_XFER || rx[3] == OP_XFER_DATA) {
                    {
                        let big = rx[8] == XT_BULK && u16::from_be_bytes([rx[14], rx[15]]) >= 512;
                        USB_PROF[!big as usize][4].fetch_add(cyc().wrapping_sub(last), Relaxed);
                    }
                }
            }
            let m = handle(&self.rx[..n], &mut self.tx, Origin::Window(t)).await;
            if m > 0 {
                // cpl_free held a slot for exactly this.
                let _ = self.host.post_cpl(t, &self.tx[..m]);
            }
        }
    }

    /// Serve the window for good, when the window's replies are the only
    /// ones (`picozorro`): the queues are looked at every `poll`, and at
    /// once when a transfer finishes.
    pub async fn run(mut self, poll: Duration) -> ! {
        loop {
            if self.holding() {
                Timer::after(poll).await;
            } else if let Either::First(rep) = select(next_reply(), Timer::after(poll)).await {
                self.deliver(rep);
            }
            self.service().await;
        }
    }
}
