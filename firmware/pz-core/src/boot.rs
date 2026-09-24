//! Boot ROM and boot stream, pure logic.
//!
//! With a boot image, the Autoconfig ROM says DIAGVALID with er_InitDiagVec
//! $0100, and after configuration the board is in **ROM mode**: reads of
//! the A16 = 0 window return the boot ROM (a DiagArea, a romtag and a small
//! 68k loader) by address, whole words, so either byte lane is right.
//! Kickstart copies it (three reads of the start, then the whole area in
//! ascending order); ROM mode ends with the read of its last word, with any
//! write, and at /BUSRST (it comes back at the next configuration).
//!
//! The loader then reads the **boot stream** from BOOT_DATA: the modules
//! (picozorro.device, picozorrousb.device, mpega.library), relocated by the
//! firmware to the memory the loader allocated and reported in BOOT_ADDR.
//! The stream, in words:
//!
//! ```text
//! $5042 ("PB")
//! per module: hunks n, then n x (bytes.l, MEMF flags.l)
//!             -- the loader AllocMems each and writes the address --
//!             n x (address.l, words.l, the hunk's words), romtag address.l
//! 0 (no more modules)
//! ```
//!
//! BOOT_STAT bit 15 (BUSY) is set while nothing is published at the read
//! position: after BOOT_CTRL and after the addresses, while core 0 builds
//! the next part. The image the firmware carries is built on the PC by
//! `tools/mkboot.py` from the hunk files ([`Image`] reads it).
//!
//! Same scheme as the other models: [`Shared`] holds what both sides see,
//! every shared word has one writer; the bus side ([`BusPort`]) is O(1) per
//! access, the host side ([`HostPort`] with [`Streamer`]) is a core-0 task.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering::*};

/// Largest boot ROM: the A16 = 0 window.
pub const ROM_BYTES: usize = 256;
/// The stream buffer: every module's words plus headers.
pub const STREAM_BYTES: usize = 48 * 1024;
/// Hunks of all modules together (one BOOT_ADDR each).
pub const MAX_ADDRS: usize = 32;
/// "PB": the stream is there.
pub const STREAM_MAGIC: u16 = 0x5042;

/// Register offsets in the A16 = 0 window (free space after the network
/// registers).
pub mod reg {
    pub const FIRST: u8 = 0x80;
    /// W: 1 = start: ROM mode off, the stream from the beginning.
    pub const CTRL: u8 = 0x80;
    /// W: the address of the hunk just allocated, high word then low word.
    pub const ADDR_HI: u8 = 0x82;
    pub const ADDR_LO: u8 = 0x84;
    /// R: bit 15 BUSY.
    pub const STAT: u8 = 0x86;
    /// R: the stream; both offsets, so `move.l` reads two words.
    pub const DATA: u8 = 0x88;
    pub const DATA2: u8 = 0x8a;
    pub const LAST: u8 = 0x8a;
}

pub mod ctrl {
    pub const START: u16 = 1;
}

pub mod stat {
    /// Nothing published at the read position yet.
    pub const BUSY: u16 = 1 << 15;
}

pub struct Shared {
    rom: UnsafeCell<[u8; ROM_BYTES]>,
    /// Bytes of boot ROM, 0 = no boot ROM. Set by the host before the bus
    /// side runs.
    rom_len: AtomicU16,
    /// Bus side: serving the ROM.
    rom_mode: AtomicBool,
    /// The boot ROM is not switched off (flash setting, update task).
    enabled: AtomicBool,
    stream: UnsafeCell<[u8; STREAM_BYTES]>,
    /// Host side: (session & 0xffff) << 16 | bytes published.
    published: AtomicU32,
    /// Bus side: bumped by BOOT_CTRL START.
    session: AtomicU32,
    addrs: [AtomicU32; MAX_ADDRS],
    /// Bus side: (session & 0xffff) << 16 | addresses written.
    addr_count: AtomicU32,
}

