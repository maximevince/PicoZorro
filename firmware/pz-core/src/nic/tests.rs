//! Host tests of the network window: the driver's side through register
//! reads and writes, the chip's side through `NicPort`.

use super::*;
use crate::autoconfig::AC_BASE_HI;
use crate::slave::{self, Event, State};
use crate::window::Window;

const MAC: [u8; 6] = [0x52, 0x5a, 0x58, 0xd1, 0xb4, 0xcd];
const OTHER: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

fn shared() -> Box<Shared> {
    Box::new(Shared::new(MAC))
}

/// Word accesses as the driver makes them.
fn rd(b: &mut BusPort, reg: u8) -> u16 {
    b.read(reg, true, true)
}
fn wr(b: &mut BusPort, reg: u8, v: u16) {
    b.write(reg, v, true, true)
}

fn frame(dst: [u8; 6], len: usize, seed: u32) -> Vec<u8> {
    let mut f: Vec<u8> = (0..len).map(|j| (seed as usize).wrapping_mul(131).wrapping_add(j * 31) as u8).collect();
    f[..6].copy_from_slice(&dst);
    f
}

/// CopyFromBuff + the TX_* sequence of the driver.
fn send(b: &mut BusPort, f: &[u8]) {
    wr(b, reg::TX_LEN, f.len() as u16);
    for c in f.chunks(2) {
        let w = u16::from(c[0]) << 8 | u16::from(*c.get(1).unwrap_or(&0xee));
        wr(b, reg::TX_DATA, w);
    }
    wr(b, reg::TX_COMMIT, 0);
}

/// RX_LEN / RX_DATA / RX_DONE; None when the queue is empty.
fn receive(b: &mut BusPort) -> Option<Vec<u8>> {
    let len = rd(b, reg::RX_LEN) as usize;
    if len == 0 {
        return None;
    }
    let mut f = Vec::with_capacity(len + 1);
    for _ in 0..len.div_ceil(2) {
        let w = rd(b, reg::RX_DATA);
        f.push((w >> 8) as u8);
        f.push(w as u8);
    }
    if len % 2 == 1 {
        assert_eq!(f.pop(), Some(0), "odd tail byte must read 0");
    }
    wr(b, reg::RX_DONE, 0);
    Some(f)
}

fn stat(b: &mut BusPort, s: Stat) -> u32 {
    let r = reg::STATS + 4 * s as u8;
    let hi = rd(b, r);
    let lo = rd(b, r + 2);
    u32::from(hi) << 16 | u32::from(lo)
}

fn online(b: &mut BusPort) {
    wr(b, reg::CTRL, ctrl::ONLINE);
}

#[test]
fn tx_frames_byte_exact() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    for len in [14, 15, 59, 60, 61, 1513, 1514] {
        let f = frame(OTHER, len, len as u32);
        send(&mut b, &f);
        let got = n.peek_tx().expect("frame queued").to_vec();
        assert_eq!(&got[..len], &f[..], "len {len}");
        assert_eq!(got.len(), len.max(TX_PAD_TO));
        assert!(got[len..].iter().all(|&x| x == 0), "padding is zero");
        n.release_tx(true);
        assert!(n.peek_tx().is_none());
    }
    assert_eq!(stat(&mut b, Stat::TxOk), 7);
    assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
}

#[test]
fn tx_rejects_bad_lengths_and_short_commits() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    for bad in [0u16, 13, 1515, 0xffff] {
        wr(&mut b, reg::TX_LEN, bad);
        assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0, "len {bad}");
        wr(&mut b, reg::INT, int::BUS_ERROR);
        wr(&mut b, reg::TX_DATA, 0x1234); // nothing open
        assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
        wr(&mut b, reg::INT, int::BUS_ERROR);
    }
    // Short commit: dropped and counted.
    wr(&mut b, reg::TX_LEN, 20);
    wr(&mut b, reg::TX_DATA, 0);
    wr(&mut b, reg::TX_COMMIT, 0);
    assert!(n.peek_tx().is_none());
    assert_eq!(stat(&mut b, Stat::TxErrors), 1);
    // Too many words.
    wr(&mut b, reg::TX_LEN, 14);
    for _ in 0..7 {
        wr(&mut b, reg::TX_DATA, 0);
    }
    assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    wr(&mut b, reg::TX_DATA, 0);
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    wr(&mut b, reg::TX_COMMIT, 0);
    assert_eq!(n.peek_tx().map(|f| f.len()), Some(TX_PAD_TO));
    n.release_tx(true);
    // TX_LEN while open drops the open frame.
    wr(&mut b, reg::TX_LEN, 14);
    wr(&mut b, reg::TX_LEN, 14);
    assert_eq!(stat(&mut b, Stat::TxErrors), 2);
    // Offline: commit drops.
    let mut f = frame(OTHER, 14, 0);
    f[13] = 1;
    wr(&mut b, reg::CTRL, 0);
    send(&mut b, &f);
    assert!(n.peek_tx().is_none());
    assert_eq!(stat(&mut b, Stat::TxErrors), 4); // the open one + this one
}

