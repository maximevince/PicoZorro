//! Backplane protocol (developer builds): a request datagram is a
//! list of bus cycles, run in order against a [`Window`]; the reply carries
//! what the reads returned. Transport-free, so the firmware builds that
//! serve the backplane share it and the host tests run it. The board is
//! anything that implements [`BusTarget`], such as the network board's
//! [`Window`].

use crate::slave::Event;
use crate::window::Window;

/// A board as the backplane drives it: register cycles with A16, reset,
/// and, for a board with memory space, bulk access to it.
pub trait BusTarget {
    fn read_at(&mut self, a16: bool, reg: u8, uds: bool, lds: bool) -> u16;
    fn write_at(&mut self, a16: bool, reg: u8, data: u16, uds: bool, lds: bool);
    fn reset(&mut self);
    /// Bulk write into the board's memory space, `offset` in bytes from the
    /// board base. `false`: no memory there (the op is refused).
    fn mem_write(&mut self, _offset: u32, _data: &[u8]) -> bool {
        false
    }
    /// Bulk read, as `mem_write`.
    fn mem_read(&mut self, _offset: u32, _out: &mut [u8]) -> bool {
        false
    }
    /// /INT: the level the backplane reports in its notification frames.
    fn irq_asserted(&self) -> bool;
}

impl BusTarget for Window<'_> {
    fn read_at(&mut self, a16: bool, reg: u8, uds: bool, lds: bool) -> u16 {
        Window::read_at(self, a16, reg, uds, lds)
    }
    fn write_at(&mut self, a16: bool, reg: u8, data: u16, uds: bool, lds: bool) {
        let _: Event = Window::write_at(self, a16, reg, data, uds, lds);
    }
    fn reset(&mut self) {
        Window::reset(self)
    }
    fn irq_asserted(&self) -> bool {
        Window::irq_asserted(self)
    }
}

pub const MAGIC: u16 = 0x5042; // "PB"
pub const VERSION: u8 = 1;
/// Request flag: answer me.
pub const F_REPLY: u8 = 0x01;
pub const F_IS_REPLY: u8 = 0x80;
/// Notification: /INT level.
pub const F_INT: u8 = 0x40;

pub const OP_WRITE: u8 = 1;
pub const OP_READ: u8 = 2;
pub const OP_WRITE_N: u8 = 3;
pub const OP_READ_N: u8 = 4;
pub const OP_RESET: u8 = 5;
/// A16 for the ops after it in this request (0 or 1); each request starts
/// at 0.
pub const OP_WINDOW: u8 = 6;
/// Bulk write into memory space: offset24, n16, n bytes.
pub const OP_MEM_WRITE: u8 = 7;
/// Bulk read from memory space: offset24, n16; the bytes go into the reply.
pub const OP_MEM_READ: u8 = 8;

pub const ST_OK: u8 = 0;
pub const ST_MALFORMED: u8 = 1;
/// A memory op on a board without memory there; the ops after it were not run.
pub const ST_REFUSED: u8 = 2;

pub const DGRAM_MAX: usize = 2048;
pub const REPLY_HDR: usize = 8;

