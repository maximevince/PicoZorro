//! GPIO numbers, `hardware/PINMAP.md`. One map for every board and firmware.

pub const D0: u8 = 0; // D0..D15 = GPIO0..15
pub const AS: u8 = 16; // active low
pub const UDS: u8 = 17; // active low
pub const LDS: u8 = 18; // active low
pub const READ: u8 = 19; // high = read
pub const A1: u8 = 20; // A1..A7 = GPIO20..26
pub const XRDY: u8 = 27; // open drain: value 0, switched by direction
pub const A16: u8 = 28; // A16..A23 = GPIO28..35
pub const CFGIN: u8 = 36; // active low; 9th bit of the match field
pub const CFGOUT: u8 = 37; // active low, push-pull
pub const BUSRST: u8 = 38; // active low
pub const ARM: u8 = 44; // active low, gate-assist option
pub const UART_TX: u8 = 46;

/// /INT to the Amiga: open drain by direction through an open-drain buffer,
/// or push-pull into an N-MOSFET (pz-app feature `int-nfet`, the PicoZorro
/// One TH). A jumper on the board picks /INT2 or /INT6.
pub const INT: u8 = 45;

/// Activity LED where there is one (the Core2350B module's LED1 via R10), and
/// the W5500 /INT input: the same pin. The W5500's INTn is push-pull
/// (VOH >= 2.4 V at 8 mA), so it drives the LED itself; with a W5500 attached
/// the pin is an input, without one the firmware may drive the LED.
pub const LED: u8 = 39;

/// SPI1 to the Ethernet chip (function F1 on these pins).
pub const NIC_MISO: u8 = 40;
pub const NIC_CS: u8 = 41;
pub const NIC_SCK: u8 = 42;
pub const NIC_MOSI: u8 = 43;
/// W5500 /INT (INTn), active low. Shares GPIO39 with the LED. GPIO47 is
/// never used: on the Core2350B it is the module's PSRAM chip select (R11).
pub const NIC_INT: u8 = LED;

pub const BUS_PIN_FIRST: u8 = 0;
/// /INT (GPIO45) is set up on its own.
pub const BUS_PIN_LAST: u8 = 38;

/// Layout of the 29-bit cycle word captured by PIO block A (`in pins, 29`):
/// GPIO0-28, i.e. D15-D0, the strobes, A7-A1, XRDY (ignored) and A16.
pub mod cw {
    pub const AS: u32 = 1 << 16;
    pub const UDS: u32 = 1 << 17;
    pub const LDS: u32 = 1 << 18;
    pub const READ: u32 = 1 << 19;
    pub const A16: u32 = 1 << 28;
    pub const BITS: u32 = 29;
    pub fn data(w: u32) -> u16 {
        w as u16
    }
    /// Even byte offset (A7..A1).
    pub fn reg(w: u32) -> u8 {
        (((w >> 20) & 0x7f) << 1) as u8
    }
    /// A16: the second 64 KiB of the board (USB, MPEG, update).
    pub fn a16(w: u32) -> bool {
        w & A16 != 0
    }
}

/// Match field seen by block B while unconfigured: {CFGIN, A23..A16}.
/// CFGIN low = bit 8 clear.
pub const MATCH_BITS: u32 = 9;
/// No sample can equal this.
pub const MATCH_NEVER: u32 = 0xffff_ffff;
/// $E8xxxx with /CFGIN asserted.
pub const MATCH_CONFIG: u32 = 0x0e8;
/// Configured, the board is 128 KiB and A16 is not compared: block B reads
/// {CFGIN, A23..A17} from A17 on.
pub const MATCH_BITS_CONFIGURED: u32 = 8;
/// First pin of the configured match field.
pub const MATCH_BASE_CONFIGURED: u8 = A16 + 1;
/// Match value for a board configured at `base` (A23..A16; A16 is 0 for a
/// 128 KiB board), /CFGIN asserted.
pub const fn match_configured(base: u8) -> u32 {
    (base >> 1) as u32
}
