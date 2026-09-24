use core::marker::PhantomData;

use embedded_hal_async::spi::SpiDevice;

use crate::chip::Chip;

#[repr(u8)]
enum Command {
    Open = 0x01,
    Close = 0x10,
    Send = 0x20,
    Receive = 0x40,
}

#[repr(u8)]
enum Interrupt {
    Receive = 0b00100_u8,
}

/// Wiznet chip in MACRAW mode
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) struct WiznetDevice<C, SPI> {
    spi: SPI,
    /// PicoZorro addition: kept for [`WiznetDevice::recover`].
    mac: [u8; 6],
    _phantom: PhantomData<C>,
}

/// PicoZorro addition: Sn_SR of a socket open in MACRAW mode.
pub(crate) const SOCK_MACRAW: u8 = 0x42;

/// Error type when initializing a new Wiznet device
pub enum InitError<SE> {
    /// Error occurred when sending or receiving SPI data
    SpiError(SE),
    /// The chip returned a version that isn't expected or supported
    InvalidChipVersion {
        /// The version that is supported
        expected: u8,
        /// The version that was returned by the chip
        actual: u8,
    },
}

impl<SE> From<SE> for InitError<SE> {
    fn from(e: SE) -> Self {
        InitError::SpiError(e)
    }
}

impl<SE> core::fmt::Debug for InitError<SE>
where
    SE: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InitError::SpiError(e) => write!(f, "SpiError({:?})", e),
            InitError::InvalidChipVersion { expected, actual } => {
                write!(f, "InvalidChipVersion {{ expected: {}, actual: {} }}", expected, actual)
            }
        }
    }
}

#[cfg(feature = "defmt")]
impl<SE> defmt::Format for InitError<SE>
where
    SE: defmt::Format,
{
    fn format(&self, f: defmt::Formatter) {
        match self {
            InitError::SpiError(e) => defmt::write!(f, "SpiError({})", e),
            InitError::InvalidChipVersion { expected, actual } => {
                defmt::write!(f, "InvalidChipVersion {{ expected: {}, actual: {} }}", expected, actual)
            }
        }
    }
}

impl<C: Chip, SPI: SpiDevice> WiznetDevice<C, SPI> {
    /// Create and initialize the driver
    pub async fn new(spi: SPI, mac_addr: [u8; 6]) -> Result<Self, InitError<SPI::Error>> {
        let mut this = Self {
            spi,
            mac: mac_addr,
            _phantom: PhantomData,
        };

        // Reset device
        this.bus_write(C::COMMON_MODE, &[0x80]).await?;

        // Check the version of the chip
        let mut version = [0];
        this.bus_read(C::COMMON_VERSION, &mut version).await?;
        if version[0] != C::CHIP_VERSION {
            error!("invalid chip version: {} (expected {})", version[0], C::CHIP_VERSION);
            return Err(InitError::InvalidChipVersion {
                actual: version[0],
                expected: C::CHIP_VERSION,
            });
        }

        // MACRAW mode with MAC filtering.
        this.configure(C::SOCKET_MODE_VALUE).await?;

        Ok(this)
    }

    /// Everything a chip reset clears: the interrupt enables, the MAC, the
    /// buffer sizes, the socket's mode; then OPEN. One write per loop pass:
    /// each await of an async fn is a state of its own, and this also runs
    /// in the runner ([`WiznetDevice::recover`]), whose code a firmware may
    /// keep in RAM.
    async fn configure(&mut self, mode: u8) -> Result<(), SPI::Error> {
        let mac = self.mac;
        let kb = [(C::BUF_SIZE / 1024) as u8];
        let writes = [
            // Enable interrupt pin
            (C::COMMON_SOCKET_INTR, &[0x01][..]),
            // Enable receive interrupt
            (C::SOCKET_INTR_MASK, &[Interrupt::Receive as u8][..]),
            // Set MAC address
            (C::COMMON_MAC, &mac[..]),
            // Set the raw socket RX/TX buffer sizes.
            (C::SOCKET_TXBUF_SIZE, &kb[..]),
            (C::SOCKET_RXBUF_SIZE, &kb[..]),
            (C::SOCKET_MODE, &[mode][..]),
            (C::SOCKET_COMMAND, &[Command::Open as u8][..]),
        ];
        for (address, data) in writes {
            self.bus_write(address, data).await?;
        }
        Ok(())
    }

    /// PicoZorro addition: the socket's status (Sn_SR), `None` for a chip
    /// without [`crate::chip::SealedChip::socket_status`] or on an SPI error.
    pub async fn socket_state(&mut self) -> Option<u8> {
        let addr = C::socket_status()?;
        let mut sr = [0u8];
        self.bus_read(addr, &mut sr).await.ok()?;
        Some(sr[0])
    }

