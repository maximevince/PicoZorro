//! MPEG audio registers (`docs/REGISTERS-MPEG.md`), pure logic.
//!
//! $60-$9E of the A16 = 1 window. The Amiga's `mpega.library` streams the
//! compressed bytes in as chunks and reads back one record per decoded
//! frame (a 16-byte header and planar PCM). The firmware's MPEG task takes
//! the chunks, decodes with minimp3 and posts the records, shaped by
//! [`shape`] (rate division, mono, gain) so less crosses the bus.
//!
//! Same scheme as `usb`: [`Shared`] holds everything both sides see, every
//! shared word has one writer, both queues are single-producer /
//! single-consumer rings with free-running `u32` indices. START bumps an
//! epoch: chunks committed before it are skipped, records of the old stream
//! are dropped when posted. The bus side ([`BusPort`]) is O(1) per register
//! access; the host side ([`HostPort`]) is the MPEG task.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering::*};

/// Input chunks the Amiga can queue.
pub const IN_SLOTS: usize = 8;
/// Largest input chunk.
pub const IN_BYTES: usize = 2048;
/// Records the firmware can queue.
pub const OUT_SLOTS: usize = 6;
/// Record header.
pub const HDR: usize = 16;
/// Samples per channel in the largest frame.
pub const MAX_FRAME: usize = 1152;
/// Largest record: header + 1152 samples x 2 channels x 2 bytes.
pub const OUT_BYTES: usize = HDR + MAX_FRAME * 2 * 2;
pub const MAGIC: u16 = 0x4d50; // "MP"
pub const VERSION: u16 = 0x0001;

/// Register offsets within the A16 = 1 window.
pub mod reg {
    pub const FIRST: u8 = 0x60;
    pub const MAGIC: u8 = 0x60;
    pub const VERSION: u8 = 0x62;
    pub const CTRL: u8 = 0x64;
    pub const STATUS: u8 = 0x66;
    pub const CONFIG: u8 = 0x68;
    pub const SCALE: u8 = 0x6a;
    /// Low 16 bits of the session number START / STOP / reset last opened.
    pub const SESSION: u8 = 0x6c;
    pub const IN_LEN: u8 = 0x70;
    pub const IN_DATA: u8 = 0x72;
    pub const IN_COMMIT: u8 = 0x74;
    pub const OUT_LEN: u8 = 0x80;
    pub const OUT_DATA: u8 = 0x82;
    /// OUT_DATA again, so a `move.l` reads two words.
    pub const OUT_DATA2: u8 = 0x84;
    pub const OUT_DONE: u8 = 0x86;
    /// Three 32-bit counters, high word first, `$90..=$9a`.
    pub const STATS: u8 = 0x90;
    pub const STATS_LAST: u8 = 0x9a;
    pub const LAST: u8 = 0x9e;
}

/// CTRL commands.
pub mod cmd {
    pub const START: u16 = 1;
    pub const STOP: u16 = 2;
}

/// CONFIG bits.
pub mod config {
    /// 0: /1, 1: /2, 2: /4.
    pub const FREQ_DIV: u16 = 0x3;
    pub const MONO: u16 = 1 << 4;
    pub const ALL: u16 = FREQ_DIV | MONO;
}

/// Record FLAGS (header word 0).
pub mod flags {
    /// No frame: the stream's input is used up.
    pub const END: u16 = 1 << 15;
    /// Bytes were skipped to find this frame.
    pub const LOST: u16 = 1 << 14;
}

/// STATS counters, in register order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Stat {
    /// Records with samples.
    Frames = 0,
    /// Records with 0 samples.
    Skipped = 1,
    /// Input chunks dropped: bad length, queue full, short commit.
    InErrors = 2,
}

/// What START hands the firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// 1, 2 or 4.
    pub freq_div: u8,
    pub mono: bool,
    /// Percent, 1..=800.
    pub scale: u16,
}

impl Config {
    fn from_regs(config: u16, scale: u16) -> Self {
        Config {
            freq_div: match config & config::FREQ_DIV {
                1 => 2,
                2 => 4,
                _ => 1,
            },
            mono: config & config::MONO != 0,
            scale: if (1..=800).contains(&scale) { scale } else { 100 },
        }
    }
}

struct Slots<const N: usize, const B: usize> {
    len: [AtomicU16; N],
    data: [UnsafeCell<[u8; B]>; N],
}

