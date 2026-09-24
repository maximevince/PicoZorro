//! PicoZorro addition: what the driver did and sent, for a firmware's report
//! line and for the case the chip loses its set-up ([`crate::Runner`] reads
//! Sn_SR on its 500 ms tick): frame counters, the register writes to the
//! common block (MR, the interrupt enables, the MAC: only the set-up writes
//! them), and the last SPI frame headers, frozen when the loss is seen. One
//! chip per firmware: the counters are global.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

/// Counters since start-up (the reader takes differences).
pub struct Stats {
    /// Frames read from the chip.
    pub rx_frames: AtomicU32,
    /// Frames written to the chip.
    pub tx_frames: AtomicU32,
    /// SPI frames that wrote the common block (W5500 block 0).
    pub common_writes: AtomicU32,
    /// Of those, the ones that wrote MR (address 0: bit 7 resets the chip).
    pub mode_writes: AtomicU32,
    /// Times the socket was found out of MACRAW mode.
    pub setup_lost: AtomicU32,
    /// The Sn_SR value seen the last time.
    pub lost_status: AtomicU32,
    /// Of the losses, the times the chip had been reset (its MAC register
    /// no longer the driver's).
    pub chip_reset: AtomicU32,
    /// Set-ups after a loss that failed on the SPI.
    pub recover_failed: AtomicU32,
    /// `common_writes` at the moment the last loss was seen.
    pub common_writes_at_loss: AtomicU32,
    /// `mode_writes` at the moment the last loss was seen.
    pub mode_writes_at_loss: AtomicU32,
}

/// The driver's counters.
pub static STATS: Stats = Stats {
    rx_frames: AtomicU32::new(0),
    tx_frames: AtomicU32::new(0),
    common_writes: AtomicU32::new(0),
    mode_writes: AtomicU32::new(0),
    setup_lost: AtomicU32::new(0),
    lost_status: AtomicU32::new(0),
    chip_reset: AtomicU32::new(0),
    recover_failed: AtomicU32::new(0),
    common_writes_at_loss: AtomicU32::new(0),
    mode_writes_at_loss: AtomicU32::new(0),
};

/// SPI frames kept: header and first data byte, length and time.
pub const TRACE_LEN: usize = 32;

/// One SPI frame: `head` = the three header bytes (address high, low,
/// control) and the first data byte sent, big-endian; `len` = data bytes;
/// `ms` = time since boot in units of 1024 timer ticks (about ms), low 16
/// bits.
#[derive(Clone, Copy, Default)]
pub struct Entry {
    /// Address high, low, control, first data byte.
    pub head: u32,
    /// Data bytes.
    pub len: u16,
    /// About ms since boot, low 16 bits.
    pub ms: u16,
}

// Written by the runner task only (one writer), read by the report. Kept
// as it was at a loss ([`freeze`]) until the report has read it
// ([`lost_trace`]), so the set-up writes that follow do not push the frames
// before the loss out.
static POS: AtomicU32 = AtomicU32::new(0);
static FROZEN: AtomicBool = AtomicBool::new(false);
static HEAD: [AtomicU32; TRACE_LEN] = [const { AtomicU32::new(0) }; TRACE_LEN];
static LEN_MS: [AtomicU32; TRACE_LEN] = [const { AtomicU32::new(0) }; TRACE_LEN];

/// An SPI frame on its way out (from the chip's `bus_read` / `bus_write`).
/// Out of line: those are instantiated per call site, all in the runner.
#[inline(never)]
pub(crate) fn note(header: [u8; 3], first: u8, len: usize) {
    if header[2] & 0b1111_1100 == 0b0000_0100 {
        STATS.common_writes.fetch_add(1, Relaxed);
        if header[0] == 0 && header[1] == 0 {
            STATS.mode_writes.fetch_add(1, Relaxed);
        }
    }
    if FROZEN.load(Relaxed) {
        return;
    }
    let i = POS.load(Relaxed);
    let k = i as usize % TRACE_LEN;
    let ms = embassy_time::Instant::now().as_ticks() >> 10;
    HEAD[k].store(u32::from_be_bytes([header[0], header[1], header[2], first]), Relaxed);
    LEN_MS[k].store((len.min(0xffff) as u32) << 16 | (ms as u32 & 0xffff), Relaxed);
    POS.store(i.wrapping_add(1), Relaxed);
}

/// The loss is seen (Sn_SR read back wrong): keep the last frames and the
/// write counters as they are now, before the set-up writes again.
pub(crate) fn freeze(status: u8) {
    let s = &STATS;
    s.setup_lost.fetch_add(1, Relaxed);
    s.lost_status.store(status as u32, Relaxed);
    s.common_writes_at_loss.store(s.common_writes.load(Relaxed), Relaxed);
    s.mode_writes_at_loss.store(s.mode_writes.load(Relaxed), Relaxed);
    FROZEN.store(true, Relaxed);
}

/// The frames frozen at the last loss, oldest first, into `out`, and the
/// trace running again; returns how many (0: none frozen).
pub fn lost_trace(out: &mut [Entry; TRACE_LEN]) -> usize {
    if !FROZEN.load(Relaxed) {
        return 0;
    }
    let p = POS.load(Relaxed) as usize;
    let n = p.min(TRACE_LEN);
    for (j, e) in out.iter_mut().take(n).enumerate() {
        let k = (p - n + j) % TRACE_LEN;
        let lm = LEN_MS[k].load(Relaxed);
        *e = Entry { head: HEAD[k].load(Relaxed), len: (lm >> 16) as u16, ms: lm as u16 };
    }
    FROZEN.store(false, Relaxed);
    n
}
