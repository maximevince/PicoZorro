//! PicoZorro slave behaviour that does not touch the RP2350: the Autoconfig
//! ROM and write registers, the register window (v0 registers and the
//! network model of `docs/REGISTERS.md`, the USB model of
//! `docs/REGISTERS-USB.md`), the pin map and the MAC address
//! rule. `no_std`, no dependencies, so it runs under `cargo test` on
//! the host (`cargo test-host` from the workspace root).
//!
//! Sources: the Amiga Hardware Reference Manual (Appendix K) and the
//! Commodore A500/A2000 Technical Reference Manual (Autoconfig, bus
//! timing), `hardware/PINMAP.md` (pins).
#![cfg_attr(not(test), no_std)]

pub mod autoconfig;
pub mod backplane;
pub mod boot;
pub mod frame;
pub mod mac;
pub mod mpeg;
pub mod nic;
pub mod pins;
pub mod slave;
pub mod update;
pub mod usb;
pub mod window;
