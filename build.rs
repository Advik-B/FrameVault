//! Build script.
//!
//! On Windows, FFmpeg is linked **statically** (see `.cargo/config.toml`,
//! `scripts/setup-windows.ps1`, and the `static` feature on `ffmpeg-next` in `Cargo.toml`).
//! vcpkg's FFmpeg pulls in Win32 components (Media Foundation, COM, etc.) whose import
//! libraries must be linked alongside the static FFmpeg archives. `ffmpeg-sys-next` emits
//! some of them (ole32, secur32, ws2_32, bcrypt, user32); we add the rest here. Listing an
//! unused system import library is harmless — the linker ignores it if nothing references it.
//!
//! No-op on non-Windows targets (Linux/macOS link FFmpeg dynamically from system libraries).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for lib in [
            "mfplat", "mfuuid", "strmiids", "ole32", "oleaut32", "uuid", "user32", "gdi32",
            "bcrypt", "secur32", "ws2_32", "advapi32", "shell32",
            // avdevice's capture backends: Video-for-Windows (vfwcap.o) and DirectShow
            // (dshow.o, which calls SHCreateStreamOnFileA from shlwapi).
            "vfw32", "shlwapi",
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
