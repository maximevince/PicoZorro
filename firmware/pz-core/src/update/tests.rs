//! Host tests of the update registers: the Amiga's side through register
//! accesses, the firmware's side through `HostPort` with a fake flash.

use super::*;

fn rd(b: &mut BusPort, r: u8) -> u16 {
    b.read(r, true, true)
}
fn wr(b: &mut BusPort, r: u8, v: u16) {
    b.write(r, v, true, true)
}
fn wr32(b: &mut BusPort, hi: u8, v: u32) {
    wr(b, hi, (v >> 16) as u16);
    wr(b, hi + 2, v as u16);
}
fn rd32(b: &mut BusPort, hi: u8) -> u32 {
    let h = rd(b, hi) as u32;
    h << 16 | rd(b, hi + 2) as u32
}
fn st(b: &mut BusPort) -> u16 {
    rd(b, reg::STATUS)
}

fn image(len: usize) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len).map(|i| (i * 7 + i / 251) as u8).collect();
    // An IMAGE_DEF at 0x114 as embassy-rp places it: marker, IMAGE_TYPE
    // item (secure Arm exe for RP2350 = $1021) with TBYB.
    if len >= 0x11c {
        v[0x114..0x118].copy_from_slice(&0xffff_ded3u32.to_le_bytes());
        v[0x118..0x11c].copy_from_slice(&(0x42 | 1 << 8 | ((0x1021u32 | 0x8000) << 16)).to_le_bytes());
    }
    v
}

/// The firmware loop in miniature: carry out commands, "program" sectors
/// into `flash` (sector 0 held back until COMMIT, as pz-app does).
struct Fw {
    flash: Vec<u8>,
    at: usize,
    crc: u32,
    first: Option<Vec<u8>>,
}

impl Fw {
    fn new() -> Self {
        Fw { flash: Vec::new(), at: 0, crc: 0, first: None }
    }

    /// As `pz-app` update_task: sectors before commands, except that a
    /// pending ABORT or BEGIN cuts in.
    fn step(&mut self, h: &mut HostPort) {
        loop {
            if matches!(h.pending_command(), Some(cmd::ABORT | cmd::BEGIN)) {
                break;
            }
            let Some(s) = h.peek_sector() else { break };
            let s = s.to_vec();
            self.crc = crc32(self.crc, &s);
            if self.at == 0 {
                self.first = Some(s.clone());
                self.flash.extend(std::iter::repeat_n(0xff, s.len()));
            } else {
                self.flash.extend(&s);
            }
            self.at += s.len();
            h.release_sector();
            h.set_progress(self.at as u32);
        }
        let Some(c) = h.take_command() else { return };
        match c.cmd {
            cmd::BEGIN => {
                self.flash.clear();
                self.at = 0;
                self.crc = 0;
                self.first = None;
                if c.size == 0 || c.size > 2 << 20 {
                    h.set_state(State::Error, error::BAD_SIZE);
                } else {
                    h.set_state(State::Receiving, 0);
                }
            }
            cmd::ABORT => h.set_state(State::Idle, 0),
            cmd::COMMIT => {
                if self.at as u32 != c.size {
                    h.set_state(State::Error, error::SHORT);
                } else if self.crc != c.crc {
                    h.set_state(State::Error, error::CRC);
                } else if self.first.as_deref().and_then(image_type).is_none() {
                    h.set_state(State::Error, error::NOT_AN_IMAGE);
                } else {
                    let f = self.first.take().unwrap();
                    self.flash[..f.len()].copy_from_slice(&f);
                    h.set_result(crc32(0, &self.flash));
                    h.set_state(State::Ready, 0);
                }
            }
            _ => h.set_state(State::Error, error::SEQUENCE),
        }
        h.ack(&c);
    }
}

/// What pzflash does, step by step, with the firmware run in between.
fn flash_image(b: &mut BusPort, h: &mut HostPort, fw: &mut Fw, img: &[u8]) -> u16 {
    wr32(b, reg::SIZE_HI, img.len() as u32);
    wr32(b, reg::CRC_HI, crc32(0, img));
    wr(b, reg::CTRL, cmd::BEGIN);
    assert_ne!(st(b) & status::BUSY, 0);
    fw.step(h);
    assert_eq!(st(b) & 0xf, State::Receiving as u16);
    for sector in img.chunks(SECTOR) {
        let mut spins = 0;
        while st(b) & status::SPACE == 0 {
            fw.step(h);
            spins += 1;
            assert!(spins < 10, "no space");
        }
        for pair in sector.chunks(2) {
            wr(b, reg::DATA, u16::from(pair[0]) << 8 | u16::from(*pair.get(1).unwrap_or(&0)));
        }
    }
    wr(b, reg::CTRL, cmd::COMMIT);
    fw.step(h);
    fw.step(h);
    st(b)
}

