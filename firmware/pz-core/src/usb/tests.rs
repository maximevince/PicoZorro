//! Host tests of the USB window: the driver's side through register reads
//! and writes, the firmware's side through `HostPort`.

use super::*;
use crate::autoconfig::{AC_BASE_HI, AC_BASE_LO};
use crate::nic;
use crate::window::Window;

fn rd(b: &mut BusPort, r: u8) -> u16 {
    b.read(r, true, true)
}
fn wr(b: &mut BusPort, r: u8, v: u16) {
    b.write(r, v, true, true)
}

fn record(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|j| seed.wrapping_mul(37).wrapping_add(j as u8 ^ (j >> 8) as u8)).collect()
}

/// REQ_LEN / REQ_DATA / REQ_COMMIT as the driver does it.
fn submit(b: &mut BusPort, rec: &[u8]) {
    wr(b, reg::REQ_LEN, rec.len() as u16);
    for c in rec.chunks(2) {
        wr(b, reg::REQ_DATA, u16::from(c[0]) << 8 | u16::from(*c.get(1).unwrap_or(&0xee)));
    }
    wr(b, reg::REQ_COMMIT, 0);
}

/// CPL_LEN / CPL_DATA / CPL_DONE; None when empty.
fn fetch(b: &mut BusPort) -> Option<Vec<u8>> {
    let len = rd(b, reg::CPL_LEN) as usize;
    if len == 0 {
        return None;
    }
    let mut v = Vec::new();
    for _ in 0..len.div_ceil(2) {
        let w = rd(b, reg::CPL_DATA);
        v.push((w >> 8) as u8);
        v.push(w as u8);
    }
    if len % 2 == 1 {
        assert_eq!(v.pop(), Some(0), "odd tail byte reads 0");
    }
    wr(b, reg::CPL_DONE, 0);
    Some(v)
}

fn req_free(b: &mut BusPort) -> u16 {
    (rd(b, reg::STATUS) >> 8) & 0x1f
}

fn stat(b: &mut BusPort, s: Stat) -> u32 {
    let r = reg::STATS + 4 * s as u8;
    u32::from(rd(b, r)) << 16 | u32::from(rd(b, r + 2))
}

#[test]
fn identity_and_unused() {
    let mut sh = Box::new(Shared::new());
    let (mut b, _h) = sh.split();
    assert_eq!(rd(&mut b, reg::MAGIC), 0x5055);
    assert_eq!(rd(&mut b, reg::VERSION), 1);
    assert_eq!(rd(&mut b, reg::CTRL), 0);
    assert_eq!(rd(&mut b, 0xc0), 0xffff);
    assert_eq!(rd(&mut b, reg::REQ_DATA), 0xffff, "write-only");
    assert_eq!(req_free(&mut b), REQ_SLOTS as u16);
}

#[test]
fn requests_byte_exact() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut out = [0u8; SLOT_BYTES];
    for len in [REQ_HDR, REQ_HDR + 1, REQ_HDR + 8, REQ_HDR + 63, SLOT_BYTES] {
        let rec = record(len, len as u8);
        submit(&mut b, &rec);
        let (n, _) = h.take_req(&mut out).expect("queued");
        assert_eq!(&out[..n], &rec[..], "len {len}");
        assert!(h.take_req(&mut out).is_none());
    }
    assert_eq!(stat(&mut b, Stat::Requests), 5);
    assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
}

#[test]
fn completions_byte_exact_and_level_irq() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::INT_ENABLE, int::CPL_AVAIL);
    assert!(!b.shared().irq_pending());
    let mut out = [0u8; SLOT_BYTES];
    submit(&mut b, &record(REQ_HDR, 1));
    let (_, t) = h.take_req(&mut out).unwrap();
    for len in [CPL_HDR, CPL_HDR + 1, CPL_HDR + 18, SLOT_BYTES] {
        let rec = record(len, 7 + len as u8);
        assert_eq!(h.post_cpl(t, &rec), Ok(true));
        assert!(b.shared().irq_pending());
        assert_eq!(rd(&mut b, reg::STATUS) & 0xff, 1);
        assert_eq!(fetch(&mut b).unwrap(), rec, "len {len}");
        assert!(!b.shared().irq_pending(), "level clears when the queue empties");
    }
    assert_eq!(h.post_cpl(t, &[0; CPL_HDR - 1]), Err(()));
    assert_eq!(stat(&mut b, Stat::Completions), 4);
}

#[test]
fn cpl_done_mid_record_skips_rest() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut out = [0u8; SLOT_BYTES];
    submit(&mut b, &record(REQ_HDR, 1));
    let (_, t) = h.take_req(&mut out).unwrap();
    let a = record(40, 1);
    let c = record(20, 2);
    h.post_cpl(t, &a).unwrap();
    h.post_cpl(t, &c).unwrap();
    assert_eq!(rd(&mut b, reg::CPL_LEN), 40);
    rd(&mut b, reg::CPL_DATA);
    wr(&mut b, reg::CPL_DONE, 0);
    assert_eq!(fetch(&mut b).unwrap(), c);
    assert_eq!(rd(&mut b, reg::CPL_DATA), 0, "empty queue reads 0");
    wr(&mut b, reg::CPL_DONE, 0); // harmless when empty
}

