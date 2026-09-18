# PicoZorro pin map

One GPIO map is shared by the firmware and the boards:
`firmware/pz-core/src/pins.rs` and `firmware/pz-hal/src/chip.rs` hold the
numbers and must match this file. The PicoZorro One TH follows it except on
four pins (GPIO18, 44, 46, 47), listed in the last section.

Rules behind the map:
- 5 V bus signals only on GPIO0-39 (5 V tolerant). GPIO40-47 are 3.3 V only.
- One PIO block sees 32 pins, window base 0 or 16. GPIO16-31 is visible to
  both windows, so strobes and register-select address bits live there.
- D0-15 + strobes + A1-A7 are contiguous from GPIO0, so block A captures the
  whole cycle with one `in pins, 27`.
- A16-A23 + /CFGIN are contiguous, so block B matches the address (and the
  "my turn in the config chain" condition) with one 9-bit compare.
- XRDY sits in the shared window (GPIO27) so block A can observe it.

| GPIO | RP2350B pin | Signal | Zorro pin | Dir | Notes |
|---|---|---|---|---|---|
| 0-15 | 77-80, 1-4, 6-9, 11-14 | D0-D15 | 75,77,79,81,83,86,84,82,80,78,76,71,69,67,65,63 | I/O | driven only in own read cycles |
| 16 | 16 | /AS | 74 | in | |
| 17 | 17 | /UDS | 72 | in | |
| 18 | 18 | /LDS | 70 | in | One TH: microSD chip select |
| 19 | 19 | READ | 68 | in | high = read |
| 20-26 | 20-23, 25-27 | A1-A7 | 29,27,26,24,21,23,28 | in | register select |
| 27 | 28 | XRDY | 18 | OD out | open drain by direction switching, value 0 |
| 28-35 | 36-40, 42-44 | A16-A23 | 45,47,52,54,56,58,57,59 | in | base address compare |
| 36 | 45 | /CFGIN | 12 | in | 9th compare bit |
| 37 | 46 | /CFGOUT | 11 | out | push-pull |
| 38 | 47 | /BUSRST | 94 | in | reset input (not pin 53) |
| 39 | 48 | NIC_INT / LED | - | in | W5500 INTn (push-pull, active low); the Core2350B's LED1 is on the same pin |
| 40 | 49 | NIC_MISO | - | in | SPI1 RX |
| 41 | 52 | NIC_CS | - | out | SPI1 CSn |
| 42 | 53 | NIC_SCK | - | out | SPI1 SCK |
| 43 | 54 | NIC_MOSI | - | out | SPI1 TX |
| 44 | 55 | /ARM | - | out | One TH: I2S data |
| 45 | 56 | /INT | 19 or 22 | out | through a driver only: GPIO45 is not 5 V tolerant. One TH: NPN transistor, high asserts, /INT2 or /INT6 by jumper |
| 46 | 57 | UART0_TX | - | out | debug log, TX only, function F11. One TH: I2S bit clock |
| 47 | 58 | - | - | - | on the Core2350B B1 / B2 the module's PSRAM /CS: never drive it. One TH: I2S word clock (B0 module only) |

Not connected on the card: /OVR, /DTACK, /SLAVE, /OWN, /BR, /BG, /BGACK,
/GBG, FC0-2, E, /VPA, /VMA, /BERR, /HLT, /RST (53), A8-A15, 7M, CDAC, /C1,
/C3, -5 V, -12 V, +12 V. Pin 91 tied to GND (required of Zorro II cards).

## PicoZorro One TH

Four pins differ from the map above
(`picozorro-one-th/README.md` has the board's full table):

| GPIO | Module pin | Shared map | PicoZorro One TH |
|---|---|---|---|
| 18 | P3.6 | /LDS | microSD chip select |
| 44 | P4.15 | /ARM | I2S data (DIN) |
| 46 | P2.4 | UART0 TX | I2S bit clock (BCK) |
| 47 | P2.3 | not connected (PSRAM chip select on B1 / B2 modules) | I2S word clock (LCK), B0 module only |

/LDS is not connected on the One TH. The firmware decides word and byte
access on /UDS alone on every board, so an even-byte write is taken as a word
write. There is no UART: debug is SWD with RTT. XRDY comes straight from
GPIO27, /INT goes through an NPN transistor on GPIO45, W5500 INTn is on
GPIO39. GPIO18 / 44 / 46 / 47 are driven only by a firmware built for this
board.
