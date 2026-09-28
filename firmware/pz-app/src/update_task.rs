//! Firmware update from the Amiga (`docs/UPDATE.md`): the flash side of
//! `pz_core::update`.
//!
//! The module boots from partition A or B of an A/B pair (partition table
//! made with picotool, docs/UPDATE.md); the bootrom maps the booted
//! partition to 0x10000000 (datasheet 5.1.19). An update is written into the
//! other partition with the bootrom's low-level flash functions (storage
//! offsets, 5.4.8.10/11), read back through the untranslated XIP window
//! (0x1c000000, 4.4) and checked; ACTIVATE reboots with REBOOT_TYPE_FLASH_UPDATE
//! for that partition (5.4.8.24), the new image, flagged try-before-you-buy,
//! runs on trial under the watchdog, and CONFIRM makes it permanent with
//! explicit_buy (5.1.17, 5.4.8.4). Without CONFIRM the watchdog ends the
//! trial and the old image boots again.
//!
//! The first sector of the new image (its IMAGE_DEF) is written last, after
//! the rest is in flash and the stream's CRC matched: a partial image never
//! has a block the bootrom would pick.
//!
//! Flash writes run from RAM with core 0's interrupts off and XIP off. Core
//! 1 is not paused (embassy-rp's `Flash` would: `pause_core1`): the bus
//! loop runs from RAM, and its few paths into flash wait while a write is
//! on (`zbus::flash_exclusive`). In the backplane build core 1 is off.

use core::sync::atomic::{AtomicU16, Ordering::Relaxed};

use defmt::{info, warn};
use embassy_rp::rom_data;
use embassy_rp::watchdog::Watchdog;
use embassy_time::{Duration, Instant, Timer};
use pz_core::update::{self, cmd, error, HostPort, State, SECTOR};

/// Running firmware, "0.1.0-<git>" or `PZ_VERSION` at build time.
pub const VERSION: &str = env!("PZ_VERSION");

/// The version once more, findable in the image: "PZVERSN:" and 16 bytes,
/// NUL-padded. tools/mkpzf.py copies it into the .pzf header.
#[used]
static VERSION_TAG: [u8; 24] = {
    let mut t = [0u8; 24];
    let tag = b"PZVERSN:";
    let v = VERSION.as_bytes();
    let mut i = 0;
    while i < 8 {
        t[i] = tag[i];
        i += 1;
    }
    let mut k = 0;
    while k < v.len() && k < 16 {
        t[8 + k] = v[k];
        k += 1;
    }
    t
};

/// A16-A23 of the board while configured, 0xffff otherwise; kept by whoever
/// serves the window (backplane task or bus loop) for the reboot stash.
pub static BASE: AtomicU16 = AtomicU16::new(0xffff);

/// How long an image on trial keeps the watchdog fed while it waits for
/// CONFIRM. The bootrom's own trial watchdog is 16.7 s; the Amiga needs
/// longer to notice the reboot, so the image extends it, but only this far.
const TRIAL: Duration = Duration::from_secs(120);

const XIP_BASE: u32 = 0x1000_0000;
const XIP_UNTRANSLATED: u32 = 0x1c00_0000;

// bootrom_constants.h (pico-sdk)
const SYS_INFO_BOOT_INFO: u32 = 0x0040;
const PT_INFO_PT_INFO: u32 = 0x0001;
const PT_INFO_PARTITION_LOCATION_AND_FLAGS: u32 = 0x0010;
const BOOT_TYPE_FLASH_UPDATE: u32 = 4;
const TBYB_BUY_PENDING: u32 = 0x1;
const REBOOT_TYPE_FLASH_UPDATE: u32 = 0x0004;
const BOOTROM_ERROR_PRECONDITION_NOT_MET: i32 = -14;

/// Watchdog scratch words for the Autoconfig base across the update reboot
/// (the bootrom uses SCRATCH2-7 for reboot parameters, 5.4.8.24).
const STASH_MAGIC: u32 = 0x505a_0000;