    /// PicoZorro addition: set the chip up again after it lost its set-up
    /// (the socket not in MACRAW mode any more). A chip that was reset
    /// behind the driver's back (its MAC register no longer ours) only
    /// needs its registers written; otherwise it is reset first, so nothing
    /// of the state that closed the socket survives. Frames in the chip's
    /// buffers are lost. Returns whether the chip had been reset.
    pub async fn recover(&mut self, mac_filter: bool) -> Result<bool, SPI::Error> {
        let mode = if mac_filter {
            C::SOCKET_MODE_VALUE | C::SOCKET_MODE_MAC_FILTER
        } else {
            C::SOCKET_MODE_VALUE & !C::SOCKET_MODE_MAC_FILTER
        };
        let mut mac = [0u8; 6];
        self.bus_read(C::COMMON_MAC, &mut mac).await?;
        let was_reset = mac != self.mac;
        if !was_reset {
            self.bus_write(C::COMMON_MODE, &[0x80]).await?;
        }
        self.configure(mode).await?;
        self.wait_socket_status(SOCK_MACRAW).await?;
        Ok(was_reset)
    }

    async fn bus_read(&mut self, address: C::Address, data: &mut [u8]) -> Result<(), SPI::Error> {
        C::bus_read(&mut self.spi, address, data).await
    }

    async fn bus_write(&mut self, address: C::Address, data: &[u8]) -> Result<(), SPI::Error> {
        C::bus_write(&mut self.spi, address, data).await
    }

    async fn reset_interrupt(&mut self, code: Interrupt) -> Result<(), SPI::Error> {
        let data = [code as u8];
        self.bus_write(C::SOCKET_INTR_CLR, &data).await
    }

    async fn get_tx_write_ptr(&mut self) -> Result<u16, SPI::Error> {
        let mut data = [0u8; 2];
        self.bus_read(C::SOCKET_TX_DATA_WRITE_PTR, &mut data).await?;
        Ok(u16::from_be_bytes(data))
    }

    async fn set_tx_write_ptr(&mut self, ptr: u16) -> Result<(), SPI::Error> {
        let data = ptr.to_be_bytes();
        self.bus_write(C::SOCKET_TX_DATA_WRITE_PTR, &data).await
    }

    async fn get_rx_read_ptr(&mut self) -> Result<u16, SPI::Error> {
        let mut data = [0u8; 2];
        self.bus_read(C::SOCKET_RX_DATA_READ_PTR, &mut data).await?;
        Ok(u16::from_be_bytes(data))
    }

    async fn set_rx_read_ptr(&mut self, ptr: u16) -> Result<(), SPI::Error> {
        let data = ptr.to_be_bytes();
        self.bus_write(C::SOCKET_RX_DATA_READ_PTR, &data).await
    }

    async fn command(&mut self, command: Command) -> Result<(), SPI::Error> {
        let data = [command as u8];
        self.bus_write(C::SOCKET_COMMAND, &data).await
    }

    async fn get_rx_size(&mut self) -> Result<u16, SPI::Error> {
        loop {
            // Wait until two sequential reads are equal
            let mut res0 = [0u8; 2];
            self.bus_read(C::SOCKET_RECVD_SIZE, &mut res0).await?;
            let mut res1 = [0u8; 2];
            self.bus_read(C::SOCKET_RECVD_SIZE, &mut res1).await?;
            if res0 == res1 {
                break Ok(u16::from_be_bytes(res0));
            }
        }
    }

    async fn get_tx_free_size(&mut self) -> Result<u16, SPI::Error> {
        let mut data = [0; 2];
        self.bus_read(C::SOCKET_TX_FREE_SIZE, &mut data).await?;
        Ok(u16::from_be_bytes(data))
    }

    /// Read bytes from the RX buffer.
    async fn read_bytes(&mut self, read_ptr: &mut u16, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        if C::AUTO_WRAP {
            self.bus_read(C::rx_addr(*read_ptr), buffer).await?;
        } else {
            let addr = *read_ptr % C::BUF_SIZE;
            if addr as usize + buffer.len() <= C::BUF_SIZE as usize {
                self.bus_read(C::rx_addr(addr), buffer).await?;
            } else {
                let n = C::BUF_SIZE - addr;
                self.bus_read(C::rx_addr(addr), &mut buffer[..n as usize]).await?;
                self.bus_read(C::rx_addr(0), &mut buffer[n as usize..]).await?;
            }
        }

        *read_ptr = (*read_ptr).wrapping_add(buffer.len() as u16);

        Ok(())
    }

