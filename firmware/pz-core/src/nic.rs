//! Network register window v2 (`docs/REGISTERS.md`), pure logic.
//!
//! The model is shared by two execution contexts on the RP2350 that never
//! wait for each other:
//! - the **bus side** ([`BusPort`]): the core-1 cycle loop answering Amiga
//!   reads and writes, hard real time, one call per bus cycle;
//! - the **NIC side** ([`NicPort`]): the embassy task on core 0 that moves
//!   frames to and from the Ethernet chip.
//!
//! All state they share lives in [`Shared`]. Every shared word has exactly
//! one writer, so only atomic loads and stores (acquire / release) are
//! needed, no read-modify-write and no locks. The two frame queues are
//! single-producer / single-consumer rings with free-running `u32` indices:
//! RX is filled by the NIC side and drained by the bus side, TX the other
//! way round. Latched interrupt bits are event counters (written by the
//! side that causes the event) compared with acknowledge counters (written
//! by the bus side when the driver writes 1 to the bit).
//!
//! Every bus-side call is O(1): at most one word of a frame is touched per
//! bus cycle.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering::*};

/// Frames in the RX queue.
pub const RX_SLOTS: usize = 16;
/// Frames in the TX queue.
pub const TX_SLOTS: usize = 4;
/// Bytes per queue slot.
pub const SLOT_BYTES: usize = 1536;
/// Shortest frame accepted: an Ethernet header.
pub const FRAME_MIN: usize = 14;
/// Longest frame accepted, without FCS (the W5500 MACRAW limit).
pub const FRAME_MAX: usize = 1514;
/// TX frames shorter than this are padded with zeros (minimum frame
/// without FCS).
pub const TX_PAD_TO: usize = 60;
/// Multicast filter entries.
pub const MCAST_ENTRIES: usize = 4;

/// Register offsets (bytes from the board base).
pub mod reg {
    pub const INT: u8 = 0x06;
    pub const INT_ENABLE: u8 = 0x10;
    pub const CTRL: u8 = 0x12;
    pub const STATUS: u8 = 0x14;
    pub const MAC0: u8 = 0x16;
    pub const MAC1: u8 = 0x18;
    pub const MAC2: u8 = 0x1a;
    pub const TX_LEN: u8 = 0x20;
    pub const TX_DATA: u8 = 0x22;
    pub const TX_COMMIT: u8 = 0x24;
    pub const RX_LEN: u8 = 0x30;
    pub const RX_DATA: u8 = 0x32;
    pub const RX_DONE: u8 = 0x34;
    /// Six 32-bit counters, high word first, `$40..=$56`.
    pub const STATS: u8 = 0x40;
    pub const STATS_LAST: u8 = 0x56;
    /// Four entries of three words, `$60..=$76`.
    pub const MCAST: u8 = 0x60;
    pub const MCAST_LAST: u8 = 0x76;
    pub const MCAST_VALID: u8 = 0x78;
    /// Data port alias, `$C0..=$FE`: a word read anywhere here is an
    /// RX_DATA read, a word write a TX_DATA write, so the driver can move
    /// a frame with `move.l (a0)+` / `movem.l` (REGISTERS.md).
    pub const DATA_ALIAS: u8 = 0xc0;
    pub const DATA_ALIAS_LAST: u8 = 0xfe;
}

/// INT / INT_ENABLE bits.
pub mod int {
    /// Level: the RX queue is not empty.
    pub const RX_AVAIL: u16 = 1 << 0;
    pub const TX_DONE: u16 = 1 << 1;
    pub const LINK_CHANGE: u16 = 1 << 2;
    pub const RX_OVERRUN: u16 = 1 << 3;
    pub const BUS_ERROR: u16 = 1 << 4;
    pub const ALL: u16 = 0x1f;
}

/// CTRL bits.
pub mod ctrl {
    pub const ONLINE: u16 = 1 << 0;
    pub const PROMISC: u16 = 1 << 1;
    pub const MULTICAST_ALL: u16 = 1 << 2;
    pub const CLEAR_STATS: u16 = 1 << 14;
    pub const RESET_FIFOS: u16 = 1 << 15;
    /// The bits CTRL keeps (the others are actions and read 0).
    pub const STORED: u16 = ONLINE | PROMISC | MULTICAST_ALL;
}