#[test]
fn stream_commit_ready() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    h.set_identity(false, false, true, 2048, "0.1.0-test");
    for len in [0x200usize, SECTOR, SECTOR + 1, 3 * SECTOR - 1, 70_001] {
        let img = image(len);
        let mut fw = Fw::new();
        let s = flash_image(&mut b, &mut h, &mut fw, &img);
        assert_eq!(s & 0xf, State::Ready as u16, "len {len}: error {}", rd(&mut b, reg::ERROR));
        assert_eq!(s & status::REFUSED, 0);
        assert_eq!(fw.flash, img, "len {len}");
        assert_eq!(rd32(&mut b, reg::DONE_HI), len as u32);
        assert_eq!(rd32(&mut b, reg::RESULT_HI), crc32(0, &img));
    }
    assert_eq!(st(&mut b) & status::PARTITIONED, status::PARTITIONED);
    assert_eq!(rd(&mut b, reg::SLOT_KB), 2048);
    let v: Vec<u8> = (0..8).flat_map(|k| rd(&mut b, reg::VERSION + 2 * k).to_be_bytes()).collect();
    assert_eq!(&v[..10], b"0.1.0-test");
    assert!(v[10..].iter().all(|&c| c == 0));
}

#[test]
fn crc_mismatch_and_not_an_image() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let img = image(10_000);
    let mut fw = Fw::new();
    wr32(&mut b, reg::SIZE_HI, img.len() as u32);
    wr32(&mut b, reg::CRC_HI, crc32(0, &img) ^ 1);
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    fw.step(&mut h);
    for sector in img.chunks(SECTOR) {
        while st(&mut b) & status::SPACE == 0 {
            fw.step(&mut h);
        }
        for pair in sector.chunks(2) {
            wr(&mut b, reg::DATA, u16::from(pair[0]) << 8 | u16::from(*pair.get(1).unwrap_or(&0)));
        }
    }
    wr(&mut b, reg::CTRL, cmd::COMMIT);
    fw.step(&mut h);
    fw.step(&mut h);
    assert_eq!(st(&mut b) & 0xf, State::Error as u16);
    assert_eq!(rd(&mut b, reg::ERROR), error::CRC);

    let junk = vec![0x5a; 5000];
    let mut fw = Fw::new();
    let s = flash_image(&mut b, &mut h, &mut fw, &junk);
    assert_eq!(s & 0xf, State::Error as u16);
    assert_eq!(rd(&mut b, reg::ERROR), error::NOT_AN_IMAGE);
}

#[test]
fn refused_writes() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut fw = Fw::new();
    // Data before BEGIN.
    wr(&mut b, reg::DATA, 1);
    assert_ne!(st(&mut b) & status::REFUSED, 0);
    // Bad command number; a second command while busy.
    wr(&mut b, reg::CTRL, 9);
    wr32(&mut b, reg::SIZE_HI, 3);
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    assert_eq!(st(&mut b) & status::REFUSED, 0, "BEGIN clears it");
    wr(&mut b, reg::CTRL, cmd::ABORT);
    assert_ne!(st(&mut b) & status::REFUSED, 0, "busy");
    fw.step(&mut h);
    // Size 3: two words fit (the second carries one byte), a third does not.
    wr(&mut b, reg::DATA, 0x0102);
    wr(&mut b, reg::DATA, 0x0300);
    assert_ne!(st(&mut b) & status::REFUSED, 0, "sticky from the refused ABORT until BEGIN");
    wr(&mut b, reg::DATA, 0x0400);
    fw.step(&mut h);
    assert_eq!(rd32(&mut b, reg::DONE_HI), 3, "the third word went nowhere");
    assert_eq!(fw.first.as_deref(), Some(&[1u8, 2, 3][..]));
    // Byte access.
    wr(&mut b, reg::CTRL, cmd::ABORT);
    fw.step(&mut h);
    b.write(reg::SIZE_LO, 7, false, true); // odd byte (/UDS high)
    assert_ne!(rd32(&mut b, reg::SIZE_HI), 7);
}

#[test]
fn no_space_until_the_sector_is_in_flash() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut fw = Fw::new();
    wr32(&mut b, reg::SIZE_HI, 4 * SECTOR as u32);
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    fw.step(&mut h);
    for _ in 0..SECTOR / 2 {
        wr(&mut b, reg::DATA, 0xabcd);
    }
    assert_eq!(st(&mut b) & status::SPACE, 0, "one sector queued");
    wr(&mut b, reg::DATA, 0xabcd);
    assert_ne!(st(&mut b) & status::REFUSED, 0);
    h.peek_sector().unwrap();
    h.release_sector();
    assert_ne!(st(&mut b) & status::SPACE, 0);
}