impl<const N: usize, const B: usize> Slots<N, B> {
    const fn new() -> Self {
        Slots { len: [const { AtomicU16::new(0) }; N], data: [const { UnsafeCell::new([0; B]) }; N] }
    }
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn slot(&self, i: u32) -> *mut [u8; B] {
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
    inq: Slots<IN_SLOTS, IN_BYTES>,
    in_w: AtomicU32, // bus side
    in_r: AtomicU32, // host side

    out: Slots<OUT_SLOTS, OUT_BYTES>,
    out_w: AtomicU32, // host side
    out_r: AtomicU32, // bus side

    // Written by the bus side.
    config: AtomicU16,
    scale: AtomicU16,
    /// The session START or STOP opened (1 running, 0 stopped), stored
    /// before `epoch`.
    running: AtomicU16,
    session_config: AtomicU16,
    session_scale: AtomicU16,
    /// Bumped by START, STOP, /BUSRST and shut-up.
    epoch: AtomicU32,
    in_flush_to: AtomicU32,
    in_errors: AtomicU32,

    // Written by the host side.
    frames: AtomicU32,
    skipped: AtomicU32,
}

// SAFETY: as `usb::Shared`: a slot is written only by its ring's producer
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
            inq: Slots::new(),
            in_w: AtomicU32::new(0),
            in_r: AtomicU32::new(0),
            out: Slots::new(),
            out_w: AtomicU32::new(0),
            out_r: AtomicU32::new(0),
            config: AtomicU16::new(0),
            scale: AtomicU16::new(100),
            running: AtomicU16::new(0),
            session_config: AtomicU16::new(0),
            session_scale: AtomicU16::new(100),
            epoch: AtomicU32::new(0),
            in_flush_to: AtomicU32::new(0),
            in_errors: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            skipped: AtomicU32::new(0),
        }
    }

    pub fn split(&mut self) -> (BusPort<'_>, HostPort<'_>) {
        let s: &Shared = self;
        let e = s.epoch.load(Acquire);
        (BusPort::new(s), HostPort { s, epoch_seen: e, flush_seen: e })
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn in_used(&self) -> u32 {
        self.in_w.load(Acquire).wrapping_sub(self.in_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn out_queued(&self) -> u32 {
        self.out_w.load(Acquire).wrapping_sub(self.out_r.load(Acquire))
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn stat(&self, i: usize) -> u32 {
        match i {
            0 => self.frames.load(Acquire),
            1 => self.skipped.load(Acquire),
            _ => self.in_errors.load(Acquire),
        }
    }
}

/// The Amiga's view.
pub struct BusPort<'a> {
    s: &'a Shared,
    in_open: bool,
    in_len: u16,
    in_words: u16,
    out_pos: u16,
    stat_snap: Option<(usize, u32)>,
}

impl<'a> BusPort<'a> {
    fn new(s: &'a Shared) -> Self {
        BusPort { s, in_open: false, in_len: 0, in_words: 0, out_pos: 0, stat_snap: None }
    }

    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    /// $60-$9E of the A16 = 1 window.
    #[inline(always)]
    pub fn is_mpeg_reg(reg: u8) -> bool {
        (reg::FIRST..=reg::LAST).contains(&reg)
    }

    /// /BUSRST or Autoconfig shut-up: as STOP. STATS stay.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        self.new_session(false);
        self.stat_snap = None;
    }

    /// Empty both queues and open a session: the host sees the new epoch,
    /// skips the chunks before it and drops records it still posts for the
    /// old one.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn new_session(&mut self, running: bool) {
        let s = self.s;
        self.in_open = false;
        s.session_config.store(s.config.load(Relaxed), Relaxed);
        s.session_scale.store(s.scale.load(Relaxed), Relaxed);
        s.running.store(running as u16, Relaxed);
        // flush_to and the session before the epoch: a host that sees the
        // new epoch sees both.
        s.in_flush_to.store(s.in_w.load(Relaxed), Release);
        s.epoch.store(s.epoch.load(Relaxed).wrapping_add(1), Release);
        s.out_r.store(s.out_w.load(Acquire), Release);
        self.out_pos = 0;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn head_len(&self) -> u16 {
        let s = self.s;
        let r = s.out_r.load(Relaxed);
        if s.out_w.load(Acquire) == r {
            return 0;
        }
        s.out.len(r) as u16
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn out_word(&mut self, advance: bool) -> u16 {
        let len = self.head_len() as usize;
        let k = self.out_pos as usize;
        if 2 * k >= len {
            return 0;
        }
        let r = self.s.out_r.load(Relaxed);
        // SAFETY: slot r is published and stays ours until out_r moves.
        let d = unsafe { &*self.s.out.slot(r) };
        let hi = u16::from(d[2 * k]) << 8;
        let lo = if 2 * k + 1 < len { u16::from(d[2 * k + 1]) } else { 0 };
        if advance {
            self.out_pos += 1;
        }
        hi | lo
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, _lds: bool) -> u16 {
        let word = uds; // /LDS ignored: slave::Slave::write
        let s = self.s;
        match reg {
            reg::MAGIC => MAGIC,
            reg::VERSION => VERSION,
            reg::CTRL => 0,
            reg::STATUS => {
                let free = IN_SLOTS as u32 - s.in_used().min(IN_SLOTS as u32);
                (free as u16) << 8 | s.out_queued().min(0xff) as u16
            }
            reg::CONFIG => s.config.load(Relaxed),
            reg::SCALE => s.scale.load(Relaxed),
            reg::SESSION => s.epoch.load(Acquire) as u16,
            reg::OUT_LEN => self.head_len(),
            reg::OUT_DATA | reg::OUT_DATA2 => self.out_word(word),
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
            return;
        }
        let s = self.s;
        match reg {
            reg::CTRL => match data {
                cmd::START => self.new_session(true),
                cmd::STOP => self.new_session(false),
                _ => {}
            },
            reg::CONFIG => s.config.store(data & config::ALL, Relaxed),
            reg::SCALE => s.scale.store(data, Relaxed),
            reg::IN_LEN => {
                if self.in_open {
                    self.in_open = false;
                    bump(&s.in_errors);
                }
                if data as usize > IN_BYTES || s.in_used() >= IN_SLOTS as u32 {
                    bump(&s.in_errors);
                    return;
                }
                self.in_open = true;
                self.in_len = data;
                self.in_words = 0;
            }
            reg::IN_DATA => {
                if !self.in_open || self.in_words >= self.in_len.div_ceil(2) {
                    return;
                }
                let w = s.in_w.load(Relaxed);
                let k = 2 * self.in_words as usize;
                // SAFETY: slot w is outside [in_r, in_w) (free at IN_LEN).
                let d = unsafe { &mut *s.inq.slot(w) };
                d[k] = (data >> 8) as u8;
                d[k + 1] = data as u8;
                self.in_words += 1;
            }
            reg::IN_COMMIT => {
                if !self.in_open {
                    return;
                }
                self.in_open = false;
                if self.in_words < self.in_len.div_ceil(2) {
                    bump(&s.in_errors);
                    return;
                }
                let w = s.in_w.load(Relaxed);
                s.inq.set_len(w, self.in_len as usize);
                s.in_w.store(w.wrapping_add(1), Release);
            }
            reg::OUT_DONE => {
                let r = s.out_r.load(Relaxed);
                if s.out_w.load(Acquire) != r {
                    s.out_r.store(r.wrapping_add(1), Release);
                }
                self.out_pos = 0;
            }
            _ => {}
        }
    }
}

/// A session as START or STOP opened it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    /// START (true) or STOP / reset (false).
    pub running: bool,
    pub config: Config,
    /// Goes back with every record of the session.
    pub epoch: u32,
}

/// The MPEG task's view.
pub struct HostPort<'a> {
    s: &'a Shared,
    epoch_seen: u32, // take_session
    flush_seen: u32, // take_chunk
}