// SAFETY: `rom` is written only by `HostPort::set_rom` before the bus side
// reads it (the caller's contract, see there). `stream` bytes below the
// published length are written only before the length is stored (release)
// and read only by the bus side after it loaded that length (acquire);
// the host writes only at or past the published length, or after a new
// session (whose bytes the bus side does not trust until published again).
unsafe impl Sync for Shared {}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub const fn new() -> Self {
        Shared {
            rom: UnsafeCell::new([0; ROM_BYTES]),
            rom_len: AtomicU16::new(0),
            rom_mode: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
            stream: UnsafeCell::new([0; STREAM_BYTES]),
            published: AtomicU32::new(0),
            session: AtomicU32::new(0),
            addrs: [const { AtomicU32::new(0) }; MAX_ADDRS],
            addr_count: AtomicU32::new(0),
        }
    }

    pub fn split(&mut self) -> (BusPort<'_>, HostPort<'_>) {
        let s: &Shared = self;
        (BusPort { s, pos: 0, addr_hi: 0, addr_n: 0 }, HostPort { s, session_seen: s.session.load(Acquire) })
    }

    /// The boot ROM is offered (the Autoconfig ROM says DIAGVALID): the
    /// image has one and it is not switched off.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn has_rom(&self) -> bool {
        self.rom_len.load(Acquire) != 0 && self.enabled.load(Acquire)
    }

    /// The image has a boot ROM (on or off).
    pub fn rom_present(&self) -> bool {
        self.rom_len.load(Acquire) != 0
    }

    /// Boot ROM on / off (the flash setting); the Autoconfig ROM follows
    /// from the next /BUSRST. One writer: the update task (or main before
    /// the bus side runs).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Release);
    }
}

/// The Amiga's view.
pub struct BusPort<'a> {
    s: &'a Shared,
    pos: u32,
    addr_hi: u16,
    addr_n: u16,
}

impl<'a> BusPort<'a> {
    pub fn shared(&self) -> &'a Shared {
        self.s
    }

    #[inline(always)]
    pub fn is_boot_reg(reg: u8) -> bool {
        (reg::FIRST..=reg::LAST).contains(&reg)
    }

    /// Serving the ROM instead of the A16 = 0 registers.
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn rom_mode(&self) -> bool {
        self.s.rom_mode.load(Relaxed)
    }

    /// The board was just configured: ROM mode, if there is a ROM.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn configured(&mut self) {
        self.s.rom_mode.store(self.s.has_rom(), Relaxed);
    }

    /// Any write to the board ends ROM mode.
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn any_write(&mut self) {
        self.s.rom_mode.store(false, Relaxed);
    }

    /// /BUSRST or shut-up.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        self.s.rom_mode.store(false, Relaxed);
    }

    /// A read in ROM mode: the ROM word at `reg` (the window aliases every
    /// 256 bytes, so `reg` is the offset in the ROM). The read of the last
    /// word ends ROM mode: Kickstart's copy is done.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn rom_read(&mut self, reg: u8) -> u16 {
        let s = self.s;
        let len = s.rom_len.load(Relaxed) as usize;
        let i = reg as usize;
        // SAFETY: written before the bus side runs (set_rom's contract).
        let rom = unsafe { &*s.rom.get() };
        let v = u16::from(rom[i]) << 8 | u16::from(rom[i + 1]);
        if i + 2 >= len {
            s.rom_mode.store(false, Relaxed);
        }
        v
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn session16(&self) -> u32 {
        self.s.session.load(Relaxed) & 0xffff
    }

    /// Bytes of this session's stream that are published.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn available(&self) -> u32 {
        let p = self.s.published.load(Acquire);
        if p >> 16 == self.session16() {
            p & 0xffff
        } else {
            0
        }
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, _lds: bool) -> u16 {
        match reg {
            reg::STAT => {
                if self.pos + 2 <= self.available() {
                    0
                } else {
                    stat::BUSY
                }
            }
            reg::DATA | reg::DATA2 => {
                if self.pos + 2 > self.available() {
                    return 0;
                }
                let i = self.pos as usize;
                // SAFETY: below the published length of this session.
                let d = unsafe { &*self.s.stream.get() };
                let v = u16::from(d[i]) << 8 | u16::from(d[i + 1]);
                if uds {
                    self.pos += 2;
                }
                v
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
            reg::CTRL => {
                if data == ctrl::START {
                    self.pos = 0;
                    self.addr_n = 0;
                    let n = s.session.load(Relaxed).wrapping_add(1);
                    s.addr_count.store((n & 0xffff) << 16, Relaxed);
                    s.session.store(n, Release);
                }
            }
            reg::ADDR_HI => self.addr_hi = data,
            reg::ADDR_LO => {
                let i = self.addr_n as usize;
                if i < MAX_ADDRS {
                    s.addrs[i].store(u32::from(self.addr_hi) << 16 | u32::from(data), Relaxed);
                    self.addr_n += 1;
                    s.addr_count.store(self.session16() << 16 | u32::from(self.addr_n), Release);
                }
            }
            _ => {}
        }
    }
}

