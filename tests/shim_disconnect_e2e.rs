use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
const POST_EXIT_OBSERVE: Duration = Duration::from_secs(4);
const PROOF_INTERVAL: Duration = Duration::from_millis(500);
const TTL_WAIT: Duration = Duration::from_secs(90);
const TTL_POLL_PAUSE: Duration = Duration::from_millis(500);
const TTL_MINUTES: &str = "1";
const FIXTURE_HOME: &str = "KWIN_MCP_DISCONNECT_FIXTURE_HOME";
const PROOF_DIR_ENV: &str = "KWIN_MCP_PROOF_DIR";
const LIVE_SOCKET_ROOT: &str = "/tmp";
const SERVER_INVOCATIONS: &str = "server-invocations.log";
const RESISTANT_PID: &str = "term-resistant.pid";
const TTL_RECEIPTS: &str = "ttl-receipts.jsonl";
const STOP_RETRY_RECEIPTS: &str = "stop-retry-receipts.jsonl";
const BACKGROUND_RECEIPTS: &str = "background-receipts.jsonl";
const BACKGROUND_INVOCATIONS: &str = "background-invocations.log";
const BACKGROUND_HELPER: &str = "background-helper.identity";
const BACKGROUND_DESCENDANT: &str = "background-descendant.identity";
const BACKGROUND_SERVER: &str = "background-server.identity";
const BACKGROUND_SERVER_RELEASE: &str = "background-server.release";
const BACKGROUND_EXPIRED: &str = "background-helper-expired.log";
const BACKGROUND_READY_WAIT: Duration = Duration::from_secs(20);
const BACKGROUND_EXIT_WAIT: Duration = Duration::from_secs(25);
const BACKGROUND_HELPER_LIFETIME: Duration = Duration::from_secs(180);
const CONTROLLED_CARGO: &str = "controlled-cargo.sh";
const PERIODIC_SWEEP_SERVER: &str = "periodic-sweep-server.sh";
const BACKGROUND_DESCENDANT_SCRIPT: &str = "background-descendant.sh";
const WATCHED_PROJECT: &str = "watched-project";
const HELD_START_RECEIPTS: &str = "held-start-receipts.jsonl";
const HELD_START_RESPONSES: &str = "held-start-responses.jsonl";
const HELD_START_INVOCATIONS: &str = "held-start-invocations.log";
const HELD_START_CARGO_IDENTITY: &str = "held-start-cargo.identity";
const HELD_START_CARGO_RELEASE: &str = "held-start-cargo.release";
const HELD_START_CARGO_FINISHED: &str = "held-start-cargo.finished";
const HELD_START_SERVER: &str = "held-start-server.sh";
const HELD_START_SERVER_IDENTITY: &str = "held-start-server.identity";
const HELD_START_SERVER_RELEASE: &str = "held-start-server.release";

struct PrivateHome(PathBuf, PathBuf);

impl PrivateHome {
    fn create() -> TestResult<Self> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let home = Self(
            PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("shim-disconnect-{}-{nonce}", std::process::id())),
            // Keep the CLI sweep fixture isolated. Live sessions use /tmp;
            // the private HOME overlay and logs stay on disk.
            PathBuf::from("/tmp").join(format!("kmd-{}-{nonce}", std::process::id())),
        );
        for path in [".config", ".local/share", ".local/state", ".cache", ".kde"] {
            std::fs::create_dir_all(home.0.join(path))?;
        }
        std::fs::set_permissions(&home.0, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir(&home.1)?;
        std::fs::set_permissions(&home.1, std::fs::Permissions::from_mode(0o700))?;
        Ok(home)
    }

    fn retain_diagnostics(&self) -> io::Result<Option<PathBuf>> {
        let Some(root) = std::env::var_os(PROOF_DIR_ENV).filter(|root| !root.is_empty()) else {
            return Ok(None);
        };
        let name = self
            .0
            .file_name()
            .ok_or_else(|| io::Error::other("private HOME has no fixture name"))?;
        let proof = PathBuf::from(root).join(name);
        std::fs::create_dir_all(&proof)?;
        std::fs::set_permissions(&proof, std::fs::Permissions::from_mode(0o700))?;
        for name in [
            "stderr.log",
            "scoped-sweep.log",
            "startup-response.json",
            SERVER_INVOCATIONS,
            RESISTANT_PID,
            TTL_RECEIPTS,
            STOP_RETRY_RECEIPTS,
            BACKGROUND_RECEIPTS,
            BACKGROUND_INVOCATIONS,
            BACKGROUND_HELPER,
            BACKGROUND_DESCENDANT,
            BACKGROUND_SERVER,
            BACKGROUND_EXPIRED,
            CONTROLLED_CARGO,
            PERIODIC_SWEEP_SERVER,
            BACKGROUND_DESCENDANT_SCRIPT,
            HELD_START_RECEIPTS,
            HELD_START_RESPONSES,
            HELD_START_INVOCATIONS,
            HELD_START_CARGO_IDENTITY,
            HELD_START_CARGO_RELEASE,
            HELD_START_CARGO_FINISHED,
            HELD_START_SERVER,
            HELD_START_SERVER_IDENTITY,
            HELD_START_SERVER_RELEASE,
        ] {
            copy_diagnostic(&self.0.join(name), &proof.join(name))?;
        }
        copy_diagnostic(
            &self.0.join(".cache/kwin-mcp-shim/build.log"),
            &proof.join("watcher-build.log"),
        )?;
        Ok(Some(proof))
    }
}