#[test]
fn request_queue_full_and_req_free() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::INT_ENABLE, int::REQ_FREE);
    for i in 0..REQ_SLOTS {
        assert_eq!(req_free(&mut b) as usize, REQ_SLOTS - i);
        submit(&mut b, &record(REQ_HDR, i as u8));
    }
    assert_eq!(req_free(&mut b), 0);
    // One more: REQ_LEN refuses, the data and commit then error too.
    submit(&mut b, &record(REQ_HDR, 99));
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    assert_eq!(stat(&mut b, Stat::ReqErrors), 1);
    wr(&mut b, reg::INT, int::BUS_ERROR);
    assert!(!b.shared().irq_pending());
    let mut out = [0u8; SLOT_BYTES];
    let (n, _) = h.take_req(&mut out).unwrap();
    assert_eq!(&out[..n], &record(REQ_HDR, 0)[..]);
    assert_eq!(req_free(&mut b), 1);
    assert!(b.shared().irq_pending(), "REQ_FREE latched");
    wr(&mut b, reg::INT, int::REQ_FREE);
    assert!(!b.shared().irq_pending());
}

#[test]
fn bad_requests() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut out = [0u8; SLOT_BYTES];
    // Too short, too long: refused.
    wr(&mut b, reg::REQ_LEN, (REQ_HDR - 1) as u16);
    wr(&mut b, reg::REQ_LEN, (SLOT_BYTES + 1) as u16);
    assert_eq!(stat(&mut b, Stat::ReqErrors), 2);
    // Short commit: dropped, counted.
    wr(&mut b, reg::REQ_LEN, REQ_HDR as u16);
    wr(&mut b, reg::REQ_DATA, 0x5055);
    wr(&mut b, reg::REQ_COMMIT, 0);
    assert!(h.take_req(&mut out).is_none());
    assert_eq!(stat(&mut b, Stat::ReqErrors), 3);
    // Data and commit without REQ_LEN: bus error.
    wr(&mut b, reg::INT, int::BUS_ERROR);
    wr(&mut b, reg::REQ_DATA, 1);
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    wr(&mut b, reg::INT, int::BUS_ERROR);
    wr(&mut b, reg::REQ_COMMIT, 1);
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    // Too many words.
    wr(&mut b, reg::INT, int::BUS_ERROR);
    wr(&mut b, reg::REQ_LEN, REQ_HDR as u16);
    for _ in 0..REQ_HDR / 2 + 1 {
        wr(&mut b, reg::REQ_DATA, 0);
    }
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    wr(&mut b, reg::REQ_COMMIT, 0);
    assert!(h.take_req(&mut out).is_some(), "the record itself was complete");
    // Byte access.
    wr(&mut b, reg::INT, int::BUS_ERROR);
    b.write(reg::INT_ENABLE, 0xffff, false, true); // odd byte (/UDS high)
    assert_eq!(rd(&mut b, reg::INT_ENABLE), 0);
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
}

#[test]
fn reset_voids_in_flight_work() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut out = [0u8; SLOT_BYTES];
    submit(&mut b, &record(REQ_HDR, 1));
    submit(&mut b, &record(REQ_HDR, 2));
    let (_, old) = h.take_req(&mut out).unwrap();
    h.post_cpl(old, &record(CPL_HDR, 1)).unwrap();
    assert!(!h.take_reset());
    wr(&mut b, reg::CTRL, ctrl::RESET_QUEUES);
    assert!(h.take_reset());
    assert!(!h.take_reset(), "once");
    assert_eq!(rd(&mut b, reg::CPL_LEN), 0, "queued completion gone");
    assert!(h.take_req(&mut out).is_none(), "queued request gone");
    assert_eq!(req_free(&mut b), REQ_SLOTS as u16);
    assert_eq!(h.post_cpl(old, &record(CPL_HDR, 3)), Ok(true));
    assert_eq!(rd(&mut b, reg::CPL_LEN), 0, "stale completion dropped");
    // New work flows again.
    let rec = record(REQ_HDR + 5, 4);
    submit(&mut b, &rec);
    let (n, t) = h.take_req(&mut out).unwrap();
    assert_eq!(&out[..n], &rec[..]);
    h.post_cpl(t, &record(CPL_HDR, 5)).unwrap();
    assert_eq!(fetch(&mut b).unwrap(), record(CPL_HDR, 5));
}

#[test]
fn completion_queue_full() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let mut out = [0u8; SLOT_BYTES];
    submit(&mut b, &record(REQ_HDR, 1));
    let (_, t) = h.take_req(&mut out).unwrap();
    for i in 0..CPL_SLOTS {
        assert!(h.cpl_free());
        assert_eq!(h.post_cpl(t, &record(CPL_HDR, i as u8)), Ok(true));
    }
    assert!(!h.cpl_free());
    assert_eq!(h.post_cpl(t, &record(CPL_HDR, 0xff)), Ok(false));
    assert_eq!(fetch(&mut b).unwrap(), record(CPL_HDR, 0));
    assert_eq!(h.post_cpl(t, &record(CPL_HDR, 0xff)), Ok(true));
}

