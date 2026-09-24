//! Firmware update registers (`docs/UPDATE.md`), pure logic.
//!
//! $C0-$FE of the A16 = 1 window. The Amiga streams a new firmware image
//! into the partition the module is not running from, then asks for a
//! "flash update" reboot into it and, once it runs, confirms it
//! (RP2350 try-before-you-buy, datasheet 5.1.17). This model is the
//! register half: a command mailbox, the image size and CRC, and a data
//! port that fills 4 KiB sector buffers, taken one at a time (see
//! `BusPort::space`). The firmware half
//! ([`HostPort`]) takes commands and sectors and reports state; the flash
//! work itself is in `pz-app`.
//!
//! Same scheme as `nic` / `usb`: every shared word has one writer, the
//! sector ring is SPSC with free-running indices.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering::*};

pub const SECTOR: usize = 4096;
const SLOTS: usize = 2;
/// Length of the version string at FW_VERSION.
pub const VERSION_LEN: usize = 16;

/// Register offsets within the A16 = 1 window.
pub mod reg {
    pub const CTRL: u8 = 0xc0;
    pub const STATUS: u8 = 0xc2;
    pub const SIZE_HI: u8 = 0xc4;
    pub const SIZE_LO: u8 = 0xc6;
    pub const CRC_HI: u8 = 0xc8;
    pub const CRC_LO: u8 = 0xca;
    pub const DATA: u8 = 0xcc;
    pub const DONE_HI: u8 = 0xd0;
    pub const DONE_LO: u8 = 0xd2;
    pub const RESULT_HI: u8 = 0xd4;
    pub const RESULT_LO: u8 = 0xd6;
    pub const SLOT_KB: u8 = 0xd8;
    pub const ERROR: u8 = 0xda;
    /// 16 ASCII bytes, eight words, NUL-padded.
    pub const VERSION: u8 = 0xe0;
    pub const VERSION_LAST: u8 = 0xee;
    pub const FIRST: u8 = 0xc0;
}

/// UPD_CTRL commands.
pub mod cmd {
    pub const BEGIN: u16 = 1;
    pub const ABORT: u16 = 2;
    pub const COMMIT: u16 = 3;
    pub const ACTIVATE: u16 = 4;
    pub const CONFIRM: u16 = 5;
    /// Boot ROM off / on: kept in flash, applies from the
    /// next Amiga reset.
    pub const BOOTROM_OFF: u16 = 6;
    pub const BOOTROM_ON: u16 = 7;
}

/// UPD_STATUS bits 3-0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum State {
    Idle = 0,
    Receiving = 1,
    Verifying = 2,
    /// Written and read back with the right CRC: ACTIVATE may follow.
    Ready = 3,
    Rebooting = 4,
    Error = 5,
}

/// UPD_STATUS flag bits.
pub mod status {
    /// The data port takes the next sector now (the last one is in flash).
    pub const SPACE: u16 = 1 << 4;
    /// A command is being carried out; wait for this to clear.
    pub const BUSY: u16 = 1 << 5;
    /// A register write was refused (data outside Receiving or past the
    /// size, no space, byte access, command while busy). Cleared by BEGIN.
    pub const REFUSED: u16 = 1 << 6;
    /// Running from partition B (else A).
    pub const SLOT_B: u16 = 1 << 8;
    /// Running on trial after an update: CONFIRM keeps this image.
    pub const BUY_PENDING: u16 = 1 << 9;
    /// A partition table with an A/B pair is present: updates possible.
    pub const PARTITIONED: u16 = 1 << 10;
    /// The boot ROM is on (the image has one and it is not switched off).
    pub const BOOTROM: u16 = 1 << 11;
}

/// UPD_ERROR values (valid in state Error).
pub mod error {
    pub const NONE: u16 = 0;
    pub const NOT_PARTITIONED: u16 = 1;
    pub const BAD_SIZE: u16 = 2;
    pub const FLASH: u16 = 3;
    pub const CRC: u16 = 4;
    /// Command not allowed in this state, or data outside Receiving.
    pub const SEQUENCE: u16 = 5;
    /// No IMAGE_DEF block in the first sector.
    pub const NOT_AN_IMAGE: u16 = 6;
    pub const BUY_FAILED: u16 = 7;
    pub const SHORT: u16 = 8;
}

struct Ring {
    len: [AtomicU16; SLOTS],
    data: [UnsafeCell<[u8; SECTOR]>; SLOTS],
}

