//! The session's network namespace when the server runs under `no_new_privs`.
//!
//! A session normally starts as `pasta [options] -- bwrap ...`: pasta creates a
//! user and network namespace, maps the user to root there and runs bwrap in
//! it. pasta keeps only three capabilities for itself, and the command it runs
//! gets the full set back only because exec restores it for root. Under
//! `no_new_privs` exec restores nothing, so bwrap starts as root without
//! `CAP_SETFCAP`, which the kernel requires of anyone who maps the parent
//! namespace's uid 0 into a new user namespace. Every bwrap map does that, with
//! or without FUSE, and fails with `setting up uid map: Operation not permitted`.
//! Every sandbox built on bwrap sets `no_new_privs`, so that is what a server
//! started inside one inherits.
//!
//! A process keeps the capabilities it holds across exec, so the order is
//! reversed here: a child of this holder creates the two namespaces itself,
//! which gives it every capability in them, and execs bwrap with all of them.
//! pasta then joins those namespaces from outside (`pasta [options] PID`), which
//! their owner may do without any privilege. Everything stays a descendant of
//! the server and inside whatever sandbox the server is in, so the session's
//! scope, process group, stdio, `/proc/<pid>/root` access and teardown are the
//! ones a session has without `no_new_privs`. The holder takes pasta's place in
//! the process tree: it is the parent of bwrap and exits with bwrap's status.

use anyhow::{Context, bail};
use nix::sys::signal::Signal;
use nix::unistd::{ForkResult, Pid};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// First argument that makes this binary act as the holder:
/// `kwin-mcp --netns-holder READY_FILE pasta [options] -- COMMAND...`.
pub const FLAG: &str = "--netns-holder";

/// What the holder prints when the kernel refuses the namespaces, so the
/// server recognizes the refusal the way it recognizes bwrap's.
pub const NAMESPACE_REFUSAL: &str = "cannot create the session's user and network namespaces";

/// pasta configures an attached namespace in well under a second.
const NETWORK_READY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(5);
/// Shell convention for a command that could not be executed.
const EXEC_FAILED: i32 = 126;
/// Shell convention for a command ended by a signal: this plus its number.
pub(crate) const SIGNAL_EXIT_BASE: i32 = 128;

#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Where pasta writes its PID once the namespace's network is configured.
    ready_file: PathBuf,
    /// pasta and its options, without a command or target.
    pasta: Vec<OsString>,
    /// What runs inside the namespaces.
    command: Vec<OsString>,
}

impl Invocation {
    /// The arguments after [`FLAG`].
    pub fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let usage = || format!("usage: kwin-mcp {FLAG} READY_FILE pasta [options] -- COMMAND...");
        let ready_file = arguments.next().map(PathBuf::from).ok_or_else(usage)?;
        let mut pasta = Vec::new();
        let mut command = Vec::new();
        let mut separated = false;
        for argument in arguments {
            if separated {
                command.push(argument);
            } else if argument == "--" {
                separated = true;
            } else {
                pasta.push(argument);
            }
        }
        if pasta.is_empty() || command.is_empty() {
            return Err(usage());
        }
        Ok(Self {
            ready_file,
            pasta,
            command,
        })
    }
}

/// Run the command in new user and network namespaces that pasta serves, and
/// return the exit code for this process.
pub fn run(invocation: &Invocation) -> i32 {
    match hold(invocation) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("kwin-mcp: {error:#}");
            1
        }
    }
}