/// The boot task's view.
pub struct HostPort<'a> {
    s: &'a Shared,
    session_seen: u32,
}

impl<'a> HostPort<'a> {
    /// Install the boot ROM (DiagArea + loader, at most 256 bytes, an even
    /// length). Call before the bus side runs: from then on the ROM is read
    /// without synchronisation. None (or empty): no boot ROM.
    pub fn set_rom(&mut self, rom: &[u8]) {
        let n = rom.len().min(ROM_BYTES) & !1;
        // SAFETY: the bus side does not read the ROM yet (the contract).
        unsafe { (&mut *self.s.rom.get())[..n].copy_from_slice(&rom[..n]) };
        self.s.rom_len.store(n as u16, Release);
    }

    /// Once after each BOOT_CTRL START: its session number.
    pub fn take_start(&mut self) -> Option<u32> {
        let n = self.s.session.load(Acquire);
        if n == self.session_seen {
            return None;
        }
        self.session_seen = n;
        Some(n)
    }

    /// A START came in after `session`: drop what is being built for it.
    pub fn stale(&self, session: u32) -> bool {
        self.s.session.load(Acquire) != session
    }

    /// Addresses the loader has written in `session`.
    pub fn addrs_written(&self, session: u32) -> usize {
        let c = self.s.addr_count.load(Acquire);
        if c >> 16 == session & 0xffff {
            (c & 0xffff) as usize
        } else {
            0
        }
    }

    pub fn addr(&self, i: usize) -> u32 {
        self.s.addrs[i].load(Relaxed)
    }

    /// Write stream bytes at `at` (at or past what is published).
    pub fn write_stream(&mut self, at: usize, bytes: &[u8]) -> bool {
        if at + bytes.len() > STREAM_BYTES {
            return false;
        }
        // SAFETY: at or past the published length: the bus side does not
        // read there until `publish` (see Shared).
        unsafe { (&mut *self.s.stream.get())[at..at + bytes.len()].copy_from_slice(bytes) };
        true
    }

    /// The bus side may read the first `len` bytes of `session`'s stream.
    pub fn publish(&mut self, session: u32, len: usize) {
        self.s.published.store((session & 0xffff) << 16 | len as u32, Release);
    }

    /// Patch a big-endian long already written (relocation).
    fn add_long(&mut self, at: usize, v: u32) {
        // SAFETY: as write_stream; `at` is past the published length.
        let d = unsafe { &mut *self.s.stream.get() };
        let old = u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]);
        d[at..at + 4].copy_from_slice(&old.wrapping_add(v).to_be_bytes());
    }
}

/// A boot image (`tools/mkboot.py`), big-endian:
///
/// ```text
/// "PZB1", modules.w, rom_len.w, rom (rom_len bytes, even)
/// per module: hunks.w, romtag_hunk.w, romtag_offset.l,
///   per hunk: mem_bytes.l, memf.l, data_bytes.l (even), data,
///             relocs.l, relocs x (target_hunk.l, offset.l)
/// ```
#[derive(Clone, Copy)]
pub struct Image<'a> {
    d: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    Magic,
    Truncated,
    TooManyHunks,
    BadReloc,
    StreamFull,
}

fn be16(d: &[u8], at: usize) -> Result<u16, ImageError> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or(ImageError::Truncated)
}

fn be32(d: &[u8], at: usize) -> Result<u32, ImageError> {
    d.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])).ok_or(ImageError::Truncated)
}

impl<'a> Image<'a> {
    pub fn new(d: &'a [u8]) -> Result<Self, ImageError> {
        if d.get(..4) != Some(b"PZB1") {
            return Err(ImageError::Magic);
        }
        let img = Image { d };
        img.rom()?;
        Ok(img)
    }

    pub fn modules(&self) -> usize {
        be16(self.d, 4).unwrap_or(0) as usize
    }

    pub fn rom(&self) -> Result<&'a [u8], ImageError> {
        let n = be16(self.d, 6)? as usize;
        self.d.get(8..8 + n).ok_or(ImageError::Truncated)
    }

    /// Offset of module `m`.
    fn module_at(&self, m: usize) -> Result<usize, ImageError> {
        let mut at = 8 + self.rom()?.len();
        for _ in 0..m {
            at = self.skip_module(at)?;
        }
        Ok(at)
    }

    fn skip_module(&self, at: usize) -> Result<usize, ImageError> {
        let n = be16(self.d, at)? as usize;
        let mut p = at + 8;
        for _ in 0..n {
            let data = be32(self.d, p + 8)? as usize;
            p += 12 + data;
            let relocs = be32(self.d, p)? as usize;
            p += 4 + 8 * relocs;
        }
        if p > self.d.len() {
            return Err(ImageError::Truncated);
        }
        Ok(p)
    }
}