pub struct Shared {
    ring: Ring,
    w: AtomicU32, // bus side
    r: AtomicU32, // host side

    // Written by the bus side.
    cmd: AtomicU16,
    cmd_seq: AtomicU32,
    size: AtomicU32,
    crc: AtomicU32,

    // Written by the host side.
    ack_seq: AtomicU32,
    status: AtomicU16, // state | flags (SLOT_B, BUY_PENDING, PARTITIONED)
    error: AtomicU16,
    done: AtomicU32,
    result: AtomicU32,
    slot_kb: AtomicU16,
    version: [AtomicU16; VERSION_LEN / 2],
}

// SAFETY: as `nic::Shared`: a sector slot is written by the bus side only
// while outside [r, w) and read by the host side only while inside.
unsafe impl Sync for Shared {}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub const fn new() -> Self {
        Shared {
            ring: Ring {
                len: [const { AtomicU16::new(0) }; SLOTS],
                data: [const { UnsafeCell::new([0; SECTOR]) }; SLOTS],
            },
            w: AtomicU32::new(0),
            r: AtomicU32::new(0),
            cmd: AtomicU16::new(0),
            cmd_seq: AtomicU32::new(0),
            size: AtomicU32::new(0),
            crc: AtomicU32::new(0),
            ack_seq: AtomicU32::new(0),
            status: AtomicU16::new(0),
            error: AtomicU16::new(0),
            done: AtomicU32::new(0),
            result: AtomicU32::new(0),
            slot_kb: AtomicU16::new(0),
            version: [const { AtomicU16::new(0) }; VERSION_LEN / 2],
        }
    }

    pub fn split(&mut self) -> (BusPort<'_>, HostPort<'_>) {
        let s: &Shared = self;
        (
            BusPort { s, fill: 0, received: 0, bus_error: false, snap: None },
            HostPort { s, seen_seq: 0 },
        )
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn busy(&self) -> bool {
        self.cmd_seq.load(Acquire) != self.ack_seq.load(Acquire)
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn state(&self) -> u16 {
        self.status.load(Acquire) & 0xf
    }
}

/// The Amiga's view.
pub struct BusPort<'a> {
    s: &'a Shared,
    /// Bytes in the sector being filled.
    fill: usize,
    /// Bytes taken since BEGIN.
    received: u32,
    /// A rule was broken; the owning window turns this into its BUS_ERROR.
    pub bus_error: bool,
    snap: Option<(u8, u32)>,
}

impl<'a> BusPort<'a> {
    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn is_update_reg(reg: u8) -> bool {
        reg >= reg::FIRST
    }

    /// Lock-step on purpose: the next sector only after the last one is in
    /// flash. While the firmware erases and programs, the core that serves
    /// a UART backplane has its interrupts off and would lose bytes; this
    /// way only idempotent STATUS polls can fall into that window. On the
    /// bus the cost is nil (programming is ~20 ms of ~4 KiB of bus cycles).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn space(&self) -> bool {
        let s = self.s;
        s.w.load(Relaxed) == s.r.load(Acquire)
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn accepting(&self) -> bool {
        !self.s.busy() && self.s.state() == State::Receiving as u16
    }

