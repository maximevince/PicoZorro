//! Host tests of the MPEG registers: the library's side through register
//! reads and writes, the firmware's side through `HostPort`.

use super::*;

fn rd(b: &mut BusPort, r: u8) -> u16 {
    b.read(r, true, true)
}
fn wr(b: &mut BusPort, r: u8, v: u16) {
    b.write(r, v, true, true)
}

/// IN_LEN / IN_DATA / IN_COMMIT as the library does it.
fn feed(b: &mut BusPort, chunk: &[u8]) {
    wr(b, reg::IN_LEN, chunk.len() as u16);
    for c in chunk.chunks(2) {
        wr(b, reg::IN_DATA, u16::from(c[0]) << 8 | u16::from(*c.get(1).unwrap_or(&0xee)));
    }
    wr(b, reg::IN_COMMIT, 0);
}

/// OUT_LEN / OUT_DATA (alternating $82 and $84, as move.l) / OUT_DONE.
fn fetch(b: &mut BusPort) -> Option<Vec<u8>> {
    let len = rd(b, reg::OUT_LEN) as usize;
    if len == 0 {
        return None;
    }
    let mut v = Vec::new();
    for k in 0..len.div_ceil(2) {
        let w = rd(b, if k % 2 == 0 { reg::OUT_DATA } else { reg::OUT_DATA2 });
        v.extend_from_slice(&w.to_be_bytes());
    }
    v.truncate(len);
    wr(b, reg::OUT_DONE, 0);
    Some(v)
}

fn in_free(b: &mut BusPort) -> u16 {
    rd(b, reg::STATUS) >> 8 & 0xf
}
fn out_queued(b: &mut BusPort) -> u16 {
    rd(b, reg::STATUS) & 0xff
}
fn stat(b: &mut BusPort, s: Stat) -> u32 {
    let r = reg::STATS + 4 * s as u8;
    u32::from(rd(b, r)) << 16 | u32::from(rd(b, r + 2))
}
fn be(v: &[u8], word: usize) -> u16 {
    u16::from_be_bytes([v[2 * word], v[2 * word + 1]])
}

const STEREO: Config = Config { freq_div: 1, mono: false, scale: 100 };

#[test]
fn identity_and_unused() {
    let mut sh = Box::new(Shared::new());
    let (mut b, _h) = sh.split();
    assert_eq!(rd(&mut b, reg::MAGIC), 0x4d50);
    assert_eq!(rd(&mut b, reg::VERSION), 1);
    assert_eq!(rd(&mut b, reg::CTRL), 0);
    assert_eq!(rd(&mut b, 0x6e), 0xffff);
    assert_eq!(rd(&mut b, reg::IN_LEN), 0xffff);
    assert_eq!(rd(&mut b, reg::OUT_LEN), 0);
    assert_eq!(in_free(&mut b), IN_SLOTS as u16);
    assert_eq!(out_queued(&mut b), 0);
    assert!(BusPort::is_mpeg_reg(0x60) && BusPort::is_mpeg_reg(0x9e));
    assert!(!BusPort::is_mpeg_reg(0x5e) && !BusPort::is_mpeg_reg(0xa0));
}

#[test]
fn start_takes_config_and_session() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    assert_eq!(h.take_session(), None);
    wr(&mut b, reg::CONFIG, 0xff12); // FREQ_DIV 2 (/4), MONO; other bits dropped
    wr(&mut b, reg::SCALE, 250);
    assert_eq!(rd(&mut b, reg::CONFIG), 0x0012);
    wr(&mut b, reg::CTRL, cmd::START);
    let sess = rd(&mut b, reg::SESSION);
    let s = h.take_session().unwrap();
    assert!(s.running);
    assert_eq!(s.config, Config { freq_div: 4, mono: true, scale: 250 });
    assert_eq!(s.epoch as u16, sess);
    assert_eq!(h.take_session(), None);
    // CONFIG after START does not change the running session; SCALE
    // applies from the next frame.
    wr(&mut b, reg::CONFIG, 0);
    assert_eq!(h.scale(), 250);
    wr(&mut b, reg::SCALE, 80);
    assert_eq!(h.scale(), 80);
    wr(&mut b, reg::SCALE, 0);
    assert_eq!(h.scale(), 100, "out of range counts as 100");
    wr(&mut b, reg::CTRL, cmd::STOP);
    let s = h.take_session().unwrap();
    assert!(!s.running);
    assert_eq!(s.config, STEREO);
    assert_ne!(rd(&mut b, reg::SESSION), sess);
}

