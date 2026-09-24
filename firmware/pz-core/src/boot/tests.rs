//! Host tests of the boot ROM and stream: Kickstart's side and the 68k
//! loader's (amiga/bootrom/boot.s) played in Rust against the window, the
//! boot task's through `Streamer`.

use super::*;
use crate::autoconfig::{AC_BASE_HI, AC_BASE_LO};
use crate::nic;
use crate::window::Window;
use std::collections::BTreeMap;

struct Hunk {
    mem: u32,
    memf: u32,
    data: Vec<u8>,
    relocs: Vec<(u32, u32)>, // (target hunk, offset)
}

struct Module {
    hunks: Vec<Hunk>,
    tag: (u16, u32),
}

/// What tools/mkboot.py writes.
fn image(rom: &[u8], mods: &[Module]) -> Vec<u8> {
    let mut v = b"PZB1".to_vec();
    v.extend((mods.len() as u16).to_be_bytes());
    v.extend((rom.len() as u16).to_be_bytes());
    v.extend(rom);
    for m in mods {
        v.extend((m.hunks.len() as u16).to_be_bytes());
        v.extend(m.tag.0.to_be_bytes());
        v.extend(m.tag.1.to_be_bytes());
        for h in &m.hunks {
            v.extend(h.mem.to_be_bytes());
            v.extend(h.memf.to_be_bytes());
            v.extend((h.data.len() as u32).to_be_bytes());
            v.extend(&h.data);
            v.extend((h.relocs.len() as u32).to_be_bytes());
            for (t, o) in &h.relocs {
                v.extend(t.to_be_bytes());
                v.extend(o.to_be_bytes());
            }
        }
    }
    v
}

fn rom() -> Vec<u8> {
    // DiagArea header shape (da_Config $90, da_Size 182) and filler.
    let mut r = vec![0x90, 0x00, 0x00, 182];
    r.extend((4..182u32).map(|i| (i * 7) as u8));
    r
}

fn modules() -> Vec<Module> {
    // Module 0: code with a pointer into its own data hunk and one to
    // itself; data pointing back at code. Module 1: one hunk, BSS larger
    // than its data.
    let code: Vec<u8> = (0..64u8).collect();
    let data: Vec<u8> = (0..32u8).map(|b| b ^ 0x5a).collect();
    vec![
        Module {
            hunks: vec![
                Hunk { mem: 80, memf: 0x10001, data: code, relocs: vec![(1, 8), (0, 20)] },
                Hunk { mem: 32, memf: 0x10003, data, relocs: vec![(0, 0)] },
            ],
            tag: (0, 4),
        },
        Module { hunks: vec![Hunk { mem: 400, memf: 0x10001, data: vec![0xaa; 12], relocs: vec![] }], tag: (0, 2) },
    ]
}

/// Board configured at $E90000 with the boot port.
fn board<'a>(nsh: &'a mut nic::Shared, b: BusPort<'a>) -> Window<'a> {
    let (nb, _) = nsh.split();
    let mut w = Window::new(nb).with_boot(b);
    w.write(AC_BASE_LO, 0x9000, true, false);
    assert_eq!(w.write(AC_BASE_HI, 0xe000, true, false), crate::slave::Event::Configured);
    w
}

fn os_read_byte(w: &mut Window, n: u8) -> u8 {
    let hi = (w.read(4 * n, true, true) >> 12) as u8;
    let lo = (w.read(4 * n + 2, true, true) >> 12) as u8;
    let v = hi << 4 | lo;
    if n == 0 {
        v
    } else {
        !v
    }
}

#[test]
fn autoconfig_offers_the_rom_only_with_one() {
    for with_rom in [true, false] {
        let mut nsh = Box::new(nic::Shared::new([0; 6]));
        let (nb, _) = nsh.split();
        let mut sh = Box::new(Shared::new());
        let (b, mut h) = sh.split();
        if with_rom {
            h.set_rom(&rom());
        }
        let mut w = Window::new(nb).with_boot(b);
        let er_type = os_read_byte(&mut w, 0);
        let vec = u16::from(os_read_byte(&mut w, 10)) << 8 | u16::from(os_read_byte(&mut w, 11));
        if with_rom {
            assert_eq!((er_type, vec), (0xd2, 0x0100));
        } else {
            assert_eq!((er_type, vec), (0xc2, 0));
        }
    }
}