    /// /BUSRST or shut-up: an update in progress is abandoned (ABORT).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        let st = self.s.state();
        if st == State::Receiving as u16 || st == State::Verifying as u16 {
            self.command(cmd::ABORT);
        }
        self.snap = None;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn command(&mut self, c: u16) {
        let s = self.s;
        if c == cmd::BEGIN {
            self.bus_error = false;
        }
        if c == cmd::BEGIN || c == cmd::ABORT {
            self.fill = 0;
            self.received = 0;
        }
        if c == cmd::COMMIT && self.fill > 0 {
            // A last, partial sector: hand it over as it is.
            self.publish();
        }
        s.cmd.store(c, Relaxed);
        s.cmd_seq.store(s.cmd_seq.load(Relaxed).wrapping_add(1), Release);
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn publish(&mut self) {
        let s = self.s;
        let w = s.w.load(Relaxed);
        s.ring.len[w as usize % SLOTS].store(self.fill as u16, Relaxed);
        s.w.store(w.wrapping_add(1), Release);
        self.fill = 0;
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn word32(&mut self, reg: u8, hi_reg: u8, v: u32, word: bool) -> u16 {
        if reg == hi_reg {
            if word {
                self.snap = Some((hi_reg, v));
            }
            (v >> 16) as u16
        } else {
            match self.snap {
                Some((r, v)) if r == hi_reg => {
                    if word {
                        self.snap = None;
                    }
                    v as u16
                }
                _ => v as u16,
            }
        }
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, _lds: bool) -> u16 {
        let word = uds; // /LDS ignored: slave::Slave::write
        if !word {
            self.bus_error = true;
        }
        let s = self.s;
        match reg {
            reg::STATUS => {
                let mut v = s.status.load(Acquire);
                if self.bus_error {
                    v |= status::REFUSED;
                }
                if s.busy() {
                    v |= status::BUSY;
                } else if self.accepting() && self.space() {
                    v |= status::SPACE;
                }
                v
            }
            reg::SIZE_HI | reg::SIZE_LO => {
                let v = s.size.load(Relaxed);
                self.word32(reg, reg::SIZE_HI, v, word)
            }
            reg::CRC_HI | reg::CRC_LO => {
                let v = s.crc.load(Relaxed);
                self.word32(reg, reg::CRC_HI, v, word)
            }
            reg::DONE_HI | reg::DONE_LO => {
                let v = s.done.load(Acquire);
                self.word32(reg, reg::DONE_HI, v, word)
            }
            reg::RESULT_HI | reg::RESULT_LO => {
                let v = s.result.load(Acquire);
                self.word32(reg, reg::RESULT_HI, v, word)
            }
            reg::SLOT_KB => s.slot_kb.load(Acquire),
            reg::ERROR => s.error.load(Acquire),
            reg::VERSION..=reg::VERSION_LAST => s.version[((reg - reg::VERSION) / 2) as usize].load(Acquire),
            _ => 0xffff,
        }
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write(&mut self, reg: u8, data: u16, uds: bool, _lds: bool) {
        if !uds {
            self.bus_error = true;
            return;
        }
        let s = self.s;
        match reg {
            reg::CTRL => {
                if s.busy() || !(cmd::BEGIN..=cmd::BOOTROM_ON).contains(&data) {
                    self.bus_error = true;
                    return;
                }
                self.command(data);
            }
            reg::SIZE_HI => s.size.store((s.size.load(Relaxed) & 0xffff) | (data as u32) << 16, Release),
            reg::SIZE_LO => s.size.store((s.size.load(Relaxed) & 0xffff_0000) | data as u32, Release),
            reg::CRC_HI => s.crc.store((s.crc.load(Relaxed) & 0xffff) | (data as u32) << 16, Release),
            reg::CRC_LO => s.crc.store((s.crc.load(Relaxed) & 0xffff_0000) | data as u32, Release),
            reg::DATA => {
                let size = s.size.load(Relaxed);
                if !self.accepting() || self.received >= size || (self.fill == 0 && !self.space()) {
                    self.bus_error = true;
                    return;
                }
                let w = s.w.load(Relaxed);
                // SAFETY: slot w is outside [r, w): checked free when its
                // first word came (space), r only grows.
                let d = unsafe { &mut *s.ring.data[w as usize % SLOTS].get() };
                d[self.fill] = (data >> 8) as u8;
                let two = size - self.received >= 2;
                if two {
                    d[self.fill + 1] = data as u8;
                }
                let n = if two { 2 } else { 1 };
                self.fill += n;
                self.received += n as u32;
                if self.fill == SECTOR || self.received == size {
                    self.publish();
                }
            }
            _ => {}
        }
    }
}

/// A command from the Amiga, for the firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    pub cmd: u16,
    /// UPD_SIZE and UPD_CRC as they were when the command came.
    pub size: u32,
    pub crc: u32,
}

/// The firmware's view.
pub struct HostPort<'a> {
    s: &'a Shared,
    seen_seq: u32,
}

impl<'a> HostPort<'a> {
    /// What the module is: which slot it runs from, whether that image is
    /// on trial, whether A/B partitions exist, how big a slot is, and the
    /// running firmware's version string.
    pub fn set_identity(&mut self, slot_b: bool, buy_pending: bool, partitioned: bool, slot_kb: u16, version: &str) {
        let s = self.s;
        let mut f = 0;
        if slot_b {
            f |= status::SLOT_B;
        }
        if buy_pending {
            f |= status::BUY_PENDING;
        }
        if partitioned {
            f |= status::PARTITIONED;
        }
        s.status.store(f | (s.status.load(Relaxed) & 0xf), Release);
        s.slot_kb.store(slot_kb, Release);
        let b = version.as_bytes();
        for (k, v) in s.version.iter().enumerate() {
            let hi = *b.get(2 * k).unwrap_or(&0);
            let lo = *b.get(2 * k + 1).unwrap_or(&0);
            v.store(u16::from_be_bytes([hi, lo]), Release);
        }
    }

