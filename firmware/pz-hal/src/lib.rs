//! PicoZorro pieces on embassy-rp that the firmware images share. Board
//! behaviour stays in the dependency-free crate `pz-core`; this crate only
//! wires it to RP2350 peripherals.
//!
//! - [`backplane`]: the backplane serve loop for any
//!   `pz_core::backplane::BusTarget`, over the UART (developer builds).
//! - [`chip`] (feature `nic-w5500` or `nic-enc28j60`): the Ethernet chip on
//!   an SPI (SPI1 on the PicoZorro boards).
//! - [`usb`]: a USB CDC ACM device with the picotool reset interface.
//! - [`usb_host`] (feature `usb-host`): the RP2350 as the USB host
//!   controller for Poseidon, for the USB window.
//!
//! Chip: feature `rp235xb` (the Core2350B, the PicoZorro boards) or
//! `rp235xa` (RP2350A boards), forwarded to embassy-rp; exactly one.
#![no_std]

#[cfg(all(feature = "rp235xa", feature = "rp235xb"))]
compile_error!("select one chip feature: rp235xa or rp235xb");
#[cfg(not(any(feature = "rp235xa", feature = "rp235xb")))]
compile_error!("select a chip feature: rp235xa or rp235xb (Core2350B)");

pub mod backplane;
#[cfg(any(feature = "nic-w5500", feature = "nic-enc28j60"))]
pub mod chip;
pub mod usb;
#[cfg(feature = "usb-host")]
pub mod usb_host;
