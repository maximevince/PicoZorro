//! Bench host (`--features bench-host`, `#[path = "../bench_host.rs"]`): with
//! no Amiga, core 1 plays the Amiga driver instead of running the bus loop.
//! The bus pins stay untouched (inputs), so this build is not for a card in
//! an Amiga. It uses only the register calls the real driver makes (RX_LEN /
//! RX_DATA / RX_DONE, TX_LEN / TX_DATA / TX_COMMIT, CTRL, STATS), from the
//! other core, as the bus loop does:
//! - counts frames by EtherType and source MAC;
//! - answers ARP for 10.42.0.2 (so the PC can talk to it without raw
//!   sockets);
//! - takes text commands on UDP port 9999 and answers there;
//! - counts UDP frames to port 9998 (broadcast test) and 9996 (multicast test);
//! - in reflect mode sends every other unicast frame to its MAC back with the
//!   MAC addresses swapped; IPv4/UDP frames also get IP addresses and ports
//!   swapped, which leaves both checksums valid, so the PC's kernel checks
//!   every reflected byte and hands the payload to an ordinary socket.

use core::fmt::Write as _;
use core::sync::atomic::Ordering::Relaxed;

use pz_core::nic::{ctrl, reg, BusPort, Stat};

pub const MY_IP: [u8; 4] = [10, 42, 0, 2];
const PORT_CMD: u16 = 9999;
const PORT_BCAST: u16 = 9998;
const PORT_MCAST: u16 = 9996;

#[derive(Default)]
struct Counts {
    frames: u32,
    arp_replies: u32,
    cmds: u32,
    bcast_probe: u32,
    mcast_probe: u32,
    reflected: u32,
    tx_waits: u32,
    ethertype: [(u16, u32); 8],
    src: [([u8; 6], u32); 8],
}

impl Counts {
    fn count(&mut self, f: &[u8]) {
        self.frames += 1;
        let et = u16::from_be_bytes([f[12], f[13]]);
        if let Some(e) = self.ethertype.iter_mut().find(|e| e.1 == 0 || e.0 == et) {
            e.0 = et;
            e.1 += 1;
        }
        let s: [u8; 6] = f[6..12].try_into().unwrap();
        if let Some(e) = self.src.iter_mut().find(|e| e.1 == 0 || e.0 == s) {
            e.0 = s;
            e.1 += 1;
        }
    }
}

struct FakeHost {
    b: BusPort<'static>,
    mac: [u8; 6],
    reflect: bool,
    c: Counts,
}

/// Core 1 entry: play the Amiga driver forever.
pub fn run(b: BusPort<'static>, mac: [u8; 6]) -> ! {
    let mut h = FakeHost { b, mac, reflect: false, c: Counts::default() };
    h.wr(reg::CTRL, ctrl::ONLINE);
    let mut buf = [0u8; 1536];
    loop {
        let len = h.rd(reg::RX_LEN) as usize;
        if len == 0 {
            continue;
        }
        for k in 0..len.div_ceil(2) {
            let w = h.rd(reg::RX_DATA);
            buf[2 * k] = (w >> 8) as u8;
            buf[2 * k + 1] = w as u8;
        }
        h.wr(reg::RX_DONE, 0);
        h.handle(&mut buf[..len]);
    }
}

impl FakeHost {
    fn rd(&mut self, r: u8) -> u16 {
        self.b.read(r, true, true)
    }

    fn wr(&mut self, r: u8, v: u16) {
        self.b.write(r, v, true, true)
    }

    fn stat(&mut self, s: Stat) -> u32 {
        let r = reg::STATS + 4 * s as u8;
        let hi = self.rd(r);
        u32::from(hi) << 16 | u32::from(self.rd(r + 2))
    }

    /// What the driver's CMD_WRITE does: wait for a slot, then the TX_* sequence.
    fn send(&mut self, f: &[u8]) {
        if (self.rd(reg::STATUS) >> 8) & 0xf == 0 {
            self.c.tx_waits += 1;
            while (self.rd(reg::STATUS) >> 8) & 0xf == 0 {}
        }
        self.wr(reg::TX_LEN, f.len() as u16);
        for c in f.chunks(2) {
            self.wr(reg::TX_DATA, u16::from(c[0]) << 8 | u16::from(*c.get(1).unwrap_or(&0)));
        }
        self.wr(reg::TX_COMMIT, 0);
    }