impl<'a> HostPort<'a> {
    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    /// Once after each START / STOP / reset: the session to run now (drop
    /// the decoder state and anything in flight).
    pub fn take_session(&mut self) -> Option<Session> {
        let s = self.s;
        let e = s.epoch.load(Acquire);
        if e == self.epoch_seen {
            return None;
        }
        self.epoch_seen = e;
        Some(Session {
            running: s.running.load(Relaxed) != 0,
            config: Config::from_regs(s.session_config.load(Relaxed), s.session_scale.load(Relaxed)),
            epoch: e,
        })
    }

    /// Copy the next input chunk into `out`; its length (0: the end
    /// marker). Chunks committed before the last START / STOP are skipped.
    pub fn take_chunk(&mut self, out: &mut [u8; IN_BYTES]) -> Option<usize> {
        let s = self.s;
        let epoch = s.epoch.load(Acquire);
        if epoch != self.flush_seen {
            self.flush_seen = epoch;
            let to = s.in_flush_to.load(Acquire);
            let r = s.in_r.load(Relaxed);
            if to != r && to.wrapping_sub(r) <= s.in_w.load(Acquire).wrapping_sub(r) {
                s.in_r.store(to, Release);
            }
        }
        let r = s.in_r.load(Relaxed);
        if s.in_w.load(Acquire) == r {
            return None;
        }
        let len = s.inq.len(r);
        // SAFETY: slot r is inside [in_r, in_w): ours until in_r moves.
        out[..len].copy_from_slice(unsafe { &(&*s.inq.slot(r))[..len] });
        s.in_r.store(r.wrapping_add(1), Release);
        Some(len)
    }

