//! This build script copies the `memory.x` file from the crate root into
//! a directory where the linker can always find it at build time.
//! For many projects this is optional, as the linker always searches the
//! project root directory -- wherever `Cargo.toml` is. However, if you
//! are using a workspace or have a more complicated build setup, this
//! build script becomes required. Additionally, by requesting that
//! Cargo re-run the build script whenever `memory.x` is changed,
//! updating `memory.x` ensures a rebuild of the application with the
//! new memory settings.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

fn main() {
    // Put `memory.x` in our output directory and ensure it's
    // on the linker search path.
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());

    // By default, Cargo will re-run a build script whenever
    // any file in the project changes. By specifying `memory.x`
    // here, we ensure the build script is only re-run when
    // `memory.x` is changed.
    println!("cargo:rerun-if-changed=memory.x");

    // Version string of the image (docs/UPDATE.md, FW_VERSION register):
    // PZ_VERSION if set, else "<crate version>-<git short hash>".
    println!("cargo:rerun-if-env-changed=PZ_VERSION");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let version = env::var("PZ_VERSION").unwrap_or_else(|_| {
        let git = std::process::Command::new("git")
            .args(["rev-parse", "--short=7", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        format!("{}-{}", env::var("CARGO_PKG_VERSION").unwrap(), git)
    });
    println!("cargo:rustc-env=PZ_VERSION={}", version);

    // minimp3 (vendor/minimp3) for the MPEG decoder: C, built with
    // arm-none-eabi-gcc only when a feature needs it.
    if env::var_os("CARGO_FEATURE_MPEG").is_some() {
        println!("cargo:rerun-if-changed=../vendor/minimp3");
        cc::Build::new()
            .file("../vendor/minimp3/minimp3.c")
            .include("../vendor/minimp3")
            .opt_level(2)
            .flag("-ffp-contract=off")
            // The M33's FPU is single precision only.
            .flag("-mcpu=cortex-m33")
            .flag("-mfpu=fpv5-sp-d16")
            .warnings(false)
            .compile("minimp3");
    }

    // The boot image (boot ROM + the Amiga modules), made by
    // tools/mkboot.py. Without one the firmware has no boot
    // ROM (the Autoconfig ROM does not say DIAGVALID).
    println!("cargo:rerun-if-env-changed=PZ_BOOT_IMG");
    let img = env::var("PZ_BOOT_IMG").unwrap_or_else(|_| "../boot.img".into());
    println!("cargo:rerun-if-changed={}", img);
    let bytes = std::fs::read(&img).unwrap_or_default();
    File::create(out.join("boot.img")).unwrap().write_all(&bytes).unwrap();

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
