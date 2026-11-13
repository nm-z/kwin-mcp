#![allow(clippy::expect_used)]

//! `--autoclean` must remove a session cache directory whose server died, and
//! only that one. The sweep runs at server startup, before any session exists,
//! so the test needs neither KDE nor a display.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SWEEP_DEADLINE: Duration = Duration::from_secs(20);

struct Server {
    child: Child,
    // Closing either pipe would end the server or fail its writes.
    _stdin: std::process::ChildStdin,
    _stdout: BufReader<std::process::ChildStdout>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pid with no process, so no live `kwin-mcp` can own its directory.
fn dead_pid(start: u32) -> u32 {
    (2..=start)
        .rev()
        .find(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        .expect("a free pid")
}

fn start_server(home: &Path) -> Server {
    start_server_with(home, &["--autoclean"], None)
}

/// A server with `flags`, and with `tmp` as its temporary directory, where
/// it keeps session workdirs, when given.
fn start_server_with(home: &Path, flags: &[&str], tmp: Option<&Path>) -> Server {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"));
    if let Some(tmp) = tmp {
        command.env("TMPDIR", tmp);
    }
    let mut child = command
        .args(flags)
        .arg("--no-viewer")
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut stdin = child.stdin.take().expect("server stdin");
    let stdout = child.stdout.take().expect("server stdout");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"leaked-twin-sweep-e2e","version":"1"}
            }
        })
    )
    .expect("send initialize");
    stdin.flush().expect("flush initialize");
    let mut stdout = BufReader::new(stdout);
    let mut reply = String::new();
    stdout
        .read_line(&mut reply)
        .expect("read initialize reply");
    assert!(reply.contains("\"result\""), "initialize failed: {reply}");
    Server {
        child,
        _stdin: stdin,
        _stdout: stdout,
    }
}

fn leak(cache: &Path, pid: u32) -> PathBuf {
    let dir = cache.join(format!("kwin-mcp-{pid}-1"));
    std::fs::create_dir_all(dir.join("home")).expect("create twin directory");
    std::fs::write(dir.join("home/blob"), vec![0u8; 4096]).expect("fill twin directory");
    dir
}

fn wait_until_removed(dir: &Path) {
    let deadline = Instant::now() + SWEEP_DEADLINE;
    while dir.exists() {
        assert!(
            Instant::now() < deadline,
            "leaked twin was not swept: {}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn autoclean_removes_a_dead_servers_twin_and_keeps_a_live_servers() {
    let root = std::env::temp_dir().join(format!("kwin-mcp-twin-sweep-{}", std::process::id()));
    let fixture = Fixture(root);
    let home = fixture.0.join("home");
    let cache = home.join(".cache/kwin-mcp");
    std::fs::create_dir_all(&cache).expect("create private cache");

    let first_leak = leak(&cache, dead_pid(4_194_000));
    let first = start_server(&home);
    wait_until_removed(&first_leak);

    // A second server must spare the twin of the first, which is still alive.
    let live = leak(&cache, first.child.id());
    let second_leak = leak(&cache, dead_pid(4_193_000));
    let second = start_server(&home);
    wait_until_removed(&second_leak);
    assert!(live.exists(), "a live server's twin was swept");
    drop(second);
    drop(first);
}

/// A session workdir with an autoclean lease file, as a server makes it
/// before its sandbox runs: still empty.
fn leased_workdir(tmp: &Path, pid: u32) -> PathBuf {
    let dir = tmp.join(format!("kwin-mcp-{pid}-1"));
    std::fs::create_dir_all(&dir).expect("create workdir");
    std::fs::write(dir.join(".kwin-mcp-autoclean"), "").expect("create lease");
    dir
}

#[test]
fn an_empty_lease_nobody_holds_does_not_keep_a_dead_servers_workdir() {
    // #974, #975: a server killed while its session started left an empty
    // lease, which kept the workdir and its disk twin for good.
    let root = std::env::temp_dir().join(format!("kwin-mcp-lease-sweep-{}", std::process::id()));
    let fixture = Fixture(root);
    let home = fixture.0.join("home");
    let cache = home.join(".cache/kwin-mcp");
    let tmp = fixture.0.join("tmp");
    std::fs::create_dir_all(&cache).expect("create private cache");
    std::fs::create_dir_all(&tmp).expect("create private tmp");

    let killed = dead_pid(4_192_000);
    let workdir = leased_workdir(&tmp, killed);
    let twin = leak(&cache, killed);
    // A start still under way: its server holds the empty lease.
    let starting = dead_pid(4_191_000);
    let held = leased_workdir(&tmp, starting);
    let held_twin = leak(&cache, starting);
    let lease = std::fs::File::open(held.join(".kwin-mcp-autoclean")).expect("open lease");
    lease.try_lock().expect("hold the lease");

    let server = start_server_with(&home, &["--autoclean"], Some(&tmp));
    wait_until_removed(&workdir);
    wait_until_removed(&twin);
    assert!(
        held.exists() && held_twin.exists(),
        "a lease still held was swept"
    );
    drop(server);
    drop(lease);
}

#[test]
fn a_server_without_autoclean_sweeps_only_dead_servers_disk_twins() {
    let root = std::env::temp_dir().join(format!("kwin-mcp-plain-sweep-{}", std::process::id()));
    let fixture = Fixture(root);
    let home = fixture.0.join("home");
    let cache = home.join(".cache/kwin-mcp");
    let tmp = fixture.0.join("tmp");
    std::fs::create_dir_all(&cache).expect("create private cache");

    let dead = dead_pid(4_190_000);
    // Without --autoclean a workdir outlives its session on purpose.
    let workdir = tmp.join(format!("kwin-mcp-{dead}-1"));
    std::fs::create_dir_all(&workdir).expect("create workdir");
    let twin = leak(&cache, dead);
    let server = start_server_with(&home, &[], Some(&tmp));
    wait_until_removed(&twin);
    assert!(workdir.exists(), "a workdir was swept without --autoclean");
    drop(server);
}