fn hold(invocation: &Invocation) -> anyhow::Result<i32> {
    let holder = nix::unistd::getpid();
    let (uid, gid) = (nix::unistd::getuid(), nix::unistd::getgid());
    let (mut entered, entered_writer) = std::io::pipe().context("create a pipe")?;
    let (proceed_reader, mut proceed) = std::io::pipe().context("create a pipe")?;
    // The holder is single-threaded: main dispatches to it before anything
    // starts a thread, so the child may allocate and exec like any process.
    match unsafe { nix::unistd::fork() }.context("fork")? {
        ForkResult::Child => {
            drop(entered);
            drop(proceed);
            let error = enter_namespaces_and_exec(
                holder,
                uid.as_raw(),
                gid.as_raw(),
                entered_writer,
                proceed_reader,
                &invocation.command,
            );
            eprintln!("kwin-mcp: {error:#}");
            std::process::exit(EXEC_FAILED);
        }
        ForkResult::Parent { child } => {
            drop(entered_writer);
            drop(proceed_reader);
            let mut byte = [0u8; 1];
            if entered.read(&mut byte).context("wait for the namespaces")? == 0 {
                // The child said why and is gone.
                return Ok(exit_code(wait_for(child)?));
            }
            let mut pasta = match attach_pasta(invocation, child) {
                Ok(pasta) => pasta,
                Err(error) => {
                    end(child, Signal::SIGKILL);
                    return Err(error);
                }
            };
            if proceed.write_all(&byte).is_err() {
                end(child, Signal::SIGKILL);
                stop(&mut pasta);
                bail!("the sandbox process ended before the network was ready");
            }
            drop(proceed);
            supervise(child, pasta)
        }
    }
}

/// Child side: become root in a new user namespace that owns a new network
/// namespace, wait until pasta serves it, then exec the command there.
fn enter_namespaces_and_exec(
    holder: Pid,
    uid: u32,
    gid: u32,
    mut entered: std::io::PipeWriter,
    mut proceed: std::io::PipeReader,
    command: &[OsString],
) -> anyhow::Error {
    let entered_namespaces = (|| -> anyhow::Result<()> {
        // bwrap sets its own with --die-with-parent; this covers the time before.
        nix::sys::prctl::set_pdeathsig(Signal::SIGKILL).context("set the parent-death signal")?;
        if nix::unistd::getppid() != holder {
            bail!("the holder exited");
        }
        nix::sched::unshare(
            nix::sched::CloneFlags::CLONE_NEWUSER | nix::sched::CloneFlags::CLONE_NEWNET,
        )
        .with_context(|| NAMESPACE_REFUSAL.to_owned())?;
        // The mapping pasta gives the command it runs.
        std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))
            .context("setting up uid map")?;
        std::fs::write("/proc/self/setgroups", "deny").context("deny setgroups")?;
        std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))
            .context("setting up gid map")?;
        // pasta also does this for the command it runs: unprivileged ICMP
        // echo sockets for the session's group.
        if let Err(error) = std::fs::write("/proc/sys/net/ipv4/ping_group_range", "0 0") {
            eprintln!("kwin-mcp: cannot set ping_group_range, ping may fail: {error}");
        }
        entered
            .write_all(&[1])
            .context("tell the holder the namespaces exist")
    })();
    if let Err(error) = entered_namespaces {
        return error;
    }
    drop(entered);
    let mut byte = [0u8; 1];
    if !matches!(proceed.read(&mut byte), Ok(1)) {
        // The holder reported why the network is unavailable.
        std::process::exit(1);
    }
    drop(proceed);
    let Some((program, arguments)) = command.split_first() else {
        return anyhow::anyhow!("no command");
    };
    let error = Command::new(program).args(arguments).exec();
    anyhow::anyhow!("start {}: {error}", Path::new(program).display())
}