/// STATUS bits (TX_FREE in 11-8 and RX_QUEUED in 7-0 are counts).
pub mod status {
    pub const LINK_UP: u16 = 1 << 15;
    pub const SPEED_100: u16 = 1 << 14;
    pub const FULL_DUPLEX: u16 = 1 << 13;
}

/// STATS counters, in register order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stat {
    RxOk = 0,
    TxOk = 1,
    RxDropped = 2,
    TxErrors = 3,
    RxCrc = 4,
    RxOverrun = 5,
}
const N_STATS: usize = 6;

/// What [`NicPort::push_rx`] did with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxResult {
    /// Queued for the Amiga (counted in rx_ok).
    Stored,
    /// Not for us under the current filter (not counted).
    Filtered,
    /// CTRL.ONLINE is clear (not counted).
    Offline,
    /// Shorter than [`FRAME_MIN`] or longer than [`FRAME_MAX`] (rx_dropped).
    BadLength,
    /// Queue full, frame dropped (rx_overrun, INT.RX_OVERRUN).
    Overrun,
}

/// A ring's storage: one cell per slot, so each side only ever borrows the
/// slot it owns at that moment (a borrow of the whole array would overlap
/// the other side's slot; Miri reports that as a data race).
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

/// Single-writer counter: load + store, no read-modify-write.
#[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
fn bump(a: &AtomicU32) {
    a.store(a.load(Relaxed).wrapping_add(1), Release);
}

/// The state both sides see. Put it in a `static` (e.g. a `StaticCell`) on
/// the target, then [`Shared::split`] it once.
pub struct Shared {
    mac: [u8; 6],

    rx: Slots<RX_SLOTS>,
    rx_w: AtomicU32, // NIC side
    rx_r: AtomicU32, // bus side

    tx: Slots<TX_SLOTS>,
    tx_w: AtomicU32,         // bus side
    tx_r: AtomicU32,         // NIC side
    tx_flush_to: AtomicU32,  // bus side: drop committed frames up to here
    tx_flush_seq: AtomicU32, // bus side: bumped after tx_flush_to

    // Written by the bus side.
    ctrl: AtomicU16,
    int_enable: AtomicU16,
    bus_error: AtomicU16, // 0 or int::BUS_ERROR
    ack_tx_done: AtomicU32,
    ack_link: AtomicU32,
    ack_overrun: AtomicU32,
    mcast: [AtomicU16; 3 * MCAST_ENTRIES],
    mcast_valid: AtomicU16,
    tx_errors_bus: AtomicU32,

    // Written by the NIC side.
    status: AtomicU16,
    ev_tx_done: AtomicU32,
    ev_link: AtomicU32,
    stats: [AtomicU32; N_STATS], // raw; RxOverrun doubles as its event count
}

// SAFETY: the slot cells are the only non-atomic shared data. A slot is
// written only by the producer of its ring while it is outside
// [read, write), and read only by the consumer while it is inside; the
// index stores (release) and loads (acquire) order the data accesses. The
// split into exactly one `BusPort` and one `NicPort` (behind `&mut self`)
// keeps one producer and one consumer per ring.
unsafe impl Sync for Shared {}

impl Shared {
    pub const fn new(mac: [u8; 6]) -> Self {
        Shared {
            mac,
            rx: Slots::new(),
            rx_w: AtomicU32::new(0),
            rx_r: AtomicU32::new(0),
            tx: Slots::new(),
            tx_w: AtomicU32::new(0),
            tx_r: AtomicU32::new(0),
            tx_flush_to: AtomicU32::new(0),
            tx_flush_seq: AtomicU32::new(0),
            ctrl: AtomicU16::new(0),
            int_enable: AtomicU16::new(0),
            bus_error: AtomicU16::new(0),
            ack_tx_done: AtomicU32::new(0),
            ack_link: AtomicU32::new(0),
            ack_overrun: AtomicU32::new(0),
            mcast: [const { AtomicU16::new(0) }; 3 * MCAST_ENTRIES],
            mcast_valid: AtomicU16::new(0),
            tx_errors_bus: AtomicU32::new(0),
            status: AtomicU16::new(0),
            ev_tx_done: AtomicU32::new(0),
            ev_link: AtomicU32::new(0),
            stats: [const { AtomicU32::new(0) }; N_STATS],
        }
    }

