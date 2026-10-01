use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const RESPONSE_WAIT: Duration = Duration::from_secs(45);
const EXIT_WAIT: Duration = Duration::from_secs(10);
const LIVE_EXIT_WAIT: Duration = Duration::from_secs(70);
const CLEANUP_GRACE: Duration = Duration::from_secs(2);
const POLL_PAUSE: Duration = Duration::from_millis(20);
const FIXTURE_HOME: &str = "KWIN_MCP_DISCONNECT_FIXTURE_HOME";

struct PrivateHome(PathBuf);

impl PrivateHome {
    fn create() -> TestResult<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let home = Self(
            PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("shim-disconnect-{}-{nonce}", std::process::id())),
        );
        for path in [".config", ".local/share", ".local/state", ".cache", ".kde"] {
            std::fs::create_dir_all(home.0.join(path))?;
        }
        Ok(home)
    }
}

impl Drop for PrivateHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shim_command(home: &Path) -> Command {
    let binary = std::env::var_os("KWIN_MCP_E2E_SHIM")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_kwin-mcp-shim").into());
    let mut command = Command::new(binary);
    command
        .args(["--autoclean", "--no-viewer"])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("KDEHOME", home.join(".kde"))
        .env("KWIN_MCP_REPO", home.join("no-repository"))
        .env_remove("KWIN_MCP_SHIM_RESUME");
    if let Some(binary) = std::env::var_os("KWIN_MCP_E2E_SERVER") {
        command.env("KWIN_MCP_BINARY", binary);
    }
    command
}

#[derive(Clone, Debug)]
struct OwnedProcess {
    pid: i32,
    starttime: u64,
    comm: String,
}

impl OwnedProcess {
    fn read(pid: i32) -> TestResult<Self> {
        let stat = procfs::process::Process::new(pid)?.stat()?;
        Ok(Self {
            pid,
            starttime: stat.starttime,
            comm: stat.comm,
        })
    }

    fn stat(&self) -> Option<procfs::process::Stat> {
        procfs::process::Process::new(self.pid)
            .ok()?
            .stat()
            .ok()
            .filter(|stat| stat.starttime == self.starttime)
    }
}

struct OwnedProcesses {
    child: Child,
    root: i32,
    owned: HashMap<i32, OwnedProcess>,
}

impl OwnedProcesses {
    fn spawn(command: &mut Command) -> TestResult<Self> {
        // Adopt the fixture's shim after the fixture exits. Reap only recorded
        // PIDs, so concurrently running tests keep ownership of their children.
        nix::sys::prctl::set_child_subreaper(true)?;
        let child = command.spawn()?;
        let root = i32::try_from(child.id())?;
        let mut processes = Self {
            child,
            root,
            owned: HashMap::new(),
        };
        processes.remember(root)?;
        Ok(processes)
    }

    fn remember(&mut self, pid: i32) -> TestResult {
        self.owned.insert(pid, OwnedProcess::read(pid)?);
        Ok(())
    }

    fn capture_tree(&mut self) -> TestResult {
        let stats: Vec<_> = procfs::process::all_processes()?
            .flatten()
            .filter_map(|process| process.stat().ok())
            .collect();
        loop {
            let mut added = false;
            for stat in &stats {
                if self.owned.contains_key(&stat.pid) {
                    continue;
                }
                let parent_is_owned = self.owned.get(&stat.ppid).is_some_and(|parent| {
                    stats.iter().any(|candidate| {
                        candidate.pid == parent.pid && candidate.starttime == parent.starttime
                    })
                });
                if parent_is_owned {
                    self.owned.insert(
                        stat.pid,
                        OwnedProcess {
                            pid: stat.pid,
                            starttime: stat.starttime,
                            comm: stat.comm.clone(),
                        },
                    );
                    added = true;
                }
            }
            if !added {
                return Ok(());
            }
        }
    }

    fn reap(&mut self) {
        let _ = self.child.try_wait();
        let me = i32::try_from(std::process::id()).ok();
        for process in self
            .owned
            .values()
            .filter(|process| process.pid != self.root)
        {
            if process
                .stat()
                .is_some_and(|stat| stat.state == 'Z' && Some(stat.ppid) == me)
            {
                let _ = waitpid(Pid::from_raw(process.pid), Some(WaitPidFlag::WNOHANG));
            }
        }
    }

    fn remaining(&mut self) -> Vec<OwnedProcess> {
        self.reap();
        self.owned
            .values()
            .filter(|process| process.stat().is_some())
            .cloned()
            .collect()
    }