#[test]
fn rom_mode_serves_the_rom_until_copied() {
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    let r = rom();
    h.set_rom(&r);
    let mut w = board(&mut nsh, b);
    let word = |i: usize| u16::from(r[i]) << 8 | u16::from(r[i + 1]);
    // Kickstart: da_Config (a byte: the word, one lane), the header, then
    // the whole area. The start is read several times; only the last word
    // ends ROM mode.
    assert_eq!(w.read(0, true, false), word(0));
    for i in (0..14).step_by(2) {
        assert_eq!(w.read(i as u8, true, true), word(i));
    }
    for i in (0..r.len()).step_by(2) {
        assert!(w.boot.as_ref().unwrap().rom_mode(), "ROM mode before word {i}");
        assert_eq!(w.read(i as u8, true, true), word(i));
    }
    assert!(!w.boot.as_ref().unwrap().rom_mode());
    assert_eq!(w.read(0x00, true, true), crate::slave::MAGIC, "the registers again");
    // Reset + configure: ROM mode again; a write ends it at once.
    w.reset();
    w.write(AC_BASE_LO, 0x9000, true, false);
    w.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w.read(0x02, true, true), word(2));
    w.write(0x04, 0x1234, true, true);
    assert_eq!(w.read(0x00, true, true), crate::slave::MAGIC);
    assert_eq!(w.read(0x04, true, true), 0x1234, "the write reached SCRATCH");
}

#[test]
fn no_rom_no_rom_mode() {
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let mut sh = Box::new(Shared::new());
    let (b, _h) = sh.split();
    let mut w = board(&mut nsh, b);
    assert_eq!(w.read(0x00, true, true), crate::slave::MAGIC);
    assert_eq!(w.read(reg::STAT, true, true), stat::BUSY, "no stream without a START");
}

/// The 68k loader, in Rust, driving the window; `pump` is core 0's boot task.
struct Loader {
    mem: BTreeMap<u32, Vec<u8>>, // allocations: base -> bytes
    next: u32,
    tags: Vec<u32>,
}

impl Loader {
    fn new() -> Self {
        Loader { mem: BTreeMap::new(), next: 0x0020_0000, tags: vec![] }
    }

    fn wait(w: &mut Window, pump: &mut dyn FnMut()) {
        for _ in 0..10 {
            if w.read(reg::STAT, true, true) & stat::BUSY == 0 {
                return;
            }
            pump();
        }
        panic!("BOOT_STAT stays busy");
    }

    fn word(w: &mut Window) -> u16 {
        w.read(reg::DATA, true, true)
    }

    /// move.l from the data port: $88 then $8A.
    fn long(w: &mut Window) -> u32 {
        u32::from(w.read(reg::DATA, true, true)) << 16 | u32::from(w.read(reg::DATA2, true, true))
    }

    fn run(&mut self, w: &mut Window, pump: &mut dyn FnMut()) {
        w.write(reg::CTRL, ctrl::START, true, true);
        Self::wait(w, pump);
        assert_eq!(Self::word(w), STREAM_MAGIC);
        loop {
            let n = Self::word(w) as usize;
            if n == 0 {
                break;
            }
            for _ in 0..n {
                let bytes = Self::long(w);
                let memf = Self::long(w);
                assert_ne!(memf & 0x10000, 0, "MEMF_CLEAR");
                let base = self.next;
                self.next += bytes.next_multiple_of(8) + 0x100;
                self.mem.insert(base, vec![0; bytes as usize]);
                w.write(reg::ADDR_HI, (base >> 16) as u16, true, true);
                w.write(reg::ADDR_LO, base as u16, true, true);
            }
            Self::wait(w, pump);
            for _ in 0..n {
                let base = Self::long(w);
                let words = Self::long(w) as usize;
                let m = self.mem.get_mut(&base).expect("a hunk the loader allocated");
                for k in 0..words {
                    let v = Self::word(w);
                    m[2 * k..2 * k + 2].copy_from_slice(&v.to_be_bytes());
                }
            }
            self.tags.push(Self::long(w));
        }
    }

    fn long_at(&self, addr: u32) -> u32 {
        let (base, m) = self.mem.range(..=addr).next_back().unwrap();
        let o = (addr - base) as usize;
        u32::from_be_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]])
    }
}

#[test]
fn stream_loads_and_relocates_every_module() {
    let mods = modules();
    let img_bytes = image(&rom(), &mods);
    let img = Image::new(&img_bytes).unwrap();
    assert_eq!(img.modules(), 2);
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    h.set_rom(img.rom().unwrap());
    let mut w = board(&mut nsh, b);
    // Kickstart's copy ends ROM mode.
    for i in (0..ROM_BYTES).step_by(2) {
        w.read(i as u8, true, true);
    }
    let h = std::cell::RefCell::new(h);
    let st: std::cell::RefCell<Option<Streamer>> = std::cell::RefCell::new(None);
    let mut pump = || {
        let mut h = h.borrow_mut();
        if let Some(s) = h.take_start() {
            *st.borrow_mut() = Some(Streamer::start(&mut h, &img, s).unwrap());
        }
        if let Some(s) = st.borrow_mut().as_mut() {
            s.step(&mut h, &img).unwrap();
        }
    };
    let mut l = Loader::new();
    l.run(&mut w, &mut pump);
    assert!(st.borrow().as_ref().unwrap().done());

    // Where each hunk went, in BOOT_ADDR order.
    let bases: Vec<u32> = l.mem.keys().copied().collect();
    assert_eq!(bases.len(), 3);
    let (c0, d0, c1) = (bases[0], bases[1], bases[2]);
    // Module 0, code: offset 8 -> data hunk + original value; offset 20 ->
    // own hunk; everything else as in the image.
    let orig = |o: usize| u32::from_be_bytes([o as u8, o as u8 + 1, o as u8 + 2, o as u8 + 3]);
    assert_eq!(l.long_at(c0 + 8), d0.wrapping_add(orig(8)));
    assert_eq!(l.long_at(c0 + 20), c0.wrapping_add(orig(20)));
    assert_eq!(l.long_at(c0 + 12), orig(12));
    assert_eq!(l.mem[&c0][64..], [0; 16], "BSS part stays clear");
    // Module 0, data: offset 0 -> code hunk.
    let d = &mods[0].hunks[1].data;
    assert_eq!(l.long_at(d0), c0.wrapping_add(u32::from_be_bytes([d[0], d[1], d[2], d[3]])));
    // Module 1.
    assert_eq!(&l.mem[&c1][..12], &[0xaa; 12]);
    assert_eq!(l.tags, vec![c0 + 4, c1 + 2]);
}