    fn handle(&mut self, f: &mut [u8]) {
        self.c.count(f);
        let et = u16::from_be_bytes([f[12], f[13]]);
        let to_me = f[..6] == self.mac;

        // ARP request for our address: reply.
        if et == 0x0806 && f.len() >= 42 && f[20..22] == [0, 1] && f[38..42] == MY_IP {
            let mut r = [0u8; 42];
            r[..6].copy_from_slice(&f[6..12]);
            r[6..12].copy_from_slice(&self.mac);
            r[12..14].copy_from_slice(&[0x08, 0x06]);
            r[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
            r[20..22].copy_from_slice(&[0, 2]);
            r[22..28].copy_from_slice(&self.mac);
            r[28..32].copy_from_slice(&MY_IP);
            r[32..42].copy_from_slice(&f[22..32]); // sender MAC + IP
            self.send(&r);
            self.c.arp_replies += 1;
            return;
        }

        let udp = udp_ports(f);
        if let Some((_, dport)) = udp {
            if dport == PORT_BCAST {
                self.c.bcast_probe += 1;
                return;
            }
            if dport == PORT_MCAST {
                self.c.mcast_probe += 1;
                return;
            }
            if dport == PORT_CMD && to_me && f[30..34] == MY_IP {
                self.c.cmds += 1;
                self.command(f);
                return;
            }
        }

        if self.reflect && to_me {
            let (d, s) = f.split_at_mut(6);
            d.swap_with_slice(&mut s[..6]);
            if udp.is_some() {
                let (a, b) = f[26..34].split_at_mut(4); // IPv4 src, dst
                a.swap_with_slice(b);
                let ihl = usize::from(f[14] & 0x0f) * 4;
                let (a, b) = f[14 + ihl..14 + ihl + 4].split_at_mut(2); // UDP ports
                a.swap_with_slice(b);
            }
            self.send(f);
            self.c.reflected += 1;
        }
    }

    fn command(&mut self, f: &[u8]) {
        let ihl = usize::from(f[14] & 0x0f) * 4;
        let p0 = 14 + ihl + 8;
        let ulen = usize::from(u16::from_be_bytes([f[14 + ihl + 4], f[14 + ihl + 5]]));
        let end = (14 + ihl + ulen).min(f.len());
        let mut cmd = [0u8; 64];
        let n = (end.saturating_sub(p0)).min(cmd.len());
        cmd[..n].copy_from_slice(&f[p0..p0 + n]);
        let cmd = core::str::from_utf8(&cmd[..n]).unwrap_or("").trim();

        let mut out = TextBuf::new();
        match cmd {
            "stats" => self.report(&mut out),
            "clear" => {
                self.c = Counts::default();
                let c = self.rd(reg::CTRL);
                self.wr(reg::CTRL, c | ctrl::CLEAR_STATS);
                let _ = write!(out, "ok");
            }
            "reflect on" | "reflect off" => {
                self.reflect = cmd.ends_with("on");
                let _ = write!(out, "ok reflect {}", self.reflect);
            }
            "mcast all" | "mcast none" | "promisc on" | "promisc off" => {
                let bit = if cmd.starts_with("mcast") { ctrl::MULTICAST_ALL } else { ctrl::PROMISC };
                let c = self.rd(reg::CTRL);
                let on = cmd.ends_with("all") || cmd.ends_with("on");
                self.wr(reg::CTRL, if on { c | bit } else { c & !bit });
                let _ = write!(out, "ok ctrl {:#06x}", self.rd(reg::CTRL));
            }
            "runt" => {
                crate::nic_task::SEND_RUNT.store(true, Relaxed);
                let _ = write!(out, "ok runt");
            }
            _ if cmd.starts_with("mcast add ") || cmd == "mcast clear" => {
                // Entry 0 only; the valid-bit protocol of REGISTERS.md.
                let v = self.rd(reg::MCAST_VALID);
                self.wr(reg::MCAST_VALID, v & !1);
                match parse_mac(cmd.trim_start_matches("mcast add ")) {
                    Some(m) if cmd != "mcast clear" => {
                        for k in 0..3 {
                            self.wr(reg::MCAST + 2 * k as u8, u16::from_be_bytes([m[2 * k], m[2 * k + 1]]));
                        }
                        self.wr(reg::MCAST_VALID, v | 1);
                        let _ = write!(out, "ok mcast entry 0 set");
                    }
                    _ => {
                        let _ = write!(out, "ok mcast entry 0 cleared");
                    }
                }
            }
            _ => {
                let _ = write!(
                    out,
                    "commands: stats clear | reflect on|off | mcast all|none | mcast add xx:xx:xx:xx:xx:xx | mcast clear | promisc on|off | runt"
                );
            }
        }
        self.reply(f, out.as_bytes());
    }

    fn report(&mut self, out: &mut TextBuf) {
        let st = self.rd(reg::STATUS);
        let s = [Stat::RxOk, Stat::TxOk, Stat::RxDropped, Stat::TxErrors, Stat::RxCrc, Stat::RxOverrun]
            .map(|x| self.stat(x));
        let (ctl, int) = (self.rd(reg::CTRL), self.rd(reg::INT));
        let _ = write!(
            out,
            "frames={} bcast_probe={} mcast_probe={} reflected={} arp={} cmds={} tx_waits={} \
             rx_ok={} tx_ok={} rx_dropped={} tx_errors={} rx_crc={} rx_overrun={} \
             status={:#06x} ctrl={:#06x} int={:#06x} int_wakeups={} filtered={}",
            self.c.frames,
            self.c.bcast_probe,
            self.c.mcast_probe,
            self.c.reflected,
            self.c.arp_replies,
            self.c.cmds,
            self.c.tx_waits,
            s[0],
            s[1],
            s[2],
            s[3],
            s[4],
            s[5],
            st,
            ctl,
            int,
            crate::chip::int_wakeups(),
            crate::nic_task::FILTERED.load(Relaxed),
        );
        let _ = write!(out, " ethertype=");
        for (et, n) in self.c.ethertype.iter().filter(|e| e.1 > 0) {
            let _ = write!(out, "{:04x}:{},", et, n);
        }
        let _ = write!(out, " src=");
        for (m, n) in self.c.src.iter().filter(|e| e.1 > 0) {
            let _ = write!(out, "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}:{},", m[0], m[1], m[2], m[3], m[4], m[5], n);
        }
    }

    /// UDP answer to the request in `f` with `payload`.
    fn reply(&mut self, f: &[u8], payload: &[u8]) {
        let ihl = usize::from(f[14] & 0x0f) * 4;
        let sport = u16::from_be_bytes([f[14 + ihl], f[14 + ihl + 1]]);
        let n = payload.len().min(1400);
        let mut r = [0u8; 14 + 20 + 8 + 1400];
        r[..6].copy_from_slice(&f[6..12]);
        r[6..12].copy_from_slice(&self.mac);
        r[12..14].copy_from_slice(&[0x08, 0x00]);
        let ip_len = (20 + 8 + n) as u16;
        r[14] = 0x45;
        r[16..18].copy_from_slice(&ip_len.to_be_bytes());
        r[20] = 0x40; // DF
        r[22] = 64; // TTL
        r[23] = 17; // UDP
        r[26..30].copy_from_slice(&MY_IP);
        r[30..34].copy_from_slice(&f[26..30]);
        let sum = ip_checksum(&r[14..34]);
        r[24..26].copy_from_slice(&sum.to_be_bytes());
        r[34..36].copy_from_slice(&PORT_CMD.to_be_bytes());
        r[36..38].copy_from_slice(&sport.to_be_bytes());
        r[38..40].copy_from_slice(&((8 + n) as u16).to_be_bytes());
        // UDP checksum 0 = not computed (allowed for IPv4).
        r[42..42 + n].copy_from_slice(&payload[..n]);
        self.send(&r[..42 + n]);
    }
}

/// (source port, destination port) of an IPv4 UDP frame.
fn udp_ports(f: &[u8]) -> Option<(u16, u16)> {
    if f.len() < 42 || f[12..14] != [0x08, 0x00] || f[14] >> 4 != 4 || f[23] != 17 {
        return None;
    }
    let ihl = usize::from(f[14] & 0x0f) * 4;
    if ihl < 20 || f.len() < 14 + ihl + 8 {
        return None;
    }
    let u = &f[14 + ihl..];
    Some((u16::from_be_bytes([u[0], u[1]]), u16::from_be_bytes([u[2], u[3]])))
}

fn ip_checksum(h: &[u8]) -> u16 {
    let mut s: u32 = h.chunks(2).map(|c| u32::from(u16::from_be_bytes([c[0], c[1]]))).sum();
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

fn parse_mac(t: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut it = t.trim().split(':');
    for b in m.iter_mut() {
        *b = u8::from_str_radix(it.next()?, 16).ok()?;
    }
    it.next().is_none().then_some(m)
}

struct TextBuf {
    b: [u8; 1400],
    n: usize,
}

impl TextBuf {
    fn new() -> Self {
        TextBuf { b: [0; 1400], n: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.b[..self.n]
    }
}

impl core::fmt::Write for TextBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let k = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}