/// What the bootrom says about this boot and the flash.
#[derive(Clone, Copy, defmt::Format)]
pub struct Layout {
    /// 0 = A, 1 = B, None = not booted from an A/B pair.
    pub running: Option<u8>,
    pub buy_pending: bool,
    pub flash_update_boot: bool,
    /// Storage offset and size of the partition to write.
    pub target: Option<(u32, u32)>,
}

impl Layout {
    pub fn slot_kb(&self) -> u16 {
        self.target.map_or(0, |(_, size)| (size / 1024) as u16)
    }
}

/// 4 KiB word-aligned scratch for the bootrom (load_partition_table,
/// explicit_buy).
static mut WORKAREA: [u32; 1024] = [0; 1024];

/// Only this module uses it, from one task at a time.
fn workarea() -> (*mut u8, usize) {
    (core::ptr::addr_of_mut!(WORKAREA) as *mut u8, 4096)
}

pub fn probe() -> Layout {
    let mut bi = [0u32; 5];
    // SAFETY: buffer and size match.
    let n = unsafe { rom_data::get_sys_info(bi.as_mut_ptr(), bi.len(), SYS_INFO_BOOT_INFO) };
    let (tt, pp, bb) = if n >= 2 && bi[0] & SYS_INFO_BOOT_INFO != 0 {
        (bi[1] >> 24, (bi[1] >> 16) & 0xff, (bi[1] >> 8) & 0xff)
    } else {
        (0, 0xff, 0)
    };
    let mut l = Layout {
        running: None,
        buy_pending: tt & TBYB_BUY_PENDING != 0,
        flash_update_boot: bb == BOOT_TYPE_FLASH_UPDATE,
        target: None,
    };
    info!("update: boot info {:08x} {:08x}", bi[1], bi[2]);
    if pp > 1 {
        return l;
    }
    let mut pt = [0u32; 1 + 3 + 2 * 16];
    let flags = PT_INFO_PT_INFO | PT_INFO_PARTITION_LOCATION_AND_FLAGS;
    // SAFETY: buffers and sizes match.
    let mut n = unsafe { rom_data::get_partition_table_info(pt.as_mut_ptr(), pt.len(), flags) };
    if n == BOOTROM_ERROR_PRECONDITION_NOT_MET {
        let (w, len) = workarea();
        unsafe { rom_data::load_partition_table(w, len, false) };
        n = unsafe { rom_data::get_partition_table_info(pt.as_mut_ptr(), pt.len(), flags) };
    }
    if n < 4 || pt[0] & flags != flags || pt[1] & (1 << 8) == 0 || pt[1] & 0xff < 2 {
        warn!("update: no usable partition table ({})", n);
        return l;
    }
    // The pair is partitions 0 (A) and 1 (B) of the partition table.
    // SAFETY: plain bootrom query.
    if unsafe { rom_data::get_b_partition(0) } != 1 {
        warn!("update: partition 1 is not the B of partition 0");
        return l;
    }
    let other = 1 - pp as usize;
    let loc = pt[4 + 2 * other];
    let first = loc & 0x1fff;
    let last = (loc >> 13) & 0x1fff;
    l.running = Some(pp as u8);
    l.target = Some((first * 4096, (last + 1 - first) * 4096));
    l
}

/// The Autoconfig base the previous image had, if it rebooted into us for
/// an update (and forget it).
pub fn take_stash(wd: &mut Watchdog<'_>) -> Option<u8> {
    let a = wd.scratch(0);
    let b = wd.scratch(1);
    wd.set_scratch(0, 0);
    wd.set_scratch(1, 0);
    (a & 0xffff_ff00 == STASH_MAGIC && b == !a).then_some(a as u8)
}

fn stash(wd: &mut Watchdog<'_>, base: Option<u8>) {
    match base {
        Some(b) => {
            let a = STASH_MAGIC | b as u32;
            wd.set_scratch(0, a);
            wd.set_scratch(1, !a);
        }
        None => {
            wd.set_scratch(0, 0);
            wd.set_scratch(1, 0);
        }
    }
}