#[test]
fn chunks_round_trip_with_end_marker() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::CTRL, cmd::START);
    h.take_session().unwrap();
    let a: Vec<u8> = (0..IN_BYTES).map(|i| (i * 7) as u8).collect();
    let odd: Vec<u8> = (0..333).map(|i| (i * 3 + 1) as u8).collect();
    feed(&mut b, &a);
    feed(&mut b, &odd);
    feed(&mut b, &[]);
    assert_eq!(in_free(&mut b), IN_SLOTS as u16 - 3);
    let mut buf = [0u8; IN_BYTES];
    assert_eq!(h.take_chunk(&mut buf), Some(IN_BYTES));
    assert_eq!(&buf[..], &a[..]);
    assert_eq!(h.take_chunk(&mut buf), Some(333));
    assert_eq!(&buf[..333], &odd[..]);
    assert_eq!(h.take_chunk(&mut buf), Some(0));
    assert_eq!(h.take_chunk(&mut buf), None);
    assert_eq!(in_free(&mut b), IN_SLOTS as u16);
    assert_eq!(stat(&mut b, Stat::InErrors), 0);
}

#[test]
fn input_errors_are_counted_not_taken() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut buf = [0u8; IN_BYTES];
    wr(&mut b, reg::IN_DATA, 0x1234); // no chunk open: ignored
    wr(&mut b, reg::IN_COMMIT, 0);
    wr(&mut b, reg::IN_LEN, IN_BYTES as u16 + 2); // too long
    assert_eq!(stat(&mut b, Stat::InErrors), 1);
    wr(&mut b, reg::IN_LEN, 4); // short commit
    wr(&mut b, reg::IN_DATA, 0x0102);
    wr(&mut b, reg::IN_COMMIT, 0);
    assert_eq!(stat(&mut b, Stat::InErrors), 2);
    wr(&mut b, reg::IN_LEN, 2); // reopened while open
    wr(&mut b, reg::IN_LEN, 2);
    wr(&mut b, reg::IN_DATA, 0x0a0b);
    wr(&mut b, reg::IN_DATA, 0x0c0d); // past the length: ignored
    b.write(reg::IN_COMMIT, 0, false, true); // odd byte write: ignored
    wr(&mut b, reg::IN_COMMIT, 0);
    assert_eq!(stat(&mut b, Stat::InErrors), 3);
    assert_eq!(h.take_chunk(&mut buf), Some(2));
    assert_eq!(&buf[..2], &[0x0a, 0x0b]);
    for _ in 0..IN_SLOTS {
        feed(&mut b, &[1, 2]);
    }
    assert_eq!(in_free(&mut b), 0);
    feed(&mut b, &[3, 4]); // full
    assert_eq!(stat(&mut b, Stat::InErrors), 4);
    for _ in 0..IN_SLOTS {
        assert_eq!(h.take_chunk(&mut buf), Some(2));
        assert_eq!(&buf[..2], &[1, 2]);
    }
    assert_eq!(h.take_chunk(&mut buf), None);
}

