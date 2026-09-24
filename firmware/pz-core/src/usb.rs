//! USB register window (`docs/REGISTERS-USB.md`), pure logic.
//!
//! The second 256-byte window of the board (A16 = 1). The Amiga driver
//! talks to the host controller in records: it writes a request record
//! (28-byte header + OUT data) through a data port and commits it, and reads
//! completion records (12-byte header + IN data) from another port. The
//! firmware's USB task takes requests, runs them on the host controller in
//! any order and posts their completions, matched by `seq`.
//!
//! Same scheme as `nic`: [`Shared`] holds everything both sides see, every
//! shared word has one writer, the two record queues are single-producer /
//! single-consumer rings with free-running `u32` indices, latched interrupt
//! bits are event counters compared with acknowledge counters. The bus side
//! ([`BusPort`]) is O(1) per register access; the host side ([`HostPort`])
//! is the embassy USB task.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering::*};

/// Request records the Amiga can queue.
pub const REQ_SLOTS: usize = 16;
/// Completion records the firmware can queue.
pub const CPL_SLOTS: usize = 16;
/// Request record header.
pub const REQ_HDR: usize = 28;
/// Completion record header.
pub const CPL_HDR: usize = 12;
/// Data bytes per record, either direction.
pub const MAX_DATA: usize = 1024;
/// Bytes per queue slot.
pub const SLOT_BYTES: usize = REQ_HDR + MAX_DATA;

pub const MAGIC: u16 = 0x5055; // "PU"
pub const VERSION: u16 = 0x0001;

/// Register offsets within the USB window.
pub mod reg {
    pub const MAGIC: u8 = 0x00;
    pub const VERSION: u8 = 0x02;
    pub const INT: u8 = 0x04;
    pub const INT_ENABLE: u8 = 0x06;
    pub const CTRL: u8 = 0x08;
    pub const STATUS: u8 = 0x0a;
    pub const REQ_LEN: u8 = 0x10;
    pub const REQ_DATA: u8 = 0x12;
    pub const REQ_COMMIT: u8 = 0x14;
    pub const CPL_LEN: u8 = 0x20;
    pub const CPL_DATA: u8 = 0x22;
    pub const CPL_DONE: u8 = 0x24;
    /// Three 32-bit counters, high word first, `$40..=$4a`.
    pub const STATS: u8 = 0x40;
    pub const STATS_LAST: u8 = 0x4a;
}

/// INT / INT_ENABLE bits.
pub mod int {
    /// Level: the completion queue is not empty.
    pub const CPL_AVAIL: u16 = 1 << 0;
    /// A request slot came free.
    pub const REQ_FREE: u16 = 1 << 1;
    /// Something on the root port changed (connect, disconnect, speed).
    pub const PORT_CHANGE: u16 = 1 << 2;
    pub const BUS_ERROR: u16 = 1 << 4;
    pub const ALL: u16 = CPL_AVAIL | REQ_FREE | PORT_CHANGE | BUS_ERROR;
}

/// CTRL bits (actions; CTRL reads 0).
pub mod ctrl {
    /// Empty both queues, abort an open request; the firmware drops what it
    /// has in flight and posts no completion for it.
    pub const RESET_QUEUES: u16 = 1 << 15;
}

/// STATUS bits (REQ_FREE in 12-8 and CPL_QUEUED in 7-0 are counts).
pub mod status {
    pub const CONNECTED: u16 = 1 << 15;
    pub const LOW_SPEED: u16 = 1 << 14;
}

/// STATS counters, in register order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stat {
    /// Request records committed.
    Requests = 0,
    /// Completion records posted.
    Completions = 1,
    /// Request records dropped: short commit, bad length, queue full.
    ReqErrors = 2,
}

/// A record ring's storage, one cell per slot (see `nic::Slots`).
struct Slots<const N: usize> {
    len: [AtomicU16; N],
    data: [UnsafeCell<[u8; SLOT_BYTES]>; N],
}

impl<const N: usize> Slots<N> {
    const fn new() -> Self {
        Slots { len: [const { AtomicU16::new(0) }; N], data: [const { UnsafeCell::new([0; SLOT_BYTES]) }; N] }
    }
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn slot(&self, i: u32) -> *mut [u8; SLOT_BYTES] {
        self.data[i as usize % N].get()
    }
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn len(&self, i: u32) -> usize {
        self.len[i as usize % N].load(Relaxed) as usize
    }
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn set_len(&self, i: u32, len: usize) {
        self.len[i as usize % N].store(len as u16, Relaxed);
    }
}

