//! Exposes the target triple as `env!("NGIAB_TARGET")` (used by `self update`
//! to pick its release asset).
fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=NGIAB_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
