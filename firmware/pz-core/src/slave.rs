//! The slave as the Amiga sees it: config state machine plus the register
//! window v0 (16-bit registers, byte offsets from the board base). The
//! network registers of window v2 (`docs/REGISTERS.md`) live in `nic`;
//! `window` routes between the two.

use crate::autoconfig::{self, AC_BASE_HI, AC_BASE_LO, AC_SHUTUP};

/// Register window v0, byte offsets from the board base.
pub mod reg {
    pub const MAGIC: u8 = 0x00; // RO 'PZ'
    pub const VERSION: u8 = 0x02; // RO
    pub const SCRATCH: u8 = 0x04; // RW; /UDS low = word write (see `write`)
    pub const INT: u8 = 0x06; // interrupt status, served by `nic`
    pub const BOOT_US_HI: u8 = 0x08; // RO cold start to "slave armed", us
    pub const BOOT_US_LO: u8 = 0x0a;
    pub const CYCLES_HI: u8 = 0x0c; // RO matched cycles served
    pub const CYCLES_LO: u8 = 0x0e;
    // 0x10-0x7e: network registers, `nic`
}

pub const MAGIC: u16 = 0x505a;
/// Window version: 1 = v0 registers only, 2 = network registers, 3 = plus the
/// data port alias at $C0-$FE (REGISTERS.md).
pub const VERSION: u16 = 0x0003;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Answers at $E8xxxx while /CFGIN is low.
    Unconfigured,
    /// Answers at `base`.
    Configured,
    /// Silent until reset.
    ShutUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    /// Base latched: move the match, assert /CFGOUT.
    Configured,
    /// Go silent, assert /CFGOUT.
    ShutUp,
}

#[derive(Clone, Copy, Debug)]
pub struct Slave {
    pub state: State,
    /// A23-A16 once configured.
    pub base: u8,
    base_lo: u8,
    pub scratch: u16,
    pub boot_us: u32,
    pub cycles: u32,
    /// Offer the boot ROM in the Autoconfig ROM.
    pub diag: bool,
    /// The Autoconfig ROM image (`autoconfig::NIC_ROM` here; another
    /// board's crate passes its own to `with_rom`).
    pub rom: &'static [u8; 16],
}

impl Default for Slave {
    fn default() -> Self {
        Self::new()
    }
}

impl Slave {
    pub const fn new() -> Self {
        Self::with_rom(autoconfig::NIC_ROM)
    }

    /// A slave with another Autoconfig identity.
    pub const fn with_rom(rom: &'static [u8; 16]) -> Self {
        Slave { state: State::Unconfigured, base: 0, base_lo: 0, scratch: 0, boot_us: 0, cycles: 0, diag: false, rom }
    }

    /// Amiga reset (/BUSRST). `boot_us` and `cycles` survive on purpose.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn reset(&mut self) {
        self.state = State::Unconfigured;
        self.base = 0;
        self.base_lo = 0;
        self.scratch = 0;
    }

    /// Read cycle at even byte offset `reg` (A7..A1). Returns the 16 data bits.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn read(&mut self, reg: u8) -> u16 {
        self.cycles = self.cycles.wrapping_add(1);
        if self.state == State::Unconfigured {
            // Only D15-D12 are significant; idle-high on the rest.
            return (u16::from(autoconfig::nibble_rom(self.rom, reg, self.diag)) << 12) | 0x0fff;
        }
        match reg {
            reg::MAGIC => MAGIC,
            reg::VERSION => VERSION,
            reg::SCRATCH => self.scratch,
            reg::BOOT_US_HI => (self.boot_us >> 16) as u16,
            reg::BOOT_US_LO => self.boot_us as u16,
            reg::CYCLES_HI => (self.cycles >> 16) as u16,
            reg::CYCLES_LO => self.cycles as u16,
            _ => 0xffff,
        }
    }

    /// Write cycle. `uds`: /UDS was asserted. `_lds` (/LDS) is ignored on
    /// every board: the One TH does not wire it (GPIO18 is the microSD chip
    /// select there). So /UDS low is a word write, an even-byte write
    /// included (no PicoZorro driver issues one), and /UDS high is the
    /// odd byte: a 68000 write cycle asserts at least one strobe, so /LDS is
    /// implied and needs no pin. SCRATCH takes the low byte then; the other
    /// models reject such a write (`nic`, `usb`, ...).
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    pub fn write(&mut self, reg: u8, data: u16, uds: bool, _lds: bool) -> Event {
        self.cycles = self.cycles.wrapping_add(1);
        if self.state == State::Unconfigured {
            if !uds {
                return Event::None; // config registers live on D15-D8
            }
            return match reg {
                AC_BASE_LO => {
                    self.base_lo = ((data >> 12) & 0x0f) as u8;
                    Event::None
                }
                AC_BASE_HI => {
                    self.base = ((((data >> 12) & 0x0f) as u8) << 4) | self.base_lo;
                    self.state = State::Configured;
                    Event::Configured
                }
                AC_SHUTUP => {
                    self.state = State::ShutUp;
                    Event::ShutUp
                }
                _ => Event::None,
            };
        }
        if reg == reg::SCRATCH {
            self.scratch = if uds { data } else { (self.scratch & 0xff00) | (data & 0x00ff) };
        }
        Event::None
    }
}

