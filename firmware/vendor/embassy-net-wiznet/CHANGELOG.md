# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->
## Unreleased - ReleaseDate

- PicoZorro vendored copy of embassy git main (https://github.com/maximevince/embassy), on the xarxa driver-channel API (`PacketBuf`, global packet pool).
- PicoZorro addition: `Control` + `Runner::run_with_control` to switch the W5500 MAC filter (Sn_MR.MFEN, applied by CLOSE + OPEN) at run time and to read PHYCFGR (speed, duplex). `Control::int_wakeups` counts frames read after waiting for the interrupt line (a check that /INT is wired). `run()` behaves as upstream.
- PicoZorro addition: on its 500 ms tick the runner reads Sn_SR; a chip that lost its set-up (socket no longer in MACRAW mode) is set up again, after a software reset unless the chip was reset already.
- PicoZorro addition: `trace`: frame counters, counts of writes to the common register block, and the last SPI frame headers, frozen when a lost set-up is seen.

## 0.3.0 - 2026-03-10

- Added experimental W6100 driver with disabled MAC filter (does not currently work with it enabled)
- Added W6300 driver
- Introduced `SOCKET_INTR_CLR` register which is needed on W6100 and later models (on W5100/W5500 this is shared with `SOCKET_INTR` and the address is the same)
- Upgrade embassy-net-driver-channel to 0.4.0

## 0.2.1 - 2025-08-26

## 0.1.1 - 2025-08-14

- First release with changelog.
