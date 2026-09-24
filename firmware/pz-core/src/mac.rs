//! MAC address: locally administered, derived from the
//! RP2350's 64-bit OTP chip ID. No EEPROM on the board.

/// `52:5a:xx:xx:xx:xx`: bit 1 of the first octet = locally administered,
/// bit 0 clear = unicast; "RZ" in the first two octets, the low 32 bits of
/// the chip ID after that.
pub fn from_chip_id(id: u64) -> [u8; 6] {
    let b = id.to_be_bytes();
    [0x52, 0x5a, b[4], b[5], b[6], b[7]]
}

#[cfg(test)]
mod tests {
    #[test]
    fn locally_administered_unicast() {
        let m = super::from_chip_id(0x0123_4567_89ab_cdef);
        assert_eq!(m, [0x52, 0x5a, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(m[0] & 0x03, 0x02);
    }
}