// ---- flash ----------------------------------------------------------------

#[repr(C)]
struct RomFns {
    connect_internal_flash: unsafe extern "C" fn(),
    flash_exit_xip: unsafe extern "C" fn(),
    flash_range_erase: unsafe extern "C" fn(u32, usize, u32, u8),
    flash_range_program: unsafe extern "C" fn(u32, *const u8, usize),
    flash_flush_cache: unsafe extern "C" fn(),
    xip_setup: unsafe extern "C" fn(),
}

/// Runs from RAM: nothing here may touch flash, XIP is off in between.
#[inline(never)]
#[link_section = ".data.ram_func"]
unsafe fn ram_erase_program(offset: u32, data: *const u8, f: *const RomFns) {
    ((*f).connect_internal_flash)();
    ((*f).flash_exit_xip)();
    // block_size 1 << 31: no block erase command, 4 KiB sector erases only
    // (as embassy-rp does).
    ((*f).flash_range_erase)(offset, SECTOR, 1 << 31, 0);
    ((*f).flash_range_program)(offset, data, SECTOR);
    ((*f).flash_flush_cache)();
    ((*f).xip_setup)();
}

/// Erase the 4 KiB sector at storage `offset` and program `data` (padded
/// with 0xff) into it.
fn write_sector(offset: u32, data: &[u8]) {
    let mut buf = [0xffu8; SECTOR];
    buf[..data.len()].copy_from_slice(data);
    // The XIP setup function the bootrom leaves in boot RAM restores the
    // read mode (5.4.8.10); run a copy of it from SRAM, as embassy-rp does.
    let mut xip = [0u32; 64];
    // SAFETY: BOOTRAM 0x400e0000 holds the 256-byte XIP setup function.
    unsafe { core::ptr::copy_nonoverlapping(0x400e_0000 as *const u32, xip.as_mut_ptr(), 64) };
    let f = RomFns {
        connect_internal_flash: rom_data::connect_internal_flash::ptr(),
        flash_exit_xip: rom_data::flash_exit_xip::ptr(),
        flash_range_erase: rom_data::flash_range_erase::ptr(),
        flash_range_program: rom_data::flash_range_program::ptr(),
        flash_flush_cache: rom_data::flash_flush_cache::ptr(),
        // SAFETY: Thumb entry of the copied function.
        xip_setup: unsafe { core::mem::transmute::<usize, unsafe extern "C" fn()>(xip.as_ptr() as usize + 1) },
    };
    crate::zbus::flash_exclusive(|| {
        cortex_m::interrupt::free(|_| {
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // SAFETY: interrupts off on this core, core 1 off or kept out of
            // flash (flash_exclusive), no DMA reads flash (all DMA in this
            // firmware is RAM <-> peripheral).
            unsafe { ram_erase_program(offset, buf.as_ptr(), &f) };
        })
    });
}

/// CRC32 of `len` bytes at storage `offset`, read uncached and untranslated.
async fn flash_crc(offset: u32, len: u32) -> u32 {
    let mut crc = 0;
    let mut at = 0;
    while at < len {
        let n = (len - at).min(SECTOR as u32);
        // SAFETY: inside the flash device; the untranslated window is
        // readable by Secure code (the default).
        let s = unsafe { core::slice::from_raw_parts((XIP_UNTRANSLATED + offset + at) as *const u8, n as usize) };
        crc = update::crc32(crc, s);
        at += n;
        embassy_futures::yield_now().await;
    }
    crc
}

// ---- settings ---------------------------------------------------------------

/// The settings sector: right after slot B (the partition table puts A at
/// 64K and B up to 4160K), so updates and pz_install leave it alone.
/// "PZC1", then flags.
const CONFIG_OFF: u32 = 4160 * 1024;
const CONFIG_MAGIC: &[u8; 4] = b"PZC1";
/// Flags bit 0: the boot ROM is switched off.
const CONFIG_BOOTROM_OFF: u32 = 1;