impl Drop for PrivateHome {
    fn drop(&mut self) {
        // The process guard has already run, so this last copy also includes
        // stderr written during failure cleanup. KWin inherits that stderr.
        let remove_home = match self.retain_diagnostics() {
            Ok(Some(proof)) => {
                eprintln!("shim disconnect diagnostics: {}", proof.display());
                true
            }
            Ok(None) => true,
            Err(error) => {
                eprintln!(
                    "could not retain shim disconnect diagnostics: {error}; private HOME retained at {}",
                    self.0.display()
                );
                false
            }
        };
        if remove_home {
            let _ = std::fs::remove_dir_all(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn copy_diagnostic(source: &Path, destination: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(source) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // The stop-retry fixture temporarily replaces its workdir with a file.
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::copy(source, destination)?;
            Ok(())
        }
        Ok(_) => Err(io::Error::other(format!(
            "diagnostic is not a regular file: {}",
            source.display()
        ))),
    }
}

fn shim_binary() -> PathBuf {
    std::env::var_os("KWIN_MCP_E2E_SHIM")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp-shim")))
}

fn configure_private_home(command: &mut Command, home: &Path) {
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("KDEHOME", home.join(".kde"))
        // The live sandbox creates a fresh /tmp. Use the prior live-fixture
        // temporary root independently of the isolated CLI sweep fixture.
        .env("TMPDIR", LIVE_SOCKET_ROOT)
        .env("KWIN_MCP_REPO", home.join("no-repository"))
        .env_remove("KWIN_MCP_RETIRE_ON_TTL")
        .env_remove("KWIN_MCP_SHIM_RESUME");
}

fn shim_command(home: &Path) -> Command {
    let mut command = Command::new(shim_binary());
    command.args(["--autoclean", "--no-viewer"]);
    configure_private_home(&mut command, home);
    if let Some(binary) = std::env::var_os("KWIN_MCP_E2E_SERVER") {
        command.env("KWIN_MCP_BINARY", binary);
    }
    command
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn real_server_binary() -> PathBuf {
    std::env::var_os("KWIN_MCP_E2E_SERVER")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_kwin-mcp")))
}

fn recording_server(home: &Path) -> TestResult<PathBuf> {
    let binary = std::fs::canonicalize(real_server_binary())?;
    let wrapper = home.join("recording-server.sh");
    let invocations = shell_quote(&home.join(SERVER_INVOCATIONS));
    let resistant_pid = shell_quote(&home.join(RESISTANT_PID));
    // The wrapper never implements MCP. Metadata, sessions, and sweeping all
    // execute the selected real server, including an installed baseline.
    // Keep the resistant descendant outside the sandbox's PID namespace so
    // killing the namespace init cannot hide a missing shim drain. Its own
    // process session also exercises descendants that escape the server SID.
    let script = format!(
        "#!/bin/bash\n\
         case \"$1\" in\n\
         --describe) printf 'describe %s\\n' \"$$\" >> {invocations} ;;\n\
         --sweep-workdirs) printf 'sweep %s\\n' \"$$\" >> {invocations} ;;\n\
         *)\n\
           printf 'server %s\\n' \"$$\" >> {invocations}\n\
           setsid bash -c 'trap \"\" TERM HUP; printf \"%s\\n\" \"$BASHPID\" > \"$1\"; while :; do sleep 1; done' shim-disconnect-descendant {resistant_pid} </dev/null >/dev/null 2>&1 &\n\
           ;;\n\
         esac\n\
         exec {} \"$@\"\n",
        shell_quote(&binary),
    );
    std::fs::write(&wrapper, script)?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
    Ok(wrapper)
}

#[derive(Clone, Copy, Debug)]
enum BackgroundCase {
    Cargo,
    PeriodicSweep,
}

impl BackgroundCase {
    fn name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::PeriodicSweep => "periodic-sweep",
        }
    }
}

fn write_executable(path: &Path, script: &str) -> TestResult {
    std::fs::write(path, script)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn watched_project(home: &Path) -> TestResult<PathBuf> {
    let repo = home.join(WATCHED_PROJECT);
    std::fs::create_dir_all(repo.join("src"))?;
    let manifest = repo.join("Cargo.toml");
    let source = repo.join("src/main.rs");
    std::fs::write(
        &manifest,
        "[package]\nname = \"shutdown-watch-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n",
    )?;
    std::fs::write(&source, "fn main() {}\n")?;
    // Suppress a startup build; each fixture mutates this after initialize.
    for path in [&manifest, &source] {
        File::open(path)?.set_modified(UNIX_EPOCH + Duration::from_secs(1))?;
    }
    Ok(repo)
}

fn held_start_scripts(home: &Path) -> TestResult<PathBuf> {
    let binary = std::fs::canonicalize(real_server_binary())?;
    let invocations = shell_quote(&home.join(HELD_START_INVOCATIONS));
    let identity = "read -r stat < \"/proc/$BASHPID/stat\"\n\
                    read -r -a fields <<< \"${stat##*) }\"\n";
    let invocation = format!(
        "printf -v arguments ' %q' \"$@\"\n\
         printf '%s %s %s unix_s=%s cwd=%q argc=%s argv=%s\\n' \"$kind\" \"$BASHPID\" \"${{fields[19]}}\" \"$EPOCHREALTIME\" \"$PWD\" \"$#\" \"$arguments\" >> {invocations}\n",
    );
    let lifetime = BACKGROUND_HELPER_LIFETIME.as_secs();
    let cargo = home.join(CONTROLLED_CARGO);
    write_executable(
        &cargo,
        &format!(
            "#!/bin/bash\n\
             set -eu\n\
             [[ ${{1-}} == build && ( $# == 1 || ( $# == 2 && ${{2-}} == --release ) ) ]] || exit 125\n\
             {identity}\
             kind=cargo-start\n\
             {invocation}\
             printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {}\n\
             while [[ ! -f {} ]]; do\n\
               (( SECONDS < {lifetime} )) || exit 124\n\
               /bin/sleep 0.02\n\
             done\n\
             kind=cargo-release\n\
             {invocation}\
             printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {}\n\
             exit 0\n",
            shell_quote(&home.join(HELD_START_CARGO_IDENTITY)),
            shell_quote(&home.join(HELD_START_CARGO_RELEASE)),
            shell_quote(&home.join(HELD_START_CARGO_FINISHED)),
        ),
    )?;
    let server = home.join(HELD_START_SERVER);
    // Only log and gate process launch. Every MCP byte and CLI operation is
    // handled by the selected real backend, and CARGO never runs a compiler.
    write_executable(
        &server,
        &format!(
            "#!/bin/bash\n\
             set -eu\n\
             {identity}\
             case \"${{1-}}\" in\n\
               --describe) kind=describe ;;\n\
               --sweep-workdirs) kind=sweep ;;\n\
               *) kind=server ;;\n\
             esac\n\
             {invocation}\
             if [[ $kind == server ]]; then\n\
               printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {}\n\
               while [[ ! -f {} ]]; do\n\
                 (( SECONDS < {lifetime} )) || exit 124\n\
                 /bin/sleep 0.02\n\
               done\n\
               kind=server-delegate\n\
               {invocation}\
             fi\n\
             exec {} \"$@\"\n",
            shell_quote(&home.join(HELD_START_SERVER_IDENTITY)),
            shell_quote(&home.join(HELD_START_SERVER_RELEASE)),
            shell_quote(&binary),
        ),
    )?;
    Ok(server)
}

fn background_scripts(home: &Path, case: BackgroundCase) -> TestResult<PathBuf> {
    let invocations = shell_quote(&home.join(BACKGROUND_INVOCATIONS));
    let helper_identity = shell_quote(&home.join(BACKGROUND_HELPER));
    let descendant_identity = shell_quote(&home.join(BACKGROUND_DESCENDANT));
    let expired = shell_quote(&home.join(BACKGROUND_EXPIRED));
    let descendant = home.join(BACKGROUND_DESCENDANT_SCRIPT);
    // Bash reads its own stat without spawning a reader. The identity marker
    // includes field 22 (birth time), even if the command name contains spaces.
    let identity = "read -r stat < \"/proc/$BASHPID/stat\"\n\
                    read -r -a fields <<< \"${stat##*) }\"\n";
    let lifetime = BACKGROUND_HELPER_LIFETIME.as_secs();
    write_executable(
        &descendant,
        &format!(
            "#!/bin/bash\n\
             set -eu\n\
             trap '' TERM HUP\n\
             /bin/sleep {lifetime} &\n\
             lease=$!\n\
             {identity}\
             printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {descendant_identity}\n\
             printf 'background descendant pid=%s birth=%s lease=%s lifetime_s={lifetime}\\n' \"$BASHPID\" \"${{fields[19]}}\" \"$lease\" >&2\n\
             wait \"$lease\"\n\
             printf 'descendant %s expired\\n' \"$BASHPID\" >> {expired}\n\
             exit 124\n",
        ),
    )?;
    let invocation = format!(
        "{identity}\
         {{ printf '%s %s %s cwd=%q argc=%s argv=' \"$kind\" \"$BASHPID\" \"${{fields[19]}}\" \"$PWD\" \"$#\"; printf ' %q' \"$@\"; printf '\\n'; }} >> {invocations}\n\
         printf 'background invocation kind=%s pid=%s birth=%s argc=%s\\n' \"$kind\" \"$BASHPID\" \"${{fields[19]}}\" \"$#\" >&2\n",
    );
    let hold = format!(
        "trap '' TERM HUP\n\
         /bin/sleep {lifetime} &\n\
         lease=$!\n\
         setsid /bin/bash {} </dev/null &\n\
         descendant=$!\n\
         {identity}\
         printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {helper_identity}\n\
         printf 'background helper kind=%s pid=%s birth=%s descendant=%s lease=%s lifetime_s={lifetime}\\n' \"$kind\" \"$BASHPID\" \"${{fields[19]}}\" \"$descendant\" \"$lease\" >&2\n\
         wait \"$lease\"\n\
         printf 'helper %s expired\\n' \"$BASHPID\" >> {expired}\n\
         exit 124\n",
        shell_quote(&descendant),
    );
    let (name, script) = match case {
        BackgroundCase::Cargo => (
            CONTROLLED_CARGO,
            format!(
                "#!/bin/bash\n\
                 set -eu\n\
                 [[ ${{1-}} == build && ( $# == 1 || ( $# == 2 && ${{2-}} == --release ) ) ]] || exit 125\n\
                 kind=cargo\n\
                 {invocation}\
                 {hold}",
            ),
        ),
        BackgroundCase::PeriodicSweep => {
            let binary = std::fs::canonicalize(real_server_binary())?;
            let server_identity = shell_quote(&home.join(BACKGROUND_SERVER));
            let server_release = shell_quote(&home.join(BACKGROUND_SERVER_RELEASE));
            (
                PERIODIC_SWEEP_SERVER,
                format!(
                    "#!/bin/bash\n\
                     set -eu\n\
                     case \"${{1-}}\" in\n\
                       --describe) kind=describe ;;\n\
                       --sweep-workdirs)\n\
                         if (( $# == 1 )); then kind=periodic-sweep; else kind=scoped-sweep; fi ;;\n\
                       *) kind=server ;;\n\
                     esac\n\
                     {invocation}\
                     if [[ $kind == server ]]; then\n\
                       printf '%s %s\\n' \"$BASHPID\" \"${{fields[19]}}\" > {server_identity}\n\
                       while [[ ! -f {server_release} ]]; do\n\
                         (( SECONDS < {lifetime} )) || exit 124\n\
                         /bin/sleep 0.02\n\
                       done\n\
                     fi\n\
                     if [[ $kind != periodic-sweep ]]; then exec {} \"$@\"; fi\n\
                     {hold}",
                    shell_quote(&binary),
                ),
            )
        }
    };
    let wrapper = home.join(name);
    write_executable(&wrapper, &script)?;
    Ok(wrapper)
}

fn read_background_identity(path: &Path) -> TestResult<Option<OwnedProcess>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // An unfinished marker is retried; PID reuse is rejected after parsing.
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() != 2 || !text.ends_with('\n') {
        return Ok(None);
    }
    let pid: i32 = fields[0].parse()?;
    let birth: u64 = fields[1].parse()?;
    let process = OwnedProcess::read(pid)?;
    assert_eq!(process.starttime, birth, "helper identity changed: {text}");
    Ok(Some(process))
}

fn assert_background_process_running(process: &OwnedProcess) -> TestResult<procfs::process::Stat> {
    let stat = process
        .stat()
        .ok_or_else(|| io::Error::other(format!("background process exited: {process:?}")))?;
    assert!(
        !matches!(stat.state, 'Z' | 'X' | 'T' | 't'),
        "background process is not running: {process:?}, state={}",
        stat.state
    );
    Ok(stat)
}

fn background_process_receipt(process: &OwnedProcess) -> Value {
    let stat = process.stat();
    let argv = stat.as_ref().and_then(|_| {
        std::fs::read(format!("/proc/{}/cmdline", process.pid))
            .ok()
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter(|argument| !argument.is_empty())
                    .map(|argument| String::from_utf8_lossy(argument).into_owned())
                    .collect::<Vec<_>>()
            })
    });
    json!({
        "pid": process.pid,
        "starttime": process.starttime,
        "comm": process.comm,
        "present": stat.is_some(),
        "ppid": stat.as_ref().map(|stat| stat.ppid),
        "sid": stat.as_ref().map(|stat| stat.session),
        "pgrp": stat.as_ref().map(|stat| stat.pgrp),
        "state": stat.as_ref().map(|stat| stat.state.to_string()),
        "argv": argv,
    })
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

    fn signal(&self, signal: Signal) -> TestResult {
        if self.stat().is_none() {
            return Err(io::Error::other(format!("owned process disappeared: {self:?}")).into());
        }
        kill(Pid::from_raw(self.pid), signal)?;
        Ok(())
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
            self.capture_tree()?;
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
        // A failed assertion must release a test-stopped server before TERM.
        // Every signal still checks the recorded PID and birth time.
        self.signal_owned(Signal::SIGCONT);
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
    endpoint: Endpoint,
    home: PrivateHome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Endpoint {
    Shim,
    StandaloneServer,
}

struct HeadlessSession {
    session_id: Option<String>,
    server: OwnedProcess,
    workdir: PathBuf,
    disk: PathBuf,
    ttl_expirations_before: usize,
}

struct StopRetryFault {
    original: PathBuf,
    parked: PathBuf,
    directory: std::fs::Metadata,
    placeholder: Option<File>,
    active: bool,
}

impl StopRetryFault {
    fn install(session: &HeadlessSession) -> TestResult<Self> {
        assert!(session.server.stat().is_some(), "owned server exited");
        let directory = std::fs::symlink_metadata(&session.workdir)?;
        assert!(directory.is_dir(), "owned workdir is not a directory");
        assert_eq!(directory.uid(), std::fs::metadata("/proc/self")?.uid());
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let parked = session.workdir.with_file_name(format!(
            "kwin-mcp-stop-retry-{}-{nonce}",
            session.server.pid
        ));
        assert_absent(&parked)?;
        std::fs::rename(&session.workdir, &parked)?;
        // Arm restoration before creating the placeholder. A setup error or
        // assertion unwinds this guard before Connection tears down processes.
        let mut fault = Self {
            original: session.workdir.clone(),
            parked,
            directory,
            placeholder: None,
            active: true,
        };
        fault.placeholder = Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&fault.original)?,
        );
        // Permission repair skips regular files; remove_dir_all returns
        // ENOTDIR. Teardown still removes the disk twin by the original name.
        Ok(fault)
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let directory = std::fs::symlink_metadata(&self.parked)?;
        if !directory.is_dir()
            || directory.dev() != self.directory.dev()
            || directory.ino() != self.directory.ino()
        {
            return Err(io::Error::other("parked workdir identity changed"));
        }
        match std::fs::symlink_metadata(&self.original) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(current) => {
                let placeholder = self
                    .placeholder
                    .as_ref()
                    .ok_or_else(|| io::Error::other("workdir path has an unowned entry"))?
                    .metadata()?;
                if !current.is_file()
                    || current.dev() != placeholder.dev()
                    || current.ino() != placeholder.ino()
                {
                    return Err(io::Error::other("workdir placeholder identity changed"));
                }
                std::fs::remove_file(&self.original)?;
            }
        }
        std::fs::rename(&self.parked, &self.original)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for StopRetryFault {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!(
                "could not restore stop-retry workdir {} from {}: {error}",
                self.original.display(),
                self.parked.display()
            );
        }
    }
}