#[test]
fn records_read_back_and_count() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::CTRL, cmd::START);
    let s = h.take_session().unwrap();
    let pcm: Vec<i16> = (0..2 * 1152).map(|i| (i as i16).wrapping_mul(29)).collect();
    let f = Frame { flags: flags::LOST, frame_bytes: 418, header: [0xff, 0xfb, 0x90, 0x64] };
    let mut rec = [0u8; OUT_BYTES];
    let n = record(&mut rec, &f, &pcm, 1152, 2, &STEREO);
    assert_eq!(n, OUT_BYTES);
    assert_eq!(h.post(s.epoch, &mut rec[..n]), Ok(true));
    let n = record(&mut rec, &Frame::default(), &[], 0, 2, &STEREO);
    assert_eq!(h.post(s.epoch, &mut rec[..n]), Ok(true));
    let n = end_record(&mut rec);
    assert_eq!(h.post(s.epoch, &mut rec[..n]), Ok(true));
    assert_eq!(out_queued(&mut b), 3);

    let v = fetch(&mut b).unwrap();
    assert_eq!(v.len(), OUT_BYTES);
    assert_eq!(be(&v, 0), flags::LOST);
    assert_eq!(be(&v, 1), 1152);
    assert_eq!(be(&v, 2), 2);
    assert_eq!(be(&v, 3), 418);
    assert_eq!(&v[8..12], &[0xff, 0xfb, 0x90, 0x64]);
    assert_eq!(be(&v, 6), rd(&mut b, reg::SESSION));
    // Planar: left samples, then right.
    assert_eq!(be(&v, 8) as i16, pcm[0]);
    assert_eq!(be(&v, 9) as i16, pcm[2]);
    assert_eq!(be(&v, 8 + 1152) as i16, pcm[1]);
    assert_eq!(be(&v, 8 + 2 * 1152 - 1) as i16, pcm[2 * 1152 - 1]);

    let v = fetch(&mut b).unwrap();
    assert_eq!((v.len(), be(&v, 0), be(&v, 1)), (HDR, 0, 0));
    let v = fetch(&mut b).unwrap();
    assert_eq!((v.len(), be(&v, 0)), (HDR, flags::END));
    assert_eq!(fetch(&mut b), None);
    wr(&mut b, reg::OUT_DONE, 0); // on an empty queue: harmless
    assert_eq!(stat(&mut b, Stat::Frames), 1);
    assert_eq!(stat(&mut b, Stat::Skipped), 1);
}

#[test]
fn queue_full_and_bad_records() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::CTRL, cmd::START);
    let s = h.take_session().unwrap();
    let mut rec = [0u8; OUT_BYTES];
    for _ in 0..OUT_SLOTS {
        assert!(h.out_free());
        let n = end_record(&mut rec);
        assert_eq!(h.post(s.epoch, &mut rec[..n]), Ok(true));
    }
    assert!(!h.out_free());
    assert_eq!(h.post(s.epoch, &mut rec[..HDR]), Ok(false));
    assert_eq!(h.post(s.epoch, &mut rec[..HDR - 1]), Err(()));
    assert_eq!(out_queued(&mut b), OUT_SLOTS as u16);
    fetch(&mut b).unwrap();
    assert!(h.out_free());
}

#[test]
fn start_voids_the_old_stream() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::CTRL, cmd::START);
    let old = h.take_session().unwrap();
    let mut rec = [0u8; OUT_BYTES];
    let n = end_record(&mut rec);
    h.post(old.epoch, &mut rec[..n]).unwrap();
    feed(&mut b, &[1, 2, 3]);
    feed(&mut b, &[4, 5]);
    // A seek: START again, then the new position's bytes.
    wr(&mut b, reg::CTRL, cmd::START);
    assert_eq!(out_queued(&mut b), 0, "START empties the output");
    feed(&mut b, &[9, 9]);
    // The task finishes a frame of the old stream: dropped.
    assert_eq!(h.post(old.epoch, &mut rec[..n]), Ok(true));
    assert_eq!(out_queued(&mut b), 0);
    let new = h.take_session().unwrap();
    assert_ne!(new.epoch, old.epoch);
    let mut buf = [0u8; IN_BYTES];
    assert_eq!(h.take_chunk(&mut buf), Some(2), "chunks before START are skipped");
    assert_eq!(&buf[..2], &[9, 9]);
    assert_eq!(h.take_chunk(&mut buf), None);
    // /BUSRST: a stopped session.
    b.reset();
    assert!(!h.take_session().unwrap().running);
}