    /// Read an ethernet frame from the device. Returns the number of bytes read.
    pub async fn read_frame(&mut self, frame: &mut [u8]) -> Result<usize, SPI::Error> {
        let rx_size = self.get_rx_size().await? as usize;
        if rx_size == 0 {
            return Ok(0);
        }

        self.reset_interrupt(Interrupt::Receive).await?;

        let mut read_ptr = self.get_rx_read_ptr().await?;

        // First two bytes gives the size of the received ethernet frame
        let expected_frame_size: usize = {
            let mut frame_bytes = [0u8; 2];
            self.read_bytes(&mut read_ptr, &mut frame_bytes).await?;
            let raw = u16::from_be_bytes(frame_bytes) as usize;
            if raw < 2 {
                // Corrupted header — advance read pointer past it and discard.
                warn!("wiznet rx: bogus frame size {}, discarding", raw);
                self.set_rx_read_ptr(read_ptr).await?;
                self.command(Command::Receive).await?;
                return Ok(0);
            }
            raw - 2
        };

        // Cap to frame buffer length to prevent out-of-bounds access when
        // a corrupted header reports a size larger than the buffer.
        let read_len = expected_frame_size.min(frame.len());

        // Read the ethernet frame
        self.read_bytes(&mut read_ptr, &mut frame[..read_len]).await?;

        // If the frame was larger than our buffer, skip the remaining bytes
        if expected_frame_size > read_len {
            read_ptr = read_ptr.wrapping_add((expected_frame_size - read_len) as u16);
        }

        // Register RX as completed
        self.set_rx_read_ptr(read_ptr).await?;
        self.command(Command::Receive).await?;

        Ok(read_len)
    }

    /// Write an ethernet frame to the device. Returns number of bytes written
    pub async fn write_frame(&mut self, frame: &[u8]) -> Result<usize, SPI::Error> {
        while self.get_tx_free_size().await? < frame.len() as u16 {}
        let write_ptr = self.get_tx_write_ptr().await?;

        if C::AUTO_WRAP {
            self.bus_write(C::tx_addr(write_ptr), frame).await?;
        } else {
            let addr = write_ptr % C::BUF_SIZE;
            if addr as usize + frame.len() <= C::BUF_SIZE as usize {
                self.bus_write(C::tx_addr(addr), frame).await?;
            } else {
                let n = C::BUF_SIZE - addr;
                self.bus_write(C::tx_addr(addr), &frame[..n as usize]).await?;
                self.bus_write(C::tx_addr(0), &frame[n as usize..]).await?;
            }
        }

        self.set_tx_write_ptr(write_ptr.wrapping_add(frame.len() as u16))
            .await?;
        self.command(Command::Send).await?;
        Ok(frame.len())
    }

    /// PicoZorro addition: switch the chip's MAC filter. Sn_MR only takes
    /// effect on OPEN, so the socket is closed and reopened; frames in the
    /// chip's buffers at that moment are lost.
    pub async fn set_mac_filter(&mut self, on: bool) -> Result<(), SPI::Error> {
        let mode = if on {
            C::SOCKET_MODE_VALUE | C::SOCKET_MODE_MAC_FILTER
        } else {
            C::SOCKET_MODE_VALUE & !C::SOCKET_MODE_MAC_FILTER
        };
        self.command(Command::Close).await?;
        self.wait_socket_status(0x00).await?; // SOCK_CLOSED
        self.bus_write(C::SOCKET_MODE, &[mode]).await?;
        self.command(Command::Open).await?;
        self.wait_socket_status(SOCK_MACRAW).await
    }

    async fn wait_socket_status(&mut self, want: u8) -> Result<(), SPI::Error> {
        match C::socket_status() {
            Some(_) => {
                for _ in 0..1000 {
                    let mut sr = [0u8];
                    // Address is not Copy: fetch it again each time.
                    if let Some(addr) = C::socket_status() {
                        self.bus_read(addr, &mut sr).await?;
                    }
                    if sr[0] == want {
                        break;
                    }
                    embassy_time::Timer::after_micros(10).await;
                }
            }
            None => embassy_time::Timer::after_millis(1).await,
        }
        Ok(())
    }

    /// PicoZorro addition: the raw PHY configuration / status register
    /// (W5500 PHYCFGR: bit 0 link, bit 1 100 Mbit/s, bit 2 full duplex).
    pub async fn phy_cfg(&mut self) -> u8 {
        let mut v = [0];
        self.bus_read(C::COMMON_PHY_CFG, &mut v).await.ok();
        v[0]
    }

    pub async fn is_link_up(&mut self) -> bool {
        let mut link = [0];
        self.bus_read(C::COMMON_PHY_CFG, &mut link).await.ok();
        link[0] & 1 == 1
    }
}