/// What the boot task does between BOOT_CTRL and the end of the stream.
pub struct Streamer {
    session: u32,
    module: usize,
    /// Index of the module's first hunk in BOOT_ADDR order.
    first_addr: usize,
    hunks: usize,
    /// Bytes built so far.
    at: usize,
    state: Step,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Waiting for the module's addresses.
    Addresses,
    Done,
}

impl Streamer {
    /// A new session: "PB" and the first module's header.
    pub fn start(host: &mut HostPort, img: &Image, session: u32) -> Result<Self, ImageError> {
        let mut st = Streamer { session, module: 0, first_addr: 0, hunks: 0, at: 0, state: Step::Addresses };
        st.put(host, &STREAM_MAGIC.to_be_bytes())?;
        st.header(host, img)?;
        host.publish(session, st.at);
        Ok(st)
    }

    pub fn done(&self) -> bool {
        self.state == Step::Done
    }

    fn put(&mut self, host: &mut HostPort, b: &[u8]) -> Result<(), ImageError> {
        if !host.write_stream(self.at, b) {
            return Err(ImageError::StreamFull);
        }
        self.at += b.len();
        Ok(())
    }

    /// The next module's header, or the end.
    fn header(&mut self, host: &mut HostPort, img: &Image) -> Result<(), ImageError> {
        if self.module >= img.modules() {
            self.put(host, &0u16.to_be_bytes())?;
            self.state = Step::Done;
            return Ok(());
        }
        let d = img.d;
        let at = img.module_at(self.module)?;
        let n = be16(d, at)? as usize;
        if self.first_addr + n > MAX_ADDRS {
            return Err(ImageError::TooManyHunks);
        }
        self.hunks = n;
        self.put(host, &(n as u16).to_be_bytes())?;
        let mut p = at + 8;
        for _ in 0..n {
            self.put(host, &d[p..p + 8])?; // mem_bytes, memf
            let data = be32(d, p + 8)? as usize;
            p += 12 + data;
            p += 4 + 8 * be32(d, p)? as usize;
        }
        self.state = Step::Addresses;
        Ok(())
    }

    /// Call when the task has time. True when it published something.
    pub fn step(&mut self, host: &mut HostPort, img: &Image) -> Result<bool, ImageError> {
        if self.state != Step::Addresses || host.addrs_written(self.session) < self.first_addr + self.hunks {
            return Ok(false);
        }
        let d = img.d;
        let at = img.module_at(self.module)?;
        let n = self.hunks;
        let base = |i: usize| host.addr(self.first_addr + i);
        let mut bases = [0u32; MAX_ADDRS];
        for (i, b) in bases.iter_mut().enumerate().take(n) {
            *b = base(i);
        }
        let mut p = at + 8;
        for i in 0..n {
            let data = be32(d, p + 8)? as usize;
            self.put(host, &bases[i].to_be_bytes())?;
            self.put(host, &((data / 2) as u32).to_be_bytes())?;
            let start = self.at;
            self.put(host, d.get(p + 12..p + 12 + data).ok_or(ImageError::Truncated)?)?;
            p += 12 + data;
            let relocs = be32(d, p)? as usize;
            for r in 0..relocs {
                let target = be32(d, p + 4 + 8 * r)? as usize;
                let off = be32(d, p + 8 + 8 * r)? as usize;
                if target >= n || off + 4 > data {
                    return Err(ImageError::BadReloc);
                }
                host.add_long(start + off, bases[target]);
            }
            p += 4 + 8 * relocs;
        }
        let tag_hunk = be16(d, at + 2)? as usize;
        let tag_off = be32(d, at + 4)?;
        if tag_hunk >= n {
            return Err(ImageError::BadReloc);
        }
        self.put(host, &bases[tag_hunk].wrapping_add(tag_off).to_be_bytes())?;
        self.first_addr += n;
        self.module += 1;
        self.header(host, img)?;
        if host.stale(self.session) {
            return Ok(false); // a new START: the task drops this Streamer
        }
        host.publish(self.session, self.at);
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