#[test]
fn stats_high_word_latches() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::CTRL, cmd::START);
    let s = h.take_session().unwrap();
    let mut rec = [0u8; OUT_BYTES];
    let pcm = [1i16; 2];
    let n = record(&mut rec, &Frame::default(), &pcm, 1, 2, &STEREO);
    h.post(s.epoch, &mut rec[..n]).unwrap();
    let hi = rd(&mut b, reg::STATS);
    fetch(&mut b).unwrap();
    h.post(s.epoch, &mut rec[..n]).unwrap();
    assert_eq!(hi, 0);
    assert_eq!(rd(&mut b, reg::STATS + 2), 1, "low word of the latched value");
    assert_eq!(stat(&mut b, Stat::Frames), 2);
}

#[test]
fn shape_mono_div_and_gain() {
    let pcm: Vec<i16> = vec![100, 300, -200, 0, 1000, -1000, 7, 9];
    let mut out = [0u8; 64];
    let get = |o: &[u8], i: usize| i16::from_be_bytes([o[2 * i], o[2 * i + 1]]);

    assert_eq!(shape(&pcm, 4, 2, &STEREO, &mut out), (4, 2));
    assert_eq!((0..8).map(|i| get(&out, i)).collect::<Vec<_>>(), vec![100, -200, 1000, 7, 300, 0, -1000, 9]);

    let mono = Config { mono: true, ..STEREO };
    assert_eq!(shape(&pcm, 4, 2, &mono, &mut out), (4, 1));
    assert_eq!((0..4).map(|i| get(&out, i)).collect::<Vec<_>>(), vec![200, -100, 0, 8]);

    let div2 = Config { freq_div: 2, ..STEREO };
    assert_eq!(shape(&pcm, 4, 2, &div2, &mut out), (2, 2));
    assert_eq!((0..4).map(|i| get(&out, i)).collect::<Vec<_>>(), vec![-50, 503, 150, -495]);

    let div4_mono = Config { freq_div: 4, mono: true, scale: 100 };
    assert_eq!(shape(&pcm, 4, 2, &div4_mono, &mut out), (1, 1));
    assert_eq!(get(&out, 0), (100 + 300 - 200 + 1000 - 1000 + 7 + 9) / 8);

    let loud = Config { scale: 800, ..STEREO };
    let big = [20000i16, -20000, 1000, -1000];
    assert_eq!(shape(&big, 2, 2, &loud, &mut out), (2, 2));
    assert_eq!((0..4).map(|i| get(&out, i)).collect::<Vec<_>>(), vec![32767, 8000, -32768, -8000]);

    let mono_src = [5i16, -5, 32767];
    let quiet = Config { scale: 50, mono: true, ..STEREO };
    assert_eq!(shape(&mono_src, 3, 1, &quiet, &mut out), (3, 1), "mono has nothing to mix");
    assert_eq!((0..3).map(|i| get(&out, i)).collect::<Vec<_>>(), vec![2, -2, 16383]);
}

#[test]
fn window_routes_60_to_9e_to_mpeg() {
    use crate::autoconfig::{AC_BASE_HI, AC_BASE_LO};
    use crate::{nic, update, usb, window::Window};
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let (nb, _) = nsh.split();
    let mut ush = Box::new(usb::Shared::new());
    let (ub, _) = ush.split();
    let mut upsh = Box::new(update::Shared::new());
    let (upb, _) = upsh.split();
    let mut sh = Box::new(Shared::new());
    let (b, mut h) = sh.split();
    let mut w = Window::with_usb(nb, ub).with_update(upb).with_mpeg(b);
    assert_ne!(w.read_at(true, reg::MAGIC, true, true), MAGIC, "unconfigured: the Autoconfig ROM answers");
    w.write(AC_BASE_LO, 0x9000, true, false);
    w.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w.read_at(true, reg::MAGIC, true, true), MAGIC);
    assert_eq!(w.read_at(true, 0x00, true, true), usb::MAGIC, "USB below $60");
    assert_eq!(w.read_at(true, 0xa0, true, true), 0xffff, "$A0-$BE: nothing");
    assert_ne!(w.read_at(false, reg::MAGIC, true, true), MAGIC, "A16 = 0: the network window (MCAST)");
    w.write_at(true, reg::CTRL, cmd::START, true, true);
    assert!(h.take_session().unwrap().running);
    w.reset();
    assert!(!h.take_session().unwrap().running, "/BUSRST stops the stream");
}