fn config_flags() -> u32 {
    // SAFETY: inside the flash device (partitioned installs have 4160K+);
    // the untranslated window is readable by Secure code.
    let d = unsafe { core::slice::from_raw_parts((XIP_UNTRANSLATED + CONFIG_OFF) as *const u8, 8) };
    if &d[..4] != CONFIG_MAGIC {
        return 0; // erased or never written: the defaults
    }
    u32::from_le_bytes([d[4], d[5], d[6], d[7]])
}

/// The boot ROM is switched off in flash.
pub fn bootrom_off() -> bool {
    config_flags() & CONFIG_BOOTROM_OFF != 0
}

fn write_config(flags: u32) {
    let mut d = [0u8; 8];
    d[..4].copy_from_slice(CONFIG_MAGIC);
    d[4..].copy_from_slice(&flags.to_le_bytes());
    write_sector(CONFIG_OFF, &d);
}

// ---- the task ---------------------------------------------------------------

struct Job {
    size: u32,
    written: u32,
    crc: u32,
    first: [u8; SECTOR],
    first_len: usize,
}

#[embassy_executor::task]
pub async fn run(mut host: HostPort<'static>, layout: Layout, mut wd: Watchdog<'static>, boot: &'static pz_core::boot::Shared) -> ! {
    host.set_identity(
        layout.running == Some(1),
        layout.buy_pending,
        layout.target.is_some(),
        layout.slot_kb(),
        VERSION,
    );
    // After set_identity, which writes the whole status word.
    host.set_bootrom(boot.has_rom());
    info!("update: {} running {:?} target {:?} buy pending {}", VERSION, layout.running, layout.target, layout.buy_pending);
    let started = Instant::now();
    let mut on_trial = layout.buy_pending;
    if on_trial {
        wd.feed(Duration::from_secs(16));
    }
    let mut job: Option<Job> = None;
    loop {
        if on_trial {
            if started.elapsed() < TRIAL {
                wd.feed(Duration::from_secs(16));
            } else if started.elapsed() < TRIAL + Duration::from_millis(50) {
                warn!("update: no CONFIRM within the trial, the watchdog will restore the old image");
            }
        }
        // Sectors first, then commands: COMMIT comes after the last sector
        // (the bus side hands the last sector over before it posts COMMIT)
        // and must find it in flash. Before each sector, a pending ABORT
        // (an Amiga reset) or BEGIN wins: both drop the queued sectors.
        loop {
            if matches!(host.pending_command(), Some(cmd::ABORT | cmd::BEGIN)) {
                break;
            }
            let Some(s) = host.peek_sector() else { break };
            let Some(j) = job.as_mut() else {
                host.release_sector();
                continue;
            };
            let (offset, _) = layout.target.unwrap();
            j.crc = update::crc32(j.crc, s);
            if j.written == 0 {
                j.first[..s.len()].copy_from_slice(s);
                j.first_len = s.len();
                // Sector 0 comes last; erase it now so the slot holds no
                // image while the rest goes in.
                write_sector(offset, &[]);
            } else {
                write_sector(offset + j.written, s);
            }
            j.written += s.len() as u32;
            host.release_sector();
            host.set_progress(j.written);
        }
        if let Some(c) = host.take_command() {
            match c.cmd {
                cmd::BEGIN => {
                    job = None;
                    match layout.target {
                        None => host.set_state(State::Error, error::NOT_PARTITIONED),
                        Some((_, size)) if c.size == 0 || c.size > size => host.set_state(State::Error, error::BAD_SIZE),
                        Some(_) => {
                            job = Some(Job { size: c.size, written: 0, crc: 0, first: [0; SECTOR], first_len: 0 });
                            host.set_progress(0);
                            host.set_result(0);
                            host.set_state(State::Receiving, error::NONE);
                            info!("update: begin, {} bytes", c.size);
                        }
                    }
                }
                cmd::ABORT => {
                    job = None;
                    host.set_state(State::Idle, error::NONE);
                }
                cmd::COMMIT => {
                    let (offset, _) = layout.target.unwrap_or((0, 0));
                    match job.take() {
                        None => host.set_state(State::Error, error::SEQUENCE),
                        Some(j) if j.written != j.size || j.size != c.size => host.set_state(State::Error, error::SHORT),
                        Some(j) if j.crc != c.crc => {
                            warn!("update: stream crc {:08x}, expected {:08x}", j.crc, c.crc);
                            host.set_state(State::Error, error::CRC)
                        }
                        Some(j) if update::image_type(&j.first[..j.first_len]).is_none() => {
                            host.set_state(State::Error, error::NOT_AN_IMAGE)
                        }
                        Some(j) => {
                            host.set_state(State::Verifying, error::NONE);
                            write_sector(offset, &j.first[..j.first_len]);
                            let got = flash_crc(offset, j.size).await;
                            host.set_result(got);
                            if got == c.crc {
                                info!("update: {} bytes in flash at {:x}, crc {:08x}", j.size, offset, got);
                                host.set_state(State::Ready, error::NONE);
                            } else {
                                warn!("update: read back crc {:08x}, expected {:08x}", got, c.crc);
                                host.set_state(State::Error, error::FLASH);
                            }
                        }
                    }
                }
                cmd::ACTIVATE => {
                    if host.state() != State::Ready as u16 {
                        host.set_state(State::Error, error::SEQUENCE);
                    } else {
                        let (offset, _) = layout.target.unwrap();
                        let base = BASE.load(Relaxed);
                        stash(&mut wd, (base <= 0xff).then_some(base as u8));
                        host.set_state(State::Rebooting, error::NONE);
                        host.ack(&c);
                        info!("update: flash update reboot into {:x}, base {:x}", offset, base);
                        Timer::after_millis(50).await; // the log and the ack go out
                        rom_data::reboot(REBOOT_TYPE_FLASH_UPDATE, 10, XIP_BASE + offset, 0);
                        Timer::after_millis(1000).await;
                        continue;
                    }
                }
                cmd::CONFIRM => {
                    if !on_trial {
                        host.set_state(State::Error, error::SEQUENCE);
                    } else {
                        let (w, len) = workarea();
                        // It rewrites the sector with our own IMAGE_DEF, so XIP
                        // is off meanwhile: no interrupt handler (in flash) may
                        // run. With interrupts on, the module stops answering
                        // and the trial watchdog restores the old image.
                        // SAFETY: word-aligned 4 KiB scratch (5.4.8.4).
                        let r = crate::zbus::flash_exclusive(|| {
                            cortex_m::interrupt::free(|_| unsafe { rom_data::explicit_buy(w, len as u32) })
                        });
                        if r == 0 {
                            on_trial = false;
                            host.set_buy_pending(false);
                            host.set_state(State::Idle, error::NONE);
                            info!("update: confirmed, this image stays");
                        } else {
                            warn!("update: explicit_buy {}", r);
                            host.set_state(State::Error, error::BUY_FAILED);
                        }
                    }
                }
                cmd::BOOTROM_OFF | cmd::BOOTROM_ON => {
                    let on = c.cmd == cmd::BOOTROM_ON;
                    if layout.target.is_none() {
                        // Only partitioned installs are known to reach 4160K.
                        host.set_state(State::Error, error::NOT_PARTITIONED);
                    } else {
                        let f = config_flags();
                        write_config(if on { f & !CONFIG_BOOTROM_OFF } else { f | CONFIG_BOOTROM_OFF });
                        boot.set_enabled(on);
                        host.set_bootrom(boot.has_rom());
                        host.set_state(State::Idle, error::NONE);
                        info!("update: boot ROM {} from the next Amiga reset", if on { "on" } else { "off" });
                    }
                }
                _ => host.set_state(State::Error, error::SEQUENCE),
            }
            host.ack(&c);
        }
        Timer::after_millis(2).await;
    }
}
