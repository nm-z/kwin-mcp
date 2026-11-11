#![allow(clippy::expect_used)]

//! `initialize` must not wait for the `--autoclean` startup sweeps (#879). An
//! orphaned workdir whose disk twin holds many files takes seconds to delete;
//! the server answers the handshake first and removes the orphan afterwards.
//! No session is started, so the test needs neither KDE nor a display.
//! `KWIN_MCP_E2E_SERVER` names another server binary.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Enough empty files that deleting them takes far longer than a handshake.
const ORPHAN_FILES: usize = 150_000;
const FILES_PER_DIRECTORY: usize = 500;
const SWEEP_DEADLINE: Duration = Duration::from_secs(120);

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn server_binary() -> PathBuf {
    std::env::var_os("KWIN_MCP_E2E_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")))
}

/// A PID no process has, so the lease it names belongs to a dead owner.
fn dead_pid(mut candidate: i32) -> i32 {
    while unsafe { nix::libc::kill(candidate, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::ESRCH)
    {
        candidate += 1;
    }
    candidate
}

/// The workdir and disk twin a server killed mid-session leaves: a lease that
/// names a dead sandbox group, and an overlay full of files.
fn seed_orphan(runtime: &Path, cache: &Path) -> (PathBuf, PathBuf) {
    let dead = dead_pid(4_100_000);
    let name = format!("kwin-mcp-{dead}-1");
    let workdir = runtime.join(&name);
    let twin = cache.join("kwin-mcp").join(&name);
    std::fs::create_dir_all(&workdir).expect("create orphaned workdir");
    std::fs::write(
        workdir.join(".kwin-mcp-autoclean"),
        format!("kwin-mcp-autoclean-v1 {dead} {dead}\n"),
    )
    .expect("write lease");
    for directory in 0..ORPHAN_FILES / FILES_PER_DIRECTORY {
        let directory = twin.join("upper").join(format!("d{directory}"));
        std::fs::create_dir_all(&directory).expect("create overlay directory");
        for file in 0..FILES_PER_DIRECTORY {
            std::fs::File::create(directory.join(format!("f{file}"))).expect("create overlay file");
        }
    }
    (workdir, twin)
}

#[test]
fn initialize_is_answered_before_the_orphan_sweep_finishes() {
    let root = PathBuf::from("/var/tmp").join(format!("kwin-mcp-handshake-{}", std::process::id()));
    let fixture = Fixture(root);
    let home = fixture.0.join("home");
    let runtime = fixture.0.join("tmp");
    let cache = home.join(".cache");
    std::fs::create_dir_all(&cache).expect("create private HOME");
    std::fs::create_dir_all(&runtime).expect("create private TMPDIR");
    let (workdir, twin) = seed_orphan(&runtime, &cache);

    let started = Instant::now();
    let mut child = Command::new(server_binary())
        .args(["--ttl", "60", "--autoclean", "--no-viewer"])
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env("TMPDIR", &runtime)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut stdin = child.stdin.take().expect("server stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"startup-handshake-e2e","version":"1"}
            }
        })
    )
    .expect("send initialize");
    stdin.flush().expect("flush initialize");
    let mut reply = String::new();
    stdout.read_line(&mut reply).expect("read initialize reply");
    let answered_after = started.elapsed();
    // Read before anything else: the sweep is racing this check.
    let orphan_left = twin.exists();
    assert!(reply.contains("\"result\""), "initialize failed: {reply}");
    assert!(
        orphan_left,
        "initialize was answered only after the orphan sweep had removed {} files ({answered_after:?})",
        ORPHAN_FILES
    );

    // The sweep still runs: the orphan is gone a little later.
    let deadline = Instant::now() + SWEEP_DEADLINE;
    while twin.exists() || workdir.exists() {
        assert!(
            Instant::now() < deadline,
            "the orphaned workdir was never swept: {} {}",
            workdir.display(),
            twin.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin);
    let _ = child.wait();
    eprintln!("initialize answered after {answered_after:?}");
}