/// Run a request's bus cycles. Returns the reply length in `out` (0: none
/// wanted, or not a backplane request).
pub fn execute<T: BusTarget>(w: &mut T, req: &[u8], out: &mut [u8]) -> usize {
    if req.len() < 6 || u16::from_be_bytes([req[0], req[1]]) != MAGIC || req[2] != VERSION || out.len() < REPLY_HDR
    {
        return 0;
    }
    let flags = req[3];
    let mut n = REPLY_HDR;
    let mut status = ST_OK;
    let mut a16 = false;
    let mut i = 6;
    let strobes = |s: u8| (s & 2 != 0, s & 1 != 0);
    while i < req.len() {
        let rest = &req[i..];
        match rest[0] {
            OP_WRITE if rest.len() >= 5 => {
                let (uds, lds) = strobes(rest[2]);
                w.write_at(a16, rest[1], u16::from_be_bytes([rest[3], rest[4]]), uds, lds);
                i += 5;
            }
            OP_READ if rest.len() >= 3 && n + 2 <= out.len() => {
                let (uds, lds) = strobes(rest[2]);
                out[n..n + 2].copy_from_slice(&w.read_at(a16, rest[1], uds, lds).to_be_bytes());
                n += 2;
                i += 3;
            }
            OP_WRITE_N if rest.len() >= 4 => {
                let cnt = u16::from_be_bytes([rest[2], rest[3]]) as usize;
                if rest.len() < 4 + 2 * cnt {
                    status = ST_MALFORMED;
                    break;
                }
                for k in 0..cnt {
                    w.write_at(a16, rest[1], u16::from_be_bytes([rest[4 + 2 * k], rest[5 + 2 * k]]), true, true);
                }
                i += 4 + 2 * cnt;
            }
            OP_READ_N if rest.len() >= 4 => {
                let cnt = u16::from_be_bytes([rest[2], rest[3]]) as usize;
                if n + 2 * cnt > out.len() {
                    status = ST_MALFORMED;
                    break;
                }
                for _ in 0..cnt {
                    out[n..n + 2].copy_from_slice(&w.read_at(a16, rest[1], true, true).to_be_bytes());
                    n += 2;
                }
                i += 4;
            }
            OP_RESET => {
                w.reset();
                i += 1;
            }
            OP_WINDOW if rest.len() >= 2 && rest[1] <= 1 => {
                a16 = rest[1] == 1;
                i += 2;
            }
            OP_MEM_WRITE if rest.len() >= 6 => {
                let off = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]);
                let cnt = u16::from_be_bytes([rest[4], rest[5]]) as usize;
                if rest.len() < 6 + cnt {
                    status = ST_MALFORMED;
                    break;
                }
                if !w.mem_write(off, &rest[6..6 + cnt]) {
                    status = ST_REFUSED;
                    break;
                }
                i += 6 + cnt;
            }
            OP_MEM_READ if rest.len() >= 6 => {
                let off = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]);
                let cnt = u16::from_be_bytes([rest[4], rest[5]]) as usize;
                if n + cnt > out.len() {
                    status = ST_MALFORMED;
                    break;
                }
                if !w.mem_read(off, &mut out[n..n + cnt]) {
                    status = ST_REFUSED;
                    break;
                }
                n += cnt;
                i += 6;
            }
            _ => {
                status = ST_MALFORMED;
                break;
            }
        }
    }
    if flags & F_REPLY == 0 && status == ST_OK {
        return 0;
    }
    out[0..2].copy_from_slice(&MAGIC.to_be_bytes());
    out[2] = VERSION;
    out[3] = F_IS_REPLY;
    out[4..6].copy_from_slice(&req[4..6]);
    out[6] = status;
    out[7] = 0;
    n
}

/// The /INT notification datagram.
pub fn int_note(level: bool) -> [u8; 7] {
    let mut note = [0u8; 7];
    note[0..2].copy_from_slice(&MAGIC.to_be_bytes());
    note[2] = VERSION;
    note[3] = F_INT;
    note[6] = level as u8;
    note
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{nic, usb};

    #[test]
    fn autoconfig_then_both_windows() {
        let mut nsh = Box::new(nic::Shared::new([0x52, 0x5a, 1, 2, 3, 4]));
        let (nb, _) = nsh.split();
        let mut ush = Box::new(usb::Shared::new());
        let (ub, _) = ush.split();
        let mut w = Window::with_usb(nb, ub);
        let req = [
            0x50, 0x42, 1, F_REPLY, 0x12, 0x34, //
            OP_RESET, //
            OP_WRITE, 0x4a, 2, 0x90, 0x00, // base low nibble, /UDS only
            OP_WRITE, 0x48, 2, 0xe0, 0x00, // base high nibble: configured
            OP_READ, 0x00, 3, // PZ
            OP_WINDOW, 1, //
            OP_READ, 0x00, 3, // PU
            OP_READ_N, 0x02, 0, 2, // USB VERSION twice
        ];
        let mut out = [0u8; 64];
        let n = execute(&mut w, &req, &mut out);
        assert_eq!(&out[..n], &[0x50, 0x42, 1, F_IS_REPLY, 0x12, 0x34, ST_OK, 0, 0x50, 0x5a, 0x50, 0x55, 0, 1, 0, 1]);
        // The next request starts at A16 = 0 again.
        let req2 = [0x50, 0x42, 1, F_REPLY, 0, 1, OP_READ, 0x00, 3];
        let n = execute(&mut w, &req2, &mut out);
        assert_eq!(&out[8..n], &[0x50, 0x5a]);
        // Bad window number, and no reply when none is asked for.
        let req3 = [0x50, 0x42, 1, 0, 0, 2, OP_WINDOW, 2];
        let n = execute(&mut w, &req3, &mut out);
        assert_eq!(out[6], ST_MALFORMED, "errors are always answered");
        assert_eq!(n, REPLY_HDR);
        let req4 = [0x50, 0x42, 1, 0, 0, 3, OP_WINDOW, 1, OP_WRITE, 0x06, 3, 0, 1];
        assert_eq!(execute(&mut w, &req4, &mut out), 0);
        assert!(!w.irq_asserted());
    }
}
