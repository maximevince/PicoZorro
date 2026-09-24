//! The whole board as the bus-cycle loop sees it: the Autoconfig slave and
//! the v0 registers (`slave`), with the network registers (`nic`,
//! `docs/REGISTERS.md`) routed to the bus half of the NIC model once the
//! board is configured, and the USB window (`usb`, `docs/REGISTERS-USB.md`,
//! A16 = 1) to the bus half of the USB model when the build has one; its
//! $C0-$FE go to the firmware update registers (`update`,
//! `docs/UPDATE.md`) and its $60-$9E to the MPEG decoder (`mpeg`,
//! `docs/REGISTERS-MPEG.md`) when the build has those. With a boot image
//! (`boot`), the A16 = 0 window serves the boot ROM after
//! configuration (ROM mode) and its $80-$8A are the boot stream.

use crate::nic::BusPort;
use crate::slave::{Event, Slave, State};
use crate::{boot, mpeg, nic, slave, update, usb};

pub struct Window<'a> {
    pub slave: Slave,
    pub nic: BusPort<'a>,
    pub usb: Option<usb::BusPort<'a>>,
    pub update: Option<update::BusPort<'a>>,
    pub mpeg: Option<mpeg::BusPort<'a>>,
    pub boot: Option<boot::BusPort<'a>>,
}

impl<'a> Window<'a> {
    pub fn new(nic: BusPort<'a>) -> Self {
        Window { slave: Slave::new(), nic, usb: None, update: None, mpeg: None, boot: None }
    }

    pub fn with_usb(nic: BusPort<'a>, usb: usb::BusPort<'a>) -> Self {
        Window { slave: Slave::new(), nic, usb: Some(usb), update: None, mpeg: None, boot: None }
    }

    /// Add the firmware update registers ($C0-$FE at A16 = 1).
    pub fn with_update(mut self, u: update::BusPort<'a>) -> Self {
        self.update = Some(u);
        self
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn routes_to_nic(&self, reg: u8) -> bool {
        self.slave.state == State::Configured && BusPort::is_nic_reg(reg)
    }

    /// Add the boot ROM and boot stream (A16 = 0: ROM mode, $80-$8A). The
    /// Autoconfig ROM offers the boot ROM if the image has one.
    pub fn with_boot(mut self, b: boot::BusPort<'a>) -> Self {
        self.slave.diag = b.shared().has_rom();
        self.boot = Some(b);
        self
    }

    /// Add the MPEG decoder registers ($60-$9E at A16 = 1).
    pub fn with_mpeg(mut self, m: mpeg::BusPort<'a>) -> Self {
        self.mpeg = Some(m);
        self
    }

    /// Add the USB window ($00-$4A at A16 = 1).
    pub fn with_usb_window(mut self, u: usb::BusPort<'a>) -> Self {
        self.usb = Some(u);
        self
    }

    /// Read cycle at even byte offset `reg`; `uds` / `lds`: strobe asserted.
    /// Only /UDS counts (low = word access): /LDS is not wired on every
    /// board, see `slave::Slave::write`.
    /// Always inlined: the bus loop runs from RAM (`.data.zbus`) and must not
    /// call into flash on its configured path (docs/UPDATE.md).
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8, uds: bool, lds: bool) -> u16 {
        if let (State::Configured, Some(b)) = (self.slave.state, self.boot.as_mut()) {
            if b.rom_mode() {
                self.slave.cycles = self.slave.cycles.wrapping_add(1);
                return b.rom_read(reg);
            }
            if boot::BusPort::is_boot_reg(reg) {
                self.slave.cycles = self.slave.cycles.wrapping_add(1);
                return b.read(reg, uds, lds);
            }
        }
        if self.routes_to_nic(reg) {
            self.slave.cycles = self.slave.cycles.wrapping_add(1);
            return self.nic.read(reg, uds, lds);
        }
        self.slave.read(reg)
    }

    /// Write cycle. Always inlined, as `read`.
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write(&mut self, reg: u8, data: u16, uds: bool, lds: bool) -> Event {
        if let (State::Configured, Some(b)) = (self.slave.state, self.boot.as_mut()) {
            b.any_write();
            if boot::BusPort::is_boot_reg(reg) {
                self.slave.cycles = self.slave.cycles.wrapping_add(1);
                b.write(reg, data, uds, lds);
                return Event::None;
            }
        }
        if self.routes_to_nic(reg) {
            self.slave.cycles = self.slave.cycles.wrapping_add(1);
            self.nic.write(reg, data, uds, lds);
            return Event::None;
        }
        let ev = self.slave.write(reg, data, uds, lds);
        if ev == Event::ShutUp {
            self.reset_models();
        }
        if ev == Event::Configured {
            if let Some(b) = self.boot.as_mut() {
                b.configured();
            }
        }
        ev
    }