/// Start pasta on the child's namespaces and wait until it configured them.
fn attach_pasta(invocation: &Invocation, child: Pid) -> anyhow::Result<std::process::Child> {
    let ready = &invocation.ready_file;
    match std::fs::remove_file(ready) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => bail!("remove stale {}: {error}", ready.display()),
    }
    let Some((program, options)) = invocation.pasta.split_first() else {
        bail!("no pasta command");
    };
    let mut command = Command::new(program);
    command
        .args(options)
        // In the foreground pasta stays a child in the session's process group.
        .arg("--foreground")
        .arg("--pid")
        .arg(ready)
        .arg(child.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::other)
        });
    }
    let mut pasta = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("start pasta: {error} (install passt)"))?;
    let deadline = Instant::now() + NETWORK_READY_TIMEOUT;
    loop {
        // pasta creates the file early and writes its PID when it is ready.
        if std::fs::metadata(ready).is_ok_and(|metadata| metadata.len() > 0) {
            return Ok(pasta);
        }
        if let Some(status) = pasta.try_wait().context("poll pasta")? {
            bail!("pasta ended with {status} before the session's network was configured");
        }
        if Instant::now() >= deadline {
            stop(&mut pasta);
            bail!(
                "pasta did not configure the session's network within {} s",
                NETWORK_READY_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(POLL);
    }
}

/// Wait for the command; without pasta the session has no network, so its end
/// ends the command too, as it does when pasta is the command's parent.
fn supervise(child: Pid, mut pasta: std::process::Child) -> anyhow::Result<i32> {
    let pasta_pid = Pid::from_raw(i32::try_from(pasta.id()).context("pasta PID")?);
    loop {
        let (pid, status) = wait_for(Pid::from_raw(-1))?;
        if pid == child {
            stop(&mut pasta);
            return Ok(exit_code((pid, status)));
        }
        if pid == pasta_pid {
            eprintln!("kwin-mcp: pasta ended with {status}; ending the session it served");
            end(child, Signal::SIGKILL);
            return Ok(1);
        }
    }
}

/// waitpid for one child, or any child for -1, retried over signals.
fn wait_for(pid: Pid) -> anyhow::Result<(Pid, ExitStatus)> {
    loop {
        let mut status = 0;
        // waitpid only writes the status it was handed.
        let reaped = unsafe { nix::libc::waitpid(pid.as_raw(), &mut status, 0) };
        if reaped >= 0 {
            return Ok((Pid::from_raw(reaped), ExitStatus::from_raw(status)));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            bail!("wait for the sandbox process: {error}");
        }
    }
}

fn exit_code((_, status): (Pid, ExitStatus)) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| SIGNAL_EXIT_BASE + signal))
        .unwrap_or(1)
}

/// Signal a child of the holder and reap it.
fn end(child: Pid, signal: Signal) {
    let _ = nix::sys::signal::kill(child, signal);
    let _ = wait_for(child);
}

fn stop(pasta: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(pasta.id()) {
        let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    let _ = pasta.wait();
}

#[cfg(test)]
mod tests {
    use super::{FLAG, Invocation};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn arguments(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_first_separator_divides_pasta_options_from_the_command() -> Result<(), String> {
        let invocation = Invocation::parse(arguments(&[
            "/disk/pasta.pid",
            "pasta",
            "--quiet",
            "--config-net",
            "--",
            "bwrap",
            "--unshare-user",
            "--",
            "bash",
        ]))?;
        assert_eq!(
            invocation,
            Invocation {
                ready_file: PathBuf::from("/disk/pasta.pid"),
                pasta: ["pasta", "--quiet", "--config-net"]
                    .map(OsString::from)
                    .to_vec(),
                command: ["bwrap", "--unshare-user", "--", "bash"]
                    .map(OsString::from)
                    .to_vec(),
            }
        );
        Ok(())
    }

    #[test]
    fn a_missing_part_is_a_usage_error() {
        for incomplete in [
            &[][..],
            &["/disk/pasta.pid"][..],
            &["/disk/pasta.pid", "pasta", "--quiet"][..],
            &["/disk/pasta.pid", "pasta", "--"][..],
            &["/disk/pasta.pid", "--", "bwrap"][..],
        ] {
            let error = Invocation::parse(arguments(incomplete));
            assert!(
                error.as_ref().is_err_and(|usage| usage.contains(FLAG)),
                "{incomplete:?}: {error:?}"
            );
        }
    }
}