fn assert_absent(path: &Path) -> TestResult {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(io::Error::other(format!("path survived: {}", path.display())).into()),
    }
}

impl Connection {
    fn start(launcher: bool) -> TestResult<Self> {
        Self::start_with_recording_server(launcher, false)
    }

    fn start_with_recording_server(launcher: bool, record_server: bool) -> TestResult<Self> {
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
        if record_server {
            assert!(
                !launcher,
                "the recording fixture launches the shim directly"
            );
            command.env("KWIN_MCP_BINARY", recording_server(&home.0)?);
        }
        Self::connect(home, command, launcher, Endpoint::Shim)
    }

    fn start_background(case: BackgroundCase) -> TestResult<(Self, Instant)> {
        let home = PrivateHome::create()?;
        let wrapper = background_scripts(&home.0, case)?;
        let mut command = shim_command(&home.0);
        // Even a delegated final sweep sees only the private CLI fixture root.
        command.env("TMPDIR", &home.1);
        match case {
            BackgroundCase::Cargo => {
                let repo = watched_project(&home.0)?;
                command.env("KWIN_MCP_REPO", &repo).env("CARGO", &wrapper);
            }
            BackgroundCase::PeriodicSweep => {
                command.env("KWIN_MCP_BINARY", &wrapper);
            }
        }
        let started = Instant::now();
        let connection = Self::connect(home, command, false, Endpoint::Shim)?;
        connection.record_background_receipt(case, "initialized", json!({}))?;
        Ok((connection, started))
    }

    fn start_held_start() -> TestResult<Self> {
        let home = PrivateHome::create()?;
        let repo = watched_project(&home.0)?;
        let server = held_start_scripts(&home.0)?;
        let mut command = shim_command(&home.0);
        command
            .env("TMPDIR", &home.1)
            .env("KWIN_MCP_REPO", &repo)
            .env("CARGO", home.0.join(CONTROLLED_CARGO))
            .env("KWIN_MCP_BINARY", server);
        Self::connect(home, command, false, Endpoint::Shim)
    }