#[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
fn bump(a: &AtomicU32) {
    a.store(a.load(Relaxed).wrapping_add(1), Release);
}

pub struct Shared {
    req: Slots<REQ_SLOTS>,
    req_w: AtomicU32, // bus side
    req_r: AtomicU32, // host side

    cpl: Slots<CPL_SLOTS>,
    cpl_w: AtomicU32, // host side
    cpl_r: AtomicU32, // bus side

    // Written by the bus side.
    int_enable: AtomicU16,
    bus_error: AtomicU16,
    ack_req_free: AtomicU32,
    ack_port: AtomicU32,
    /// Bumped by RESET_QUEUES, /BUSRST and shut-up: requests taken before
    /// it are void.
    epoch: AtomicU32,
    req_flush_to: AtomicU32,
    requests: AtomicU32,
    req_errors: AtomicU32,

    // Written by the host side.
    status: AtomicU16,
    ev_req_free: AtomicU32,
    ev_port: AtomicU32,
    completions: AtomicU32,
}

// SAFETY: as `nic::Shared`: a slot is written only by its ring's producer
// while outside [read, write) and read only by the consumer while inside;
// index stores (release) and loads (acquire) order the data accesses, and
// `split` hands out exactly one producer and one consumer per ring.
unsafe impl Sync for Shared {}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub const fn new() -> Self {
        Shared {
            req: Slots::new(),
            req_w: AtomicU32::new(0),
            req_r: AtomicU32::new(0),
            cpl: Slots::new(),
            cpl_w: AtomicU32::new(0),
            cpl_r: AtomicU32::new(0),
            int_enable: AtomicU16::new(0),
            bus_error: AtomicU16::new(0),
            ack_req_free: AtomicU32::new(0),
            ack_port: AtomicU32::new(0),
            epoch: AtomicU32::new(0),
            req_flush_to: AtomicU32::new(0),
            requests: AtomicU32::new(0),
            req_errors: AtomicU32::new(0),
            status: AtomicU16::new(0),
            ev_req_free: AtomicU32::new(0),
            ev_port: AtomicU32::new(0),
            completions: AtomicU32::new(0),
        }
    }

    pub fn split(&mut self) -> (BusPort<'_>, HostPort<'_>) {
        let s: &Shared = self;
        let e = s.epoch.load(Acquire);
        (BusPort::new(s), HostPort { s, epoch_seen: e, flush_seen: e })
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn int_status(&self) -> u16 {
        let mut v = self.bus_error.load(Acquire);
        if self.cpl_w.load(Acquire) != self.cpl_r.load(Acquire) {
            v |= int::CPL_AVAIL;
        }
        if self.ev_req_free.load(Acquire) != self.ack_req_free.load(Acquire) {
            v |= int::REQ_FREE;
        }
        if self.ev_port.load(Acquire) != self.ack_port.load(Acquire) {
            v |= int::PORT_CHANGE;
        }
        v
    }

    /// (INT & INT_ENABLE) != 0.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn irq_pending(&self) -> bool {
        self.int_status() & self.int_enable.load(Acquire) != 0
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn req_used(&self) -> u32 {
        self.req_w.load(Acquire).wrapping_sub(self.req_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn cpl_queued(&self) -> u32 {
        self.cpl_w.load(Acquire).wrapping_sub(self.cpl_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn stat(&self, i: usize) -> u32 {
        match i {
            0 => self.requests.load(Acquire),
            1 => self.completions.load(Acquire),
            _ => self.req_errors.load(Acquire),
        }
    }
}

/// The Amiga's view.
pub struct BusPort<'a> {
    s: &'a Shared,
    req_open: bool,
    req_len: u16,
    req_words: u16,
    cpl_pos: u16,
    stat_snap: Option<(usize, u32)>,
}

impl<'a> BusPort<'a> {
    fn new(s: &'a Shared) -> Self {
        BusPort { s, req_open: false, req_len: 0, req_words: 0, cpl_pos: 0, stat_snap: None }
    }

    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn set_bus_error(&self) {
        self.s.bus_error.store(int::BUS_ERROR, Release);
    }

    /// /BUSRST or Autoconfig shut-up: queues empty, in-flight work void,
    /// interrupts off and acknowledged. STATS stay.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        let s = self.s;
        self.reset_queues();
        s.int_enable.store(0, Release);
        s.ack_req_free.store(s.ev_req_free.load(Acquire), Release);
        s.ack_port.store(s.ev_port.load(Acquire), Release);
        s.bus_error.store(0, Release);
        self.stat_snap = None;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn reset_queues(&mut self) {
        let s = self.s;
        self.req_open = false;
        // flush_to before the epoch: a host that sees the new epoch sees
        // where to skip to.
        s.req_flush_to.store(s.req_w.load(Relaxed), Release);
        s.epoch.store(s.epoch.load(Relaxed).wrapping_add(1), Release);
        s.cpl_r.store(s.cpl_w.load(Acquire), Release);
        self.cpl_pos = 0;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn head_len(&self) -> u16 {
        let s = self.s;
        let r = s.cpl_r.load(Relaxed);
        if s.cpl_w.load(Acquire) == r {
            return 0;
        }
        s.cpl.len(r) as u16
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn cpl_word(&mut self, advance: bool) -> u16 {
        let len = self.head_len() as usize;
        let k = self.cpl_pos as usize;
        if 2 * k >= len {
            return 0;
        }
        let r = self.s.cpl_r.load(Relaxed);
        // SAFETY: slot r is published and stays ours until cpl_r moves.
        let d = unsafe { &*self.s.cpl.slot(r) };
        let hi = u16::from(d[2 * k]) << 8;
        let lo = if 2 * k + 1 < len { u16::from(d[2 * k + 1]) } else { 0 };
        if advance {
            self.cpl_pos += 1;
        }
        hi | lo
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, _lds: bool) -> u16 {
        let word = uds; // /LDS ignored: slave::Slave::write
        if !word {
            self.set_bus_error();
        }
        let s = self.s;
        match reg {
            reg::MAGIC => MAGIC,
            reg::VERSION => VERSION,
            reg::INT => s.int_status(),
            reg::INT_ENABLE => s.int_enable.load(Relaxed),
            reg::CTRL => 0,
            reg::STATUS => {
                let free = REQ_SLOTS as u32 - s.req_used().min(REQ_SLOTS as u32);
                s.status.load(Acquire) | (free as u16) << 8 | s.cpl_queued().min(0xff) as u16
            }
            reg::CPL_LEN => self.head_len(),
            reg::CPL_DATA => self.cpl_word(word),
            reg::STATS..=reg::STATS_LAST => {
                let off = (reg - reg::STATS) as usize;
                let i = off / 4;
                if off.is_multiple_of(4) {
                    let v = s.stat(i);
                    if word {
                        self.stat_snap = Some((i, v));
                    }
                    (v >> 16) as u16
                } else {
                    let v = match self.stat_snap {
                        Some((j, v)) if j == i => {
                            if word {
                                self.stat_snap = None;
                            }
                            v
                        }
                        _ => s.stat(i),
                    };
                    v as u16
                }
            }
            _ => 0xffff,
        }
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write(&mut self, reg: u8, data: u16, uds: bool, _lds: bool) {
        if !uds {
            self.set_bus_error();
            return;
        }
        let s = self.s;
        match reg {
            reg::INT => {
                if data & int::REQ_FREE != 0 {
                    s.ack_req_free.store(s.ev_req_free.load(Acquire), Release);
                }
                if data & int::PORT_CHANGE != 0 {
                    s.ack_port.store(s.ev_port.load(Acquire), Release);
                }
                if data & int::BUS_ERROR != 0 {
                    s.bus_error.store(0, Release);
                }
            }
            reg::INT_ENABLE => s.int_enable.store(data & int::ALL, Release),
            reg::CTRL => {
                if data & ctrl::RESET_QUEUES != 0 {
                    self.reset_queues();
                }
            }
            reg::REQ_LEN => {
                if self.req_open {
                    self.req_open = false;
                    bump(&s.req_errors);
                }
                let len = data as usize;
                if !(REQ_HDR..=SLOT_BYTES).contains(&len) || s.req_used() >= REQ_SLOTS as u32 {
                    bump(&s.req_errors);
                    self.set_bus_error();
                    return;
                }
                self.req_open = true;
                self.req_len = data;
                self.req_words = 0;
            }
            reg::REQ_DATA => {
                if !self.req_open || self.req_words >= self.req_len.div_ceil(2) {
                    self.set_bus_error();
                    return;
                }
                let w = s.req_w.load(Relaxed);
                let k = 2 * self.req_words as usize;
                // SAFETY: slot w is outside [req_r, req_w) (free at REQ_LEN).
                let d = unsafe { &mut *s.req.slot(w) };
                d[k] = (data >> 8) as u8;
                d[k + 1] = data as u8;
                self.req_words += 1;
            }
            reg::REQ_COMMIT => {
                if !self.req_open {
                    self.set_bus_error();
                    return;
                }
                self.req_open = false;
                if self.req_words < self.req_len.div_ceil(2) {
                    bump(&s.req_errors);
                    return;
                }
                let w = s.req_w.load(Relaxed);
                s.req.set_len(w, self.req_len as usize);
                s.req_w.store(w.wrapping_add(1), Release);
                bump(&s.requests);
            }
            reg::CPL_DONE => {
                let r = s.cpl_r.load(Relaxed);
                if s.cpl_w.load(Acquire) != r {
                    s.cpl_r.store(r.wrapping_add(1), Release);
                }
                self.cpl_pos = 0;
            }
            _ => {}
        }
    }
}

/// A request taken from the queue. `epoch` goes back with its completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub epoch: u32,
}

/// The USB task's view.
pub struct HostPort<'a> {
    s: &'a Shared,
    epoch_seen: u32, // take_reset
    flush_seen: u32, // take_req
}

impl<'a> HostPort<'a> {
    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    /// True once after each queue reset (RESET_QUEUES, /BUSRST, shut-up):
    /// drop everything in flight.
    pub fn take_reset(&mut self) -> bool {
        let e = self.s.epoch.load(Acquire);
        if e == self.epoch_seen {
            return false;
        }
        self.epoch_seen = e;
        true
    }

    /// Copy the next request record into `out` and free its slot. Returns
    /// its length and the ticket for its completion. A request is stamped
    /// with the epoch read before it was taken, so a reset in between voids
    /// it at worst (the driver's timeout covers that), never the reverse.
    pub fn take_req(&mut self, out: &mut [u8; SLOT_BYTES]) -> Option<(usize, Ticket)> {
        let s = self.s;
        let epoch = s.epoch.load(Acquire);
        if epoch != self.flush_seen {
            self.flush_seen = epoch;
            // Skip what was queued before the reset: forward only, and
            // only within what is published.
            let to = s.req_flush_to.load(Acquire);
            let r = s.req_r.load(Relaxed);
            if to != r && to.wrapping_sub(r) <= s.req_w.load(Acquire).wrapping_sub(r) {
                s.req_r.store(to, Release);
                bump(&s.ev_req_free);
            }
        }
        let r = s.req_r.load(Relaxed);
        if s.req_w.load(Acquire) == r {
            return None;
        }
        let len = s.req.len(r);
        // SAFETY: slot r is inside [req_r, req_w): ours until req_r moves.
        out[..len].copy_from_slice(unsafe { &(&*s.req.slot(r))[..len] });
        s.req_r.store(r.wrapping_add(1), Release);
        bump(&s.ev_req_free);
        Some((len, Ticket { epoch }))
    }

    /// Room for one more completion.
    pub fn cpl_free(&self) -> bool {
        (self.s.cpl_queued() as usize) < CPL_SLOTS
    }

    /// Post a completion record (header + IN data). Ok(false): the queue is
    /// full, try again; Ok(true): posted or dropped because its request
    /// predates a queue reset. Err: longer than a slot.
    pub fn post_cpl(&mut self, t: Ticket, rec: &[u8]) -> Result<bool, ()> {
        let s = self.s;
        if rec.len() < CPL_HDR || rec.len() > SLOT_BYTES {
            return Err(());
        }
        if s.epoch.load(Acquire) != t.epoch {
            return Ok(true);
        }
        let w = s.cpl_w.load(Relaxed);
        if w.wrapping_sub(s.cpl_r.load(Acquire)) >= CPL_SLOTS as u32 {
            return Ok(false);
        }
        // SAFETY: slot w is outside [cpl_r, cpl_w).
        unsafe { (&mut *s.cpl.slot(w))[..rec.len()].copy_from_slice(rec) };
        s.cpl.set_len(w, rec.len());
        s.cpl_w.store(w.wrapping_add(1), Release);
        bump(&s.completions);
        Ok(true)
    }

    /// Root port state; a change raises PORT_CHANGE.
    pub fn set_port(&mut self, connected: bool, low_speed: bool) {
        let s = self.s;
        let mut v = 0;
        if connected {
            v |= status::CONNECTED;
        }
        if connected && low_speed {
            v |= status::LOW_SPEED;
        }
        let old = s.status.load(Relaxed);
        s.status.store(v, Release);
        if old != v {
            bump(&s.ev_port);
        }
    }
}

#[cfg(test)]
mod tests;
