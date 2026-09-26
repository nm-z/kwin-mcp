//! FUSE inside the isolated session.
//!
//! bwrap runs everything with no_new_privs, so the setuid `fusermount`
//! binaries cannot mount, and root is unmapped in the sandbox's user
//! namespace. The sandbox is instead started with CAP_SYS_ADMIN (scoped to its
//! own user namespace), which only one process keeps: this helper. The session
//! entrypoint clears the inheritable and ambient sets for everything else, and
//! no_new_privs stops file capabilities from restoring it.
//!
//! `fusermount` and `fusermount3` inside the sandbox are this binary. As a
//! client it forwards its arguments, working directory, `_FUSE_*` environment,
//! and the descriptors those variables name (plus stdio) to the helper, which
//! runs the real binary with that capability and returns its exit status. The
//! real binaries are the same setuid helpers FUSE already trusts with arbitrary
//! unprivileged input: they only mount and unmount FUSE filesystems on paths
//! the user owns.

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::{Deserialize, Serialize};
use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// Helper socket, in the sandbox's private /tmp so host processes cannot
/// reach it through the shared workdir.
pub const HELPER_SOCKET: &str = "/tmp/.kwin-mcp-fuse.sock";
/// Where the sandbox sees the real fusermount binaries.
pub const REAL_BINARY_DIR: &str = "/run/kwin-mcp-fuse";
/// The FUSE helper programs the bridge serves, as named on the host.
pub const PROGRAMS: [&str; 2] = ["fusermount", "fusermount3"];
/// Most descriptors one request may carry (stdio plus `_FUSE_*` sockets).
const MAX_FDS: usize = 8;
/// Largest request message.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Descriptor floor for received fds, above any target number they map to.
const HIGH_FD_BASE: RawFd = 100;

#[derive(Serialize, Deserialize)]
struct Request {
    program: String,
    args: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    /// Descriptor number each passed fd takes in the real binary.
    targets: Vec<RawFd>,
}

/// Program name when this binary was invoked as a fusermount shim.
pub fn shim_program(argv0: &str) -> Option<&'static str> {
    let name = Path::new(argv0).file_name()?.to_str()?;
    PROGRAMS.into_iter().find(|program| *program == name)
}

/// Client side: forward this invocation to the helper and return its exit code.
pub fn run_client(program: &str) -> i32 {
    match forward(program) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{program} (kwin-mcp FUSE bridge): {error:#}");
            1
        }
    }
}

fn forward(program: &str) -> anyhow::Result<i32> {
    let mut targets: Vec<RawFd> = vec![0, 1, 2];
    let mut env = Vec::new();
    for (key, value) in std::env::vars() {
        if !key.starts_with("_FUSE_") {
            continue;
        }
        if let Ok(fd) = value.parse::<RawFd>()
            && fd > 2
            && !targets.contains(&fd)
            && nix::fcntl::fcntl(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, nix::fcntl::FcntlArg::F_GETFD).is_ok()
        {
            targets.push(fd);
        }
        env.push((key, value));
    }
    anyhow::ensure!(targets.len() <= MAX_FDS, "too many FUSE descriptors");
    let request = Request {
        program: program.to_owned(),
        args: std::env::args().skip(1).collect(),
        cwd: std::env::current_dir()?.display().to_string(),
        env,
        targets: targets.clone(),
    };
    let body = serde_json::to_vec(&request)?;
    anyhow::ensure!(body.len() <= MAX_REQUEST_BYTES, "request too large");
    let mut stream = UnixStream::connect(HELPER_SOCKET)
        .map_err(|error| anyhow::anyhow!("connect {HELPER_SOCKET}: {error}"))?;
    let fds = [ControlMessage::ScmRights(&targets)];
    sendmsg::<()>(stream.as_raw_fd(), &[IoSlice::new(&body)], &fds, MsgFlags::empty(), None)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut status = [0u8; 4];
    stream.read_exact(&mut status)?;
    Ok(i32::from_le_bytes(status))
}