    /// The next command, if one is waiting. Answer it with [`HostPort::ack`]
    /// once its state change is visible (the Amiga sees BUSY until then).
    pub fn take_command(&mut self) -> Option<Command> {
        let s = self.s;
        let seq = s.cmd_seq.load(Acquire);
        if seq == self.seen_seq {
            return None;
        }
        self.seen_seq = seq;
        Some(Command { cmd: s.cmd.load(Relaxed), size: s.size.load(Acquire), crc: s.crc.load(Acquire) })
    }

    /// The command waiting, without taking it: lets a loop that takes
    /// sectors first decide whether the command may cut in.
    pub fn pending_command(&self) -> Option<u16> {
        let s = self.s;
        if s.cmd_seq.load(Acquire) == self.seen_seq {
            return None;
        }
        Some(s.cmd.load(Relaxed))
    }

    /// Done with the last command: clears BUSY. BEGIN and ABORT drop
    /// sectors still queued from before.
    pub fn ack(&mut self, c: &Command) {
        let s = self.s;
        if c.cmd == cmd::BEGIN || c.cmd == cmd::ABORT {
            s.r.store(s.w.load(Acquire), Release);
        }
        s.ack_seq.store(self.seen_seq, Release);
    }

    pub fn set_state(&mut self, st: State, err: u16) {
        let s = self.s;
        s.error.store(err, Release);
        s.status.store((s.status.load(Relaxed) & !0xf) | st as u16, Release);
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn state(&self) -> u16 {
        self.s.state()
    }

    pub fn set_bootrom(&mut self, on: bool) {
        let s = self.s;
        let v = s.status.load(Relaxed);
        s.status.store(if on { v | status::BOOTROM } else { v & !status::BOOTROM }, Release);
    }

    pub fn set_buy_pending(&mut self, pending: bool) {
        let s = self.s;
        let v = s.status.load(Relaxed);
        s.status.store(if pending { v | status::BUY_PENDING } else { v & !status::BUY_PENDING }, Release);
    }

    pub fn set_progress(&mut self, done: u32) {
        self.s.done.store(done, Release);
    }

    pub fn set_result(&mut self, crc: u32) {
        self.s.result.store(crc, Release);
    }

    /// The next full (or last, partial) sector: its bytes. Call
    /// [`HostPort::release_sector`] when it is in flash.
    pub fn peek_sector(&mut self) -> Option<&[u8]> {
        let s = self.s;
        let r = s.r.load(Relaxed);
        if s.w.load(Acquire) == r {
            return None;
        }
        let len = s.ring.len[r as usize % SLOTS].load(Relaxed) as usize;
        // SAFETY: slot r is inside [r, w): ours until r moves, which needs
        // `&mut self` (release_sector), so the borrow has ended by then.
        Some(unsafe { &(&*s.ring.data[r as usize % SLOTS].get())[..len] })
    }

    pub fn release_sector(&mut self) {
        let s = self.s;
        let r = s.r.load(Relaxed);
        if s.w.load(Acquire) != r {
            s.r.store(r.wrapping_add(1), Release);
        }
    }
}

/// CRC-32 (IEEE 802.3, as zlib's `crc32`), incremental: start with 0.
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// Image type flags of the first IMAGE_DEF block in `sector` (the start of
/// an image; datasheet 5.9, picobin.h: block marker $FFFFDED3, item
/// IMAGE_TYPE $42 with its flags in the upper half word). None if there is
/// no such block.
pub fn image_type(sector: &[u8]) -> Option<u16> {
    const START: u32 = 0xffff_ded3;
    let words = sector.len() / 4;
    let word = |i: usize| u32::from_le_bytes([sector[4 * i], sector[4 * i + 1], sector[4 * i + 2], sector[4 * i + 3]]);
    // The bootrom searches the first 4 KiB, word aligned.
    for i in 0..words.saturating_sub(1) {
        if word(i) != START {
            continue;
        }
        let item = word(i + 1);
        if item & 0xff == 0x42 && (item >> 8) & 0xff == 1 {
            return Some((item >> 16) as u16);
        }
    }
    None
}

/// Try-before-you-buy flag in the image type (picobin.h
/// PICOBIN_IMAGE_TYPE_EXE_TBYB_BITS).
pub const IMAGE_TYPE_TBYB: u16 = 0x8000;

#[cfg(test)]
mod tests;