#[test]
fn begin_drops_old_sectors_and_reset_aborts() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut fw = Fw::new();
    wr32(&mut b, reg::SIZE_HI, 2 * SECTOR as u32);
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    fw.step(&mut h);
    for _ in 0..SECTOR / 2 {
        wr(&mut b, reg::DATA, 0x1111);
    }
    // Restart before the firmware took the sector.
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    let c = h.take_command().unwrap();
    h.ack(&c);
    assert!(h.peek_sector().is_none(), "old sector dropped");
    h.set_state(State::Receiving, 0);
    b.reset();
    let c = h.take_command().unwrap();
    assert_eq!(c.cmd, cmd::ABORT);
}

#[test]
fn image_type_finds_tbyb() {
    let img = image(SECTOR);
    assert_eq!(image_type(&img), Some(0x1021 | IMAGE_TYPE_TBYB));
    assert_eq!(image_type(&[0u8; 64]), None);
    // zlib's check value.
    assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(crc32(0, b"1234"), b"56789"), 0xcbf4_3926);
}

#[test]
fn window_routes_c0_to_update() {
    use crate::autoconfig::{AC_BASE_HI, AC_BASE_LO};
    use crate::{nic, usb, window::Window};
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let (nb, _) = nsh.split();
    let mut ush = Box::new(usb::Shared::new());
    let (ub, _) = ush.split();
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    h.set_identity(true, false, true, 2048, "x");
    let mut w = Window::with_usb(nb, ub).with_update(b);
    w.write(AC_BASE_LO, 0x9000, true, false);
    w.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w.read_at(true, 0x00, true, true), 0x5055, "USB below $C0");
    assert_eq!(w.read_at(true, reg::SLOT_KB, true, true), 2048);
    assert_ne!(w.read_at(true, reg::STATUS, true, true) & status::SLOT_B, 0);
    // A16 = 0, $C0-$FE: the network window's data alias (RX_DATA, empty queue).
    assert_eq!(w.read_at(false, reg::STATUS, true, true), 0x0000, "A16 = 0: network window");
    // Without a USB model the update registers still answer.
    let mut nsh2 = Box::new(nic::Shared::new([0; 6]));
    let (nb2, _) = nsh2.split();
    let mut sh2 = Box::new(Shared::new());
    let (b2, _) = sh2.split();
    let mut w2 = Window::new(nb2).with_update(b2);
    w2.write(AC_BASE_LO, 0x9000, true, false);
    w2.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w2.read_at(true, 0x00, true, true), 0xffff);
    assert_eq!(w2.read_at(true, reg::ERROR, true, true), 0);
}

/// COMMIT right behind the last sector: the sector goes into flash first
/// (not judged short). ABORT behind a sector: the sector is dropped.
#[test]
fn commit_waits_for_sectors_abort_does_not() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let img = image(SECTOR + 100);
    let mut fw = Fw::new();
    wr32(&mut b, reg::SIZE_HI, img.len() as u32);
    wr32(&mut b, reg::CRC_HI, crc32(0, &img));
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    fw.step(&mut h);
    for (k, sector) in img.chunks(SECTOR).enumerate() {
        if k > 0 {
            fw.step(&mut h);
        }
        for pair in sector.chunks(2) {
            wr(&mut b, reg::DATA, u16::from(pair[0]) << 8 | u16::from(*pair.get(1).unwrap_or(&0)));
        }
    }
    wr(&mut b, reg::CTRL, cmd::COMMIT);
    assert_eq!(h.pending_command(), Some(cmd::COMMIT));
    assert!(h.peek_sector().is_some());
    fw.step(&mut h);
    assert_eq!(st(&mut b) & 0xf, State::Ready as u16, "error {}", rd(&mut b, reg::ERROR));
    assert_eq!(fw.flash, img);

    let mut fw = Fw::new();
    wr32(&mut b, reg::SIZE_HI, img.len() as u32);
    wr(&mut b, reg::CTRL, cmd::BEGIN);
    fw.step(&mut h);
    for pair in img[..SECTOR].chunks(2) {
        wr(&mut b, reg::DATA, u16::from(pair[0]) << 8 | u16::from(pair[1]));
    }
    wr(&mut b, reg::CTRL, cmd::ABORT);
    fw.step(&mut h);
    assert!(fw.flash.is_empty());
    assert!(h.peek_sector().is_none());
    assert_eq!(st(&mut b) & 0xf, State::Idle as u16);
}

#[test]
fn bootrom_commands_are_accepted() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    for c in [cmd::BOOTROM_OFF, cmd::BOOTROM_ON] {
        wr(&mut b, reg::CTRL, c);
        assert_eq!(st(&mut b) & status::REFUSED, 0);
        let got = h.take_command().unwrap();
        assert_eq!(got.cmd, c);
        h.set_bootrom(c == cmd::BOOTROM_ON);
        h.ack(&got);
        assert_eq!(st(&mut b) & status::BOOTROM != 0, c == cmd::BOOTROM_ON);
    }
    wr(&mut b, reg::CTRL, 8);
    assert_ne!(st(&mut b) & status::REFUSED, 0, "unknown commands are refused");
}