    fn record_held_start_receipt(&self, event: &str, details: Value) -> TestResult {
        let invocations = std::fs::read_to_string(self.home.0.join(HELD_START_INVOCATIONS))?;
        let receipt = json!({
            "event": event,
            "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            "shim_pid": self.shim,
            "original_connection": self.processes.owned.get(&self.shim).map(background_process_receipt),
            "shim_binary": shim_binary(),
            "shim_argv": ["--autoclean", "--no-viewer"],
            "real_server_binary": std::fs::canonicalize(real_server_binary())?,
            "server_wrapper": self.home.0.join(HELD_START_SERVER),
            "cargo_wrapper": self.home.0.join(CONTROLLED_CARGO),
            "private_home": self.home.0,
            "tmpdir": self.home.1,
            "watched_project": self.home.0.join(WATCHED_PROJECT),
            "stdin_retained": self.input.is_some(),
            "stdout_retained": self.output.is_some(),
            "server_invocations": invocations.lines().filter(|line| line.starts_with("server ")).count(),
            "invocations": invocations,
            "responses": std::fs::read_to_string(self.home.0.join(HELD_START_RESPONSES)).ok(),
            "processes": self.recorded_tree().iter().map(background_process_receipt).collect::<Vec<_>>(),
            "details": details,
        });
        let mut output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.0.join(HELD_START_RECEIPTS))?;
        writeln!(output, "{receipt}")?;
        output.flush()?;
        eprintln!("held start cancellation receipt: {receipt}");
        self.retain_proof()
    }

    fn held_start_process(&mut self, marker: &str) -> TestResult<OwnedProcess> {
        let deadline = Instant::now() + BACKGROUND_READY_WAIT;
        loop {
            self.processes.capture_tree()?;
            if let Some(process) = read_background_identity(&self.home.0.join(marker))? {
                self.processes.capture_tree()?;
                assert!(
                    self.processes
                        .owned
                        .get(&process.pid)
                        .is_some_and(|owned| owned.starttime == process.starttime),
                    "held start marker is not an owned process: {process:?}"
                );
                assert_eq!(assert_background_process_running(&process)?.ppid, self.shim);
                return Ok(process);
            }
            assert!(self.processes.child.try_wait()?.is_none(), "{}", self.log());
            assert!(
                Instant::now() < deadline,
                "marker {marker} timed out: {}",
                self.log()
            );
            thread::sleep(POLL_PAUSE);
        }
    }

    fn record_background_receipt(
        &self,
        case: BackgroundCase,
        event: &str,
        details: Value,
    ) -> TestResult {
        let repo = self.home.0.join(WATCHED_PROJECT);
        let receipt = json!({
            "case": case.name(),
            "event": event,
            "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            "shim_pid": self.shim,
            "shim_binary": shim_binary(),
            "shim_argv": ["--autoclean", "--no-viewer"],
            "real_server_binary": real_server_binary(),
            "wrapper": self.home.0.join(match case {
                BackgroundCase::Cargo => CONTROLLED_CARGO,
                BackgroundCase::PeriodicSweep => PERIODIC_SWEEP_SERVER,
            }),
            "private_home": self.home.0,
            "tmpdir": self.home.1,
            "watched_project": repo,
            "watched_manifest": std::fs::read_to_string(repo.join("Cargo.toml")).ok(),
            "watched_source": std::fs::read_to_string(repo.join("src/main.rs")).ok(),
            "helper_lifetime_ms": BACKGROUND_HELPER_LIFETIME.as_millis(),
            "shutdown_bound_ms": BACKGROUND_EXIT_WAIT.as_millis(),
            "stdin_retained": self.input.is_some(),
            "stdout_retained": self.output.is_some(),
            "invocations": std::fs::read_to_string(self.home.0.join(BACKGROUND_INVOCATIONS)).ok(),
            "helper_identity": std::fs::read_to_string(self.home.0.join(BACKGROUND_HELPER)).ok(),
            "descendant_identity": std::fs::read_to_string(self.home.0.join(BACKGROUND_DESCENDANT)).ok(),
            "processes": self.recorded_tree().iter().map(background_process_receipt).collect::<Vec<_>>(),
            "details": details,
        });
        let mut output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.0.join(BACKGROUND_RECEIPTS))?;
        writeln!(output, "{receipt}")?;
        output.flush()?;
        eprintln!("background shutdown receipt: {receipt}");
        self.retain_proof()
    }

    fn background_helpers(&mut self) -> TestResult<(OwnedProcess, OwnedProcess)> {
        let deadline = Instant::now() + BACKGROUND_READY_WAIT;
        loop {
            // Capture ancestry before consulting a marker. The marker alone
            // never grants signaling or cleanup ownership of a PID.
            self.processes.capture_tree()?;
            let helper = read_background_identity(&self.home.0.join(BACKGROUND_HELPER))?;
            let descendant = read_background_identity(&self.home.0.join(BACKGROUND_DESCENDANT))?;
            if let (Some(helper), Some(descendant)) = (helper, descendant) {
                self.processes.capture_tree()?;
                for process in [&helper, &descendant] {
                    assert!(
                        self.processes
                            .owned
                            .get(&process.pid)
                            .is_some_and(|owned| owned.starttime == process.starttime),
                        "marker does not match a captured owned process: {process:?}"
                    );
                }
                let helper_stat = assert_background_process_running(&helper)?;
                let descendant_stat = assert_background_process_running(&descendant)?;
                assert_eq!(helper_stat.ppid, self.shim, "shim launched its helper");
                assert_eq!(descendant_stat.ppid, helper.pid, "helper owns descendant");
                assert_eq!(descendant_stat.session, descendant.pid, "setsid succeeded");
                assert_ne!(descendant_stat.session, helper_stat.session);
                // Both scripts spawn their bounded sleep before their marker.
                let leases_ready = [&helper, &descendant].iter().all(|parent| {
                    self.processes.owned.values().any(|process| {
                        process.stat().is_some_and(|stat| {
                            stat.comm == "sleep"
                                && stat.ppid == parent.pid
                                && !matches!(stat.state, 'Z' | 'X')
                        })
                    })
                });
                if !leases_ready {
                    assert!(Instant::now() < deadline, "bounded leases did not start");
                    thread::sleep(POLL_PAUSE);
                    continue;
                }
                let status = std::fs::read_to_string(format!("/proc/{}/status", descendant.pid))?;
                let ignored = status
                    .lines()
                    .find_map(|line| line.strip_prefix("SigIgn:\t"))
                    .ok_or_else(|| io::Error::other("descendant has no ignored-signal mask"))?;
                let ignored = u64::from_str_radix(ignored.trim(), 16)?;
                let term_bit = u32::try_from(nix::libc::SIGTERM - 1)?;
                assert_ne!(
                    ignored & (1u64 << term_bit),
                    0,
                    "descendant must ignore TERM"
                );
                descendant.signal(Signal::SIGTERM)?;
                assert_background_process_running(&descendant)?;
                return Ok((helper, descendant));
            }
            assert!(
                self.processes.child.try_wait()?.is_none(),
                "shim exited before its background helper was ready: {}",
                self.log()
            );
            assert!(
                Instant::now() < deadline,
                "background helper did not become ready: {}",
                self.log()
            );
            thread::sleep(POLL_PAUSE);
        }
    }

    fn release_background_server(&mut self, case: BackgroundCase) -> TestResult<OwnedProcess> {
        let deadline = Instant::now() + BACKGROUND_READY_WAIT;
        loop {
            self.processes.capture_tree()?;
            if let Some(server) = read_background_identity(&self.home.0.join(BACKGROUND_SERVER))? {
                self.processes.capture_tree()?;
                assert!(
                    self.processes
                        .owned
                        .get(&server.pid)
                        .is_some_and(|owned| owned.starttime == server.starttime),
                    "normal stdio server wrapper is not in the captured owned tree"
                );
                let stat = assert_background_process_running(&server)?;
                assert_eq!(stat.ppid, self.shim);
                assert_eq!(stat.session, server.pid);
                self.record_background_receipt(
                    case,
                    "server-captured-before-delegation",
                    json!({"server":background_process_receipt(&server)}),
                )?;
                std::fs::write(self.home.0.join(BACKGROUND_SERVER_RELEASE), b"delegate\n")?;
                return Ok(server);
            }
            assert!(
                self.processes.child.try_wait()?.is_none(),
                "shim exited before its server"
            );
            assert!(
                Instant::now() < deadline,
                "normal stdio server wrapper did not start"
            );
            thread::sleep(POLL_PAUSE);
        }
    }

    fn assert_background_shutdown(
        &mut self,
        case: BackgroundCase,
        started: Instant,
        helper: &OwnedProcess,
        descendant: &OwnedProcess,
    ) -> TestResult {
        self.processes.capture_tree()?;
        let recorded = self.recorded_tree();
        let recorded_before_eof: Vec<_> = recorded.iter().map(background_process_receipt).collect();
        assert!(
            !recorded
                .iter()
                .any(|process| process.comm == "kwin_wayland"),
            "background fixture started a desktop"
        );
        self.record_background_receipt(
            case,
            "before-eof",
            json!({
                "helper": background_process_receipt(helper),
                "term_resistant_descendant": background_process_receipt(descendant),
                "fixture_elapsed_ms": started.elapsed().as_millis(),
            }),
        )?;
        // Proof copying can take time. Check liveness and the TTL margin again
        // immediately before dropping the only stdin writer.
        assert!(self.input.is_some());
        assert!(self.processes.child.try_wait()?.is_none());
        assert_background_process_running(helper)?;
        assert_background_process_running(descendant)?;
        assert_absent(&self.home.0.join(BACKGROUND_EXPIRED))?;
        assert!(
            started.elapsed() + BACKGROUND_EXIT_WAIT < BACKGROUND_HELPER_LIFETIME,
            "fixture has insufficient time left before its helper lifetime"
        );
        let disconnected = Instant::now();
        self.disconnect_input();
        let status = self.processes.wait_for_child(BACKGROUND_EXIT_WAIT)?;
        let elapsed = disconnected.elapsed();
        // Check identities before remaining(), reap(), or guard cleanup can
        // hide a leaked process or an adopted zombie. There is no cleanup wait.
        let survivors: Vec<_> = self
            .recorded_tree()
            .iter()
            .filter_map(|process| process.stat().map(|_| background_process_receipt(process)))
            .collect();
        let fixture_elapsed = started.elapsed();
        self.record_background_receipt(
            case,
            "shim-exit",
            json!({
                "exit_status": status.to_string(),
                "exit_success": status.success(),
                "elapsed_ms": elapsed.as_millis(),
                "fixture_elapsed_ms": fixture_elapsed.as_millis(),
                "recorded_before_eof": recorded_before_eof,
                "survivors_at_exit": survivors,
                "helper_lifetime_expired": self.home.0.join(BACKGROUND_EXPIRED).try_exists()?,
            }),
        )?;
        assert!(status.success(), "shim failed: {status}\n{}", self.log());
        assert!(
            elapsed < BACKGROUND_EXIT_WAIT,
            "shutdown exceeded its bound"
        );
        assert!(
            fixture_elapsed < BACKGROUND_HELPER_LIFETIME,
            "helper lifetime cannot count as shutdown cleanup"
        );
        assert_absent(&self.home.0.join(BACKGROUND_EXPIRED))?;
        assert!(
            survivors.is_empty(),
            "background identities survived shim exit: {survivors:?}\n{}",
            self.log()
        );
        assert!(
            self.output.is_some(),
            "test must retain stdout at shim exit"
        );
        eprintln!(
            "background_eof: case={} shim_pid={} helper={helper:?} term_resistant={descendant:?} elapsed_ms={} identities_absent_at_exit=true helper_lifetime_expired=false stdout_retained=true",
            case.name(),
            self.shim,
            elapsed.as_millis()
        );
        Ok(())
    }

    fn start_ttl(endpoint: Endpoint) -> TestResult<Self> {
        assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
        for name in ["KWIN_MCP_E2E_SHIM", "KWIN_MCP_E2E_SERVER", PROOF_DIR_ENV] {
            assert!(
                std::env::var_os(name).is_some_and(|value| !value.is_empty()),
                "TTL proof requires {name}"
            );
        }
        let home = PrivateHome::create()?;
        let mut command = match endpoint {
            Endpoint::Shim => shim_command(&home.0),
            Endpoint::StandaloneServer => {
                let mut command = Command::new(real_server_binary());
                command.args(["--autoclean", "--no-viewer"]);
                configure_private_home(&mut command, &home.0);
                command
            }
        };
        command.args(["--ttl", TTL_MINUTES]);
        let connection = Self::connect(home, command, false, endpoint)?;
        connection.record_ttl_receipt("initialized", None, json!({}))?;
        Ok(connection)
    }

    fn connect(
        home: PrivateHome,
        mut command: Command,
        launcher: bool,
        endpoint: Endpoint,
    ) -> TestResult<Self> {
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
            endpoint,
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

    fn start_headless(&mut self) -> TestResult<HeadlessSession> {
        let ttl_expirations_before = self.ttl_expirations();
        let started = self.rpc(
            2,
            "tools/call",
            json!({"name":"session_start","arguments":{"width":800,"height":600}}),
        )?;
        self.processes.capture_tree()?;
        std::fs::write(
            self.home.0.join("startup-response.json"),
            serde_json::to_vec_pretty(&started)?,
        )?;
        self.retain_proof()?;
        let session = &started["result"]["structuredContent"];
        assert_eq!(session["status"], "started", "{started}");
        assert_eq!(session["viewer"]["state"], "closed", "{started}");
        let session_id = session["session_id"].as_str().map(str::to_owned);
        let pid: i32 = match self.endpoint {
            Endpoint::Shim => session_id
                .as_deref()
                .and_then(|id| id.strip_prefix('s'))
                .ok_or_else(|| io::Error::other("session_start returned no shim session_id"))?
                .parse()?,
            Endpoint::StandaloneServer => self.processes.root,
        };
        let server = self
            .processes
            .owned
            .get(&pid)
            .cloned()
            .ok_or_else(|| io::Error::other("server is not in the test-owned tree"))?;
        let stat = server
            .stat()
            .ok_or_else(|| io::Error::other("owned server exited during session_start"))?;
        if self.endpoint == Endpoint::Shim {
            assert_eq!(stat.ppid, self.shim, "shim directly owns the real server");
            assert_eq!(stat.session, pid, "server owns its process session");
        }
        assert!(
            self.processes
                .owned
                .values()
                .any(|process| process.comm == "kwin_wayland" && process.stat().is_some()),
            "real compositor is in the owned tree"
        );
        let workdir = PathBuf::from(
            session["workdir"]
                .as_str()
                .ok_or_else(|| io::Error::other("session_start returned no workdir"))?,
        );
        assert_eq!(
            workdir,
            Path::new(LIVE_SOCKET_ROOT).join(format!("kwin-mcp-{pid}")),
            "live fixture uses the production socket root"
        );
        let disk = self
            .home
            .0
            .join(".cache/kwin-mcp")
            .join(format!("kwin-mcp-{pid}"));
        assert!(workdir.is_dir(), "socket workdir exists after startup");
        assert!(disk.is_dir(), "overlay directory exists after startup");
        Ok(HeadlessSession {
            session_id,
            server,
            workdir,
            disk,
            ttl_expirations_before,
        })
    }

    fn ttl_expirations(&self) -> usize {
        self.log()
            .lines()
            .filter(|line| line.starts_with("ttl: session idle"))
            .count()
    }

    fn record_ttl_receipt(
        &self,
        event: &str,
        session: Option<&HeadlessSession>,
        details: Value,
    ) -> TestResult {
        let session = session.map(|session| {
            json!({
                "session_id": session.session_id,
                "server_pid": session.server.pid,
                "server_starttime": session.server.starttime,
                "server_state": session.server.stat().map(|stat| stat.state.to_string()),
                "workdir": session.workdir,
                "workdir_exists": session.workdir.try_exists().ok(),
                "disk_workdir": session.disk,
                "disk_workdir_exists": session.disk.try_exists().ok(),
            })
        });
        let mut receipt = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.0.join(TTL_RECEIPTS))?;
        writeln!(
            receipt,
            "{}",
            json!({
                "event": event,
                "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
                "endpoint": format!("{:?}", self.endpoint),
                "connection_pid": self.processes.root,
                "shim_binary": shim_binary(),
                "server_binary": real_server_binary(),
                "ttl_minutes": TTL_MINUTES,
                "ttl_wait_ms": TTL_WAIT.as_millis(),
                "private_home": self.home.0,
                "live_tmpdir": LIVE_SOCKET_ROOT,
                "session": session,
                "processes": self.process_receipts(),
                "details": details,
            })
        )?;
        receipt.flush()?;
        self.retain_proof()
    }

    fn record_stop_retry_receipt(
        &self,
        event: &str,
        session: &HeadlessSession,
        fault: &StopRetryFault,
        details: Value,
    ) -> TestResult {
        let mut receipt = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.0.join(STOP_RETRY_RECEIPTS))?;
        writeln!(
            receipt,
            "{}",
            json!({
                "event": event,
                "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
                "shim_pid": self.shim,
                "shim_binary": shim_binary(),
                "server_binary": real_server_binary(),
                "session_id": session.session_id,
                "server_pid": session.server.pid,
                "server_starttime": session.server.starttime,
                "server_current_starttime": session.server.stat().map(|stat| stat.starttime),
                "workdir": session.workdir,
                "workdir_exists": session.workdir.try_exists().ok(),
                "workdir_is_file": session.workdir.is_file(),
                "disk_workdir": session.disk,
                "disk_workdir_exists": session.disk.try_exists().ok(),
                "parked_workdir": fault.parked,
                "parked_workdir_exists": fault.parked.try_exists().ok(),
                "parked_workdir_dev": fault.directory.dev(),
                "parked_workdir_ino": fault.directory.ino(),
                "fault_active": fault.active,
                "processes": self.process_receipts(),
                "details": details,
            })
        )?;
        receipt.flush()?;
        self.retain_proof()
    }

    fn only_connection_remains(&mut self) -> TestResult<bool> {
        self.processes.capture_tree()?;
        let remaining = self.processes.remaining();
        assert!(
            remaining
                .iter()
                .any(|process| process.pid == self.processes.root),
            "stdio endpoint exited while the connection was retained: {remaining:?}\n{}",
            self.log()
        );
        Ok(remaining.len() == 1)
    }

    fn ping_until(&mut self, deadline: Instant) -> TestResult<Value> {
        let response = self.rpc_until(3, "ping", json!({}), deadline)?;
        assert_eq!(response["result"], json!({}), "{response}");
        assert!(response.get("error").is_none(), "{response}");
        Ok(response)
    }

    fn session_listing_until(&mut self, deadline: Instant) -> TestResult<(Value, String)> {
        assert_eq!(self.endpoint, Endpoint::Shim);
        let response = self.rpc_until(
            4,
            "tools/call",
            json!({"name":"session_list","arguments":{}}),
            deadline,
        )?;
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        let listing = response["result"]["content"][0]["text"]
            .as_str()
            .ok_or_else(|| io::Error::other(format!("session_list returned no text: {response}")))?
            .to_owned();
        Ok((response, listing))
    }

    fn wait_for_ttl(&mut self, session: &HeadlessSession) -> TestResult {
        let waiting = Instant::now();
        let deadline = waiting + TTL_WAIT;
        loop {
            // These requests terminate at the shim or MCP transport and do
            // not call a tool that refreshes the real session's activity.
            let (listing_response, no_live_sessions) = match self.endpoint {
                Endpoint::Shim => {
                    let (response, listing) = self.session_listing_until(deadline)?;
                    let id = session
                        .session_id
                        .as_deref()
                        .ok_or_else(|| io::Error::other("shim TTL fixture has no session_id"))?;
                    let no_live_id = !listing
                        .lines()
                        .any(|line| line.starts_with(&format!("{id}:")));
                    (
                        Some(response),
                        no_live_id && listing.starts_with("No live sessions."),
                    )
                }
                Endpoint::StandaloneServer => (None, true),
            };
            let ping = self.ping_until(deadline)?;
            let root_only = self.only_connection_remains()?;
            let workdir_removed = !session.workdir.try_exists()?;
            let disk_removed = !session.disk.try_exists()?;
            let ttl_observed = self.ttl_expirations() > session.ttl_expirations_before;
            let expired =
                no_live_sessions && root_only && workdir_removed && disk_removed && ttl_observed;
            self.record_ttl_receipt(
                if expired { "ttl-expired" } else { "ttl-poll" },
                Some(session),
                json!({
                    "elapsed_ms": waiting.elapsed().as_millis(),
                    "session_list": listing_response,
                    "ping": ping,
                    "no_live_sessions": no_live_sessions,
                    "only_connection_remains": root_only,
                    "ttl_observed": ttl_observed,
                    "stdin_retained": self.input.is_some(),
                    "stdout_retained": self.output.is_some(),
                }),
            )?;
            assert!(
                waiting.elapsed() < TTL_WAIT,
                "TTL exceeded 90 seconds: {}",
                self.log()
            );
            if expired {
                assert_absent(&session.workdir)?;
                assert_absent(&session.disk)?;
                assert!(self.input.is_some(), "TTL must retain stdin");
                assert!(self.output.is_some(), "TTL must retain stdout");
                if self.endpoint == Endpoint::Shim {
                    assert!(session.server.stat().is_none(), "full server survived TTL");
                } else {
                    assert!(session.server.stat().is_some(), "standalone server exited");
                }
                return Ok(());
            }
            thread::sleep(TTL_POLL_PAUSE.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn resistant_descendant(&mut self, session: &HeadlessSession) -> TestResult<OwnedProcess> {
        let deadline = Instant::now() + EXIT_WAIT;
        let marker = self.home.0.join(RESISTANT_PID);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "descendant did not record its PID"
            );
            thread::sleep(POLL_PAUSE);
        };
        self.processes.capture_tree()?;
        let process = self
            .processes
            .owned
            .get(&pid)
            .cloned()
            .ok_or_else(|| io::Error::other("resistant descendant is not test-owned"))?;
        let stat = process
            .stat()
            .ok_or_else(|| io::Error::other("resistant descendant exited during setup"))?;
        assert_eq!(stat.ppid, session.server.pid, "real server owns descendant");
        assert_eq!(
            stat.session, process.pid,
            "descendant owns its process session"
        );
        assert_ne!(
            stat.session, session.server.pid,
            "descendant escaped server SID"
        );
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let ignored = status
            .lines()
            .find_map(|line| line.strip_prefix("SigIgn:\t"))
            .ok_or_else(|| io::Error::other("descendant has no ignored-signal mask"))?;
        let ignored = u64::from_str_radix(ignored.trim(), 16)?;
        let term_bit = u32::try_from(nix::libc::SIGTERM - 1)?;
        assert_ne!(
            ignored & (1u64 << term_bit),
            0,
            "descendant must ignore TERM"
        );
        process.signal(Signal::SIGTERM)?;
        assert!(process.stat().is_some(), "descendant must survive TERM");
        Ok(process)
    }

    fn recorded_tree(&self) -> Vec<OwnedProcess> {
        let mut recorded: Vec<_> = self.processes.owned.values().cloned().collect();
        recorded.sort_by_key(|process| process.pid);
        recorded
    }

    fn process_receipts(&self) -> Vec<Value> {
        self.recorded_tree()
            .iter()
            .map(|process| {
                json!({
                    "pid": process.pid,
                    "starttime": process.starttime,
                    "comm": process.comm,
                    "state": process.stat().map(|stat| stat.state.to_string()),
                })
            })
            .collect()
    }

    fn retain_proof(&self) -> TestResult {
        let Some(proof) = self.home.retain_diagnostics()? else {
            return Ok(());
        };
        std::fs::write(
            proof.join("owned-tree.json"),
            serde_json::to_vec_pretty(&json!({
                "shim_pid": self.shim,
                "private_home": self.home.0,
                "live_tmpdir": LIVE_SOCKET_ROOT,
                "private_cli_tmpdir": self.home.1,
                "server_binary": real_server_binary(),
                "shim_binary": shim_binary(),
                "processes": self.process_receipts(),
            }))?,
        )?;
        for process in self.processes.owned.values() {
            if process.stat().is_none() {
                continue;
            }
            let workdir = Path::new(LIVE_SOCKET_ROOT).join(format!("kwin-mcp-{}", process.pid));
            for name in ["kwin.log", "kwin_wayland.log", "kwin-wayland.log"] {
                copy_diagnostic(
                    &workdir.join(name),
                    &proof.join(format!("server-{}-{name}", process.pid)),
                )?;
            }
        }
        Ok(())
    }

    fn disconnect_input(&mut self) {
        // Drop the only writer into the actual client-to-shim pipe.
        self.input.take();
    }

    fn assert_session_removed(&mut self, session: &HeadlessSession) -> TestResult {
        let remaining = self.processes.remaining();
        assert!(
            remaining.is_empty(),
            "owned tree survived shim exit: {remaining:?}\n{}",
            self.log()
        );
        assert_absent(&session.workdir)?;
        assert_absent(&session.disk)?;
        Ok(())
    }

    fn assert_no_later_reaper(&mut self, session: &HeadlessSession) -> TestResult {
        // Require cleanup before observing, without launching another server
        // or invoking --sweep-workdirs from the test to repair a leak.
        self.assert_session_removed(session)?;
        let invocations = self.home.0.join(SERVER_INVOCATIONS);
        let at_exit = std::fs::read_to_string(&invocations)?;
        assert_eq!(
            at_exit
                .lines()
                .filter(|line| line.starts_with("server "))
                .count(),
            1,
            "shutdown must not start a replacement server: {at_exit}"
        );
        let deadline = Instant::now() + POST_EXIT_OBSERVE;
        while Instant::now() < deadline {
            thread::sleep(POLL_PAUSE);
            assert_eq!(
                std::fs::read_to_string(&invocations)?,
                at_exit,
                "a server or workdir reaper ran after shim exit"
            );
            self.assert_session_removed(session)?;
        }
        Ok(())
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
        self.rpc_until(id, method, params, Instant::now() + RESPONSE_WAIT)
    }

    fn rpc_until(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> TestResult<Value> {
        let deadline = deadline.min(Instant::now() + RESPONSE_WAIT);
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        self.response_until(id, deadline)
    }

    fn response_until(&mut self, id: u64, deadline: Instant) -> TestResult<Value> {
        self.response_value_until(&json!(id), deadline, None)
    }

    fn response_value_until(
        &mut self,
        id: &Value,
        deadline: Instant,
        transcript: Option<&Path>,
    ) -> TestResult<Value> {
        loop {
            let value = self.message_until(deadline)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("unexpected stdout EOF: {}", self.log()),
                )
            })?;
            if let Some(path) = transcript {
                append_response(path, &value)?;
            }
            if value.get("id") == Some(id) {
                return Ok(value);
            }
        }
    }

    fn message_until(&mut self, deadline: Instant) -> TestResult<Option<Value>> {
        let mut next_proof = Instant::now();
        loop {
            if std::env::var_os(PROOF_DIR_ENV).is_some() && Instant::now() >= next_proof {
                self.processes.capture_tree()?;
                // Snapshot available compositor logs before a failed startup
                // can autoclean its workdir. The merged stderr is copied too.
                self.retain_proof()?;
                next_proof = Instant::now() + PROOF_INTERVAL;
            }
            assert!(
                Instant::now() < deadline,
                "stdio message timed out: {}",
                self.log()
            );
            while let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.buffered.drain(..=end).collect();
                // The launching fixture also emits libtest progress lines.
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    return Ok(Some(value));
                }
            }
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
            if read == 0 {
                return Ok(None);
            }
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
        if let Err(error) = self.retain_proof() {
            eprintln!("could not retain owned shim proof: {error}");
        }
        self.input.take();
        self.output.take();
        // Field order drops the process guard before the private HOME.
    }
}