/// The helper's fusermount runs as the namespace's root, which fusermount
/// trusts completely. Hold requests to what it allows an unprivileged user:
/// mount only on a directory the user owns, and unmount only FUSE mounts.
fn authorize(request: &Request) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mut unmount = false;
    let mut mountpoint = None;
    let mut options_done = false;
    let mut args = request.args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            _ if options_done => mountpoint = Some(arg),
            "--" => options_done = true,
            "-o" => {
                args.next();
            }
            "-u" | "--unmount" => unmount = true,
            "-h" | "--help" | "-V" | "--version" => return Ok(()),
            option if option.starts_with('-') => {}
            _ => mountpoint = Some(arg),
        }
    }
    let Some(mountpoint) = mountpoint else { return Ok(()) };
    // Lexical absolute path: a dead FUSE mount cannot be canonicalized.
    let mut path = std::path::PathBuf::from(&request.cwd);
    for component in Path::new(mountpoint).components() {
        match component {
            std::path::Component::RootDir => path = std::path::PathBuf::from("/"),
            std::path::Component::ParentDir => {
                path.pop();
            }
            std::path::Component::Normal(part) => path.push(part),
            std::path::Component::CurDir | std::path::Component::Prefix(_) => {}
        }
    }
    if unmount {
        let mounts = procfs::process::Process::myself()?.mountinfo()?;
        anyhow::ensure!(
            mounts.0.iter().any(|mount| mount.mount_point == path && mount.fs_type.starts_with("fuse")),
            "{} is not a FUSE mount",
            path.display()
        );
        return Ok(());
    }
    let owner = std::fs::metadata(&path)?.uid();
    let user = std::fs::metadata("/proc/self")?.uid();
    anyhow::ensure!(owner == user, "mountpoint {} is not owned by the user", path.display());
    Ok(())
}

/// Helper side: serve requests until the sandbox exits.
pub fn run_helper(socket: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || {
            if let Err(error) = serve(stream) {
                eprintln!("kwin-mcp FUSE helper: {error:#}");
            }
        });
    }
    Ok(())
}

fn serve(mut stream: UnixStream) -> anyhow::Result<()> {
    let mut body = vec![0u8; MAX_REQUEST_BYTES];
    let mut space = nix::cmsg_space!([RawFd; MAX_FDS]);
    let (length, mut received) = {
        let mut iov = [IoSliceMut::new(&mut body)];
        let message = recvmsg::<()>(stream.as_raw_fd(), &mut iov, Some(&mut space), MsgFlags::MSG_CMSG_CLOEXEC)?;
        let mut received: Vec<OwnedFd> = Vec::new();
        for control in message.cmsgs()? {
            if let ControlMessageOwned::ScmRights(fds) = control {
                // SAFETY: SCM_RIGHTS hands this process new descriptors it owns.
                received.extend(fds.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
            }
        }
        (message.bytes, received)
    };
    body.truncate(length);
    let request: Request = serde_json::from_slice(&body)?;
    anyhow::ensure!(PROGRAMS.contains(&request.program.as_str()), "refusing to run {}", request.program);
    anyhow::ensure!(
        received.len() == request.targets.len() && request.targets.len() >= 3 && request.targets[..3] == [0, 1, 2],
        "descriptor mismatch"
    );
    anyhow::ensure!(request.env.iter().all(|(key, _)| key.starts_with("_FUSE_")), "unexpected environment");
    if let Err(error) = authorize(&request) {
        stream.write_all(&1i32.to_le_bytes())?;
        if let Some(stderr) = received.get(2) {
            let _ = nix::unistd::write(stderr, format!("{}: {error:#}\n", request.program).as_bytes());
        }
        return Ok(());
    }
    let extra: Vec<(OwnedFd, RawFd)> = received
        .split_off(3)
        .into_iter()
        .zip(request.targets[3..].iter().copied())
        .enumerate()
        .map(|(index, (fd, target))| {
            let floor = HIGH_FD_BASE + RawFd::try_from(index).unwrap_or(0);
            let high = nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_DUPFD_CLOEXEC(floor))?;
            // SAFETY: F_DUPFD_CLOEXEC returned a new descriptor this process owns.
            Ok((unsafe { OwnedFd::from_raw_fd(high) }, target))
        })
        .collect::<nix::Result<_>>()?;
    let mut stdio = received.into_iter();
    let (Some(stdin), Some(stdout), Some(stderr)) = (stdio.next(), stdio.next(), stdio.next()) else {
        anyhow::bail!("missing stdio");
    };
    let mut command = std::process::Command::new(Path::new(REAL_BINARY_DIR).join(&request.program));
    command
        .args(&request.args)
        .current_dir(&request.cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(request.env.iter().map(|(key, value)| (key, value)))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    let mapping: Vec<(RawFd, RawFd)> = extra.iter().map(|(fd, target)| (fd.as_raw_fd(), *target)).collect();
    // SAFETY: only async-signal-safe dup2 calls run between fork and exec.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
            for (source, target) in &mapping {
                if nix::libc::dup2(*source, *target) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let status = command.spawn()?.wait()?;
    drop(extra);
    let code = status
        .code()
        .unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0));
    stream.write_all(&code.to_le_bytes())?;
    Ok(())
}
