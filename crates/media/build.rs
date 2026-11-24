//! Warns loudly when openh264 would be built without its SIMD assembly.
//! openh264-sys2 silently falls back to plain C if `nasm` is missing, which
//! makes H.264 encode/decode 2-3x slower and 1440p30 unreachable.

fn main() {
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    let h264 = std::env::var_os("CARGO_FEATURE_H264").is_some();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if !h264 || !matches!(arch.as_str(), "x86_64" | "x86") {
        return;
    }
    let nasm_ok = std::process::Command::new("nasm")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !nasm_ok {
        println!(
            "cargo:warning=nasm not found: openh264 will be built WITHOUT SIMD assembly \
             (2-3x slower video encode/decode). Install nasm (Fedora: `sudo dnf install nasm`), \
             then `cargo clean -p openh264-sys2` and rebuild."
        );
    }
}