fn append_response(path: &Path, value: &Value) -> io::Result<()> {
    let mut output = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(output, "{value}")?;
    output.flush()
}

fn assert_cancelled_held_start(cancelled: Value, surviving: Value) -> TestResult {
    let mut connection = Connection::start_held_start()?;
    let original = OwnedProcess::read(connection.shim)?;
    let transcript = connection.home.0.join(HELD_START_RESPONSES);
    connection.record_held_start_receipt(
        "initialized",
        json!({"cancelled_id":cancelled, "surviving_id":surviving}),
    )?;
    let source = connection.home.0.join(WATCHED_PROJECT).join("src/main.rs");
    std::fs::write(&source, "fn main() { /* trigger the private watcher */ }\n")?;
    File::open(&source)?.set_modified(SystemTime::now())?;
    let cargo = connection.held_start_process(HELD_START_CARGO_IDENTITY)?;
    let process = procfs::process::Process::new(cargo.pid)?;
    assert_eq!(process.cwd()?, connection.home.0.join(WATCHED_PROJECT));
    let argv = process.cmdline()?;
    let build = argv
        .iter()
        .position(|argument| argument == "build")
        .ok_or_else(|| io::Error::other(format!("CARGO wrapper received no build: {argv:?}")))?;
    assert!(argv[build..] == ["build"] || argv[build..] == ["build", "--release"]);
    connection.record_held_start_receipt(
        "build-held",
        json!({"cargo":background_process_receipt(&cargo), "argv":argv}),
    )?;

    // Both IDs carry invalid dimensions, so even the unfixed shim can be
    // measured without a desktop. Invocation counts expose unwanted launches.
    let requests = [cancelled.clone(), surviving.clone()].map(|id| {
        json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
            "params":{"name":"session_start", "arguments":{"width":0,"height":600}}})
    });
    for request in &requests {
        connection.send(request.clone())?;
    }
    let cancellation = json!({"jsonrpc":"2.0", "method":"notifications/cancelled",
        "params":{"requestId":cancelled, "reason":"held start regression"}});
    connection.send(cancellation.clone())?;
    connection.send(json!({"jsonrpc":"2.0","id":194,"method":"ping"}))?;
    let barrier = connection.response_value_until(
        &json!(194),
        Instant::now() + RESPONSE_WAIT,
        Some(&transcript),
    )?;
    connection.record_held_start_receipt(
        "cancelled-before-build-release",
        json!({"requests":requests,"cancellation":cancellation,"ping_barrier":barrier}),
    )?;
    assert_eq!(barrier["result"], json!({}), "{barrier}");
    assert!(barrier.get("error").is_none(), "{barrier}");
    assert_eq!(
        connection
            .log()
            .matches("session_start waits for the kwin-mcp build in progress")
            .count(),
        2,
        "both requests must be held before cancellation: {}",
        connection.log()
    );
    assert_background_process_running(&cargo)?;
    assert_absent(&connection.home.0.join(HELD_START_CARGO_FINISHED))?;
    assert_absent(&connection.home.0.join(HELD_START_CARGO_RELEASE))?;
    let held_invocations = std::fs::read_to_string(connection.home.0.join(HELD_START_INVOCATIONS))?;
    assert_eq!(
        held_invocations
            .lines()
            .filter(|line| line.starts_with("server "))
            .count(),
        0
    );

    // The ping response orders cancellation processing before this release.
    std::fs::write(
        connection.home.0.join(HELD_START_CARGO_RELEASE),
        b"release\n",
    )?;
    connection
        .record_held_start_receipt("build-release-requested", json!({"ping_barrier":barrier}))?;
    let server = connection.held_start_process(HELD_START_SERVER_IDENTITY)?;
    assert_eq!(
        assert_background_process_running(&server)?.session,
        server.pid
    );
    assert_eq!(
        std::fs::read_to_string(connection.home.0.join(HELD_START_CARGO_FINISHED))?,
        format!("{} {}\n", cargo.pid, cargo.starttime),
        "the held build must actually reach its release"
    );
    connection.record_held_start_receipt(
        "build-released-backend-held",
        json!({"cargo":background_process_receipt(&cargo),"server":background_process_receipt(&server)}),
    )?;
    std::fs::write(
        connection.home.0.join(HELD_START_SERVER_RELEASE),
        b"delegate\n",
    )?;
    let response = connection.response_value_until(
        &surviving,
        Instant::now() + RESPONSE_WAIT,
        Some(&transcript),
    )?;
    connection
        .record_held_start_receipt("surviving-start-response", json!({"response":response}))?;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("width 0 out of range ")),
        "the surviving request must reach real dimension validation: {response}"
    );
    connection.send(json!({"jsonrpc":"2.0","id":195,"method":"ping"}))?;
    let ping = connection.response_value_until(
        &json!(195),
        Instant::now() + RESPONSE_WAIT,
        Some(&transcript),
    )?;
    assert_eq!(ping["result"], json!({}), "{ping}");
    assert!(ping.get("error").is_none(), "{ping}");
    let deadline = Instant::now() + EXIT_WAIT;
    while !connection.only_connection_remains()? {
        assert!(
            Instant::now() < deadline,
            "backend did not retire: {}",
            connection.log()
        );
        thread::sleep(POLL_PAUSE);
    }
    assert_eq!(
        OwnedProcess::read(connection.shim)?.starttime,
        original.starttime
    );
    assert!(connection.input.is_some() && connection.output.is_some());
    connection.record_held_start_receipt("original-connection-retained", json!({"ping":ping}))?;

    connection.disconnect_input();
    let status = connection.processes.wait_for_child(BACKGROUND_EXIT_WAIT)?;
    let survivors: Vec<_> = connection
        .recorded_tree()
        .iter()
        .filter_map(|process| process.stat().map(|_| background_process_receipt(process)))
        .collect();
    // Drain through actual stdout EOF. A late cancelled response cannot be
    // discarded by a wait for a different ID or hidden behind a ping barrier.
    let deadline = Instant::now() + EXIT_WAIT;
    while let Some(value) = connection.message_until(deadline)? {
        append_response(&transcript, &value)?;
    }
    connection.record_held_start_receipt(
        "cleanup",
        json!({"exit_status":status.to_string(),"exit_success":status.success(),
            "survivors_at_exit":survivors,"stdout_eof":true}),
    )?;
    assert!(
        status.success(),
        "shim failed: {status}\n{}",
        connection.log()
    );
    assert!(
        survivors.is_empty(),
        "owned processes survived cleanup: {survivors:?}"
    );

    let invocations = std::fs::read_to_string(connection.home.0.join(HELD_START_INVOCATIONS))?;
    let lines: Vec<_> = invocations.lines().collect();
    for (kind, process) in [
        ("cargo-start", &cargo),
        ("cargo-release", &cargo),
        ("server", &server),
        ("server-delegate", &server),
    ] {
        let matching: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with(&format!("{kind} ")))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "unexpected {kind} invocation count: {invocations}"
        );
        assert!(
            matching[0].starts_with(&format!("{kind} {} {} ", process.pid, process.starttime)),
            "invocation identity changed: {invocations}"
        );
    }
    assert!(
        lines.iter().any(|line| line.starts_with("describe ")),
        "metadata must use the real server"
    );
    let released = lines
        .iter()
        .position(|line| line.starts_with("cargo-release "));
    let launched = lines.iter().position(|line| line.starts_with("server "));
    assert!(
        released < launched,
        "backend launch must follow actual build release: {invocations}"
    );
    let responses: Vec<Value> = std::fs::read_to_string(&transcript)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert!(
        !responses
            .iter()
            .any(|value| value.get("id") == Some(&cancelled)),
        "cancelled held requests must receive no response: {responses:?}"
    );
    assert_eq!(
        responses
            .iter()
            .filter(|value| value.get("id") == Some(&surviving))
            .count(),
        1
    );
    assert!(connection.log().contains(&format!(
        "cancelled held session_start {cancelled}: held start regression"
    )));
    assert_eq!(
        connection
            .log()
            .lines()
            .filter(|line| line.starts_with("kwin-mcp v") && line.ends_with(" session_start"))
            .count(),
        1
    );
    assert!(
        !connection.log().contains("host_xdg_dir ready"),
        "validation must precede workdir creation"
    );
    for root in [Path::new(LIVE_SOCKET_ROOT), connection.home.1.as_path()] {
        assert_absent(&root.join(format!("kwin-mcp-{}", server.pid)))?;
    }
    Ok(())
}

