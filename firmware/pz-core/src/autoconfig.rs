//! Autoconfig ROM (expansion.library's view of the board before it is
//! configured).
//!
//! Facts used here (Amiga Hardware Reference Manual, Appendix K; NDK
//! `libraries/configregs.h`):
//! - logical byte N is read as two nibbles on D15-D12: high at offset 4N, low
//!   at 4N+2;
//! - every nibble except those of er_Type (offsets $00/$02) is inverted,
//!   unused registers included, so they read $F;
//! - the OS writes (byte << 4) to $4A, then the whole byte to $48, as 68000
//!   byte writes (/UDS only, data on D15-D8). $48 configures the board;
//! - any write to $4C shuts the board up; $4E is ignored.

/// Commodore's reserved hacker manufacturer ID.
pub const MANUFACTURER: u16 = 2011;
pub const PRODUCT: u8 = 0x5a;
/// "31337": the OS ignores it, tools show it.
pub const SERIAL: u32 = 31337;
/// Zorro II, no memlist, 128 KB: the register window at A16 = 0, the USB /
/// MPEG / update window at A16 = 1. DIAGVALID is added when a boot ROM is offered.
pub const ER_TYPE: u8 = 0xc2;
/// er_Type bit 4: a DiagArea at er_InitDiagVec (the boot ROM).
pub const ERTF_DIAGVALID: u8 = 0x10;
/// er_InitDiagVec with the boot ROM: $0100 from the board base, i.e. window
/// offset 0 through the 256-byte alias (0 would mean none).
pub const DIAG_VEC: u16 = 0x0100;
/// I/O board, can be shut up.
pub const ER_FLAGS: u8 = 0x00;

/// Autoconfig write registers, byte offsets in config space.
pub const AC_BASE_HI: u8 = 0x48; // A23-A20; this write configures
pub const AC_BASE_LO: u8 = 0x4a; // A19-A16; written first
pub const AC_SHUTUP: u8 = 0x4c;

/// A static in RAM on the target: the bus loop reads it (docs/UPDATE.md).
#[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
static ER_ROM: [u8; 16] = [
    ER_TYPE,
    PRODUCT,
    ER_FLAGS,
    0x00, // er_Reserved03, must be 0
    (MANUFACTURER >> 8) as u8,
    MANUFACTURER as u8,
    (SERIAL >> 24) as u8,
    (SERIAL >> 16) as u8,
    (SERIAL >> 8) as u8,
    SERIAL as u8,
    0x00,
    0x00, // er_InitDiagVec, unused
    0x00,
    0x00,
    0x00,
    0x00, // reserved
];

/// The network board's ROM.
pub static NIC_ROM: &[u8; 16] = &ER_ROM;

/// Physical nibble the config ROM returns on D15-D12 at even offset `reg`
/// (0x00..=0xfe). `diag`: the board offers the boot ROM (DIAGVALID and
/// er_InitDiagVec set).
#[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
pub fn nibble(reg: u8, diag: bool) -> u8 {
    nibble_rom(&ER_ROM, reg, diag)
}

/// `nibble` for a given 16-byte ROM image (another board's identity).
#[cfg_attr(all(target_arch = "arm", target_os = "none"), link_section = ".data.zbus")]
pub fn nibble_rom(rom: &[u8; 16], reg: u8, diag: bool) -> u8 {
    let idx = usize::from(reg >> 2);
    let mut byte = rom.get(idx).copied().unwrap_or(0x00);
    if diag {
        match idx {
            0 => byte |= ERTF_DIAGVALID,
            10 => byte = (DIAG_VEC >> 8) as u8,
            11 => byte = DIAG_VEC as u8,
            _ => {}
        }
    }
    let nib = if reg & 2 != 0 { byte & 0x0f } else { byte >> 4 };
    if idx == 0 {
        nib
    } else {
        !nib & 0x0f
    }
}