#[test]
fn tx_queue_full_and_tx_done() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    wr(&mut b, reg::INT_ENABLE, int::TX_DONE);
    assert_eq!(rd(&mut b, reg::STATUS) >> 8 & 0xf, 4);
    for i in 0..TX_SLOTS {
        send(&mut b, &frame(OTHER, 64, i as u32));
    }
    assert_eq!(rd(&mut b, reg::STATUS) >> 8 & 0xf, 0);
    wr(&mut b, reg::TX_LEN, 64); // no free slot
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    assert!(!sh_irq(&b));
    n.release_tx(true);
    assert_eq!(rd(&mut b, reg::STATUS) >> 8 & 0xf, 1);
    assert_ne!(rd(&mut b, reg::INT) & int::TX_DONE, 0);
    assert!(sh_irq(&b));
    wr(&mut b, reg::INT, int::TX_DONE);
    assert_eq!(rd(&mut b, reg::INT) & int::TX_DONE, 0);
    assert!(!sh_irq(&b));
    // Order is kept.
    for i in 1..TX_SLOTS {
        assert_eq!(n.peek_tx().unwrap()[6..], frame(OTHER, 64, i as u32)[6..]);
        n.release_tx(true);
    }
}

fn sh_irq(b: &BusPort) -> bool {
    b.shared().irq_pending()
}

#[test]
fn rx_in_order_then_empty() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    let f1 = frame(MAC, 61, 1);
    let f2 = frame([0xff; 6], 1514, 2);
    assert_eq!(n.push_rx(&f1), RxResult::Stored);
    assert_eq!(n.push_rx(&f2), RxResult::Stored);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xff, 2);
    assert_eq!(receive(&mut b).unwrap(), f1);
    assert_eq!(receive(&mut b).unwrap(), f2);
    assert_eq!(rd(&mut b, reg::RX_LEN), 0);
    assert_eq!(rd(&mut b, reg::RX_DATA), 0);
    wr(&mut b, reg::RX_DONE, 0); // harmless when empty
    assert_eq!(stat(&mut b, Stat::RxOk), 2);
}

#[test]
fn rx_done_early_and_reads_past_end() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    let f1 = frame(MAC, 100, 1);
    let f2 = frame(MAC, 15, 2);
    n.push_rx(&f1);
    n.push_rx(&f2);
    rd(&mut b, reg::RX_DATA);
    wr(&mut b, reg::RX_DONE, 0); // skip the rest of f1
    assert_eq!(rd(&mut b, reg::RX_LEN), 15);
    for _ in 0..8 {
        rd(&mut b, reg::RX_DATA);
    }
    assert_eq!(rd(&mut b, reg::RX_DATA), 0, "past the end");
    wr(&mut b, reg::RX_DONE, 0);
    assert_eq!(rd(&mut b, reg::RX_LEN), 0);
}