#[test]
fn cancelled_numeric_held_start_preserves_same_valued_string_request() -> TestResult {
    assert_cancelled_held_start(json!(193), json!("193"))
}

#[test]
fn cancelled_string_held_start_preserves_same_valued_numeric_request() -> TestResult {
    assert_cancelled_held_start(json!("193"), json!(193))
}

#[test]
fn stdin_eof_drains_running_cargo_wrapper_and_its_descendants() -> TestResult {
    let case = BackgroundCase::Cargo;
    let (mut connection, started) = Connection::start_background(case)?;
    let source = connection.home.0.join(WATCHED_PROJECT).join("src/main.rs");
    std::fs::write(&source, "fn main() { /* trigger the private watcher */ }\n")?;
    File::open(&source)?.set_modified(SystemTime::now())?;
    connection.record_background_receipt(
        case,
        "private-source-changed",
        json!({"source":source}),
    )?;
    let (helper, descendant) = connection.background_helpers()?;
    let process = procfs::process::Process::new(helper.pid)?;
    assert_eq!(process.cwd()?, connection.home.0.join(WATCHED_PROJECT));
    let argv = process.cmdline()?;
    let build = argv
        .iter()
        .position(|argument| argument == "build")
        .ok_or_else(|| io::Error::other(format!("watcher did not invoke cargo build: {argv:?}")))?;
    assert!(
        argv[build..] == ["build"] || argv[build..] == ["build", "--release"],
        "unexpected controlled CARGO arguments: {argv:?}"
    );
    assert!(
        argv.iter().any(|argument| {
            argument
                == &connection
                    .home
                    .0
                    .join(CONTROLLED_CARGO)
                    .display()
                    .to_string()
        }),
        "watcher did not use the controlled CARGO path: {argv:?}"
    );
    assert!(
        connection
            .log()
            .contains("source changed; running cargo build"),
        "watcher did not report its real build invocation: {}",
        connection.log()
    );
    connection.assert_background_shutdown(case, started, &helper, &descendant)
}