#[test]
fn port_change() {
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    wr(&mut b, reg::INT_ENABLE, int::PORT_CHANGE);
    h.set_port(false, false);
    assert!(!b.shared().irq_pending(), "no change");
    h.set_port(true, true);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xc000, status::CONNECTED | status::LOW_SPEED);
    assert!(b.shared().irq_pending());
    wr(&mut b, reg::INT, int::PORT_CHANGE);
    assert!(!b.shared().irq_pending());
    h.set_port(false, true);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xc000, 0, "no speed without a device");
    assert_ne!(rd(&mut b, reg::INT) & int::PORT_CHANGE, 0);
}

#[test]
fn stats_do_not_tear() {
    let mut sh = Box::new(Shared::new());
    let (mut b, _h) = sh.split();
    sh_requests(&b).store(0x0001_ffff, Relaxed);
    let hi = rd(&mut b, reg::STATS);
    sh_requests(&b).store(0x0002_0000, Relaxed);
    let lo = rd(&mut b, reg::STATS + 2);
    assert_eq!((hi, lo), (0x0001, 0xffff));
}

fn sh_requests<'a>(b: &BusPort<'a>) -> &'a AtomicU32 {
    &b.shared().requests
}

/// The window: the USB half answers at A16 = 1 once configured, the
/// Autoconfig ROM before; /INT from either model.
#[test]
fn window_routing() {
    let mut nsh = Box::new(nic::Shared::new([0; 6]));
    let (nb, _nn) = nsh.split();
    let mut ush = Box::new(Shared::new());
    let (ub, mut uh) = ush.split();
    let mut w = Window::with_usb(nb, ub);
    // Unconfigured: A16 not decoded, the ROM's nibble at 0.
    assert_eq!(w.read_at(true, 0, true, true), w.read_at(false, 0, true, true));
    w.write(AC_BASE_LO, 0x9000, true, false);
    w.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w.read_at(true, reg::MAGIC, true, true), 0x5055);
    assert_eq!(w.read_at(false, 0, true, true), 0x505a);
    w.write_at(true, reg::INT_ENABLE, int::PORT_CHANGE, true, true);
    assert!(!w.irq_asserted());
    uh.set_port(true, false);
    assert!(w.irq_asserted());
    w.reset();
    assert!(!w.irq_asserted(), "reset clears INT_ENABLE and acknowledges");
    // No USB model: the upper half reads $FFFF.
    let mut nsh2 = Box::new(nic::Shared::new([0; 6]));
    let (nb2, _) = nsh2.split();
    let mut w2 = Window::new(nb2);
    w2.write(AC_BASE_LO, 0x9000, true, false);
    w2.write(AC_BASE_HI, 0xe000, true, false);
    assert_eq!(w2.read_at(true, reg::MAGIC, true, true), 0xffff);
}

/// Bus side and host side on two threads: records cross both ways intact.
#[test]
fn two_threads_spsc_stress() {
    const N: u32 = if cfg!(miri) { 200 } else { 50_000 };
    let mut sh = Box::new(Shared::new());
    let (mut b, mut h) = sh.split();
    let req_len = |i: u32| REQ_HDR + (i as usize * 7919) % (MAX_DATA + 1);
    let cpl_len = |i: u32| CPL_HDR + (i as usize * 104_729) % (MAX_DATA + 1);
    std::thread::scope(|sc| {
        sc.spawn(move || {
            let mut out = [0u8; SLOT_BYTES];
            let (mut taken, mut posted) = (0u32, 0u32);
            let mut tickets = std::collections::VecDeque::new();
            while taken < N || posted < N {
                if let Some((n, t)) = h.take_req(&mut out) {
                    assert_eq!(&out[..n], &record(req_len(taken), taken as u8)[..], "req {taken}");
                    tickets.push_back(t);
                    taken += 1;
                }
                if let Some(&t) = tickets.front() {
                    if h.post_cpl(t, &record(cpl_len(posted), posted as u8 ^ 0x55)) == Ok(true) {
                        tickets.pop_front();
                        posted += 1;
                    }
                }
            }
        });
        sc.spawn(move || {
            let (mut sent, mut got) = (0u32, 0u32);
            while sent < N || got < N {
                if let Some(c) = fetch(&mut b) {
                    assert_eq!(c, record(cpl_len(got), got as u8 ^ 0x55), "cpl {got}");
                    got += 1;
                }
                if sent < N && req_free(&mut b) > 0 {
                    submit(&mut b, &record(req_len(sent), sent as u8));
                    sent += 1;
                }
            }
            assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
            assert_eq!(stat(&mut b, Stat::ReqErrors), 0);
        });
    });
}
