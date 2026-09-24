//! Serial link framing: a datagram plus its CRC-16/CCITT-FALSE
//! (big-endian), COBS-encoded, with a 0x00 byte before and after. Used by
//! the register backplane.

/// CRC-16/CCITT-FALSE: poly 0x1021, init 0xffff, no reflection.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Worst-case encoded size of a datagram of `n` bytes, delimiters included.
pub const fn encoded_max(n: usize) -> usize {
    n + 2 + (n + 2) / 254 + 3
}

/// Frame `datagram` into `out`: 0x00, COBS(datagram + CRC), 0x00. Returns
/// the number of bytes written. `out` needs `encoded_max(datagram.len())`.
pub fn encode(datagram: &[u8], out: &mut [u8]) -> usize {
    let crc = crc16(datagram).to_be_bytes();
    out[0] = 0;
    let mut code_at = 1;
    let mut w = 2;
    let mut code: u8 = 1;
    for &b in datagram.iter().chain(crc.iter()) {
        if b == 0 {
            out[code_at] = code;
            code_at = w;
            w += 1;
            code = 1;
        } else {
            out[w] = b;
            w += 1;
            code += 1;
            if code == 0xff {
                out[code_at] = code;
                code_at = w;
                w += 1;
                code = 1;
            }
        }
    }
    out[code_at] = code;
    out[w] = 0;
    w + 1
}

/// Decode one COBS block sequence (no delimiter) in place.
fn cobs_decode(buf: &mut [u8]) -> Option<usize> {
    let len = buf.len();
    let (mut r, mut w) = (0, 0);
    while r < len {
        let code = buf[r] as usize;
        if code == 0 || r + code > len {
            return None;
        }
        for i in 1..code {
            buf[w] = buf[r + i];
            w += 1;
        }
        r += code;
        if code != 0xff && r < len {
            buf[w] = 0;
            w += 1;
        }
    }
    Some(w)
}

/// Collects received bytes into checked datagrams.
pub struct Deframer<const N: usize> {
    buf: [u8; N],
    len: usize,
    overflow: bool,
    /// Frames dropped: bad COBS, bad CRC, too long.
    pub errors: u32,
}

impl<const N: usize> Default for Deframer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Deframer<N> {
    pub const fn new() -> Self {
        Deframer { buf: [0; N], len: 0, overflow: false, errors: 0 }
    }

    /// Feed one byte. When it completes a good frame, returns the datagram
    /// length; the datagram is then `self.datagram(len)` until the next push.
    pub fn push(&mut self, b: u8) -> Option<usize> {
        if b != 0 {
            if self.len < N {
                self.buf[self.len] = b;
                self.len += 1;
            } else {
                self.overflow = true;
            }
            return None;
        }
        let len = core::mem::replace(&mut self.len, 0);
        if core::mem::replace(&mut self.overflow, false) {
            self.errors = self.errors.wrapping_add(1);
            return None;
        }
        if len == 0 {
            return None;
        }
        match cobs_decode(&mut self.buf[..len]) {
            Some(n) if n >= 2 && crc16(&self.buf[..n - 2]) == u16::from_be_bytes([self.buf[n - 2], self.buf[n - 1]]) => {
                Some(n - 2)
            }
            _ => {
                self.errors = self.errors.wrapping_add(1);
                None
            }
        }
    }

    pub fn datagram(&self, len: usize) -> &[u8] {
        &self.buf[..len]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(d: &[u8]) {
        let mut enc = vec![0u8; encoded_max(d.len())];
        let n = encode(d, &mut enc);
        assert!(enc[1..n - 1].iter().all(|&b| b != 0));
        let mut df: Deframer<4096> = Deframer::new();
        let mut got = None;
        for &b in &enc[..n] {
            if let Some(len) = df.push(b) {
                got = Some(df.datagram(len).to_vec());
            }
        }
        assert_eq!(got.as_deref(), Some(d));
        assert_eq!(df.errors, 0);
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn roundtrips() {
        for n in [0, 1, 2, 253, 254, 255, 256, 508, 509, 1000, 1540, 2048] {
            roundtrip(&vec![0u8; n]);
            roundtrip(&vec![7u8; n]);
            roundtrip(&(0..n).map(|i| (i * 37 % 256) as u8).collect::<Vec<_>>());
        }
    }

    #[test]
    fn bad_crc_is_dropped() {
        let mut enc = [0u8; 32];
        let n = encode(b"hello", &mut enc);
        enc[3] ^= 0x10;
        let mut df: Deframer<64> = Deframer::new();
        assert!(enc[..n].iter().all(|&b| df.push(b).is_none()));
        assert_eq!(df.errors, 1);
    }
}