    /// The two halves. `&mut self` makes sure there is only one of each.
    pub fn split(&mut self) -> (BusPort<'_>, NicPort<'_>) {
        let s: &Shared = self;
        (BusPort::new(s), NicPort { s, flush_seen: s.tx_flush_seq.load(Acquire) })
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    /// Current INT value.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn int_status(&self) -> u16 {
        let mut v = self.bus_error.load(Acquire);
        if self.rx_w.load(Acquire) != self.rx_r.load(Acquire) {
            v |= int::RX_AVAIL;
        }
        if self.ev_tx_done.load(Acquire) != self.ack_tx_done.load(Acquire) {
            v |= int::TX_DONE;
        }
        if self.ev_link.load(Acquire) != self.ack_link.load(Acquire) {
            v |= int::LINK_CHANGE;
        }
        if self.stats[Stat::RxOverrun as usize].load(Acquire) != self.ack_overrun.load(Acquire) {
            v |= int::RX_OVERRUN;
        }
        v
    }

    /// (INT & INT_ENABLE) != 0. Callable from either side; whoever drives
    /// the /INT pin polls this (the board must also be configured, see
    /// `Window::irq_asserted`).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn irq_pending(&self) -> bool {
        self.int_status() & self.int_enable.load(Acquire) != 0
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn rx_queued(&self) -> u32 {
        self.rx_w.load(Acquire).wrapping_sub(self.rx_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn tx_used(&self) -> u32 {
        self.tx_w.load(Acquire).wrapping_sub(self.tx_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn raw_stat(&self, i: usize) -> u32 {
        let v = self.stats[i].load(Acquire);
        if i == Stat::TxErrors as usize {
            v.wrapping_add(self.tx_errors_bus.load(Acquire))
        } else {
            v
        }
    }
}

/// The Amiga's view: register reads and writes from the bus-cycle loop.
/// Where the next TX_DATA word goes: inside the TX slot opened by TX_LEN.
#[derive(Clone, Copy)]
struct TxCursor(*mut u8);
// SAFETY: only the bus side (one core) writes through it, into a slot the
// consumer does not touch until TX_COMMIT publishes it.
unsafe impl Send for TxCursor {}

pub struct BusPort<'a> {
    s: &'a Shared,
    tx_open: bool,
    tx_len: u16,
    /// TX_DATA words still expected in the open frame; 0 when none is open.
    tx_left: u16,
    tx_next: TxCursor,
    rx_pos: u16, // next word of the head frame
    stat_base: [u32; N_STATS],
    stat_snap: Option<(usize, u32)>,
}

impl<'a> BusPort<'a> {
    fn new(s: &'a Shared) -> Self {
        BusPort {
            s,
            tx_open: false,
            tx_len: 0,
            tx_left: 0,
            tx_next: TxCursor(core::ptr::null_mut()),
            rx_pos: 0,
            stat_base: [0; N_STATS],
            stat_snap: None,
        }
    }

    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    /// Offsets this port answers once the board is configured.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn is_nic_reg(reg: u8) -> bool {
        reg == reg::INT || (0x10..=0x7e).contains(&reg) || reg >= reg::DATA_ALIAS
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn set_bus_error(&self) {
        self.s.bus_error.store(int::BUS_ERROR, Release);
    }

    /// Amiga reset (/BUSRST) or Autoconfig shut-up: queues empty, offline,
    /// interrupts off and acknowledged, multicast table cleared. STATS stay.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        let s = self.s;
        self.reset_fifos();
        s.int_enable.store(0, Release);
        s.ctrl.store(0, Release);
        for m in &s.mcast {
            m.store(0, Release);
        }
        s.mcast_valid.store(0, Release);
        s.ack_tx_done.store(s.ev_tx_done.load(Acquire), Release);
        s.ack_link.store(s.ev_link.load(Acquire), Release);
        s.ack_overrun.store(s.stats[Stat::RxOverrun as usize].load(Acquire), Release);
        s.bus_error.store(0, Release);
        self.stat_snap = None;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn reset_fifos(&mut self) {
        let s = self.s;
        self.tx_open = false;
        self.tx_left = 0;
        // RX: we are the consumer, drop everything published so far.
        s.rx_r.store(s.rx_w.load(Acquire), Release);
        self.rx_pos = 0;
        // TX: we are the producer; ask the consumer to skip what is queued.
        s.tx_flush_to.store(s.tx_w.load(Relaxed), Release);
        s.tx_flush_seq.store(s.tx_flush_seq.load(Relaxed).wrapping_add(1), Release);
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn head_len(&self) -> u16 {
        let s = self.s;
        let r = s.rx_r.load(Relaxed);
        if s.rx_w.load(Acquire) == r {
            return 0;
        }
        // Published (r != w): the acquire above orders this load.
        s.rx.len(r) as u16
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn rx_word(&mut self, advance: bool) -> u16 {
        let len = self.head_len() as usize;
        let k = self.rx_pos as usize;
        if 2 * k >= len {
            return 0;
        }
        let r = self.s.rx_r.load(Relaxed);
        // SAFETY: slot r is published (head_len saw r != w with acquire)
        // and the producer does not touch it until rx_r moves past it.
        let d = unsafe { &*self.s.rx.slot(r) };
        let hi = u16::from(d[2 * k]) << 8;
        let lo = if 2 * k + 1 < len { u16::from(d[2 * k + 1]) } else { 0 };
        if advance {
            self.rx_pos += 1;
        }
        hi | lo
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn stat(&self, i: usize) -> u32 {
        self.s.raw_stat(i).wrapping_sub(self.stat_base[i])
    }

    /// A word write to TX_DATA: as `write(TX_DATA, data, true, true)`.
    /// Also the bus loop's fast path (`Window::write_fast`).
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn tx_data(&mut self, data: u16) {
        if self.tx_left == 0 {
            self.set_bus_error(); // no frame open, or more words than TX_LEN
            return;
        }
        // SAFETY: TX_LEN pointed the cursor into slot tx_w, which is outside
        // [tx_r, tx_w) (checked free there; tx_r only grows), so the consumer
        // does not touch it; tx_left bounds it to the frame's
        // ceil(len / 2) <= SLOT_BYTES / 2 words.
        unsafe {
            self.tx_next.0.cast::<[u8; 2]>().write(data.to_be_bytes());
            self.tx_next.0 = self.tx_next.0.add(2);
        }
        self.tx_left -= 1;
    }

    /// A word read of RX_DATA: as `read(RX_DATA, true, true)`. Also the bus
    /// loop's fast path (`Window::read_fast`).
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn rx_data(&mut self) -> u16 {
        self.rx_word(true)
    }

    /// Read cycle. `uds`: /UDS was asserted (low = word access; /LDS ignored).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, _lds: bool) -> u16 {
        let word = uds; // /LDS ignored: slave::Slave::write
        if !word {
            self.set_bus_error();
        }
        let s = self.s;
        match reg {
            reg::INT => s.int_status(),
            reg::INT_ENABLE => s.int_enable.load(Relaxed),
            reg::CTRL => s.ctrl.load(Relaxed),
            reg::STATUS => {
                let tx_free = TX_SLOTS as u32 - s.tx_used().min(TX_SLOTS as u32);
                s.status.load(Acquire) | (tx_free as u16) << 8 | s.rx_queued().min(0xff) as u16
            }
            reg::MAC0 => u16::from_be_bytes([s.mac[0], s.mac[1]]),
            reg::MAC1 => u16::from_be_bytes([s.mac[2], s.mac[3]]),
            reg::MAC2 => u16::from_be_bytes([s.mac[4], s.mac[5]]),
            reg::RX_LEN => self.head_len(),
            reg::RX_DATA | reg::DATA_ALIAS..=reg::DATA_ALIAS_LAST => self.rx_word(word),
            reg::STATS..=reg::STATS_LAST => {
                let off = (reg - reg::STATS) as usize;
                let i = off / 4;
                if off.is_multiple_of(4) {
                    let v = self.stat(i);
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
                        _ => self.stat(i),
                    };
                    v as u16
                }
            }
            reg::MCAST..=reg::MCAST_LAST => s.mcast[((reg - reg::MCAST) / 2) as usize].load(Relaxed),
            reg::MCAST_VALID => s.mcast_valid.load(Relaxed),
            _ => 0xffff,
        }
    }

    /// Write cycle. `uds`: /UDS was asserted (low = word access; /LDS ignored).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write(&mut self, reg: u8, data: u16, uds: bool, _lds: bool) {
        if !uds {
            self.set_bus_error();
            return;
        }
        let s = self.s;
        match reg {
            reg::INT => {
                if data & int::TX_DONE != 0 {
                    s.ack_tx_done.store(s.ev_tx_done.load(Acquire), Release);
                }
                if data & int::LINK_CHANGE != 0 {
                    s.ack_link.store(s.ev_link.load(Acquire), Release);
                }
                if data & int::RX_OVERRUN != 0 {
                    s.ack_overrun.store(s.stats[Stat::RxOverrun as usize].load(Acquire), Release);
                }
                if data & int::BUS_ERROR != 0 {
                    s.bus_error.store(0, Release);
                }
            }
            reg::INT_ENABLE => s.int_enable.store(data & int::ALL, Release),
            reg::CTRL => {
                if data & ctrl::RESET_FIFOS != 0 {
                    self.reset_fifos();
                }
                if data & ctrl::CLEAR_STATS != 0 {
                    for i in 0..N_STATS {
                        self.stat_base[i] = s.raw_stat(i);
                    }
                    self.stat_snap = None;
                }
                s.ctrl.store(data & ctrl::STORED, Release);
            }
            reg::TX_LEN => {
                if self.tx_open {
                    self.tx_open = false;
                    self.tx_left = 0;
                    bump(&s.tx_errors_bus);
                }
                let len = data as usize;
                if !(FRAME_MIN..=FRAME_MAX).contains(&len) || s.tx_used() >= TX_SLOTS as u32 {
                    self.set_bus_error();
                    return;
                }
                self.tx_open = true;
                self.tx_len = data;
                self.tx_left = data.div_ceil(2);
                self.tx_next = TxCursor(s.tx.slot(s.tx_w.load(Relaxed)).cast::<u8>());
            }
            reg::TX_DATA | reg::DATA_ALIAS..=reg::DATA_ALIAS_LAST => self.tx_data(data),
            reg::TX_COMMIT => {
                if !self.tx_open {
                    self.set_bus_error();
                    return;
                }
                self.tx_open = false;
                let short = self.tx_left != 0;
                self.tx_left = 0;
                if short || s.ctrl.load(Relaxed) & ctrl::ONLINE == 0 {
                    bump(&s.tx_errors_bus);
                    return;
                }
                let w = s.tx_w.load(Relaxed);
                s.tx.set_len(w, self.tx_len as usize);
                s.tx_w.store(w.wrapping_add(1), Release);
            }
            reg::RX_DONE => {
                let r = s.rx_r.load(Relaxed);
                if s.rx_w.load(Acquire) != r {
                    s.rx_r.store(r.wrapping_add(1), Release);
                }
                self.rx_pos = 0;
            }
            reg::MCAST..=reg::MCAST_LAST => s.mcast[((reg - reg::MCAST) / 2) as usize].store(data, Release),
            reg::MCAST_VALID => s.mcast_valid.store(data & ((1 << MCAST_ENTRIES) - 1), Release),
            _ => {}
        }
    }
}

/// The Ethernet chip's view: frames in and out, link state, counters.
pub struct NicPort<'a> {
    s: &'a Shared,
    flush_seen: u32,
}

impl<'a> NicPort<'a> {
    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    /// Would the frame with this destination address be delivered?
    pub fn accepts(&self, dst: &[u8; 6]) -> bool {
        let s = self.s;
        let c = s.ctrl.load(Acquire);
        if c & ctrl::PROMISC != 0 || *dst == s.mac || *dst == [0xff; 6] {
            return true;
        }
        if dst[0] & 1 == 0 {
            return false; // unicast, not ours
        }
        if c & ctrl::MULTICAST_ALL != 0 {
            return true;
        }
        let valid = s.mcast_valid.load(Acquire);
        (0..MCAST_ENTRIES).any(|e| {
            valid & (1 << e) != 0
                && (0..3).all(|k| s.mcast[3 * e + k].load(Relaxed).to_be_bytes() == [dst[2 * k], dst[2 * k + 1]])
        })
    }

    /// True when the chip's own MAC filter must be off (W5500 Sn_MR.MFEN = 0)
    /// because frames beyond unicast-to-us and broadcast are wanted.
    pub fn needs_all_frames(&self) -> bool {
        let s = self.s;
        s.ctrl.load(Acquire) & (ctrl::PROMISC | ctrl::MULTICAST_ALL) != 0 || s.mcast_valid.load(Acquire) != 0
    }

    /// Free RX slots.
    pub fn rx_free(&self) -> usize {
        RX_SLOTS - self.s.rx_queued().min(RX_SLOTS as u32) as usize
    }

    /// Offer a received frame (no FCS) to the Amiga.
    pub fn push_rx(&mut self, frame: &[u8]) -> RxResult {
        let s = self.s;
        if s.ctrl.load(Acquire) & ctrl::ONLINE == 0 {
            return RxResult::Offline;
        }
        if !(FRAME_MIN..=FRAME_MAX).contains(&frame.len()) {
            bump(&s.stats[Stat::RxDropped as usize]);
            return RxResult::BadLength;
        }
        let dst: &[u8; 6] = frame[..6].try_into().unwrap();
        if !self.accepts(dst) {
            return RxResult::Filtered;
        }
        let w = s.rx_w.load(Relaxed);
        if w.wrapping_sub(s.rx_r.load(Acquire)) >= RX_SLOTS as u32 {
            bump(&s.stats[Stat::RxOverrun as usize]);
            return RxResult::Overrun;
        }
        // SAFETY: slot w is outside [rx_r, rx_w): the consumer does not read it.
        unsafe { (&mut *s.rx.slot(w))[..frame.len()].copy_from_slice(frame) };
        s.rx.set_len(w, frame.len());
        s.rx_w.store(w.wrapping_add(1), Release);
        bump(&s.stats[Stat::RxOk as usize]);
        RxResult::Stored
    }

    fn apply_flush(&mut self) {
        let s = self.s;
        let seq = s.tx_flush_seq.load(Acquire);
        if seq == self.flush_seen {
            return;
        }
        self.flush_seen = seq;
        let to = s.tx_flush_to.load(Acquire);
        let r = s.tx_r.load(Relaxed);
        // Only move forward, and only within what was published.
        if to.wrapping_sub(r) <= TX_SLOTS as u32 {
            s.tx_r.store(to, Release);
        }
    }

    /// The next frame to send, padded to [`TX_PAD_TO`] bytes. Call
    /// [`NicPort::release_tx`] when the chip has taken it.
    pub fn peek_tx(&mut self) -> Option<&[u8]> {
        self.apply_flush();
        let s = self.s;
        let r = s.tx_r.load(Relaxed);
        if s.tx_w.load(Acquire) == r {
            return None;
        }
        let len = s.tx.len(r);
        let out = len.max(TX_PAD_TO);
        // SAFETY: slot r is inside [tx_r, tx_w): ours until tx_r moves,
        // which only `release_tx` does (it needs `&mut self`, so the
        // returned borrow has ended by then).
        let d = unsafe { &mut *s.tx.slot(r) };
        d[len..out].fill(0);
        Some(&d[..out])
    }

    /// Done with the frame from [`NicPort::peek_tx`]: `ok` = the chip took
    /// it (tx_ok), else tx_errors. Either way the slot is free (TX_DONE).
    pub fn release_tx(&mut self, ok: bool) {
        let s = self.s;
        let r = s.tx_r.load(Relaxed);
        if s.tx_w.load(Acquire) == r {
            return;
        }
        s.tx_r.store(r.wrapping_add(1), Release);
        bump(&s.stats[if ok { Stat::TxOk } else { Stat::TxErrors } as usize]);
        bump(&s.ev_tx_done);
    }

    /// Link state from the PHY; a change of `up` raises LINK_CHANGE.
    pub fn set_link(&mut self, up: bool, speed_100: bool, full_duplex: bool) {
        let s = self.s;
        let mut v = 0;
        if up {
            v |= status::LINK_UP;
        }
        if speed_100 {
            v |= status::SPEED_100;
        }
        if full_duplex {
            v |= status::FULL_DUPLEX;
        }
        let old = s.status.load(Relaxed); // we are the only writer
        s.status.store(v, Release);
        if (old ^ v) & status::LINK_UP != 0 {
            bump(&s.ev_link);
        }
    }

    /// Count a frame the chip reported with a bad CRC.
    pub fn count_crc_error(&mut self) {
        bump(&self.s.stats[Stat::RxCrc as usize]);
    }

    /// Count a frame lost on the NIC side (e.g. an SPI error).
    pub fn count_rx_dropped(&mut self) {
        bump(&self.s.stats[Stat::RxDropped as usize]);
    }
}

#[cfg(test)]
mod tests;