    /// The bus loop's fast path: a word read at A16 = 0 of a hot register
    /// (MAGIC, VERSION, SCRATCH, RX_DATA and its alias $C0-$FE) while
    /// configured and not serving
    /// the boot ROM. `None`: take [`Window::read_at`]. Same result and side
    /// effects as `read_at(false, reg, true, true)`; it skips the generic
    /// dispatch, ~3/4 of a read's time on core 1 (measured).
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read_fast(&mut self, reg: u8) -> Option<u16> {
        if !self.fast_ok() {
            return None;
        }
        let v = match reg {
            slave::reg::MAGIC => slave::MAGIC,
            slave::reg::VERSION => slave::VERSION,
            slave::reg::SCRATCH => self.slave.scratch,
            nic::reg::RX_DATA | nic::reg::DATA_ALIAS..=nic::reg::DATA_ALIAS_LAST => self.nic.rx_data(),
            _ => return None,
        };
        self.slave.cycles = self.slave.cycles.wrapping_add(1);
        Some(v)
    }

    /// The fast path applies now (configured, not serving the boot ROM).
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn fast_ok(&self) -> bool {
        self.slave.state == State::Configured && !self.boot.as_ref().is_some_and(|b| b.rom_mode())
    }

    /// `reg` is one [`Window::write_fast`] takes. Such a write changes no
    /// state the next bus cycle depends on, so the bus loop may finish the
    /// cycle before doing it.
    #[inline(always)]
    pub fn is_fast_write_reg(reg: u8) -> bool {
        reg == slave::reg::SCRATCH || reg == nic::reg::TX_DATA || reg >= nic::reg::DATA_ALIAS
    }

    /// As [`Window::read_fast`] for a word write (SCRATCH, TX_DATA and its
    /// alias $C0-$FE): `false`
    /// = take [`Window::write_at`]; else the same effect as `write_at(false,
    /// reg, data, true, true)`, which returns `Event::None` for these.
    #[inline(always)]
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write_fast(&mut self, reg: u8, data: u16) -> bool {
        if !self.fast_ok() {
            return false;
        }
        match reg {
            slave::reg::SCRATCH => self.slave.scratch = data,
            nic::reg::TX_DATA | nic::reg::DATA_ALIAS..=nic::reg::DATA_ALIAS_LAST => self.nic.tx_data(data),
            _ => return false,
        }
        self.slave.cycles = self.slave.cycles.wrapping_add(1);
        true
    }

