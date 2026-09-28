# PicoZorro firmware (Rust, embassy)

The firmware of the PicoZorro cards: an RP2350B serves the Zorro II bus from
PIO and core 1, and runs the Ethernet chip, the USB host and the MPEG audio
decoder on core 0. Cargo workspace:

| Crate | What |
|---|---|
| `pz-core` | `no_std`, no dependencies: Autoconfig ROM and slave (`autoconfig`, `slave`), register window (`window`), network model (`nic`: rings, STATS, MCAST), USB window (`usb`), MPEG registers (`mpeg`), firmware update registers (`update`), boot ROM and boot stream (`boot`), pin map (`pins`: one map for every board), MAC rule (`mac`). Runs on the host: `cargo test-host`. |
| `pz-hal` | embassy-rp pieces the images share: `chip` (the Ethernet chip on SPI1, features `nic-w5500` / `nic-enc28j60` / `nic-poll`, which `pz-app` forwards), `usb::cdc_with_reset` (CDC ACM plus the picotool reset interface), `usb_host` (the RP2350 as USB host for Poseidon, feature `usb-host`), `backplane` (see "Developer features"). |
| `pz-app` | The RP2350B images, below. |

Images (`pz-app` binaries):

| Binary | What |
|---|---|
| `picozorro` | The card's firmware. Core 1: the bus slave (`zbus.rs`, from RAM). Core 0: the Ethernet chip if one answers on SPI1 (none: bus slave only, link down, TX frames dropped), the USB host or the USB log, the MPEG decoder, the firmware update and boot stream tasks, a text log on GPIO46 (115200 8N1). |
| `hello` | First light without a probe: `log` over USB CDC, chip stepping, package, OTP chip ID and MAC, LED heartbeat, picotool reset interface. |
| `nic-demo` | Ethernet chip check on SPI1 (W5500 by default, ENC28J60 with `nic-enc28j60`): DHCP, ping, TCP echo on port 7, defmt over RTT. |
| `bus-listen` | Passive bus listener for a card's first power-up in an Amiga: drives nothing on the bus, reports the bus pins and /AS statistics once per second over the UART and USB CDC. |

## Build

Needs rustup (the toolchain file installs stable with the
`thumbv8m.main-none-eabihf` target) and `arm-none-eabi-gcc` for the MPEG
decoder (minimp3 is C).

    cargo build --release                       # every image, default features
    cargo test-host                             # pz-core's tests on the host

The default features build the PicoZorro One's image: XRDY from the GPIO,
/INT through its transistor, the USB port as host. The One TH and the One
SMD share one pin map and one image.

For bring-up, the same image with the USB port as a CDC log instead of the
USB host (`usb-log`):

    cargo build --release --bin picozorro --no-default-features \
        --features nic-w5500,mpeg,oc240,xrdy-direct,int-nfet,usb-log

The ELFs land in `target/thumbv8m.main-none-eabihf/release/`.

`build.rs` embeds a boot image if there is one (`firmware/boot.img`, or the
path in `PZ_BOOT_IMG`), made by `tools/mkboot.py` from the Amiga modules;
without one the card has no boot ROM. `PZ_VERSION` overrides the version
string (default: crate version and git short hash).

## Flash

With an SWD probe (Raspberry Pi Debug Probe, J-Link, ...) and probe-rs 0.25
or later (`cargo install probe-rs-tools --locked`), `cargo run` flashes and
streams the defmt log over RTT:

    cargo run --release --bin picozorro [--features ...]

Without a probe, as a UF2 file: hold BOOTSEL while the module powers up,
it shows up as a USB drive, copy the file onto it. `make fw-uf2` (top
level) writes `firmware/picozorro-install.uf2`, which installs `picozorro`
partitioned (`docs/UPDATE.md`); `make fw-uf2 BIN=hello` writes a plain UF2
of any other image. Or over USB with picotool:

    picotool load -f -x -t elf target/thumbv8m.main-none-eabihf/release/picozorro