#[test]
fn rx_overrun_drops_newest() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    wr(&mut b, reg::INT_ENABLE, int::RX_OVERRUN);
    for i in 0..RX_SLOTS as u32 {
        assert_eq!(n.push_rx(&frame(MAC, 64, i)), RxResult::Stored);
    }
    assert!(!sh_irq(&b));
    assert_eq!(n.push_rx(&frame(MAC, 64, 99)), RxResult::Overrun);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xff, 16);
    assert_ne!(rd(&mut b, reg::INT) & int::RX_OVERRUN, 0);
    assert!(sh_irq(&b));
    assert_eq!(stat(&mut b, Stat::RxOverrun), 1);
    for i in 0..RX_SLOTS as u32 {
        assert_eq!(receive(&mut b).unwrap(), frame(MAC, 64, i), "frame {i}");
    }
    assert!(receive(&mut b).is_none());
    wr(&mut b, reg::INT, int::RX_OVERRUN);
    assert_eq!(rd(&mut b, reg::INT), 0);
}

#[test]
fn rx_filter_and_offline() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    let mc = [0x01, 0x00, 0x5e, 0x00, 0x00, 0xfb]; // mDNS
    assert_eq!(n.push_rx(&frame(MAC, 64, 0)), RxResult::Offline);
    online(&mut b);
    assert_eq!(n.push_rx(&frame(MAC, 64, 0)), RxResult::Stored);
    assert_eq!(n.push_rx(&frame([0xff; 6], 64, 0)), RxResult::Stored);
    assert_eq!(n.push_rx(&frame(OTHER, 64, 0)), RxResult::Filtered);
    assert_eq!(n.push_rx(&frame(mc, 64, 0)), RxResult::Filtered);
    assert!(!n.needs_all_frames());
    // Multicast entry 2.
    wr(&mut b, reg::MCAST + 12, 0x0100);
    wr(&mut b, reg::MCAST + 14, 0x5e00);
    wr(&mut b, reg::MCAST + 16, 0x00fb);
    assert_eq!(n.push_rx(&frame(mc, 64, 0)), RxResult::Filtered, "entry not valid yet");
    wr(&mut b, reg::MCAST_VALID, 0xffff);
    assert_eq!(rd(&mut b, reg::MCAST_VALID), 0xf);
    wr(&mut b, reg::MCAST_VALID, 1 << 2);
    assert!(n.needs_all_frames());
    assert_eq!(n.push_rx(&frame(mc, 64, 0)), RxResult::Stored);
    let mc2 = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
    assert_eq!(n.push_rx(&frame(mc2, 64, 0)), RxResult::Filtered);
    wr(&mut b, reg::CTRL, ctrl::ONLINE | ctrl::MULTICAST_ALL);
    assert_eq!(n.push_rx(&frame(mc2, 64, 0)), RxResult::Stored);
    assert_eq!(n.push_rx(&frame(OTHER, 64, 0)), RxResult::Filtered);
    wr(&mut b, reg::CTRL, ctrl::ONLINE | ctrl::PROMISC);
    assert_eq!(n.push_rx(&frame(OTHER, 64, 0)), RxResult::Stored);
    assert_eq!(n.push_rx(&frame(OTHER, 13, 0)), RxResult::BadLength);
    assert_eq!(n.push_rx(&frame(OTHER, 1515, 0)), RxResult::BadLength);
    assert_eq!(stat(&mut b, Stat::RxDropped), 2);
    assert_eq!(stat(&mut b, Stat::RxOk), 5);
}

#[test]
fn interrupts_mask_level_and_w1c() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    assert_eq!(rd(&mut b, reg::INT), 0);
    n.push_rx(&frame(MAC, 64, 0));
    assert_eq!(rd(&mut b, reg::INT), int::RX_AVAIL);
    assert!(!sh_irq(&b), "masked");
    wr(&mut b, reg::INT_ENABLE, 0xffff);
    assert_eq!(rd(&mut b, reg::INT_ENABLE), int::ALL);
    assert!(sh_irq(&b));
    wr(&mut b, reg::INT, int::RX_AVAIL); // level: no effect
    assert_eq!(rd(&mut b, reg::INT), int::RX_AVAIL);
    // Driver sequence: mask RX, drain, unmask.
    wr(&mut b, reg::INT_ENABLE, int::ALL & !int::RX_AVAIL);
    assert!(!sh_irq(&b));
    receive(&mut b).unwrap();
    n.push_rx(&frame(MAC, 64, 1)); // arrives before unmask
    wr(&mut b, reg::INT_ENABLE, int::ALL);
    assert!(sh_irq(&b), "frame that arrived meanwhile raises /INT again");
    receive(&mut b).unwrap();
    assert!(!sh_irq(&b));
    // Link change latches.
    n.set_link(true, true, true);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xe000, 0xe000);
    assert_eq!(rd(&mut b, reg::INT), int::LINK_CHANGE);
    n.set_link(true, false, true); // speed only: no new event
    wr(&mut b, reg::INT, int::LINK_CHANGE);
    assert_eq!(rd(&mut b, reg::INT), 0);
    n.set_link(false, false, false);
    assert_eq!(rd(&mut b, reg::INT), int::LINK_CHANGE);
    assert_eq!(rd(&mut b, reg::STATUS) & 0xe000, 0);
}