/// Plays expansion.library's part as documented in NDK configregs.h and AROS
/// m68k-amiga expansion (ReadExpansionRom, WriteExpansionByte).
#[cfg(test)]
mod tests {
    use super::*;

    /// One logical byte = nibbles on D15-D12 at 4n and 4n+2; all but byte 0 inverted.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn os_read_byte(s: &mut Slave, n: u8) -> u8 {
        let hi = (s.read(4 * n) >> 12) as u8;
        let lo = (s.read(4 * n + 2) >> 12) as u8;
        let v = hi << 4 | lo;
        if n == 0 {
            v
        } else {
            !v
        }
    }

    /// WriteExpansionByte: p[off+2] = byte << 4; p[off] = byte; both are byte
    /// writes to even addresses: /UDS only, data on D15-D8.
    #[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
    fn os_write_byte(s: &mut Slave, off: u8, byte: u8) -> Event {
        let ev = s.write(off + 2, u16::from(byte << 4) << 8, true, false);
        assert_eq!(ev, Event::None);
        s.write(off, u16::from(byte) << 8, true, false)
    }

    #[test]
    fn rom_is_stable_and_correct() {
        let mut s = Slave::new();
        let rom: Vec<u8> = (0..16).map(|n| os_read_byte(&mut s, n)).collect();
        // Kickstart reads the ROM 12 times and wants identical results.
        for _ in 0..11 {
            let again: Vec<u8> = (0..16).map(|n| os_read_byte(&mut s, n)).collect();
            assert_eq!(again, rom);
        }
        assert_eq!(rom[0], 0xc2); // er_Type: ZII, 128 KB
        assert_eq!(rom[1], autoconfig::PRODUCT);
        assert_eq!(rom[2] & 0x40, 0); // ERFF_NOSHUTUP clear
        assert_eq!(rom[3], 0); // er_Reserved03
        assert_eq!(u16::from(rom[4]) << 8 | u16::from(rom[5]), 2011);
        assert!(rom[10..16].iter().all(|&b| b == 0));
        // Physical view: unused registers read $F, er_Type is not inverted.
        assert_eq!((autoconfig::nibble(0x00, false), autoconfig::nibble(0x02, false)), (0xc, 0x2));
        assert_eq!((autoconfig::nibble(0x3c, false), autoconfig::nibble(0x40, false)), (0xf, 0xf));
    }

    #[test]
    fn configure_and_register_window() {
        let mut s = Slave::new();
        // A write without /UDS must not configure, whatever /LDS says.
        assert_eq!(s.write(AC_BASE_HI, 0x00e9, false, true), Event::None);
        assert_eq!(s.write(AC_BASE_HI, 0x00e9, false, false), Event::None);
        assert_eq!(s.state, State::Unconfigured);
        // An Autoconfig write with /LDS low as well is still a /UDS write.
        assert_eq!(s.write(AC_BASE_LO, 0x0000, true, true), Event::None);

        assert_eq!(os_write_byte(&mut s, AC_BASE_HI, 0xe9), Event::Configured);
        assert_eq!((s.state, s.base), (State::Configured, 0xe9));

        assert_eq!(s.read(reg::MAGIC), MAGIC);
        s.write(reg::SCRATCH, 0xa55a, true, true);
        assert_eq!(s.read(reg::SCRATCH), 0xa55a);
        // /LDS has no effect: /UDS low is a word write, /UDS high the low byte.
        s.write(reg::SCRATCH, 0x1200, true, false); // even byte: taken as a word
        assert_eq!(s.read(reg::SCRATCH), 0x1200);
        s.write(reg::SCRATCH, 0x5634, false, true); // odd byte: low byte only
        assert_eq!(s.read(reg::SCRATCH), 0x1234);
        s.write(reg::SCRATCH, 0x0078, false, false); // /LDS not seen (One TH)
        assert_eq!(s.read(reg::SCRATCH), 0x1278);
        assert_eq!(s.read(0x80), 0xffff); // unused register
    }

    #[test]
    fn reset_and_shutup() {
        let mut s = Slave::new();
        os_write_byte(&mut s, AC_BASE_HI, 0xe9);
        let cycles = s.cycles;
        s.reset();
        assert_eq!(s.state, State::Unconfigured);
        assert_eq!(s.cycles, cycles); // survives a reset
        assert_eq!(os_read_byte(&mut s, 0), 0xc2);
        assert_eq!(s.write(AC_SHUTUP, 0, true, false), Event::ShutUp);
        assert_eq!(s.state, State::ShutUp);
    }
}
