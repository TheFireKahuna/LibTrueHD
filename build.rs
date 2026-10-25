//! Derives the decoder part of `truehd_atmos_version_string()` from Cargo.lock, so the string
//! names the truehd commit that was actually compiled in (LibTrueHD#2).

use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = fs::read_to_string("Cargo.lock").expect("Cargo.lock");
    let entry = lock
        .split("[[package]]")
        .find(|p| p.lines().any(|l| l.trim() == "name = \"truehd\""))
        .expect("truehd in Cargo.lock");
    let field = |key: &str| {
        entry
            .lines()
            .find_map(|l| l.trim().strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"'))
            .unwrap_or_else(|| panic!("truehd {key} in Cargo.lock"))
            .to_owned()
    };
    let version = field("version");
    // A path override (a [patch] to a local checkout) has no source line, and no commit to name.
    let source = entry.lines().any(|l| l.trim_start().starts_with("source = ")).then(|| field("source"));
    let rev = source.as_deref().map_or("local", |s| {
        let rev = s.rsplit('#').next().expect("truehd commit in Cargo.lock");
        &rev[..12]
    });
    println!("cargo:rustc-env=TRUEHD_CRATE_VERSION={version}");
    println!("cargo:rustc-env=TRUEHD_CRATE_COMMIT={rev}");
}