    /// SCALE as the Amiga last wrote it (it applies from the next frame,
    /// as MPEGA_scale does): percent, 1..=800, else 100.
    pub fn scale(&self) -> u16 {
        let v = self.s.scale.load(Relaxed);
        if (1..=800).contains(&v) {
            v
        } else {
            100
        }
    }

    /// Room for one more record.
    pub fn out_free(&self) -> bool {
        (self.s.out_queued() as usize) < OUT_SLOTS
    }

    /// Post a record built by [`record`]. Ok(false): the queue is full, try
    /// again; Ok(true): posted, or dropped because its session is over.
    /// Err: shorter than a header or longer than a slot.
    /// The record's SESSION word is set here.
    pub fn post(&mut self, epoch: u32, rec: &mut [u8]) -> Result<bool, ()> {
        let s = self.s;
        if rec.len() < HDR || rec.len() > OUT_BYTES {
            return Err(());
        }
        rec[12..14].copy_from_slice(&(epoch as u16).to_be_bytes());
        if s.epoch.load(Acquire) != epoch {
            return Ok(true);
        }
        let w = s.out_w.load(Relaxed);
        if w.wrapping_sub(s.out_r.load(Acquire)) >= OUT_SLOTS as u32 {
            return Ok(false);
        }
        // SAFETY: slot w is outside [out_r, out_w).
        unsafe { (&mut *s.out.slot(w))[..rec.len()].copy_from_slice(rec) };
        s.out.set_len(w, rec.len());
        s.out_w.store(w.wrapping_add(1), Release);
        let samples = u16::from_be_bytes([rec[2], rec[3]]);
        if u16::from_be_bytes([rec[0], rec[1]]) & flags::END == 0 {
            bump(if samples != 0 { &s.frames } else { &s.skipped });
        }
        Ok(true)
    }
}

/// What one decoded frame looked like, for the record header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub flags: u16,
    /// Compressed bytes the frame used.
    pub frame_bytes: u16,
    /// The frame's 4-byte MPEG header.
    pub header: [u8; 4],
}

/// Build a record in `out`: the header, then `pcm` (interleaved, `samples`
/// per channel, `channels` 1 or 2) shaped by `cfg`. Returns its length.
pub fn record(out: &mut [u8; OUT_BYTES], f: &Frame, pcm: &[i16], samples: usize, channels: usize, cfg: &Config) -> usize {
    let (n, ch) = shape(pcm, samples, channels, cfg, &mut out[HDR..]);
    let words = [f.flags, n as u16, ch as u16, f.frame_bytes];
    for (i, w) in words.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&w.to_be_bytes());
    }
    out[8..12].copy_from_slice(&f.header);
    out[12..16].fill(0);
    HDR + 2 * n * ch
}

/// The END record: no frame, the stream's input is used up.
pub fn end_record(out: &mut [u8; OUT_BYTES]) -> usize {
    out[..HDR].fill(0);
    out[..2].copy_from_slice(&flags::END.to_be_bytes());
    HDR
}

/// Output shaping (REGISTERS-MPEG.md): mono (L + R) / 2 when asked and
/// stereo, then the mean of `freq_div` samples, then the gain, saturated.
/// Writes planar big-endian samples to `out`; returns (samples per
/// channel, channels).
pub fn shape(pcm: &[i16], samples: usize, channels: usize, cfg: &Config, out: &mut [u8]) -> (usize, usize) {
    let div = cfg.freq_div.max(1) as usize;
    let mix = cfg.mono && channels == 2;
    let out_ch = if mix { 1 } else { channels };
    let n = samples / div;
    // Sum over div samples (and both channels when mixing): divide once.
    let den = (div * if mix { 2 } else { 1 }) as i32;
    let scale = cfg.scale as i32;
    for c in 0..out_ch {
        for k in 0..n {
            let mut acc = 0i32;
            for j in 0..div {
                let i = (k * div + j) * channels;
                acc += if mix { pcm[i] as i32 + pcm[i + 1] as i32 } else { pcm[i + c] as i32 };
            }
            let mut v = acc / den;
            if scale != 100 {
                v = v * scale / 100;
            }
            let v = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            let o = 2 * (c * n + k);
            out[o..o + 2].copy_from_slice(&v.to_be_bytes());
        }
    }
    (n, out_ch)
}

#[cfg(test)]
mod tests;
