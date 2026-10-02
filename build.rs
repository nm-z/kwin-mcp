use std::path::{Path, PathBuf};

fn git(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn watch(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());
}

fn watch_git_identity() {
    if Path::new(".git").is_file() {
        watch(Path::new(".git"));
    }
    if let Some(path) = git(&["rev-parse", "--git-path", "HEAD"]) {
        let path = Path::new(&path);
        if path.exists() {
            watch(path);
        }
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        let mut path = PathBuf::from(path);
        if !path.exists()
            && let Some(log) = git(&["rev-parse", "--git-path", &format!("logs/{reference}")])
            && Path::new(&log).exists()
        {
            watch(Path::new(&log));
            return;
        }
        if !path.exists()
            && let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"])
            && Path::new(&packed).exists()
        {
            watch(Path::new(&packed));
        }
        while !path.exists() {
            if !path.pop() {
                return;
            }
        }
        watch(&path);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in ["build.rs", "Cargo.toml", "Cargo.lock", "src"] {
        watch(Path::new(path));
    }
    watch_git_identity();
    println!("cargo:rerun-if-env-changed=KWIN_MCP_BUILD_NUMBER");
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());

    let build_file = ".build_number";
    let n = match std::env::var("KWIN_MCP_BUILD_NUMBER") {
        Ok(value) => value.parse::<u32>()?,
        Err(std::env::VarError::NotPresent) => {
            let previous: u32 = std::fs::read_to_string(build_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let next = previous.checked_add(1).ok_or("build number overflow")?;
            std::fs::write(build_file, next.to_string())?;
            next
        }
        Err(error) => return Err(error.into()),
    };

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

    Ok(())
}