#[test]
fn byte_access_is_an_error_without_side_effects() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    n.push_rx(&frame(MAC, 64, 0));
    let w0 = b.read(reg::RX_DATA, false, true); // odd byte (/UDS high)
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    wr(&mut b, reg::INT, int::BUS_ERROR);
    assert_eq!(rd(&mut b, reg::RX_DATA), w0, "byte read did not advance");
    b.write(reg::CTRL, 0, false, true);
    assert_eq!(rd(&mut b, reg::CTRL), ctrl::ONLINE, "byte write ignored");
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
    // /LDS has no effect: /UDS low is a word access, /UDS high a byte one.
    wr(&mut b, reg::INT, int::BUS_ERROR);
    b.read(reg::RX_DATA, true, false);
    assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0, "even byte: a word read");
    b.write(reg::CTRL, 0, false, false);
    assert_eq!(rd(&mut b, reg::CTRL), ctrl::ONLINE, "/UDS high: ignored");
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
}

#[test]
fn stats_snapshot_and_clear() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    for _ in 0..3 {
        n.push_rx(&frame(MAC, 64, 0));
    }
    let r = reg::STATS + 4 * Stat::RxOk as u8;
    assert_eq!(rd(&mut b, r), 0);
    n.push_rx(&frame(MAC, 64, 0)); // between the two halves
    assert_eq!(rd(&mut b, r + 2), 3, "low half from the snapshot");
    assert_eq!(stat(&mut b, Stat::RxOk), 4);
    // Wrap across the half-word boundary (too slow under Miri).
    if cfg!(miri) {
        return;
    }
    for _ in 0..0x1_0000 - 4 {
        n.push_rx(&frame(MAC, 64, 0));
        receive(&mut b);
    }
    assert_eq!(stat(&mut b, Stat::RxOk), 0x1_0000);
    wr(&mut b, reg::CTRL, ctrl::ONLINE | ctrl::CLEAR_STATS);
    assert_eq!(rd(&mut b, reg::CTRL), ctrl::ONLINE);
    assert_eq!(stat(&mut b, Stat::RxOk), 0);
    n.push_rx(&frame(MAC, 64, 0));
    assert_eq!(stat(&mut b, Stat::RxOk), 1);
}

#[test]
fn reset_fifos_and_bus_reset() {
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    wr(&mut b, reg::INT_ENABLE, int::ALL);
    wr(&mut b, reg::MCAST_VALID, 1);
    n.push_rx(&frame(MAC, 64, 0));
    send(&mut b, &frame(OTHER, 64, 0));
    send(&mut b, &frame(OTHER, 64, 1));
    wr(&mut b, reg::CTRL, ctrl::ONLINE | ctrl::RESET_FIFOS);
    assert_eq!(rd(&mut b, reg::RX_LEN), 0);
    assert!(n.peek_tx().is_none(), "committed TX frames dropped");
    assert_eq!(rd(&mut b, reg::STATUS) >> 8 & 0xf, 4);
    assert_eq!(rd(&mut b, reg::CTRL), ctrl::ONLINE);
    // Queues work after a flush.
    send(&mut b, &frame(OTHER, 64, 2));
    assert_eq!(n.peek_tx().unwrap()[6..], frame(OTHER, 64, 2)[6..]);
    n.release_tx(true);
    // Bus reset: everything but STATS.
    n.push_rx(&frame(MAC, 64, 0));
    n.set_link(true, true, true);
    wr(&mut b, reg::TX_LEN, 14);
    b.reset();
    assert_eq!(rd(&mut b, reg::RX_LEN), 0);
    assert_eq!(rd(&mut b, reg::CTRL), 0);
    assert_eq!(rd(&mut b, reg::INT_ENABLE), 0);
    assert_eq!(rd(&mut b, reg::MCAST_VALID), 0);
    assert_eq!(rd(&mut b, reg::INT), 0, "latched bits acknowledged");
    assert_eq!(stat(&mut b, Stat::TxOk), 1);
    wr(&mut b, reg::TX_DATA, 0);
    assert_ne!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0, "open TX frame aborted");
}