    /// Read cycle with A16: 0 = the window above, 1 = the USB window (only
    /// once configured; unconfigured, A16 is not decoded and the Autoconfig
    /// ROM answers; without a USB model the upper half reads $FFFF).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read_at(&mut self, a16: bool, reg: u8, uds: bool, lds: bool) -> u16 {
        if !a16 || self.slave.state != State::Configured {
            return self.read(reg, uds, lds);
        }
        self.slave.cycles = self.slave.cycles.wrapping_add(1);
        if update::BusPort::is_update_reg(reg) {
            return match self.update.as_mut() {
                Some(u) => u.read(reg, uds, lds),
                None => 0xffff,
            };
        }
        if let (true, Some(m)) = (mpeg::BusPort::is_mpeg_reg(reg), self.mpeg.as_mut()) {
            return m.read(reg, uds, lds);
        }
        match self.usb.as_mut() {
            Some(u) => u.read(reg, uds, lds),
            None => 0xffff,
        }
    }

    /// Write cycle with A16.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write_at(&mut self, a16: bool, reg: u8, data: u16, uds: bool, lds: bool) -> Event {
        if !a16 || self.slave.state != State::Configured {
            return self.write(reg, data, uds, lds);
        }
        self.slave.cycles = self.slave.cycles.wrapping_add(1);
        if let Some(b) = self.boot.as_mut() {
            b.any_write();
        }
        if update::BusPort::is_update_reg(reg) {
            if let Some(u) = self.update.as_mut() {
                u.write(reg, data, uds, lds);
            }
        } else if let (true, Some(m)) = (mpeg::BusPort::is_mpeg_reg(reg), self.mpeg.as_mut()) {
            m.write(reg, data, uds, lds);
        } else if let Some(u) = self.usb.as_mut() {
            u.write(reg, data, uds, lds);
        }
        Event::None
    }

    /// Amiga reset (/BUSRST).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        self.slave.reset();
        if let Some(b) = self.boot.as_ref() {
            // The boot ROM switch applies from here (update BOOTROM_ON/OFF).
            self.slave.diag = b.shared().has_rom();
        }
        self.reset_models();
    }

    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn reset_models(&mut self) {
        self.nic.reset();
        if let Some(u) = self.usb.as_mut() {
            u.reset();
        }
        if let Some(u) = self.update.as_mut() {
            u.reset();
        }
        if let Some(m) = self.mpeg.as_mut() {
            m.reset();
        }
        if let Some(b) = self.boot.as_mut() {
            b.reset();
        }
    }

    /// Drive /INT: configured and an enabled interrupt bit is set in either
    /// window.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn irq_asserted(&self) -> bool {
        self.slave.state == State::Configured
            && (self.nic.shared().irq_pending() || self.usb.as_ref().is_some_and(|u| u.shared().irq_pending()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nic::{reg, Shared};

    /// The fast path against the full one, on two windows driven alike.
    #[test]
    fn fast_path_matches_full_path() {
        let mut sa = Box::new(Shared::new([2, 0, 0, 0, 0, 1]));
        let mut sb = Box::new(Shared::new([2, 0, 0, 0, 0, 1]));
        let (na, mut pa) = sa.split();
        let (nb, mut pb) = sb.split();
        let mut a = Window::new(na);
        let mut b = Window::new(nb);
        // Unconfigured: the fast path declines.
        assert_eq!(a.read_fast(slave::reg::MAGIC), None);
        assert!(!a.write_fast(slave::reg::SCRATCH, 1));
        for w in [&mut a, &mut b] {
            w.slave.state = State::Configured;
            w.write(reg::CTRL, crate::nic::ctrl::ONLINE, true, true);
        }
        let rx: Vec<u8> = (0..60u8).collect();
        let mut rxf = rx.clone();
        rxf[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        pa.push_rx(&rxf);
        pb.push_rx(&rxf);
        for &r in &[slave::reg::MAGIC, slave::reg::VERSION, slave::reg::SCRATCH] {
            assert_eq!(a.read_fast(r), Some(b.read_at(false, r, true, true)));
        }
        assert!(a.write_fast(slave::reg::SCRATCH, 0xbeef));
        b.write_at(false, slave::reg::SCRATCH, 0xbeef, true, true);
        assert_eq!(a.read_fast(slave::reg::SCRATCH), Some(0xbeef));
        assert_eq!(b.read_at(false, slave::reg::SCRATCH, true, true), 0xbeef);
        // RX: RX_LEN on the full path, the data on both.
        assert_eq!(a.read_at(false, reg::RX_LEN, true, true), b.read_at(false, reg::RX_LEN, true, true));
        for _ in 0..30 {
            assert_eq!(a.read_fast(reg::RX_DATA), Some(b.read_at(false, reg::RX_DATA, true, true)));
        }
        // TX: a frame through the fast path's TX_DATA, the other through the full one.
        for w in [&mut a, &mut b] {
            w.write_at(false, reg::TX_LEN, 60, true, true);
        }
        for i in 0..30u16 {
            assert!(a.write_fast(reg::TX_DATA, i * 0x0101));
            b.write_at(false, reg::TX_DATA, i * 0x0101, true, true);
        }
        for w in [&mut a, &mut b] {
            w.write_at(false, reg::TX_COMMIT, 0, true, true);
        }
        assert_eq!(pa.peek_tx(), pb.peek_tx());
        assert!(pa.peek_tx().is_some());
        // The data alias $C0-$FE: a frame written as longs (two words per
        // long, ascending addresses) and read back the same way, on both paths.
        for w in [&mut a, &mut b] {
            w.write_at(false, reg::TX_LEN, 64, true, true);
        }
        for i in 0..32u16 {
            let r = reg::DATA_ALIAS + ((2 * i) as u8 & 0x3f);
            assert!(a.write_fast(r, i ^ 0xa5a5));
            b.write_at(false, r, i ^ 0xa5a5, true, true);
        }
        for w in [&mut a, &mut b] {
            w.write_at(false, reg::TX_COMMIT, 0, true, true);
        }
        pa.release_tx(true);
        pb.release_tx(true);
        let fa = pa.peek_tx().map(|f| f.to_vec());
        assert_eq!(fa, pb.peek_tx().map(|f| f.to_vec()));
        assert_eq!(&fa.unwrap()[..4], &[0xa5, 0xa5, 0xa5, 0xa4]);
        let mut rxf2: Vec<u8> = (100..164u8).collect();
        rxf2[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        for (w, p) in [(&mut a, &mut pa), (&mut b, &mut pb)] {
            w.write_at(false, reg::RX_DONE, 0, true, true);
            p.push_rx(&rxf2);
            assert_eq!(w.read_at(false, reg::RX_LEN, true, true), 64);
        }
        for i in 0..32usize {
            let r = reg::DATA_ALIAS + (2 * i) as u8;
            let want = u16::from_be_bytes([rxf2[2 * i], rxf2[2 * i + 1]]);
            assert_eq!(a.read_fast(r), Some(want));
            assert_eq!(b.read_at(false, r, true, true), want);
        }
        // Not a hot register: declined.
        assert_eq!(a.read_fast(reg::RX_LEN), None);
        assert!(!a.write_fast(reg::TX_LEN, 60));
        assert_eq!(a.slave.cycles, b.slave.cycles);
    }
}