    fn wait_for_child(&mut self, timeout: Duration) -> TestResult<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "owned child did not exit").into(),
                );
            }
            thread::sleep(POLL_PAUSE);
        }
    }

    fn signal_owned(&mut self, signal: Signal) {
        for process in self.remaining() {
            // Check the recorded birth time immediately before signaling.
            if process.stat().is_some() {
                let _ = kill(Pid::from_raw(process.pid), signal);
            }
        }
    }

    fn wait_for_cleanup(&mut self) {
        let deadline = Instant::now() + CLEANUP_GRACE;
        while !self.remaining().is_empty() && Instant::now() < deadline {
            thread::sleep(POLL_PAUSE);
        }
    }
}

impl Drop for OwnedProcesses {
    fn drop(&mut self) {
        let _ = self.capture_tree();
        self.signal_owned(Signal::SIGTERM);
        self.wait_for_cleanup();
        self.signal_owned(Signal::SIGKILL);
        // Child remains safe to kill if reading /proc failed during setup.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        self.wait_for_cleanup();
        let remaining = self.remaining();
        if !remaining.is_empty() {
            eprintln!("owned process cleanup did not finish: {remaining:?}");
        }
    }
}

struct Connection {
    processes: OwnedProcesses,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    buffered: Vec<u8>,
    shim: i32,
    home: PrivateHome,
}

impl Connection {
    fn start(launcher: bool) -> TestResult<Self> {
        let home = PrivateHome::create()?;
        let mut command = if launcher {
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "launching_client_fixture",
                    "--ignored",
                    "--nocapture",
                    "--quiet",
                    "--test-threads=1",
                ])
                .env(FIXTURE_HOME, &home.0);
            command
        } else {
            shim_command(&home.0)
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(home.0.join("stderr.log"))?);
        let mut processes = OwnedProcesses::spawn(&mut command)?;
        let input = processes.child.stdin.take();
        let output = processes.child.stdout.take();
        let mut connection = Self {
            shim: processes.root,
            processes,
            input,
            output,
            buffered: Vec::new(),
            home,
        };
        if launcher {
            let deadline = Instant::now() + EXIT_WAIT;
            let ready = connection.home.0.join("shim.pid");
            loop {
                if let Ok(text) = std::fs::read_to_string(&ready)
                    && let Ok(pid) = text.trim().parse::<i32>()
                {
                    let process = procfs::process::Process::new(pid)?.stat()?;
                    assert_eq!(process.ppid, connection.processes.root, "fixture owns shim");
                    connection.processes.remember(pid)?;
                    connection.shim = pid;
                    break;
                }
                assert!(Instant::now() < deadline, "fixture did not record its shim");
                thread::sleep(POLL_PAUSE);
            }
        }
        let initialized = connection.rpc(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18", "capabilities":{},
                "clientInfo":{"name":"shim-disconnect-test","version":"1"}
            }),
        )?;
        assert!(initialized["result"].is_object(), "{initialized}");
        connection.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
        connection.processes.capture_tree()?;
        Ok(connection)
    }

    fn send(&mut self, value: Value) -> io::Result<()> {
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "test input is closed"))?;
        writeln!(input, "{value}")?;
        input.flush()
    }

    fn rpc(&mut self, id: u64, method: &str, params: Value) -> TestResult<Value> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + RESPONSE_WAIT;
        loop {
            while let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.buffered.drain(..=end).collect();
                // The launching fixture also emits libtest progress lines.
                if let Ok(value) = serde_json::from_slice::<Value>(&line)
                    && value["id"] == id
                {
                    return Ok(value);
                }
            }
            assert!(
                Instant::now() < deadline,
                "response {id} timed out: {}",
                self.log()
            );
            let output = self.output.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "test output is closed")
            })?;
            let polled = {
                let mut fds = [PollFd::new(output.as_fd(), PollFlags::POLLIN)];
                poll(&mut fds, PollTimeout::from(100u8))
            };
            match polled {
                Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
                Ok(_) => {}
                Err(error) => return Err(error.into()),
            }
            let mut bytes = [0u8; 4096];
            let read = output.read(&mut bytes)?;
            assert_ne!(read, 0, "unexpected stdout EOF: {}", self.log());
            self.buffered.extend_from_slice(&bytes[..read]);
        }
    }

    fn disconnect_output(&mut self) -> TestResult {
        // This drops the only actual pipe reader, not a channel receiver.
        self.output.take();
        let request = json!({"jsonrpc":"2.0","id":99,"method":"ping"});
        if let Err(error) = self.send(request)
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
        Ok(())
    }

    fn wait_for_exit(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = self.processes.remaining();
            if remaining.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shim {} or owned descendants survived disconnect: {remaining:?}\n{}",
                self.shim,
                self.log()
            );
            thread::sleep(POLL_PAUSE);
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.0.join("stderr.log")).unwrap_or_default()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.input.take();
        self.output.take();
        // Field order drops the process guard before the private HOME.
    }
}