#[test]
fn stdin_eof_drains_unscoped_periodic_sweep_and_its_descendants() -> TestResult {
    let case = BackgroundCase::PeriodicSweep;
    let (mut connection, started) = Connection::start_background(case)?;
    let response_deadline = Instant::now() + RESPONSE_WAIT;
    connection.send(json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"session_stop","arguments":{}}
    }))?;
    let server = connection.release_background_server(case)?;
    let stopped = connection.response_until(2, response_deadline)?;
    connection.record_background_receipt(
        case,
        "unbound-stop-response",
        json!({"response":stopped}),
    )?;
    assert!(stopped.get("error").is_none(), "{stopped}");
    assert_ne!(stopped["result"]["isError"], true, "{stopped}");
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "none",
        "{stopped}"
    );
    let (helper, descendant) = connection.background_helpers()?;
    let argv = procfs::process::Process::new(helper.pid)?.cmdline()?;
    assert_eq!(
        argv.last().map(String::as_str),
        Some("--sweep-workdirs"),
        "periodic sweep must have no owner arguments: {argv:?}"
    );
    assert_eq!(
        argv.iter()
            .filter(|argument| *argument == "--sweep-workdirs")
            .count(),
        1,
        "{argv:?}"
    );
    let invocations = std::fs::read_to_string(connection.home.0.join(BACKGROUND_INVOCATIONS))?;
    let servers: Vec<_> = invocations
        .lines()
        .filter(|line| line.starts_with("server "))
        .collect();
    assert_eq!(
        servers.len(),
        1,
        "stop must create one real server: {invocations}"
    );
    let fields: Vec<_> = servers[0].split_whitespace().collect();
    let pid: i32 = fields[1].parse()?;
    let birth: u64 = fields[2].parse()?;
    assert_eq!(server.pid, pid);
    assert_eq!(server.starttime, birth);
    assert!(
        server.stat().is_none(),
        "unbound stop did not retire its real server: {server:?}"
    );
    assert!(connection.log().contains(&format!("child {pid} exited (")));
    assert!(
        invocations
            .lines()
            .any(|line| line.starts_with("describe ")),
        "metadata was not delegated: {invocations}"
    );
    connection.assert_background_shutdown(case, started, &helper, &descendant)?;
    let final_invocations =
        std::fs::read_to_string(connection.home.0.join(BACKGROUND_INVOCATIONS))?;
    let scoped: Vec<_> = final_invocations
        .lines()
        .filter(|line| line.starts_with("scoped-sweep "))
        .collect();
    assert_eq!(
        scoped.len(),
        1,
        "final sweep was not delegated: {final_invocations}"
    );
    assert!(
        scoped[0]
            .split_whitespace()
            .any(|argument| argument == pid.to_string()),
        "final sweep did not select the retired server: {final_invocations}"
    );
    assert_eq!(
        final_invocations
            .lines()
            .filter(|line| line.starts_with("periodic-sweep "))
            .count(),
        1
    );
    connection.record_background_receipt(
        case,
        "scoped-final-sweep-delegated",
        json!({"owner_pid":pid}),
    )
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, a GPU session, both binary overrides, and proof receipts; no host viewer"]
fn shim_stop_cleanup_failure_retains_session_for_retry() -> TestResult {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    for name in ["KWIN_MCP_E2E_SHIM", "KWIN_MCP_E2E_SERVER", PROOF_DIR_ENV] {
        assert!(
            std::env::var_os(name).is_some_and(|value| !value.is_empty()),
            "stop-retry proof requires {name}"
        );
    }
    let mut connection = Connection::start(false)?;
    let session = connection.start_headless()?;
    let id = session
        .session_id
        .as_deref()
        .ok_or_else(|| io::Error::other("stop-retry fixture has no shim session_id"))?;
    let original_shim = OwnedProcess::read(connection.shim)?;
    connection.processes.capture_tree()?;
    let recorded = connection.recorded_tree();
    // Declare the fault after Connection so it restores the owned directory
    // before an assertion failure closes stdio and runs process cleanup.
    let mut fault = StopRetryFault::install(&session)?;
    connection.record_stop_retry_receipt("fault-installed", &session, &fault, json!({}))?;
    let failed = connection.rpc(
        5,
        "tools/call",
        json!({"name":"session_stop","arguments":{"session_id":id}}),
    )?;
    connection.record_stop_retry_receipt(
        "stop-response",
        &session,
        &fault,
        json!({"response":failed}),
    )?;
    assert_eq!(failed["error"]["code"], -32603, "{failed}");
    let error = failed["error"]["message"]
        .as_str()
        .ok_or_else(|| io::Error::other(format!("stop returned no cleanup error: {failed}")))?;
    assert!(
        error.contains(&session.workdir.display().to_string())
            && error.contains("Cleanup is still owned")
            && error.contains(&format!("os error {}", nix::libc::ENOTDIR)),
        "stop did not report the injected cleanup failure: {failed}"
    );
    assert!(session.workdir.is_file(), "cleanup removed the placeholder");
    assert!(fault.parked.is_dir(), "cleanup removed the parked workdir");
    let (listed, listing) = connection.session_listing_until(Instant::now() + RESPONSE_WAIT)?;
    connection.record_stop_retry_receipt(
        "route-readback",
        &session,
        &fault,
        json!({"session_list":listed}),
    )?;
    assert!(
        listing
            .lines()
            .any(|line| line.starts_with(&format!("{id}: server pid {},", session.server.pid))),
        "failed stop lost the original route: {listed}"
    );
    let retained_server = session
        .server
        .stat()
        .ok_or_else(|| io::Error::other("failed stop retired the original server"))?;
    assert_ne!(retained_server.state, 'Z', "original server is a zombie");
    assert_eq!(retained_server.ppid, connection.shim);
    assert_eq!(retained_server.starttime, session.server.starttime);
    fault.restore()?;
    assert!(
        session.workdir.is_dir(),
        "original workdir was not restored"
    );
    assert_absent(&fault.parked)?;
    connection.record_stop_retry_receipt("fault-restored", &session, &fault, json!({}))?;
    let stopped = connection.rpc(
        6,
        "tools/call",
        json!({"name":"session_stop","arguments":{"session_id":id}}),
    )?;
    connection.record_stop_retry_receipt(
        "retry-response",
        &session,
        &fault,
        json!({"response":stopped}),
    )?;
    assert!(stopped.get("error").is_none(), "{stopped}");
    assert_ne!(stopped["result"]["isError"], true, "{stopped}");
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "cleaned",
        "retry did not clean the original server's retained ownership: {stopped}"
    );
    assert_eq!(
        stopped["result"]["structuredContent"]["workdir_removed"],
        session.workdir.display().to_string(),
        "{stopped}"
    );
    let deadline = Instant::now() + EXIT_WAIT;
    while !connection.only_connection_remains()? {
        assert!(
            Instant::now() < deadline,
            "successful stop left owned processes"
        );
        thread::sleep(POLL_PAUSE);
    }
    for process in recorded
        .iter()
        .filter(|process| process.pid != connection.shim)
    {
        assert!(
            process.stat().is_none(),
            "recorded process survived: {process:?}"
        );
    }
    assert_absent(&session.workdir)?;
    assert_absent(&session.disk)?;
    assert_absent(&fault.parked)?;
    let (listed, listing) = connection.session_listing_until(Instant::now() + RESPONSE_WAIT)?;
    assert!(listing.starts_with("No live sessions."), "{listed}");
    let ping = connection.ping_until(Instant::now() + RESPONSE_WAIT)?;
    assert!(original_shim.stat().is_some(), "original shim was replaced");
    assert!(
        connection.input.is_some(),
        "successful stop must retain stdin"
    );
    assert!(
        connection.output.is_some(),
        "successful stop must retain stdout"
    );
    connection.record_stop_retry_receipt(
        "final-ping",
        &session,
        &fault,
        json!({
            "session_list":listed,
            "ping":ping,
            "original_shim_starttime":original_shim.starttime,
            "recorded_tree_removed":true,
            "stdin_retained":connection.input.is_some(),
            "stdout_retained":connection.output.is_some(),
        }),
    )?;
    connection.disconnect_input();
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    connection.wait_for_exit(EXIT_WAIT);
    eprintln!(
        "stop_retry: session_id={id} server={:?} original_route_retained=true fault_cleared=true same_id_retry=true recorded_owned_tree={recorded:?} owned_workdirs_removed=true parked_workdir_removed=true final_ping=true",
        session.server
    );
    Ok(())
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, a GPU session, both binary overrides, and proof receipts; no host viewer"]
fn shim_ttl_retires_two_headless_servers_on_one_connection() -> TestResult {
    let mut connection = Connection::start_ttl(Endpoint::Shim)?;
    let original_shim = connection
        .processes
        .owned
        .get(&connection.shim)
        .cloned()
        .ok_or_else(|| io::Error::other("TTL fixture did not record its shim"))?;
    let mut previous_id: Option<String> = None;
    let mut retired = Vec::with_capacity(2);
    for cycle in 1..=2 {
        let session = connection.start_headless()?;
        let id = session
            .session_id
            .as_deref()
            .ok_or_else(|| io::Error::other("TTL fixture has no shim session_id"))?;
        assert_ne!(
            previous_id.as_deref(),
            Some(id),
            "shim reused an expired ID"
        );
        connection.record_ttl_receipt(
            "session-started",
            Some(&session),
            json!({"cycle":cycle,"status":"started","viewer_state":"closed"}),
        )?;
        let (response, listing) =
            connection.session_listing_until(Instant::now() + RESPONSE_WAIT)?;
        assert!(
            listing
                .lines()
                .any(|line| line.starts_with(&format!("{id}:"))),
            "new session is absent from session_list: {response}"
        );
        connection.record_ttl_receipt(
            "session-listed",
            Some(&session),
            json!({"cycle":cycle,"session_list":response}),
        )?;
        connection.wait_for_ttl(&session)?;
        assert!(
            original_shim.stat().is_some(),
            "TTL replaced the original shim"
        );
        assert!(connection.processes.child.try_wait()?.is_none());
        previous_id = Some(id.to_owned());
        retired.push(session);
    }
    let ping = connection.ping_until(Instant::now() + RESPONSE_WAIT)?;
    assert!(connection.only_connection_remains()?);
    for session in &retired {
        assert!(session.server.stat().is_none(), "full server survived TTL");
        assert_absent(&session.workdir)?;
        assert_absent(&session.disk)?;
    }
    assert!(
        original_shim.stat().is_some(),
        "final ping used a replacement shim"
    );
    connection.record_ttl_receipt(
        "final-ping",
        None,
        json!({
            "cycles_completed":2,
            "original_shim_starttime":original_shim.starttime,
            "ping":ping,
            "stdin_retained":connection.input.is_some(),
            "stdout_retained":connection.output.is_some(),
        }),
    )?;
    connection.disconnect_input();
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    eprintln!(
        "shim_ttl: shim={original_shim:?} expired_cycles=2 live_sessions=0 full_servers=0 owned_workdirs=0 final_ping=true"
    );
    Ok(())
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, a GPU session, both binary overrides, and proof receipts; no host viewer"]
fn standalone_ttl_retains_stdio_and_accepts_a_new_headless_session() -> TestResult {
    let mut connection = Connection::start_ttl(Endpoint::StandaloneServer)?;
    let first = connection.start_headless()?;
    connection.record_ttl_receipt(
        "session-started",
        Some(&first),
        json!({"cycle":1,"status":"started","viewer_state":"closed"}),
    )?;
    connection.wait_for_ttl(&first)?;
    assert!(connection.processes.child.try_wait()?.is_none());
    let second = connection.start_headless()?;
    assert_eq!(second.server.pid, first.server.pid, "server PID changed");
    assert_eq!(
        second.server.starttime, first.server.starttime,
        "session_start used a replacement server"
    );
    connection.record_ttl_receipt(
        "session-started",
        Some(&second),
        json!({"cycle":2,"status":"started","viewer_state":"closed"}),
    )?;
    // Stop the restarted session immediately, without another idle wait.
    let stopped = connection.rpc(
        5,
        "tools/call",
        json!({"name":"session_stop","arguments":{}}),
    )?;
    assert_eq!(
        stopped["result"]["structuredContent"]["status"], "stopped",
        "{stopped}"
    );
    let deadline = Instant::now() + EXIT_WAIT;
    while !connection.only_connection_remains()? {
        assert!(
            Instant::now() < deadline,
            "restarted session left owned descendants"
        );
        thread::sleep(POLL_PAUSE);
    }
    assert_absent(&second.workdir)?;
    assert_absent(&second.disk)?;
    connection.record_ttl_receipt(
        "session-stopped",
        Some(&second),
        json!({"response":stopped}),
    )?;
    let ping = connection.ping_until(Instant::now() + RESPONSE_WAIT)?;
    assert!(
        first.server.stat().is_some(),
        "standalone stdio endpoint exited"
    );
    connection.record_ttl_receipt(
        "final-ping",
        Some(&second),
        json!({
            "sessions_started":2,
            "ttl_expirations":1,
            "second_session_stopped":true,
            "ping":ping,
            "stdin_retained":connection.input.is_some(),
            "stdout_retained":connection.output.is_some(),
        }),
    )?;
    connection.disconnect_input();
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    eprintln!(
        "standalone_ttl: server={:?} expired=true stdio_retained=true restarted=true second_session_stopped=true final_ping=true",
        first.server
    );
    Ok(())
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
    let session = connection.start_headless()?;
    let recorded = connection.recorded_tree();
    connection.disconnect_output()?;
    connection.wait_for_exit(LIVE_EXIT_WAIT);
    assert!(connection.input.is_some(), "test must retain stdin");
    connection.assert_session_removed(&session)?;
    assert!(connection.processes.wait_for_child(EXIT_WAIT)?.success());
    eprintln!(
        "live_stdout_disconnect: shim_pid={} server_pid={} recorded_owned_tree={recorded:?} socket_workdir={} socket_workdir_removed=true disk_workdir={} disk_workdir_removed=true stdin_retained=true exited=true",
        connection.shim,
        session.server.pid,
        session.workdir.display(),
        session.disk.display()
    );
    Ok(())
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, and a GPU session; no host viewer"]
fn stdin_eof_drains_stopped_server_and_term_resistant_descendant() -> TestResult {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut connection = Connection::start_with_recording_server(false, true)?;
    let session = connection.start_headless()?;
    let resistant = connection.resistant_descendant(&session)?;
    session.server.signal(Signal::SIGSTOP)?;
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        let stat = session
            .server
            .stat()
            .ok_or_else(|| io::Error::other("owned server exited before client EOF"))?;
        if stat.state == 'T' {
            break;
        }
        assert!(Instant::now() < deadline, "owned server did not stop");
        thread::sleep(POLL_PAUSE);
    }
    connection.processes.capture_tree()?;
    let recorded = connection.recorded_tree();
    let disconnected = Instant::now();
    connection.disconnect_input();
    let status = connection.processes.wait_for_child(LIVE_EXIT_WAIT)?;
    let elapsed = disconnected.elapsed();
    assert!(
        status.success(),
        "shim failed during EOF shutdown: {status}"
    );
    assert!(
        elapsed < LIVE_EXIT_WAIT,
        "EOF escalation exceeded its bound"
    );
    assert!(connection.output.is_some(), "test must retain stdout");
    connection.assert_session_removed(&session)?;
    connection.assert_no_later_reaper(&session)?;
    eprintln!(
        "stopped_server_eof: shim_pid={} server={:?} term_resistant={resistant:?} recorded_owned_tree={recorded:?} elapsed_ms={} socket_workdir={} disk_workdir={} removed_at_shim_exit=true later_reaper=false stdout_retained=true",
        connection.shim,
        session.server,
        elapsed.as_millis(),
        session.workdir.display(),
        session.disk.display()
    );
    Ok(())
}

#[test]
#[ignore = "requires KWin, bubblewrap, input devices, and a GPU session; no host viewer"]
fn last_child_exit_then_stdin_eof_drains_its_descendants() -> TestResult {
    assert_eq!(std::env::var("KWIN_MCP_E2E").as_deref(), Ok("1"));
    let mut connection = Connection::start_with_recording_server(false, true)?;
    let session = connection.start_headless()?;
    let resistant = connection.resistant_descendant(&session)?;
    connection.processes.capture_tree()?;
    let recorded = connection.recorded_tree();
    session.server.signal(Signal::SIGKILL)?;
    let exit_record = format!("child {} exited (", session.server.pid);
    let deadline = Instant::now() + EXIT_WAIT;
    loop {
        // Observe the shim handling its last ChildExited event, then close
        // the client pipe immediately. No extra tool call starts a server.
        if connection.log().contains(&exit_record) {
            connection.disconnect_input();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shim did not observe the last child exit: {}",
            connection.log()
        );
        thread::sleep(POLL_PAUSE);
    }
    let disconnected = Instant::now();
    let status = connection.processes.wait_for_child(LIVE_EXIT_WAIT)?;
    let elapsed = disconnected.elapsed();
    assert!(
        status.success(),
        "shim failed after last child exit: {status}"
    );
    assert!(
        elapsed < LIVE_EXIT_WAIT,
        "last-child drain exceeded its bound"
    );
    assert!(connection.output.is_some(), "test must retain stdout");
    connection.assert_session_removed(&session)?;
    connection.assert_no_later_reaper(&session)?;
    eprintln!(
        "last_child_eof: shim_pid={} server={:?} term_resistant={resistant:?} recorded_owned_tree={recorded:?} elapsed_ms={} socket_workdir={} disk_workdir={} removed_at_shim_exit=true later_reaper=false stdout_retained=true",
        connection.shim,
        session.server,
        elapsed.as_millis(),
        session.workdir.display(),
        session.disk.display()
    );
    Ok(())
}

fn run_scoped_sweep(home: &PrivateHome, owners: &[String]) -> TestResult<ExitStatus> {
    let mut command = Command::new(real_server_binary());
    command
        .arg("--sweep-workdirs")
        .args(owners)
        .env("HOME", &home.0)
        .env("TMPDIR", &home.1)
        .env("XDG_CACHE_HOME", home.0.join(".cache"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(File::create(home.0.join("scoped-sweep.log"))?);
    let mut processes = OwnedProcesses::spawn(&mut command)?;
    processes.wait_for_child(EXIT_WAIT)
}

#[test]
fn scoped_workdir_sweep_preserves_other_owners_and_rejects_invalid_pids() -> TestResult {
    let home = PrivateHome::create()?;
    // PIDs beyond the current kernel allocation limit cannot be concurrently
    // assigned to another session. Confirm absence before creating fixtures.
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")?
        .trim()
        .parse()?;
    let selected = pid_max
        .checked_add(1)
        .ok_or_else(|| io::Error::other("kernel PID limit overflow"))?;
    let other = selected
        .checked_add(1)
        .ok_or_else(|| io::Error::other("fixture PID overflow"))?;
    let paths = |pid: u32| {
        [
            home.1.join(format!("kwin-mcp-{pid}")),
            home.0
                .join(".cache/kwin-mcp")
                .join(format!("kwin-mcp-{pid}")),
        ]
    };
    for pid in [selected, other] {
        assert_absent(&PathBuf::from(format!("/proc/{pid}")))?;
        for path in paths(pid) {
            std::fs::create_dir_all(&path)?;
            std::fs::write(path.join("fixture.txt"), format!("owner={pid}\n"))?;
        }
    }
    assert!(
        run_scoped_sweep(&home, &[selected.to_string()])?.success(),
        "PID-scoped sweep failed"
    );
    for path in paths(selected) {
        assert_absent(&path)?;
    }
    for path in paths(other) {
        assert_eq!(
            std::fs::read_to_string(path.join("fixture.txt"))?,
            format!("owner={other}\n"),
            "scoped sweep changed another owner's workdir"
        );
    }
    // A valid owner followed by an invalid PID must fail before any sweep,
    // rather than applying a partially parsed filter or reverting to global.
    assert!(
        !run_scoped_sweep(&home, &[other.to_string(), "0".to_owned()])?.success(),
        "invalid owner PID was accepted"
    );
    for path in paths(other) {
        assert_eq!(
            std::fs::read_to_string(path.join("fixture.txt"))?,
            format!("owner={other}\n"),
            "invalid PID input removed another owner's workdir"
        );
    }
    eprintln!(
        "scoped_sweep: selected_owner={selected} other_owner={other} selected_pair_removed=true other_pair_preserved=true invalid_input_rejected=true"
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