#[test]
fn window_routes_and_keeps_v0() {
    let mut sh = shared();
    let (b, _n) = sh.split();
    let mut w = Window::new(b);
    // Unconfigured: Autoconfig nibbles, no NIC access.
    assert_eq!(w.read(reg::CTRL, true, true) & 0x0fff, 0x0fff);
    assert_eq!(w.write(AC_BASE_HI + 2, 0x9000, true, false), Event::None);
    assert_eq!(w.write(AC_BASE_HI, 0xe900, true, false), Event::Configured);
    assert_eq!(w.slave.state, State::Configured);
    assert_eq!(w.read(slave::reg::MAGIC, true, true), slave::MAGIC);
    assert_eq!(w.read(slave::reg::VERSION, true, true), 0x0003);
    w.write(slave::reg::SCRATCH, 0x0012, false, true); // odd byte write still fine
    assert_eq!(w.read(slave::reg::SCRATCH, true, true), 0x0012);
    assert_eq!(w.read(reg::INT, true, true), 0);
    let mac = [w.read(reg::MAC0, true, true), w.read(reg::MAC1, true, true), w.read(reg::MAC2, true, true)];
    assert_eq!(mac, [0x525a, 0x58d1, 0xb4cd]);
    assert_eq!(w.read(0x1c, true, true), 0xffff, "unused");
    assert_eq!(w.read(0x80, true, true), 0xffff, "above the NIC block");
    assert_eq!(w.read(reg::TX_LEN, true, true), 0xffff, "write-only");
    // /INT only while configured.
    w.write(reg::INT_ENABLE, int::ALL, true, true);
    w.write(reg::INT, 0, false, true); // odd byte write: BUS_ERROR
    assert!(w.irq_asserted());
    w.reset();
    assert!(!w.irq_asserted());
    assert_eq!(w.slave.state, State::Unconfigured);
}

/// Two threads, one per side, both directions at once, byte-exact.
#[test]
fn two_threads_spsc_stress() {
    const N: u32 = if cfg!(miri) { 300 } else { 100_000 };
    let mut sh = shared();
    let (mut b, mut n) = sh.split();
    online(&mut b);
    let len_of = |i: u32| 14 + (i as usize * 7919) % (FRAME_MAX - 13);
    std::thread::scope(|sc| {
        sc.spawn(move || {
            // NIC side: push RX frames, consume TX frames.
            let (mut sent, mut got) = (0, 0);
            while sent < N || got < N {
                if sent < N && n.rx_free() > 0 {
                    assert_eq!(n.push_rx(&frame(MAC, len_of(sent), sent)), RxResult::Stored);
                    sent += 1;
                }
                if let Some(f) = n.peek_tx() {
                    let want = frame(OTHER, len_of(got), got ^ 0x5555);
                    assert_eq!(&f[..want.len()], &want[..], "tx frame {got}");
                    n.release_tx(true);
                    got += 1;
                }
            }
        });
        sc.spawn(move || {
            // Bus side: drain RX, send TX, only through registers.
            let (mut sent, mut got) = (0, 0);
            while sent < N || got < N {
                if let Some(f) = receive(&mut b) {
                    assert_eq!(f, frame(MAC, len_of(got), got), "rx frame {got}");
                    got += 1;
                }
                if sent < N && rd(&mut b, reg::STATUS) >> 8 & 0xf > 0 {
                    send(&mut b, &frame(OTHER, len_of(sent), sent ^ 0x5555));
                    sent += 1;
                }
            }
            assert_eq!(rd(&mut b, reg::INT) & int::BUS_ERROR, 0);
            assert_eq!(stat(&mut b, Stat::RxOverrun), 0);
        });
    });
}
