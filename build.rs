fn main() {
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    let build_file = ".build_number";
    let n: u32 = std::fs::read_to_string(build_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    let _ = std::fs::write(build_file, n.to_string());

    println!("cargo:rustc-env=GIT_HASH={hash}");
    println!("cargo:rustc-env=BUILD_NUMBER={n}");

    // Rasterize the high-visibility cursor SVG once at build time so the binary
    // carries a ready-to-blit PNG (no runtime SVG engine). Height picked so the
    // overlay is clearly larger than any native 24/32px cursor.
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let svg_path = "cursor_v6_fixed.svg";
    let png_out = format!("{out_dir}/cursor.png");
    let status = std::process::Command::new("rsvg-convert")
        .args(["-h", "72", "-o", &png_out, svg_path])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("rsvg-convert failed with status {s}"),
        Err(e) => panic!("rsvg-convert not runnable ({e}); install librsvg"),
    }
    println!("cargo:rerun-if-changed={svg_path}");

    // Run build.rs every build so GIT_HASH and BUILD_NUMBER are always fresh.
    println!("cargo:rerun-if-changed=NONEXISTENT_FILE_FORCE_RERUN");
}
