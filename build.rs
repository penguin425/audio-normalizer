// build.rs — link LAME (libmp3lame) for MP3 encoding output.
//
// MP3 *decoding* is handled entirely in pure Rust by symphonia, but MP3
// *encoding* has no mature pure-Rust implementation, so we FFI to LAME — the
// reference encoder. This script locates libmp3lame via pkg-config, then falls
// back to a few standard library paths, and emits a clear error (rather than an
// opaque linker error) if it is missing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_MP3_ENCODING").is_none() {
        return;
    }

    let via_pkgconfig = std::process::Command::new("pkg-config")
        .args(["--exists", "lame"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if via_pkgconfig {
        println!("cargo:rustc-link-lib=dylib=mp3lame");
    } else {
        let candidates = [
            "/usr/lib/x86_64-linux-gnu/libmp3lame.so",
            "/usr/lib/x86_64-linux-gnu/libmp3lame.a",
            "/usr/local/lib/libmp3lame.so",
            "/usr/lib/libmp3lame.so",
        ];
        if candidates.iter().any(|p| std::path::Path::new(p).exists()) {
            println!("cargo:rustc-link-lib=dylib=mp3lame");
        } else {
            panic!(
                "libmp3lame not found. MP3 output requires LAME.\n  \
                 Install it with: apt-get install -y libmp3lame-dev\n  \
                 (or the equivalent on your system), or build without the \
                 `mp3-encoding` feature."
            );
        }
    }
}