#[test]
fn shim_exits_when_stdout_closes_with_stdin_open() -> TestResult {
    let mut connection = Connection::start(false)?;
    connection.disconnect_output()?;
    connection.wait_for_exit(EXIT_WAIT);
    assert!(connection.input.is_some(), "test must retain stdin");
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    eprintln!(
        "stdout_disconnect: shim_pid={} stdin_retained=true exited=true",
        connection.shim
    );
    Ok(())
}

#[test]
fn shim_exits_when_launching_client_dies_with_stdin_open() -> TestResult {
    let mut connection = Connection::start(true)?;
    // The fixture exits normally without dropping or signaling its shim.
    // This test keeps both pipe endpoints open throughout that exit.
    std::fs::write(connection.home.0.join("exit-client"), b"exit\n")?;
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    connection.wait_for_exit(EXIT_WAIT);
    assert!(connection.input.is_some(), "test must retain stdin");
    assert!(connection.output.is_some(), "test must retain stdout");
    eprintln!(
        "parent_disconnect: client_pid={} shim_pid={} stdin_retained=true stdout_retained=true exited=true",
        connection.processes.root, connection.shim
    );
    Ok(())
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, and a GPU session; no host viewer"]
fn stdout_disconnect_removes_its_headless_session() -> TestResult {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut connection = Connection::start(false)?;
    let started = connection.rpc(
        2,
        "tools/call",
        json!({"name":"session_start","arguments":{"width":800,"height":600}}),
    )?;
    connection.processes.capture_tree()?;
    let session = &started["result"]["structuredContent"];
    assert_eq!(session["status"], "started", "{started}");
    assert_eq!(session["viewer"]["state"], "closed", "{started}");
    let server: i32 = session["session_id"]
        .as_str()
        .and_then(|id| id.strip_prefix('s'))
        .ok_or_else(|| io::Error::other("session_start returned no shim session_id"))?
        .parse()?;
    assert!(
        connection.processes.owned.contains_key(&server),
        "server is test-owned"
    );
    assert!(
        connection
            .processes
            .owned
            .values()
            .any(|process| process.comm == "kwin_wayland"),
        "real compositor is in the owned tree"
    );
    let workdir = PathBuf::from(
        session["workdir"]
            .as_str()
            .ok_or_else(|| io::Error::other("session_start returned no workdir"))?,
    );
    assert_eq!(workdir, PathBuf::from(format!("/tmp/kwin-mcp-{server}")));
    let disk = connection
        .home
        .0
        .join(".cache/kwin-mcp")
        .join(format!("kwin-mcp-{server}"));
    assert!(workdir.exists(), "socket workdir exists before disconnect");
    assert!(disk.exists(), "overlay directory exists before disconnect");
    let mut recorded: Vec<_> = connection.processes.owned.values().cloned().collect();
    recorded.sort_by_key(|process| process.pid);
    connection.disconnect_output()?;
    connection.wait_for_exit(LIVE_EXIT_WAIT);
    assert!(connection.input.is_some(), "test must retain stdin");
    assert!(
        !workdir.exists(),
        "socket workdir survived: {}",
        workdir.display()
    );
    assert!(
        !disk.exists(),
        "overlay directory survived: {}",
        disk.display()
    );
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    eprintln!(
        "live_stdout_disconnect: shim_pid={} server_pid={server} recorded_owned_tree={recorded:?} socket_workdir={} socket_workdir_removed=true disk_workdir={} disk_workdir_removed=true stdin_retained=true exited=true",
        connection.shim,
        workdir.display(),
        disk.display()
    );
    Ok(())
}

#[test]
#[ignore = "subprocess fixture for the launching-client regression"]
fn launching_client_fixture() -> TestResult {
    let Some(home) = std::env::var_os(FIXTURE_HOME).map(PathBuf::from) else {
        return Ok(());
    };
    let mut command = shim_command(&home);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let processes = OwnedProcesses::spawn(&mut command)?;
    let pending = home.join("shim.pid.pending");
    std::fs::write(&pending, format!("{}\n", processes.root))?;
    std::fs::rename(pending, home.join("shim.pid"))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if home.join("exit-client").exists() {
            // The outer test has recorded the shim and owns its cleanup now.
            // Exit keeps the inherited input writer open in the outer test.
            std::process::exit(0);
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not receive its exit marker"
        );
        thread::sleep(POLL_PAUSE);
    }
}