`-f` reboots an image that has the reset interface (`hello`, `bus-listen`,
`picozorro` with `usb-log`) into BOOTSEL first; otherwise hold BOOTSEL while
the module powers up. picotool needs access to the RP2350's USB device
(VID 2e8a): a udev rule such as

    SUBSYSTEM=="usb", ATTRS{idVendor}=="2e8a", MODE="0660", TAG+="uaccess"

in `/etc/udev/rules.d/99-picotool.rules`, or sudo.

`cargo run`, `picotool load` and a plain UF2 write the image to the start
of flash. The A/B partition layout that firmware updates from the Amiga need
is described in `docs/UPDATE.md`.

## Features (`pz-app`)

Default (the PicoZorro One): `nic-w5500`, `mpeg`, `oc240`, `xrdy-direct`,
`int-nfet`, `usb-host`. The other features are alternatives for other
wiring or for bring-up; `usb-log` and `nic-enc28j60` replace a default, so
they go with `--no-default-features` and the rest of the list.

| Feature | What |
|---|---|
| `nic-w5500` | W5500 Ethernet chip on SPI1. |
| `nic-enc28j60` | ENC28J60 instead (`nic-demo`; use with `--no-default-features`). |
| `nic-poll` | W5500 without /INT: the RX size is polled every 500 us; frees GPIO39. |
| `xrdy-direct` | XRDY driven from GPIO27 (the PicoZorro One). Without it: an on-board gate pulls XRDY, the PIO drives /ARM. |
| `int-nfet` | /INT through a transistor (N-MOSFET or NPN): GPIO45 push-pull, high asserts (the PicoZorro One). Without it: an open-drain buffer, low asserts. |
| `mpeg` | MPEG audio decoder for mpega.library (`docs/REGISTERS-MPEG.md`). Needs `arm-none-eabi-gcc`. |
| `usb-host` | The USB port as host for Poseidon (`docs/REGISTERS-USB.md`). Not with `usb-log`. |
| `usb-prof` | `usb-host` plus a timing profile and flight recorder of USB transfers, read over SWD. |
| `write-guard` | `usb-host` refuses writes to the mass-storage sticks listed in `pz_hal::usb_host`. |
| `usb-log` | The USB port as a device instead of `usb-host`: CDC ACM text log plus the picotool reset interface, for bring-up. |
| `oc240` | clk_sys 240 MHz at 1.15 V (above the RP2350's 150 MHz rating). Without `oc240` / `oc300`: 150 MHz. |
| `oc300` | clk_sys 300 MHz at 1.25 V. |
| `bus-prof` | Core 1 cycle counts per bus cycle and loop pass in the log. |
| `bench-host` | Developer build, see below. |
| `uart-backplane` | Developer build, see below. |

## Developer features

`bench-host`, `uart-backplane` and pz-hal's `backplane` module are links
that let the firmware run without an Amiga: `bench-host` has core 1 play
the Amiga driver for network tests from a PC, `uart-backplane` serves the
register window over UART0 (GPIO0/1) so that FS-UAE or a PC tool can drive
the card's models. Neither image is for a card in an Amiga (GPIO0/1 are bus
data lines). The test bed they connect to is not published yet.

## embassy

The embassy crates come from embassy git main, for the RP2350 USB host
driver that no release has yet, through a fork
(`https://github.com/maximevince/embassy`) with embassy-rp fixes for the
USB host and BufferedUart. The workspace `Cargo.toml` swaps them in with
`[patch.crates-io]`; the crates keep their crates.io version requirements.
On main, embassy-net runs on the xarxa stack: sockets have no buffers of
their own and take packets from a global pool.

`vendor/embassy-net-wiznet` is main's W5500 driver plus PicoZorro's
additions (MAC filter switch, PHY state, interrupt wake-up count, recovery
of a chip that lost its set-up), see its `CHANGELOG.md`. `vendor/minimp3`
is lieff/minimp3, unchanged. `pz-app/memory.x` and `build.rs` follow
embassy's `examples/rp235x`.