#[test]
fn a_new_start_begins_again() {
    let img_bytes = image(&rom(), &modules());
    let img = Image::new(&img_bytes).unwrap();
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    h.set_rom(img.rom().unwrap());
    let mut w = board(&mut nsh, b);
    w.write(reg::CTRL, ctrl::START, true, true); // also ends ROM mode
    let s1 = h.take_start().unwrap();
    let mut st = Streamer::start(&mut h, &img, s1).unwrap();
    assert_eq!(w.read(reg::STAT, true, true), 0);
    assert_eq!(w.read(reg::DATA, true, true), STREAM_MAGIC);
    w.write(reg::ADDR_HI, 0x20, true, true);
    w.write(reg::ADDR_LO, 0, true, true);
    // The loader restarts (a second boot): the old session's addresses and
    // stream do not count.
    w.write(reg::CTRL, ctrl::START, true, true);
    assert_eq!(w.read(reg::STAT, true, true), stat::BUSY, "old stream no longer valid");
    assert!(!st.step(&mut h, &img).unwrap());
    let s2 = h.take_start().unwrap();
    assert_ne!(s1, s2);
    assert_eq!(h.addrs_written(s2), 0);
    st = Streamer::start(&mut h, &img, s2).unwrap();
    assert_eq!(w.read(reg::DATA, true, true), STREAM_MAGIC);
    assert_eq!(w.read(reg::DATA, true, true), 2, "module 0 has two hunks");
    assert!(!st.done());
}

#[test]
fn bad_images_are_refused() {
    assert_eq!(Image::new(b"PZB0\0\0\0\0").err(), Some(ImageError::Magic));
    assert_eq!(Image::new(b"PZB1\0\x01\x01\0").err(), Some(ImageError::Truncated));
    let mut sh = Box::new(Shared::new());
    let (_b, mut h) = sh.split();
    // A relocation outside its hunk.
    let bad = image(
        &[],
        &[Module { hunks: vec![Hunk { mem: 8, memf: 0x10001, data: vec![0; 8], relocs: vec![(0, 6)] }], tag: (0, 0) }],
    );
    let img = Image::new(&bad).unwrap();
    let mut st = Streamer::start(&mut h, &img, 0).unwrap();
    h.s.addrs[0].store(0x1000, Relaxed);
    h.s.addr_count.store(1, Relaxed);
    assert_eq!(st.step(&mut h, &img).err(), Some(ImageError::BadReloc));
    // More than the stream holds.
    let big = image(
        &[],
        &[Module {
            hunks: vec![Hunk { mem: 60_000, memf: 0x10001, data: vec![0; 50_000], relocs: vec![] }],
            tag: (0, 0),
        }],
    );
    let img = Image::new(&big).unwrap();
    let mut st = Streamer::start(&mut h, &img, 0).unwrap();
    assert_eq!(st.step(&mut h, &img).err(), Some(ImageError::StreamFull));
}

#[test]
fn switched_off_from_the_next_reset() {
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    h.set_rom(&rom());
    let s: &Shared = b.shared();
    let mut w = board(&mut nsh, b);
    assert!(w.boot.as_ref().unwrap().rom_mode());
    // Off (BOOTROM_OFF): nothing changes until the Amiga resets.
    s.set_enabled(false);
    assert!(s.rom_present() && !s.has_rom());
    w.reset();
    assert_eq!(os_read_byte(&mut w, 0), 0xc2, "no DIAGVALID after the reset");
    w.write(AC_BASE_LO, 0x9000, true, false);
    w.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w.read(0x00, true, true), crate::slave::MAGIC, "a plain board: no ROM mode");
    // On again.
    s.set_enabled(true);
    w.reset();
    assert_eq!(os_read_byte(&mut w, 0), 0xd2);
}
