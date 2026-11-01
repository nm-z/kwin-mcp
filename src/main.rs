// A stdio client's exit also closes its diagnostic reader. Cleanup must not
// panic on EPIPE while stopping the client's scopes and deleting workdirs.
macro_rules! eprintln {
    ($($arguments:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($arguments)*);
    }};
}

mod file_import;

mod fuse_bridge {
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
    /// Where the sandbox sees the copied kwin-mcp image used to run the helper.
    pub const BRIDGE_EXECUTABLE: &str = "/run/kwin-mcp-fuse/bridge";
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
                && nix::fcntl::fcntl(
                    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
                    nix::fcntl::FcntlArg::F_GETFD,
                )
                .is_ok()
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
        sendmsg::<()>(
            stream.as_raw_fd(),
            &[IoSlice::new(&body)],
            &fds,
            MsgFlags::empty(),
            None,
        )?;
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
        let Some(mountpoint) = mountpoint else {
            return Ok(());
        };
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
                mounts
                    .0
                    .iter()
                    .any(|mount| mount.mount_point == path && mount.fs_type.starts_with("fuse")),
                "{} is not a FUSE mount",
                path.display()
            );
            return Ok(());
        }
        let owner = std::fs::metadata(&path)?.uid();
        let user = std::fs::metadata("/proc/self")?.uid();
        anyhow::ensure!(
            owner == user,
            "mountpoint {} is not owned by the user",
            path.display()
        );
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
            let message = recvmsg::<()>(
                stream.as_raw_fd(),
                &mut iov,
                Some(&mut space),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )?;
            let mut received: Vec<OwnedFd> = Vec::new();
            for control in message.cmsgs()? {
                if let ControlMessageOwned::ScmRights(fds) = control {
                    // SAFETY: SCM_RIGHTS hands this process new descriptors it owns.
                    received.extend(
                        fds.into_iter()
                            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                    );
                }
            }
            (message.bytes, received)
        };
        body.truncate(length);
        let request: Request = serde_json::from_slice(&body)?;
        anyhow::ensure!(
            PROGRAMS.contains(&request.program.as_str()),
            "refusing to run {}",
            request.program
        );
        anyhow::ensure!(
            received.len() == request.targets.len()
                && request.targets.len() >= 3
                && request.targets[..3] == [0, 1, 2],
            "descriptor mismatch"
        );
        anyhow::ensure!(
            request.env.iter().all(|(key, _)| key.starts_with("_FUSE_")),
            "unexpected environment"
        );
        if let Err(error) = authorize(&request) {
            stream.write_all(&1i32.to_le_bytes())?;
            if let Some(stderr) = received.get(2) {
                let _ = nix::unistd::write(
                    stderr,
                    format!("{}: {error:#}\n", request.program).as_bytes(),
                );
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
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdio.next(), stdio.next(), stdio.next())
        else {
            anyhow::bail!("missing stdio");
        };
        let mut command =
            std::process::Command::new(Path::new(REAL_BINARY_DIR).join(&request.program));
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .envs(request.env.iter().map(|(key, value)| (key, value)))
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        let mapping: Vec<(RawFd, RawFd)> = extra
            .iter()
            .map(|(fd, target)| (fd.as_raw_fd(), *target))
            .collect();
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
        let code = status.code().unwrap_or_else(|| {
            128 + std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)
        });
        stream.write_all(&code.to_le_bytes())?;
        Ok(())
    }
}
mod host_environment;
mod input_bridge;
mod session_registry;
mod kwin_script {
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::Duration;

    use crate::KwinError;

    const KWIN_SCRIPTING_PATH: &str = "/Scripting";
    const KWIN_SCRIPTING_INTERFACE: &str = "org.kde.kwin.Scripting";
    const KWIN_SCRIPT_INTERFACE: &str = "org.kde.kwin.Script";
    const KWIN_CALLBACK_INTERFACE: &str = "org.kde.KWinMCP";
    const SCRIPT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
    const SCRIPT_FINISH_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
    /// Bound on the wait for a script's callback. Scripts answer in milliseconds;
    /// one that never answers must not hold its KWin's script lock indefinitely.
    const SCRIPT_RESULT_TIMEOUT: Duration = Duration::from_secs(10);
    pub(crate) const SCRIPT_DIRECTORY: &str = "kwin-scripts";

    static NEXT_SCRIPT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// KWin numbers a loaded script by how many scripts are loaded when it loads,
    /// and exports it at `/Scripting/Script<number>`. Overlapping lifecycles can
    /// therefore claim the same path, and unloading one removes the path the other
    /// still uses. Each KWin admits one lifecycle (load, run, callback, unload)
    /// at a time.
    static SCRIPT_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    fn script_lock(kwin_unique: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = SCRIPT_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(kwin_unique.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    pub(crate) async fn run_kwin_script(
        conn: &zbus::Connection,
        kwin_unique: &str,
        host_xdg_dir: &Path,
        script_body: &str,
    ) -> Result<String, KwinError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            KwinError::Msg(format!("KWin script requires a Tokio runtime: {error}"))
        })?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let pid = std::process::id();
        let sequence = NEXT_SCRIPT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let marker = format!("kwin-mcp-{pid}-{timestamp}-{sequence}");
        let callback_path = format!("/KWinMCP/{pid}_{timestamp}_{sequence}");
        let our_name = conn
            .unique_name()
            .ok_or_else(|| KwinError::Msg("no bus name".to_owned()))?
            .to_string();
        let our_name_json = serde_json::to_string(&our_name)?;
        let callback_path_json = serde_json::to_string(&callback_path)?;
        let script = format!(
            "{script_body}\n\
        callDBus({our_name_json},{callback_path_json},'{KWIN_CALLBACK_INTERFACE}','result',JSON.stringify(result));"
        );
        let object_path = zbus::zvariant::ObjectPath::try_from(callback_path.clone())?;
        let disk_scripts = crate::session_disk_path(host_xdg_dir).join(SCRIPT_DIRECTORY);
        // Sessions created before this directory was introduced can survive a
        // source reload, but their mount namespace has no disk-backed alias.
        let (script_directory, container_directory) = if disk_scripts.is_dir() {
            (disk_scripts, host_xdg_dir.join(SCRIPT_DIRECTORY))
        } else {
            (host_xdg_dir.to_owned(), host_xdg_dir.to_owned())
        };
        let filename = format!("{marker}.js");
        let script_file = script_directory.join(&filename);
        let container_script_path = container_directory
            .join(filename)
            .to_string_lossy()
            .to_string();
        // Held until this script is unloaded, including when a dropped caller
        // leaves the unload to the cleanup task.
        let lock = script_lock(kwin_unique).lock_owned().await;
        let mut cleanup = ScriptCleanup {
            conn: conn.clone(),
            kwin_unique: kwin_unique.to_owned(),
            object_path,
            marker,
            script_file,
            runtime,
            file_created: false,
            callback_attempted: false,
            load_attempted: false,
            cleanup_started: false,
            lock: Some(lock),
        };

        let result = async {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&cleanup.script_file)
                .and_then(|mut file| {
                    cleanup.file_created = true;
                    file.write_all(script.as_bytes())
                })
                .map_err(|error| {
                    KwinError::Msg(format!(
                        "prepare KWin script: {}",
                        crate::describe_write_failure(&cleanup.script_file, &error)
                    ))
                })?;
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let callback = KWinCallback {
                tx: std::sync::Mutex::new(Some(tx)),
            };
            cleanup.callback_attempted = true;
            let registered = conn
                .object_server()
                .at(&cleanup.object_path, callback)
                .await?;
            eprintln!(
                "run_kwin_script: our_name={our_name} path={callback_path} registered={registered}"
            );
            if !registered {
                cleanup.callback_attempted = false;
                return Err(KwinError::Msg(format!(
                    "failed to register callback at {callback_path}"
                )));
            }

            let scripting: zbus::Proxy = zbus::proxy::Builder::new(conn)
                .destination(kwin_unique)?
                .path(KWIN_SCRIPTING_PATH)?
                .interface(KWIN_SCRIPTING_INTERFACE)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            cleanup.load_attempted = true;
            let (script_id,): (i32,) = scripting
                .call("loadScript", &(&container_script_path, &cleanup.marker))
                .await?;
            if script_id < 0 {
                return Err(KwinError::Msg(format!(
                    "KWin loadScript failed, id={script_id}"
                )));
            }
            let script_proxy: zbus::Proxy = zbus::proxy::Builder::new(conn)
                .destination(kwin_unique)?
                .path(format!("{KWIN_SCRIPTING_PATH}/Script{script_id}"))?
                .interface(KWIN_SCRIPT_INTERFACE)?
                .cache_properties(zbus::proxy::CacheProperties::No)
                .build()
                .await?;
            script_proxy.call::<_, (), ()>("run", &()).await?;
            match tokio::time::timeout(SCRIPT_RESULT_TIMEOUT, rx).await {
                Ok(payload) => {
                    payload.map_err(|_| KwinError::Msg("KWin callback channel closed".to_owned()))
                }
                Err(_) => Err(KwinError::Msg(format!(
                    "KWin script {} gave no result within {SCRIPT_RESULT_TIMEOUT:?}",
                    cleanup.marker
                ))),
            }
        }
        .await;

        test_script_cleanup_gate(&cleanup).await;
        let cleanup_result = cleanup.finish().await;
        match result {
            Ok(payload) => {
                cleanup_result?;
                Ok(payload)
            }
            Err(error) => Err(error),
        }
    }

    // Gate one actual cleanup in a debug E2E test so its owned compositor can
    // be paused after the script result, without mocking a D-Bus reply.
    async fn test_script_cleanup_gate(cleanup: &ScriptCleanup) {
        if !cfg!(debug_assertions) {
            return;
        }
        let Some(root) = std::env::var_os("KWIN_MCP_TEST_SCRIPT_CLEANUP_GATE").map(PathBuf::from)
        else {
            return;
        };
        let ready = root.join("ready.json");
        let release = root.join("release");
        if ready.exists() || release.exists() {
            return;
        }
        let temporary = root.join("ready.pending");
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        else {
            return;
        };
        let payload = serde_json::json!({
            "marker": cleanup.marker,
            "callback_name": cleanup.conn.unique_name().map(ToString::to_string),
            "callback_path": cleanup.object_path.as_str(),
            "kwin_name": cleanup.kwin_unique,
        });
        if file.write_all(payload.to_string().as_bytes()).is_err() {
            return;
        }
        drop(file);
        if std::fs::rename(&temporary, &ready).is_err() {
            return;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !release.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    struct ScriptCleanup {
        conn: zbus::Connection,
        kwin_unique: String,
        object_path: zbus::zvariant::ObjectPath<'static>,
        marker: String,
        script_file: PathBuf,
        runtime: tokio::runtime::Handle,
        file_created: bool,
        callback_attempted: bool,
        load_attempted: bool,
        cleanup_started: bool,
        lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    }

    impl ScriptCleanup {
        fn start(
            &mut self,
            cleanup_timeout: Duration,
        ) -> Option<tokio::task::JoinHandle<Result<(), KwinError>>> {
            if self.cleanup_started {
                return None;
            }
            self.cleanup_started = true;
            let file_result = if self.file_created {
                match std::fs::remove_file(&self.script_file) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => {
                        eprintln!(
                            "KWin script cleanup {}: remove {}: {error}",
                            self.marker,
                            self.script_file.display()
                        );
                        Err(KwinError::from(error))
                    }
                }
            } else {
                Ok(())
            };
            let conn = self.conn.clone();
            let kwin_unique = self.kwin_unique.clone();
            let object_path = self.object_path.clone();
            let marker = self.marker.clone();
            let callback_attempted = self.callback_attempted;
            let load_attempted = self.load_attempted;
            let lock = self.lock.take();

            Some(self.runtime.spawn(async move {
            // Released only once the unload below has finished.
            let _lock = lock;
            let callback_cleanup = async {
                if !callback_attempted {
                    return Ok(());
                }
                match tokio::time::timeout(
                    cleanup_timeout,
                    conn.object_server().remove::<KWinCallback, _>(&object_path),
                )
                .await
                {
                    Ok(Ok(_)) | Ok(Err(zbus::Error::InterfaceNotFound)) => Ok(()),
                    Ok(Err(error)) => Err(KwinError::from(error)),
                    Err(_) => Err(KwinError::Msg(format!(
                        "KWin callback cleanup timed out at {object_path} after {cleanup_timeout:?}"
                    ))),
                }
            };
            let unload_cleanup = async {
                if !load_attempted {
                    return Ok(());
                }
                let reply = tokio::time::timeout(
                    cleanup_timeout,
                    conn.call_method(
                        Some(kwin_unique.as_str()),
                        KWIN_SCRIPTING_PATH,
                        Some(KWIN_SCRIPTING_INTERFACE),
                        "unloadScript",
                        &(marker.as_str(),),
                    ),
                )
                .await
                .map_err(|_| {
                    KwinError::Msg(format!(
                        "KWin unloadScript cleanup timed out for {marker} after {cleanup_timeout:?}"
                    ))
                })??;
                let (_,): (bool,) = reply.body().deserialize()?;
                Ok(())
            };
            let (callback_result, unload_result) =
                tokio::join!(callback_cleanup, unload_cleanup);
            if let Err(error) = &callback_result {
                eprintln!("KWin script cleanup {marker}: {error}");
            }
            if let Err(error) = &unload_result {
                eprintln!("KWin script cleanup {marker}: {error}");
            }
            file_result.and(callback_result).and(unload_result)
        }))
        }

        async fn finish(&mut self) -> Result<(), KwinError> {
            match self.start(SCRIPT_FINISH_CLEANUP_TIMEOUT) {
                Some(task) => task.await.map_err(|error| {
                    KwinError::Msg(format!("KWin script cleanup task failed: {error}"))
                })?,
                None => Ok(()),
            }
        }
    }

    impl Drop for ScriptCleanup {
        fn drop(&mut self) {
            if let Some(task) = self.start(SCRIPT_CLEANUP_TIMEOUT) {
                drop(task);
            }
        }
    }

    struct KWinCallback {
        tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
    }

    #[zbus::interface(name = "org.kde.KWinMCP")]
    impl KWinCallback {
        #[zbus(name = "result")]
        fn result(&self, payload: String) {
            match self.tx.lock() {
                Ok(mut sender) => {
                    if let Some(tx) = sender.take()
                        && tx.send(payload).is_err()
                    {
                        eprintln!("KWin callback receiver dropped");
                    }
                }
                Err(error) => eprintln!("KWin callback lock poisoned: {error}"),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{Arc, Duration, script_lock};

        #[tokio::test]
        async fn one_kwin_admits_one_script_lifecycle_at_a_time() {
            let first = script_lock(":1.lock-test").lock_owned().await;
            let waiting = tokio::time::timeout(
                Duration::from_millis(50),
                script_lock(":1.lock-test").lock_owned(),
            )
            .await;
            assert!(
                waiting.is_err(),
                "second lifecycle ran while the first held the lock"
            );
            drop(first);
            let _second = script_lock(":1.lock-test").lock_owned().await;
        }

        #[test]
        fn each_kwin_has_its_own_script_lock() {
            assert!(Arc::ptr_eq(&script_lock(":1.a"), &script_lock(":1.a")));
            assert!(!Arc::ptr_eq(&script_lock(":1.a"), &script_lock(":1.b")));
        }
    }
}
use kwin_script::run_kwin_script;
mod wallet_mediator {
    //! A session-local KWallet for the isolated session.
    //!
    //! Session apps (Chrome with --password-store=kwallet6) talk to
    //! `org.kde.kwalletd6` on a private bus served by this module. The session
    //! never reaches the host's wallet or Secret Service: at `session_start` one
    //! guarded, read-only snapshot of the host wallet is taken, and from then on
    //! every call is answered from memory. Writes land in the session's copy only
    //! and vanish with the session.
    //!
    //! The snapshot is taken so that it cannot disturb the host:
    //!
    //! - it never starts a service: `org.kde.ksecretd` must already own its name,
    //!   and every call is sent with NoAutoStart;
    //! - it never unlocks or prompts: a locked wallet is refused, and the lock
    //!   state comes from `org.kde.ksecretd` only, never from
    //!   `org.freedesktop.secrets` (gnome-keyring aborts when a short-lived
    //!   client disconnects mid-request);
    //! - snapshots are serialized across every kwin-mcp server on the host and
    //!   spaced apart, so many sessions starting together make no burst;
    //! - it does not depend on kwalletd6 being healthy: when kwalletd6 is missing
    //!   or does not answer, the same wallet is read from ksecretd directly with
    //!   the standard Secret Service calls (a wedged kwalletd6 after a user-bus
    //!   restart otherwise left every session without its Chrome Safe Storage key,
    //!   and Chrome then drops every cookie it cannot decrypt).

    use futures::StreamExt;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use zbus::message::Type as MessageType;
    use zbus::zvariant::Value;

    pub const SERVICE: &str = "org.kde.kwalletd6";
    pub const PATH: &str = "/modules/kwalletd6";
    pub const INTERFACE: &str = "org.kde.KWallet";
    /// The only Secret Service the lock state is read from.
    const SECRET_BACKEND: &str = "org.kde.ksecretd";
    /// App id the snapshot's own handle is opened under.
    const SNAPSHOT_APP: &str = "kwin-mcp-snapshot";
    /// Bound for each host call during the snapshot.
    const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(3);
    /// Bound for each way of reading the wallet.
    const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(6);
    /// Bound for the whole snapshot, lock wait included, inside session_start's
    /// 20 s hard limit.
    const SNAPSHOT_BUDGET: Duration = Duration::from_secs(12);
    /// Minimum gap between two snapshots on this host.
    const SNAPSHOT_SPACING: Duration = Duration::from_millis(100);
    /// After kwalletd6 fails to answer, snapshots read ksecretd directly for this
    /// long instead of each waiting out HOST_CALL_TIMEOUT.
    const KWALLETD_BACKOFF: Duration = Duration::from_secs(60);
    /// Limits that keep a runaway wallet from filling the server's memory.
    const MAX_ENTRIES: usize = 20_000;
    const MAX_BYTES: usize = 64 << 20;

    /// KWallet entry types.
    const TYPE_UNKNOWN: i32 = 0;
    const TYPE_PASSWORD: i32 = 1;
    const TYPE_STREAM: i32 = 2;
    const TYPE_MAP: i32 = 3;

    #[derive(Clone, serde::Serialize, serde::Deserialize)]
    struct Entry {
        kind: i32,
        data: Vec<u8>,
    }

    /// The wallet the session sees: the host snapshot plus the session's own writes.
    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    pub struct Wallet {
        name: String,
        folders: BTreeMap<String, BTreeMap<String, Entry>>,
    }

    impl Wallet {
        pub fn entry_count(&self) -> usize {
            self.folders.values().map(BTreeMap::len).sum()
        }

        fn entry(&self, folder: &str, key: &str) -> Option<&Entry> {
            self.folders.get(folder)?.get(key)
        }

        fn write(&mut self, folder: &str, key: &str, kind: i32, data: Vec<u8>) -> bool {
            match self.folders.get_mut(folder) {
                Some(entries) => {
                    entries.insert(key.to_owned(), Entry { kind, data });
                    true
                }
                None => false,
            }
        }
    }

    /// Outcome of the host snapshot, reported by session_start.
    pub struct Snapshot {
        pub wallet: Option<Wallet>,
        pub reason: String,
    }

    #[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
    pub(super) struct State {
        wallet: Option<Wallet>,
        /// Handles the session opened, with the app id each belongs to.
        handles: HashMap<i32, String>,
        next_handle: i32,
        next_transaction: i32,
    }

    pub struct WalletMediator {
        state: Arc<Mutex<State>>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    }

    impl Drop for WalletMediator {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    type HostResult<T> = zbus::Result<T>;

    async fn host_call<B, R>(proxy: &zbus::Proxy<'_>, method: &str, body: &B) -> HostResult<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        tokio::time::timeout(
            HOST_CALL_TIMEOUT,
            proxy.call_with_flags::<_, _, R>(
                method,
                zbus::proxy::MethodFlags::NoAutoStart.into(),
                body,
            ),
        )
        .await
        .map_err(|_| {
            zbus::Error::Failure(format!(
                "{method}: the host service did not answer in {HOST_CALL_TIMEOUT:?}"
            ))
        })??
        .ok_or_else(|| zbus::Error::Failure(format!("{method}: no reply")))
    }

    async fn name_has_owner(host: &zbus::Connection, name: &str) -> bool {
        let Ok(proxy) = zbus::fdo::DBusProxy::new(host).await else {
            return false;
        };
        let Ok(name) = zbus::names::BusName::try_from(name) else {
            return false;
        };
        proxy.name_has_owner(name).await.unwrap_or(false)
    }

    /// Serializes snapshots across processes with a lock file in the user runtime
    /// directory, and spaces them SNAPSHOT_SPACING apart.
    struct HostSlot {
        file: nix::fcntl::Flock<std::fs::File>,
        /// Touched when kwalletd6 does not answer; see KWALLETD_BACKOFF.
        kwalletd_unreachable: std::path::PathBuf,
    }

    impl HostSlot {
        async fn take(runtime: &std::path::Path) -> Result<Self, String> {
            let directory = runtime.join("kwin-mcp");
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("create {}: {error}", directory.display()))?;
            let path = directory.join("wallet-snapshot.lock");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|error| format!("open {}: {error}", path.display()))?;
            loop {
                match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                    Ok(locked) => {
                        let slot = Self {
                            file: locked,
                            kwalletd_unreachable: directory.join("kwalletd-unreachable"),
                        };
                        let since = slot
                            .file
                            .metadata()
                            .and_then(|meta| meta.modified())
                            .ok()
                            .and_then(|time| time.elapsed().ok());
                        if let Some(wait) =
                            since.and_then(|elapsed| SNAPSHOT_SPACING.checked_sub(elapsed))
                        {
                            tokio::time::sleep(wait).await;
                        }
                        return Ok(slot);
                    }
                    Err((returned, nix::errno::Errno::EWOULDBLOCK)) => {
                        file = returned;
                        tokio::time::sleep(SNAPSHOT_SPACING).await;
                    }
                    Err((_, error)) => return Err(format!("lock {}: {error}", path.display())),
                }
            }
        }

        fn kwalletd_recently_unreachable(&self) -> bool {
            std::fs::metadata(&self.kwalletd_unreachable)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|elapsed| elapsed < KWALLETD_BACKOFF)
        }

        fn mark_kwalletd(&self, unreachable: bool) {
            if unreachable {
                let _ = std::fs::write(&self.kwalletd_unreachable, b"");
            } else {
                let _ = std::fs::remove_file(&self.kwalletd_unreachable);
            }
        }
    }

    impl Drop for HostSlot {
        fn drop(&mut self) {
            // The modification time is what spaces the next snapshot.
            let _ = self.file.set_modified(std::time::SystemTime::now());
        }
    }

    fn refused(reason: impl Into<String>) -> Snapshot {
        Snapshot {
            wallet: None,
            reason: reason.into(),
        }
    }

    /// Set to read the host wallet from ksecretd only (diagnostics).
    const FROM_SECRET_SERVICE_ENV: &str = "KWIN_MCP_WALLET_FROM_SECRET_SERVICE";

    /// Why one way of reading the host wallet did not work.
    enum Failure {
        /// A decision, not a fault: the wallet is disabled, locked or not listed.
        /// No other way of reading it is tried.
        Refused(String),
        /// kwalletd6 is missing, wedged or erroring. ksecretd, the Secret Service
        /// behind it, may still answer, so the copy is tried from there.
        Unreachable(String),
    }

    fn unreachable_error(what: &str) -> impl FnOnce(zbus::Error) -> Failure + '_ {
        move |error| Failure::Unreachable(format!("{what}: {error}"))
    }

    /// Take the one read-only snapshot of the host wallet, or say why not.
    pub async fn snapshot(host: &zbus::Connection, runtime: &std::path::Path) -> Snapshot {
        let guarded = async {
            let slot = HostSlot::take(runtime).await?;
            let outcome = snapshot_locked(host, &slot).await;
            drop(slot);
            Ok::<_, String>(outcome)
        };
        match tokio::time::timeout(SNAPSHOT_BUDGET, guarded).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(reason)) => refused(reason),
            Err(_) => refused(format!(
                "host wallet snapshot did not finish within {} s",
                SNAPSHOT_BUDGET.as_secs()
            )),
        }
    }

    async fn snapshot_locked(host: &zbus::Connection, slot: &HostSlot) -> Snapshot {
        // Diagnostic switch: read from ksecretd even while kwalletd6 answers, to
        // exercise the fallback.
        let why = if std::env::var_os(FROM_SECRET_SERVICE_ENV).is_some() {
            format!("{FROM_SECRET_SERVICE_ENV} is set")
        } else if slot.kwalletd_recently_unreachable() {
            format!(
                "host kwalletd6 did not answer within the last {} s",
                KWALLETD_BACKOFF.as_secs()
            )
        } else {
            let why = match tokio::time::timeout(SNAPSHOT_TIMEOUT, read_via_kwalletd(host)).await {
                Ok(Ok(snapshot)) => {
                    slot.mark_kwalletd(false);
                    return snapshot;
                }
                Ok(Err(Failure::Refused(reason))) => return refused(reason),
                Ok(Err(Failure::Unreachable(reason))) => reason,
                Err(_) => "host kwalletd6 snapshot timed out".to_owned(),
            };
            slot.mark_kwalletd(true);
            why
        };
        match tokio::time::timeout(SNAPSHOT_TIMEOUT, read_via_secret_service(host, &why)).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(Failure::Refused(reason) | Failure::Unreachable(reason))) => {
                refused(format!("{why}; {reason}"))
            }
            Err(_) => refused(format!("{why}; reading ksecretd timed out")),
        }
    }

    async fn read_via_kwalletd(host: &zbus::Connection) -> Result<Snapshot, Failure> {
        if !name_has_owner(host, SECRET_BACKEND).await {
            return Err(Failure::Refused(format!(
                "{SECRET_BACKEND} is not running on the host; kwin-mcp does not start it"
            )));
        }
        if !name_has_owner(host, SERVICE).await {
            return Err(Failure::Unreachable(format!(
                "{SERVICE} is not running on the host; kwin-mcp does not start it"
            )));
        }
        let proxy = zbus::Proxy::new(host, SERVICE, PATH, INTERFACE)
            .await
            .map_err(unreachable_error("host kwalletd6 proxy"))?;
        match host_call::<_, bool>(&proxy, "isEnabled", &()).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(Failure::Refused(
                    "KWallet is disabled on the host".to_owned(),
                ));
            }
            Err(error) => {
                return Err(Failure::Unreachable(format!(
                    "host kwalletd6 unavailable: {error}"
                )));
            }
        }
        let name: String = host_call(&proxy, "networkWallet", &())
            .await
            .map_err(unreachable_error("networkWallet"))?;
        let listed: Vec<String> = host_call(&proxy, "wallets", &())
            .await
            .map_err(unreachable_error("wallets"))?;
        if !listed.contains(&name) {
            return Err(Failure::Refused(format!(
                "host wallet '{name}' is not listed by kwalletd6; kwin-mcp does not create it"
            )));
        }
        match find_collection(host, &name).await {
            Some((_, false)) => {}
            Some((_, true)) => {
                return Err(Failure::Refused(format!(
                    "host wallet '{name}' is locked; kwin-mcp does not unlock it"
                )));
            }
            None => {
                return Err(Failure::Refused(format!(
                    "host Secret Service has no readable collection for '{name}'"
                )));
            }
        }
        let handle: i32 =
            match host_call(&proxy, "open", &(name.as_str(), 0i64, SNAPSHOT_APP)).await {
                Ok(handle) if handle >= 0 => handle,
                Ok(_) => {
                    return Err(Failure::Unreachable(
                        "host kwalletd6 refused to open the wallet".to_owned(),
                    ));
                }
                Err(error) => return Err(Failure::Unreachable(format!("open: {error}"))),
            };
        let copied = copy_entries(&proxy, handle, &name).await;
        // The snapshot's own handle is always released, whatever the copy did.
        let _ = host_call::<_, i32>(&proxy, "close", &(handle, false, SNAPSHOT_APP)).await;
        let wallet = copied.map_err(Failure::Unreachable)?;
        let reason = format!(
            "session-local copy of host wallet '{name}': {} entries",
            wallet.entry_count()
        );
        Ok(Snapshot {
            wallet: Some(wallet),
            reason,
        })
    }

    /// The collection holding `wallet`, and whether it is locked.
    async fn find_collection(
        host: &zbus::Connection,
        wallet: &str,
    ) -> Option<(zbus::zvariant::OwnedObjectPath, bool)> {
        let service = zbus::Proxy::new(
            host,
            SECRET_BACKEND,
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service",
        )
        .await
        .ok()?;
        let collections = service
            .get_property::<Vec<zbus::zvariant::OwnedObjectPath>>("Collections")
            .await
            .ok()?;
        for path in collections {
            let Ok(collection) = zbus::Proxy::new(
                host,
                SECRET_BACKEND,
                path.clone(),
                "org.freedesktop.Secret.Collection",
            )
            .await
            else {
                continue;
            };
            let label = collection
                .get_property::<String>("Label")
                .await
                .unwrap_or_default();
            if label == wallet || path.as_str().rsplit('/').next() == Some(wallet) {
                let locked = collection.get_property::<bool>("Locked").await.ok()?;
                return Some((path, locked));
            }
        }
        None
    }

    /// Wallet the host's own default points at when kwalletd6 cannot be asked.
    const DEFAULT_WALLET: &str = "kdewallet";

    /// Read the wallet straight from ksecretd with the standard Secret Service
    /// calls (OpenSession "plain", GetSecrets). It never unlocks anything: a
    /// locked collection is refused, and GetSecrets on an unlocked one cannot
    /// prompt. kwalletd6 is a thin layer over these items: the KWallet folder is
    /// the item's `server` attribute, the entry name its `user`, and `type` is
    /// plaintext, binary or map.
    async fn read_via_secret_service(
        host: &zbus::Connection,
        why: &str,
    ) -> Result<Snapshot, Failure> {
        use std::collections::HashMap;
        use zbus::zvariant::{OwnedObjectPath, Value};
        if !name_has_owner(host, SECRET_BACKEND).await {
            return Err(Failure::Refused(format!(
                "{SECRET_BACKEND} is not running on the host; kwin-mcp does not start it"
            )));
        }
        let (collection_path, locked) =
            find_collection(host, DEFAULT_WALLET).await.ok_or_else(|| {
                Failure::Refused(format!(
                    "{SECRET_BACKEND} has no readable collection '{DEFAULT_WALLET}'"
                ))
            })?;
        if locked {
            return Err(Failure::Refused(format!(
                "host wallet '{DEFAULT_WALLET}' is locked; kwin-mcp does not unlock it"
            )));
        }
        let service = zbus::Proxy::new(
            host,
            SECRET_BACKEND,
            "/org/freedesktop/secrets",
            "org.freedesktop.Secret.Service",
        )
        .await
        .map_err(unreachable_error("ksecretd proxy"))?;
        let collection = zbus::Proxy::new(
            host,
            SECRET_BACKEND,
            collection_path.clone(),
            "org.freedesktop.Secret.Collection",
        )
        .await
        .map_err(unreachable_error("ksecretd collection proxy"))?;
        let items: Vec<OwnedObjectPath> = collection
            .get_property("Items")
            .await
            .map_err(unreachable_error("Items"))?;
        if items.len() > MAX_ENTRIES {
            return Err(Failure::Refused(
                "host wallet is too large to copy into a session".to_owned(),
            ));
        }
        let (_, session): (zbus::zvariant::OwnedValue, OwnedObjectPath) = host_call(
            &service,
            "OpenSession",
            &("plain", Value::new(String::new())),
        )
        .await
        .map_err(unreachable_error("OpenSession"))?;
        let secrets = host_call::<
            _,
            HashMap<OwnedObjectPath, (OwnedObjectPath, Vec<u8>, Vec<u8>, String)>,
        >(&service, "GetSecrets", &(&items, &session))
        .await;
        let mut found: Vec<(u64, String, String, Entry)> = Vec::new();
        let mut result: Result<(), Failure> = Ok(());
        match secrets {
            Err(error) => result = Err(Failure::Unreachable(format!("GetSecrets: {error}"))),
            Ok(secrets) => {
                for (path, (_, _, value, _)) in secrets {
                    let Ok(item) = zbus::Proxy::new(
                        host,
                        SECRET_BACKEND,
                        path.clone(),
                        "org.freedesktop.Secret.Item",
                    )
                    .await
                    else {
                        continue;
                    };
                    let attributes: HashMap<String, String> =
                        match item.get_property("Attributes").await {
                            Ok(attributes) => attributes,
                            Err(_) => continue,
                        };
                    let (Some(folder), Some(key)) =
                        (attributes.get("server"), attributes.get("user"))
                    else {
                        continue;
                    };
                    let modified: u64 = item.get_property("Modified").await.unwrap_or(0);
                    let kind = match attributes.get("type").map(String::as_str) {
                        Some("plaintext") => TYPE_PASSWORD,
                        Some("map") => TYPE_MAP,
                        Some("binary") => TYPE_STREAM,
                        _ if std::str::from_utf8(&value).is_ok() => TYPE_PASSWORD,
                        _ => TYPE_STREAM,
                    };
                    found.push((
                        modified,
                        folder.clone(),
                        key.clone(),
                        Entry { kind, data: value },
                    ));
                }
            }
        }
        // The session is closed whatever the read did.
        if let Ok(proxy) = zbus::Proxy::new(
            host,
            SECRET_BACKEND,
            session,
            "org.freedesktop.Secret.Session",
        )
        .await
        {
            let _ = host_call::<_, ()>(&proxy, "Close", &()).await;
        }
        result?;
        // Two items can share a folder and name (the host wallet has two "Chrome
        // Safe Storage" items, written by different clients): the most recently
        // modified one is used.
        found.sort_by_key(|(modified, ..)| *modified);
        let mut wallet = Wallet {
            name: DEFAULT_WALLET.to_owned(),
            folders: BTreeMap::new(),
        };
        let (mut bytes, mut conflicts) = (0usize, 0usize);
        for (_, folder, key, entry) in found {
            bytes += entry.data.len();
            if bytes > MAX_BYTES {
                return Err(Failure::Refused(
                    "host wallet is too large to copy into a session".to_owned(),
                ));
            }
            let previous = wallet
                .folders
                .entry(folder)
                .or_default()
                .insert(key, entry.clone());
            conflicts += usize::from(previous.is_some_and(|previous| previous.data != entry.data));
        }
        let reason = format!(
            "session-local copy of host wallet '{DEFAULT_WALLET}' read from ksecretd because {why}: {} entries{}",
            wallet.entry_count(),
            if conflicts > 0 {
                format!(
                    " ({conflicts} duplicate name(s) held different values; the newest was used)"
                )
            } else {
                String::new()
            }
        );
        Ok(Snapshot {
            wallet: Some(wallet),
            reason,
        })
    }

    async fn copy_entries(
        proxy: &zbus::Proxy<'_>,
        handle: i32,
        name: &str,
    ) -> Result<Wallet, String> {
        let describe = |what: &str, error: zbus::Error| format!("{what}: {error}");
        let mut wallet = Wallet {
            name: name.to_owned(),
            folders: BTreeMap::new(),
        };
        let (mut entries, mut bytes) = (0usize, 0usize);
        // kwalletd6 over a Secret Service backend lists a folder once per item it
        // holds (2848 names for 17 folders on the dev machine), so names are
        // deduplicated before each is read.
        let folders: std::collections::BTreeSet<String> =
            host_call::<_, Vec<String>>(proxy, "folderList", &(handle, SNAPSHOT_APP))
                .await
                .map_err(|e| describe("folderList", e))?
                .into_iter()
                .collect();
        for folder in folders {
            let keys: std::collections::BTreeSet<String> = host_call::<_, Vec<String>>(
                proxy,
                "entryList",
                &(handle, folder.as_str(), SNAPSHOT_APP),
            )
            .await
            .map_err(|e| describe("entryList", e))?
            .into_iter()
            .collect();
            let mut copy = BTreeMap::new();
            for key in keys {
                let args = (handle, folder.as_str(), key.as_str(), SNAPSHOT_APP);
                let kind: i32 = host_call(proxy, "entryType", &args)
                    .await
                    .map_err(|e| describe("entryType", e))?;
                let data: Vec<u8> = match kind {
                    TYPE_PASSWORD => host_call::<_, String>(proxy, "readPassword", &args)
                        .await
                        .map_err(|e| describe("readPassword", e))?
                        .into_bytes(),
                    TYPE_MAP => host_call(proxy, "readMap", &args)
                        .await
                        .map_err(|e| describe("readMap", e))?,
                    _ => host_call(proxy, "readEntry", &args)
                        .await
                        .map_err(|e| describe("readEntry", e))?,
                };
                entries += 1;
                bytes += data.len();
                if entries > MAX_ENTRIES || bytes > MAX_BYTES {
                    return Err("host wallet is too large to copy into a session".to_owned());
                }
                copy.insert(key, Entry { kind, data });
            }
            wallet.folders.insert(folder, copy);
        }
        Ok(wallet)
    }

    impl WalletMediator {
        /// Serve org.kde.kwalletd6 on the private bus at `bus_address` from
        /// `wallet`. None answers as a disabled KWallet.
        pub async fn start(bus_address: &str, wallet: Option<Wallet>) -> zbus::Result<Self> {
            Self::start_state(
                bus_address,
                State {
                    wallet,
                    next_handle: 1,
                    next_transaction: 1,
                    ..State::default()
                },
            )
            .await
        }
        pub(super) fn saved_state(&self) -> std::io::Result<State> {
            self.state
                .lock()
                .map(|state| state.clone())
                .map_err(|_| std::io::Error::other("wallet state lock poisoned"))
        }
        pub(super) async fn start_state(bus_address: &str, saved: State) -> zbus::Result<Self> {
            let session = zbus::connection::Builder::address(bus_address)?
                .name(SERVICE)?
                .build()
                .await?;
            let state = Arc::new(Mutex::new(saved));
            let mut calls = zbus::MessageStream::from(&session);
            let task = {
                let (session, state) = (session.clone(), state.clone());
                tokio::spawn(async move {
                    while let Some(Ok(message)) = calls.next().await {
                        if message.message_type() != MessageType::MethodCall {
                            continue;
                        }
                        let (session, state) = (session.clone(), state.clone());
                        tokio::spawn(async move {
                            if let Err(error) = handle_call(&session, &state, &message).await {
                                eprintln!("session wallet: {error}");
                                let _ = session
                                    .reply_error(
                                        &message.header(),
                                        "org.freedesktop.DBus.Error.Failed",
                                        &error.to_string(),
                                    )
                                    .await;
                            }
                        });
                    }
                })
            };
            Ok(Self {
                state,
                tasks: vec![task],
            })
        }

        /// Stop serving and drop the session's wallet. Nothing on the host is
        /// touched.
        pub async fn shutdown(mut self) {
            for task in self.tasks.drain(..) {
                task.abort();
            }
            if let Ok(mut state) = self.state.lock() {
                *state = State::default();
            }
        }
    }

    const INTROSPECTION: &str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \
\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\"><node><interface name=\"org.kde.KWallet\"/></node>";

    /// The wallet behind `handle` when `app` owns it.
    fn owned<'a>(state: &'a mut State, handle: i32, app: &str) -> Option<&'a mut Wallet> {
        if state.handles.get(&handle).is_some_and(|owner| owner == app) {
            state.wallet.as_mut()
        } else {
            None
        }
    }

    async fn handle_call(
        session: &zbus::Connection,
        state: &Arc<Mutex<State>>,
        message: &zbus::Message,
    ) -> zbus::Result<()> {
        let header = message.header();
        let member = header
            .member()
            .map(|member| member.to_string())
            .unwrap_or_default();
        let interface = header
            .interface()
            .map(|interface| interface.to_string())
            .unwrap_or_default();
        match (interface.as_str(), member.as_str()) {
            ("org.freedesktop.DBus.Introspectable", "Introspect") => {
                return session.reply(&header, &INTROSPECTION).await;
            }
            ("org.freedesktop.DBus.Peer", "Ping") => return session.reply(&header, &()).await,
            ("org.freedesktop.DBus.Peer", "GetMachineId") => {
                let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
                return session.reply(&header, &id.trim()).await;
            }
            _ => {}
        }
        if header.path().map(|path| path.as_str()) != Some(PATH)
            || (interface != INTERFACE && !interface.is_empty())
        {
            return session
                .reply_error(
                    &header,
                    "org.freedesktop.DBus.Error.UnknownObject",
                    &"kwin-mcp exposes only org.kde.KWallet at /modules/kwalletd6",
                )
                .await;
        }
        let body = message.body();
        let signature = body.signature().to_string_no_parens();
        let deny = |reason: &str| format!("kwin-mcp: {member} {reason}");
        let bad = |error: zbus::Error| zbus::Error::Failure(format!("{member}: {error}"));

        // Replies are built under the lock and sent after it is released.
        enum Reply {
            Bool(bool),
            Int(i32),
            Text(String),
            Bytes(Vec<u8>),
            List(Vec<String>),
            Dict(HashMap<String, Value<'static>>),
            Denied,
            /// An asynchronous open: the transaction id, then the result signal.
            Async(i32, i32),
        }
        let wallet_name = state
            .lock()
            .map_err(|_| zbus::Error::Failure("wallet state poisoned".to_owned()))?
            .wallet
            .as_ref()
            .map(|wallet| wallet.name.clone());
        let Some(name) = wallet_name else {
            return match member.as_str() {
                "isEnabled" => session.reply(&header, &false).await,
                "wallets" => session.reply(&header, &Vec::<String>::new()).await,
                "open" | "openPath" => session.reply(&header, &-1i32).await,
                _ => {
                    session
                        .reply_error(
                            &header,
                            "org.freedesktop.DBus.Error.AccessDenied",
                            &deny(
                                "is unavailable: the host wallet is not copied into this session",
                            ),
                        )
                        .await
                }
            };
        };
        let reply = {
            let mut state = state
                .lock()
                .map_err(|_| zbus::Error::Failure("wallet state poisoned".to_owned()))?;
            match member.as_str() {
                "isEnabled" => Reply::Bool(true),
                "networkWallet" | "localWallet" => Reply::Text(name),
                "wallets" => Reply::List(vec![name]),
                "users" => Reply::List(Vec::new()),
                "isOpen" if signature == "s" => {
                    let requested: String = body.deserialize().map_err(bad)?;
                    Reply::Bool(requested == name && !state.handles.is_empty())
                }
                "isOpen" => {
                    let handle: i32 = body.deserialize().map_err(bad)?;
                    Reply::Bool(state.handles.contains_key(&handle))
                }
                "open" | "openAsync" => {
                    let (requested, app, asynchronous) = if member == "open" {
                        let (requested, _window, app): (String, i64, String) =
                            body.deserialize().map_err(bad)?;
                        (requested, app, false)
                    } else {
                        let (requested, _window, app, _session): (String, i64, String, bool) =
                            body.deserialize().map_err(bad)?;
                        (requested, app, true)
                    };
                    if requested != name {
                        Reply::Int(-1)
                    } else {
                        let handle = state.next_handle;
                        state.next_handle += 1;
                        state.handles.insert(handle, app);
                        if asynchronous {
                            let transaction = state.next_transaction;
                            state.next_transaction += 1;
                            Reply::Async(transaction, handle)
                        } else {
                            Reply::Int(handle)
                        }
                    }
                }
                "openPath" | "openPathAsync" => Reply::Int(-1),
                "close" if signature == "ibs" => {
                    let (handle, _force, app): (i32, bool, String) =
                        body.deserialize().map_err(bad)?;
                    if state
                        .handles
                        .get(&handle)
                        .is_some_and(|owner| *owner == app)
                    {
                        state.handles.remove(&handle);
                        Reply::Int(0)
                    } else {
                        Reply::Int(-1)
                    }
                }
                "close" => Reply::Int(-1),
                "folderList" => {
                    let (handle, app): (i32, String) = body.deserialize().map_err(bad)?;
                    Reply::List(
                        owned(&mut state, handle, &app)
                            .map(|wallet| wallet.folders.keys().cloned().collect())
                            .unwrap_or_default(),
                    )
                }
                "hasFolder" => {
                    let (handle, folder, app): (i32, String, String) =
                        body.deserialize().map_err(bad)?;
                    Reply::Bool(
                        owned(&mut state, handle, &app)
                            .is_some_and(|wallet| wallet.folders.contains_key(&folder)),
                    )
                }
                "entryList" => {
                    let (handle, folder, app): (i32, String, String) =
                        body.deserialize().map_err(bad)?;
                    Reply::List(
                        owned(&mut state, handle, &app)
                            .and_then(|wallet| wallet.folders.get(&folder))
                            .map(|entries| entries.keys().cloned().collect())
                            .unwrap_or_default(),
                    )
                }
                "hasEntry" | "entryType" | "readPassword" | "readEntry" | "readMap" => {
                    let (handle, folder, key, app): (i32, String, String, String) =
                        body.deserialize().map_err(bad)?;
                    let entry = owned(&mut state, handle, &app)
                        .and_then(|wallet| wallet.entry(&folder, &key))
                        .cloned();
                    match member.as_str() {
                        "hasEntry" => Reply::Bool(entry.is_some()),
                        "entryType" => Reply::Int(entry.map_or(TYPE_UNKNOWN, |entry| entry.kind)),
                        "readPassword" => Reply::Text(
                            entry
                                .map(|entry| String::from_utf8_lossy(&entry.data).into_owned())
                                .unwrap_or_default(),
                        ),
                        _ => Reply::Bytes(entry.map(|entry| entry.data).unwrap_or_default()),
                    }
                }
                "entriesList" | "mapList" | "passwordList" => {
                    let (handle, folder, app): (i32, String, String) =
                        body.deserialize().map_err(bad)?;
                    let wanted = match member.as_str() {
                        "mapList" => Some(TYPE_MAP),
                        "passwordList" => Some(TYPE_PASSWORD),
                        _ => None,
                    };
                    let mut dict = HashMap::new();
                    if let Some(entries) = owned(&mut state, handle, &app)
                        .and_then(|wallet| wallet.folders.get(&folder))
                    {
                        for (key, entry) in entries
                            .iter()
                            .filter(|(_, entry)| wanted.is_none_or(|kind| entry.kind == kind))
                        {
                            let value =
                                if entry.kind == TYPE_PASSWORD && wanted == Some(TYPE_PASSWORD) {
                                    Value::from(String::from_utf8_lossy(&entry.data).into_owned())
                                } else {
                                    Value::from(entry.data.clone())
                                };
                            dict.insert(key.clone(), value);
                        }
                    }
                    Reply::Dict(dict)
                }
                // Writes change the session's copy only.
                "createFolder" => {
                    let (handle, folder, app): (i32, String, String) =
                        body.deserialize().map_err(bad)?;
                    Reply::Bool(owned(&mut state, handle, &app).is_some_and(|wallet| {
                        wallet.folders.entry(folder).or_default();
                        true
                    }))
                }
                "removeFolder" => {
                    let (handle, folder, app): (i32, String, String) =
                        body.deserialize().map_err(bad)?;
                    Reply::Bool(
                        owned(&mut state, handle, &app)
                            .is_some_and(|wallet| wallet.folders.remove(&folder).is_some()),
                    )
                }
                "writePassword" => {
                    let (handle, folder, key, value, app): (i32, String, String, String, String) =
                        body.deserialize().map_err(bad)?;
                    let done = owned(&mut state, handle, &app).is_some_and(|wallet| {
                        wallet.write(&folder, &key, TYPE_PASSWORD, value.into_bytes())
                    });
                    Reply::Int(if done { 0 } else { -1 })
                }
                "writeEntry" | "writeMap" => {
                    let (handle, folder, key, value, kind, app): (
                        i32,
                        String,
                        String,
                        Vec<u8>,
                        i32,
                        String,
                    ) = if signature == "issayis" {
                        body.deserialize().map_err(bad)?
                    } else {
                        let (handle, folder, key, value, app): (
                            i32,
                            String,
                            String,
                            Vec<u8>,
                            String,
                        ) = body.deserialize().map_err(bad)?;
                        (
                            handle,
                            folder,
                            key,
                            value,
                            if member == "writeMap" {
                                TYPE_MAP
                            } else {
                                TYPE_STREAM
                            },
                            app,
                        )
                    };
                    let done = owned(&mut state, handle, &app)
                        .is_some_and(|wallet| wallet.write(&folder, &key, kind, value));
                    Reply::Int(if done { 0 } else { -1 })
                }
                "removeEntry" => {
                    let (handle, folder, key, app): (i32, String, String, String) =
                        body.deserialize().map_err(bad)?;
                    let done = owned(&mut state, handle, &app).is_some_and(|wallet| {
                        wallet
                            .folders
                            .get_mut(&folder)
                            .is_some_and(|entries| entries.remove(&key).is_some())
                    });
                    Reply::Int(if done { 0 } else { -1 })
                }
                _ => Reply::Denied,
            }
        };
        match reply {
            Reply::Bool(value) => session.reply(&header, &value).await,
            Reply::Int(value) => session.reply(&header, &value).await,
            Reply::Text(value) => session.reply(&header, &value).await,
            Reply::Bytes(value) => session.reply(&header, &value).await,
            Reply::List(value) => session.reply(&header, &value).await,
            Reply::Dict(value) => session.reply(&header, &value).await,
            Reply::Denied => {
                session
                    .reply_error(
                        &header,
                        "org.freedesktop.DBus.Error.AccessDenied",
                        &deny("is not available in the session-local wallet"),
                    )
                    .await
            }
            Reply::Async(transaction, handle) => {
                // The app learns its transaction id first, then the result.
                session.reply(&header, &transaction).await?;
                session
                    .emit_signal(
                        None::<&str>,
                        PATH,
                        INTERFACE,
                        "walletAsyncOpened",
                        &(transaction, handle),
                    )
                    .await?;
                session
                    .emit_signal(None::<&str>, PATH, INTERFACE, "walletOpened", &(handle,))
                    .await
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn wallet() -> Wallet {
            let mut wallet = Wallet {
                name: "kdewallet".to_owned(),
                folders: BTreeMap::new(),
            };
            wallet.folders.entry("Chrome Keys".to_owned()).or_default();
            assert!(wallet.write(
                "Chrome Keys",
                "Chrome Safe Storage",
                TYPE_PASSWORD,
                b"secret".to_vec()
            ));
            wallet
        }

        #[test]
        fn multi_argument_calls_are_matched_without_outer_parens() -> zbus::Result<()> {
            let message = zbus::Message::method_call(PATH, "close")?
                .interface(INTERFACE)?
                .build(&(1i32, false, "app"))?;
            assert_eq!(message.body().signature().to_string_no_parens(), "ibs");
            Ok(())
        }

        #[test]
        fn writes_need_an_existing_folder_and_stay_in_the_copy() {
            let mut wallet = wallet();
            assert!(!wallet.write("Other", "key", TYPE_STREAM, vec![1]));
            assert_eq!(wallet.entry_count(), 1);
            assert!(wallet.write("Chrome Keys", "extra", TYPE_MAP, vec![2, 3]));
            assert_eq!(wallet.entry_count(), 2);
            assert_eq!(
                wallet.entry("Chrome Keys", "extra").map(|entry| entry.kind),
                Some(TYPE_MAP)
            );
        }

        #[test]
        fn handles_belong_to_the_app_that_opened_them() {
            let mut state = State {
                wallet: Some(wallet()),
                next_handle: 1,
                ..State::default()
            };
            state.handles.insert(7, "chrome".to_owned());
            assert!(owned(&mut state, 7, "chrome").is_some());
            assert!(owned(&mut state, 7, "other").is_none());
            assert!(owned(&mut state, 8, "chrome").is_none());
        }
    }
}

use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use serde::{Deserialize, Serialize};
use serde_aux::field_attributes::deserialize_number_from_string;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type McpError = rmcp::ErrorData;

#[derive(Debug, thiserror::Error)]
enum KwinError {
    #[error(transparent)]
    Zbus(#[from] zbus::Error),
    #[error(transparent)]
    Zvariant(#[from] zbus::zvariant::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Nix(#[from] nix::Error),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
    #[error(transparent)]
    TryFromInt(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
    #[error(transparent)]
    SystemTime(#[from] std::time::SystemTimeError),
    #[error(transparent)]
    Atspi(#[from] atspi::AtspiError),
    #[error(transparent)]
    Png(#[from] png::EncodingError),
    #[error("{0}")]
    Msg(String),
}

impl From<KwinError> for McpError {
    fn from(e: KwinError) -> Self {
        McpError::internal_error(e.to_string(), None)
    }
}

fn describe_channel_failure(mut error: McpError, id: &str, operation: &str) -> McpError {
    let transport = error.message.to_ascii_lowercase();
    if !transport.contains("broken pipe")
        && !transport.contains("connection reset")
        && !transport.contains("connection closed")
    {
        return error;
    }
    let original = error.message.to_string();
    let (channel, label) = match operation {
        "screenshot" | "window_list" | "window_activate" => {
            ("kwin_control", "KWin control/capture connection")
        }
        _ => ("desktop_control", "desktop control connection"),
    };
    let mut data = match error.data.take() {
        Some(serde_json::Value::Object(data)) => data,
        Some(value) => serde_json::Map::from_iter([("original_data".to_owned(), value)]),
        None => serde_json::Map::new(),
    };
    if let Some(reason) = data.remove("reason") {
        data.insert("original_reason".to_owned(), reason);
    }
    data.insert(
        "reason".to_owned(),
        serde_json::json!("session_channel_failed"),
    );
    data.insert("session_id".to_owned(), serde_json::json!(id));
    data.insert("operation".to_owned(), serde_json::json!(operation));
    data.insert("channel".to_owned(), serde_json::json!(channel));
    data.insert("transport_error".to_owned(), serde_json::json!(original));
    data.insert(
        "sandbox_exit_confirmed".to_owned(),
        serde_json::json!(false),
    );
    data.insert("oom_confirmed".to_owned(), serde_json::json!(false));
    error.message = format!(
        "Session {id}: {operation} failed on the {label}: {original}. \
         The sandbox launcher has not reported an exit; OOM is not confirmed. \
         Call session_stop for this handle, then session_start without session_id \
         and relaunch apps to continue. Inspect prior input before repeating an input action."
    )
    .into();
    error.data = Some(serde_json::Value::Object(data));
    error
}

// ── Kernel / protocol constants ──────────────────────────────────────────

// Linux /proc/net/route RTF_UP bit.
const IPV4_ROUTE_UP: u32 = 0x1;

// Linux evdev keycode for LeftShift (include/uapi/linux/input-event-codes.h).
const LINUX_KEY_LEFTSHIFT: u32 = 42;

// KWin org.kde.KWin.EIS.RemoteDesktop.connectToEIS() capabilities bitfield.
// bit 0 (0b001) = keyboard, bit 1 (0b010) = pointer, bit 2 (0b100) = touch.
// 0b011 = keyboard + pointer (what this server needs).
const EIS_CAPS_KBD_POINTER: i32 = 0b011;
const EIS_CALLBACK_VERSION: u32 = 1;

// ── Timings ──────────────────────────────────────────────────────────────

use std::time::Duration;

// Session startup: general session-bus / wayland socket wait.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_POLL: Duration = Duration::from_millis(50);
// The session entrypoint writes kwin_wayland's exit status here when it exits.
const KWIN_EXIT_FILE: &str = "kwin-exit";

// Hard wall-clock limit for the entire session_start tool — abort if exceeded.
const SESSION_START_HARD_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_START_MAX_SECONDS: u64 = 300;

// Session overlay layers and the FUSE bridge image live on disk under
// ${XDG_CACHE_HOME:-~/.cache}/kwin-mcp (/tmp is RAM with a per-user quota);
// session_start needs this much free there unless disk_min_free_mib says otherwise.
const SESSION_DISK_MIN_FREE: u64 = 2 << 30;
const DEFAULT_STATS_SECONDS: u64 = 3600;

// EIS (Emulated Input Sender) negotiation.
const EIS_NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const EIS_NEGOTIATION_POLL: Duration = Duration::from_millis(50);

// xdg-dbus-proxy socket appearance.
const DBUS_PROXY_TIMEOUT: Duration = Duration::from_secs(3);
const DBUS_PROXY_POLL: Duration = Duration::from_millis(20);

// libdbus uses this portable limit even on Linux, whose sun_path is larger.
// https://dbus.freedesktop.org/doc/api/html/group__DBusSysdeps.html
// (_DBUS_MAX_SUN_PATH_LENGTH). Session apps also connect through libdbus.
const DBUS_SOCKET_PATH_MAX: usize = 99;
// The longest socket name bound directly in a session workdir, and the root
// used instead of TMPDIR when TMPDIR would push it past DBUS_SOCKET_PATH_MAX.
const WORKDIR_LONGEST_SOCKET: &str = "service_bus_socket";
const SHORT_RUNTIME_ROOT: &str = "/tmp";

// KWin unique-name discovery: per-candidate introspect probe timeout.
const KWIN_NAME_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

// Default AT-SPI tree deadline and find_ui_elements hard timeout.
const ATSPI_TRAVERSAL_TIMEOUT: Duration = Duration::from_secs(5);
const ACCESSIBILITY_TREE_TIMEOUT_MIN_SECONDS: u32 = 1;
const ACCESSIBILITY_TREE_TIMEOUT_MAX_SECONDS: u32 = 30;
// keyboard_key checks a browser page's focus over AT-SPI (#180) within this
// time and node count; past either it makes no claim.
const KEY_FOCUS_CHECK_TIMEOUT: Duration = Duration::from_secs(1);
const KEY_FOCUS_CHECK_NODES: usize = 5000;
const SCREENSHOT_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const SCREENSHOT_TOOL_TIMEOUT: Duration = Duration::from_secs(20);
// Percent of compared dialog pixels that may differ between the composited
// screen and the window capture before a screenshot is rejected.
const TRANSIENT_MISMATCH_PERCENT: u64 = 1;
// How long screenshot recaptures a dialog missing from the composited screen.
const TRANSIENT_CAPTURE_WAIT: Duration = Duration::from_secs(2);
// A click that closes one window leaves KWin without an active window until
// the next one maps and takes focus; screenshot waits this long for one.
const ACTIVE_WINDOW_WAIT: Duration = Duration::from_secs(2);
const ACTIVE_WINDOW_POLL: Duration = Duration::from_millis(100);
const NO_ACTIVE_WINDOW: &str = "KWin script error: No active window";

// Largest accessibility_tree reply, in characters. The tree is returned as the
// deepest run of whole levels that fits, so one call cannot blow an agent's
// context window.
const A11Y_TREE_MAX_CHARS: usize = 30_000;

// Let KWin publish an activated window before reading its geometry.
const WINDOW_ACTIVATION_SETTLE: Duration = Duration::from_millis(50);
/// Listings tried before a window that activation cannot find is reported closed,
/// and the pause between them.
const WINDOW_LOOKUP_ATTEMPTS: u32 = 10;
const WINDOW_LOOKUP_RETRY: Duration = Duration::from_millis(100);
/// How long an input send may wait for KWin to drain a full EIS socket.
const EIS_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

// mouse_scroll splits its delta (pixels when smooth, notches when discrete) into one
// report per notch, at most SCROLL_MAX_REPORTS of them, sent SCROLL_REPORT_INTERVAL_MS
// apart, so a call's duration is bounded whatever its delta. Wayland axis values carry
// 10 per wheel notch and Chromium scrolls 120 px per notch, so a smooth pixel is 1/12
// of an axis unit.
const SCROLL_PIXELS_PER_NOTCH: u32 = 120;
const SCROLL_PIXELS_PER_AXIS_UNIT: f32 = 12.0;
const SCROLL_MAX_REPORTS: u32 = 32;
const SCROLL_REPORT_INTERVAL_MS: f64 = 16.0;
// EIS scroll_discrete takes value120 units: 120 = one wheel notch.
const SCROLL_VALUE120_PER_NOTCH: i32 = 120;

// launch_app: window-appear polling.
const LAUNCH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const LAUNCH_WINDOW_TIMEOUT: Duration = Duration::from_secs(15);
const LAUNCH_WINDOW_TIMEOUT_MAX_SECS: u64 = 120;
// The most stdout an expect_window=false result quotes; the file has the rest.
const LAUNCH_STDOUT_TAIL_BYTES: usize = 4096;
// The window baseline is read before submission, but a KWin that does not answer
// within this bound must not hold the command back: it is read after submission.
const LAUNCH_BASELINE_TIMEOUT: Duration = Duration::from_secs(5);
// The call deadline leaves room for the baseline reads and CDP retries.
const LAUNCH_CALL_OVERHEAD_TIMEOUT: Duration = Duration::from_secs(30);
// expect_window: false returns a running command's PID, or its fast exit status.
const LAUNCH_PID_TIMEOUT: Duration = Duration::from_secs(5);
const LAUNCH_EXIT_STATUS_GRACE: Duration = Duration::from_secs(1);
const LAUNCH_LATE_WINDOW_GRACE: Duration = Duration::from_secs(2);
const LAUNCH_TIMEOUT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
// A launch that exits without a window reports this many stderr lines, read
// from the bytes sandbox.log gained since the command was submitted.
const LAUNCH_STDERR_TAIL_LINES: usize = 20;
const LAUNCH_STDERR_READ_BYTES: u64 = 64 * 1024;

// KWin 6.7.4 and older buffer a frame's last partial page in a QFile and drop
// it when a non-blocking pipe is full at close. Frames are captured into a
// memfd instead, which never blocks a write; the fill is checked this often.
const SCREENSHOT_FILL_POLL: Duration = Duration::from_millis(5);

// A screenshot shortly after input waits for the app to handle it and repaint:
// it polls frames until the screen has been unchanged for SCREENSHOT_SETTLE_QUIET,
// giving up SCREENSHOT_SETTLE_LIMIT after the last input event.
const SCREENSHOT_SETTLE_QUIET: Duration = Duration::from_millis(200);
const SCREENSHOT_SETTLE_POLL: Duration = Duration::from_millis(40);
const SCREENSHOT_SETTLE_LIMIT: Duration = Duration::from_millis(1500);

// launch_app: CDP connect retry.
const CDP_CONNECT_POLLS: u32 = 25; // 5s total (reuses LAUNCH_POLL_INTERVAL)

// screenshot cursor=true: half-edge of crop region around cursor (output covers
// CURSOR_ZOOM_HALF_EDGE*2 source pixels). Render size / source pixels = zoom factor.
const CURSOR_ZOOM_HALF_EDGE: i32 = 200;

// ── Virtual-session display & font settings ──────────────────────────────

// Compiled-in defaults. Overridable at server launch via --width/--height CLI
// flags, and per-session via session_start's optional width/height params
// (unless the server was launched with --no-override, which pins the CLI/
// compiled size and silently ignores tool params).
const VIRTUAL_SCREEN_WIDTH: u32 = 3840;
const VIRTUAL_SCREEN_HEIGHT: u32 = 2160;
// Sanity bounds for any requested dimension. Max matches the viewer's
// PipeWire format-pod range ceiling (kwin-viewer.rs build_format_pod).
const MIN_SCREEN_DIM: u32 = 240;
const MAX_SCREEN_DIM: u32 = 8192;

const KDE_SCALE_FACTOR: &str = "1"; // 1 | 2 | 3
const KDE_FORCE_FONT_DPI: u32 = 96; // 96 | 120 | 144 | 192
const KDE_HINT_STYLE: &str = "hintnone"; // hintnone | hintslight | hintmedium | hintfull
const KDE_SUB_PIXEL: &str = "none"; // none | rgb | bgr | vrgb | vbgr

const UI_FONT_FAMILY: &str = "Noto Sans";
const UI_FONT_SIZE: u32 = 14;
const UI_FONT_SIZE_SMALL: u32 = 12;

const FIXED_FONT_FAMILY: &str = "Hack";
const FIXED_FONT_SIZE: u32 = 14;

const FONT_WEIGHT_REGULAR: u32 = 400;
const FONT_WEIGHT_BOLD: u32 = 700;

fn qt_font_spec(family: &str, size: u32, weight: u32, bold_suffix: bool) -> String {
    // Qt KConfig font format: family,size,-1,5,weight,0,0,0,0,0,0,0,0,0,0,1[,Bold]
    let suffix = if bold_suffix { ",Bold" } else { "" };
    format!("{family},{size},-1,5,{weight},0,0,0,0,0,0,0,0,0,0,1{suffix}")
}

// ── Evdev keycodes ───────────────────────────────────────────────────────

use keyboard_codes::Platform;

fn char_key(ch: char) -> Result<(u32, bool), McpError> {
    let (raw, shifted) = match ch {
        'a'..='z'
        | '0'..='9'
        | '`'
        | '-'
        | '='
        | '['
        | ']'
        | '\\'
        | ';'
        | '\''
        | ','
        | '.'
        | '/'
        | ' '
        | '\t'
        | '\n' => (ch, false),
        'A'..='Z' => (ch.to_ascii_lowercase(), true),
        '~' => ('`', true),
        '!' => ('1', true),
        '@' => ('2', true),
        '#' => ('3', true),
        '$' => ('4', true),
        '%' => ('5', true),
        '^' => ('6', true),
        '&' => ('7', true),
        '*' => ('8', true),
        '(' => ('9', true),
        ')' => ('0', true),
        '_' => ('-', true),
        '+' => ('=', true),
        '{' => ('[', true),
        '}' => (']', true),
        '|' => ('\\', true),
        ':' => (';', true),
        '"' => ('\'', true),
        '<' => (',', true),
        '>' => ('.', true),
        '?' => ('/', true),
        other => Err(McpError::invalid_params(
            format!("unmapped char '{other}'"),
            None,
        ))?,
    };
    // Punctuation keys not in keyboard-codes crate — use evdev codes directly
    let code: u32 = match raw {
        '`' => 41,  // KEY_GRAVE
        '-' => 12,  // KEY_MINUS
        '=' => 13,  // KEY_EQUAL
        '[' => 26,  // KEY_LEFTBRACE
        ']' => 27,  // KEY_RIGHTBRACE
        '\\' => 43, // KEY_BACKSLASH
        ';' => 39,  // KEY_SEMICOLON
        '\'' => 40, // KEY_APOSTROPHE
        ',' => 51,  // KEY_COMMA
        '.' => 52,  // KEY_DOT
        '/' => 53,  // KEY_SLASH
        ' ' => 57,  // KEY_SPACE
        '\t' => 15, // KEY_TAB
        '\n' => 28, // KEY_ENTER
        _ => {
            let key_str = String::from(raw);
            let input: keyboard_codes::KeyboardInput = key_str.parse().map_err(|e| {
                McpError::invalid_params(format!("keycode parse '{ch}': {e}"), None)
            })?;
            u32::try_from(input.to_code(Platform::Linux)).map_err(|e| {
                McpError::invalid_params(format!("keycode overflow '{ch}': {e}"), None)
            })?
        }
    };
    Ok((code, shifted))
}

/// Linux evdev codes for the modifiers a combo may name.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;

const COMBO_SYNTAX: &str = "use modifier+key, e.g. ctrl+shift+t, ctrl+minus, alt+F4; modifiers: ctrl, shift, alt, super; keys: a single character, or a name such as Return, Escape, Tab, Space, Backspace, Delete, Home, End, PageUp, PageDown, Up, Down, Left, Right, F1-F12, NumLock, minus, plus, equal, comma, period, slash, backslash, semicolon, apostrophe, grave, bracketleft, bracketright";

fn modifier_code(token: &str) -> Option<u32> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "ctl" => Some(KEY_LEFTCTRL),
        "shift" => Some(LINUX_KEY_LEFTSHIFT),
        "alt" | "option" => Some(KEY_LEFTALT),
        "super" | "meta" | "win" | "windows" | "cmd" | "command" | "logo" => Some(KEY_LEFTMETA),
        _ => None,
    }
}

/// Resolve one non-modifier key token to its evdev code and whether it needs
/// Shift, or None when the token names no key.
fn named_key(token: &str) -> Option<(u32, bool)> {
    let named = match token.to_ascii_lowercase().as_str() {
        "return" | "enter" => Some(28_u32),             // KEY_ENTER
        "backspace" => Some(14),                        // KEY_BACKSPACE
        "tab" => Some(15),                              // KEY_TAB
        "escape" | "esc" => Some(1),                    // KEY_ESC
        "space" => Some(57),                            // KEY_SPACE
        "delete" | "del" => Some(111),                  // KEY_DELETE
        "insert" | "ins" => Some(110),                  // KEY_INSERT
        "home" => Some(102),                            // KEY_HOME
        "end" => Some(107),                             // KEY_END
        "pageup" | "page_up" | "pgup" => Some(104),     // KEY_PAGEUP
        "pagedown" | "page_down" | "pgdn" => Some(109), // KEY_PAGEDOWN
        "up" => Some(103),                              // KEY_UP
        "down" => Some(108),                            // KEY_DOWN
        "left" => Some(105),                            // KEY_LEFT
        "right" => Some(106),                           // KEY_RIGHT
        "numlock" | "num_lock" => Some(69),             // KEY_NUMLOCK
        "capslock" | "caps_lock" => Some(58),           // KEY_CAPSLOCK
        "menu" => Some(127),                            // KEY_COMPOSE
        "print" | "printscreen" => Some(99),            // KEY_SYSRQ
        "minus" | "hyphen" | "dash" => Some(12),        // KEY_MINUS
        "equal" | "equals" => Some(13),                 // KEY_EQUAL
        "comma" => Some(51),                            // KEY_COMMA
        "period" | "dot" => Some(52),                   // KEY_DOT
        "slash" => Some(53),                            // KEY_SLASH
        "backslash" => Some(43),                        // KEY_BACKSLASH
        "semicolon" => Some(39),                        // KEY_SEMICOLON
        "apostrophe" | "quote" => Some(40),             // KEY_APOSTROPHE
        "grave" | "backtick" => Some(41),               // KEY_GRAVE
        "bracketleft" | "leftbracket" => Some(26),      // KEY_LEFTBRACE
        "bracketright" | "rightbracket" => Some(27),    // KEY_RIGHTBRACE
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(87),
        "f12" => Some(88),
        _ => None,
    };
    if let Some(code) = named {
        return Some((code, false));
    }
    if token.eq_ignore_ascii_case("plus") {
        return Some((13, true)); // Shift+KEY_EQUAL
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        // A bare letter in a combo names the key, not an uppercase character.
        (Some(ch), None) => char_key(ch.to_ascii_lowercase()).ok(),
        _ => None,
    }
}

/// Parse a key or combo into modifier codes plus one main key. Every token
/// must resolve; an unparseable combo fails before any input is sent.
fn parse_combo(key: &str) -> Result<(Vec<u32>, u32), McpError> {
    let invalid = |detail: String| {
        McpError::invalid_params(
            format!("{detail} in key combo '{key}'; {COMBO_SYNTAX}"),
            None,
        )
    };
    if key.is_empty() {
        return Err(invalid("empty key".to_owned()));
    }
    // A single character (including '+') is always that key.
    let mut chars = key.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        let (code, shifted) =
            char_key(ch).map_err(|_| invalid(format!("unsupported key '{ch}'")))?;
        return Ok((
            if shifted {
                vec![LINUX_KEY_LEFTSHIFT]
            } else {
                Vec::new()
            },
            code,
        ));
    }
    // "ctrl++" names the '+' key: a trailing empty token after '+'.
    let mut tokens: Vec<&str> = key.split('+').collect();
    if key.ends_with("++") {
        tokens.truncate(tokens.len().saturating_sub(2));
        tokens.push("+");
    }
    let Some((last, prefix)) = tokens.split_last() else {
        return Err(invalid("empty key".to_owned()));
    };
    let mut mods = Vec::new();
    for token in prefix {
        let token = token.trim();
        let code =
            modifier_code(token).ok_or_else(|| invalid(format!("'{token}' is not a modifier")))?;
        if !mods.contains(&code) {
            mods.push(code);
        }
    }
    let last = last.trim();
    if last.is_empty() {
        return Err(invalid("missing key after '+'".to_owned()));
    }
    if let Some(code) = modifier_code(last) {
        // A lone modifier, or a chord of modifiers, presses the last one.
        return Ok((mods, code));
    }
    let (code, shifted) =
        named_key(last).ok_or_else(|| invalid(format!("unknown key '{last}'")))?;
    if shifted && !mods.contains(&LINUX_KEY_LEFTSHIFT) {
        mods.push(LINUX_KEY_LEFTSHIFT);
    }
    Ok((mods, code))
}

fn btn_code(btn: Option<&str>) -> Result<u32, McpError> {
    match btn {
        Some("left") | None => Ok(0x110),
        Some("right") => Ok(0x111),
        Some("middle") => Ok(0x112),
        Some(bad) => Err(McpError::invalid_params(
            format!("unknown button '{bad}' — use left/right/middle"),
            None,
        )),
    }
}

// ── KWin D-Bus proxies ──────────────────────────────────────────────────

#[zbus::proxy(
    interface = "org.kde.KWin.EIS.RemoteDesktop",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/EIS/RemoteDesktop"
)]
trait KWinEis {
    #[zbus(name = "connectToEIS")]
    fn connect_to_eis(&self, capabilities: i32) -> zbus::Result<(zbus::zvariant::OwnedFd, i32)>;
}

#[zbus::proxy(
    interface = "org.kde.KWin.ScreenShot2",
    default_service = "org.kde.KWin",
    default_path = "/org/kde/KWin/ScreenShot2"
)]
trait KWinScreenShot2 {
    #[zbus(name = "CaptureWindow")]
    fn capture_window(
        &self,
        handle: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        pipe_fd: zbus::zvariant::OwnedFd,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;

    #[zbus(name = "CaptureScreen")]
    fn capture_screen(
        &self,
        name: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        pipe_fd: zbus::zvariant::OwnedFd,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

// Timing profile: MacKenzie and Buxton (CHI 1992), plus a fitted sample of
// Dhakal et al. (CHI 2018). See data/README.md for data and modeling choices.
const POINTER_REPORT_INTERVAL: Duration = Duration::from_millis(8);
const DEFAULT_TARGET_WIDTH: f64 = 20.0;
const FITTS_INTERCEPT_MS: f64 = 230.0;
const FITTS_SLOPE_MS: f64 = 166.0;
const FITTS_RESIDUAL_MS: f64 = 64.0;
// A key still down after the compositor's repeat delay auto-repeats. KDE defaults
// to 600 ms, and this host is configured for 300 ms. The keyboard fit's lognormal
// tail is unbounded, so typed and tapped keys clip holds to 250 ms.
const TYPED_KEY_HOLD_MAX: Duration = Duration::from_millis(250);
// Typing paces every key from the profile, which takes about 110 ms per character.
// Longer inputs compress their intervals to fit this budget instead of outlasting
// the caller, and never go below the profile's 10 ms interval floor.
const TYPING_BUDGET: Duration = Duration::from_secs(45);
const TYPING_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(12);
const KEYBOARD_PASTE_SHORTCUT: &str = "shift+Insert";
const TYPED_KEY_INTERVAL_FLOOR: Duration = Duration::from_millis(10);

#[derive(Deserialize)]
struct HumanInputProfile {
    bigrams: std::collections::BTreeMap<String, [f64; 6]>,
}
fn human_input_profile() -> anyhow::Result<&'static HumanInputProfile> {
    static PROFILE: std::sync::OnceLock<Result<HumanInputProfile, String>> =
        std::sync::OnceLock::new();
    PROFILE
        .get_or_init(|| {
            serde_json::from_str(include_str!("../data/input-profile.json"))
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("invalid embedded input profile: {error}"))
}
fn normal_sample() -> f64 {
    let radius = (-2.0 * rand::random::<f64>().max(f64::MIN_POSITIVE).ln()).sqrt();
    radius * (std::f64::consts::TAU * rand::random::<f64>()).cos()
}
#[derive(Clone, Copy)]
struct InputPace {
    speed: f64,
    target_width: f64,
}
impl InputPace {
    fn new(speed: Option<f64>, target_width: Option<f64>) -> Result<Self, McpError> {
        let speed = speed.unwrap_or(1.0);
        let target_width = target_width.unwrap_or(DEFAULT_TARGET_WIDTH);
        if !speed.is_finite() || speed <= 0.0 || !target_width.is_finite() || target_width <= 0.0 {
            return Err(McpError::invalid_params(
                "speed and target_width must be finite and positive",
                None,
            ));
        }
        Ok(Self {
            speed,
            target_width,
        })
    }
    fn delay(self, ms: f64) -> anyhow::Result<Duration> {
        Ok(Duration::try_from_secs_f64(ms / (1000.0 * self.speed))?)
    }
    fn key_timing(self, current: char, next: Option<char>) -> anyhow::Result<(Duration, Duration)> {
        let profile = human_input_profile()?;
        let pair: String = [
            Some(current.to_ascii_lowercase()),
            next.map(|c| c.to_ascii_lowercase()),
        ]
        .into_iter()
        .flatten()
        .collect();
        let stats = profile
            .bigrams
            .get(&pair)
            .or_else(|| profile.bigrams.get("*"))
            .ok_or_else(|| anyhow::anyhow!("input profile has no fallback"))?;
        let hold_z = normal_sample();
        let iki_z = stats[5] * hold_z + (1.0 - stats[5].powi(2)).max(0.0).sqrt() * normal_sample();
        Ok((
            self.delay((stats[1] + stats[2] * hold_z).exp())?,
            self.delay((stats[3] + stats[4] * iki_z).exp())?,
        ))
    }
    fn hold(self) -> anyhow::Result<Duration> {
        Ok(self.key_timing('\0', None)?.0)
    }
    fn key_hold(self) -> anyhow::Result<Duration> {
        Ok(capped_key_hold(self.hold()?))
    }
    async fn move_pointer(self, eis: &Eis, start: [f64; 2], end: [f64; 2]) -> anyhow::Result<()> {
        let distance = (end[0] - start[0]).hypot(end[1] - start[1]);
        if distance < f64::EPSILON {
            return Ok(());
        }
        let mean =
            FITTS_INTERCEPT_MS + FITTS_SLOPE_MS * (distance / self.target_width + 1.0).log2();
        let duration = self.delay(
            (mean + FITTS_RESIDUAL_MS * normal_sample())
                .max(POINTER_REPORT_INTERVAL.as_secs_f64() * 1000.0),
        )?;
        let bend = (normal_sample() * distance * 0.03).clamp(-30.0, 30.0);
        let normal = [
            -(end[1] - start[1]) / distance,
            (end[0] - start[0]) / distance,
        ];
        if distance > 200.0 {
            let correction = (self.target_width * 0.2).min(6.0) * (0.5 + rand::random::<f64>());
            let near = [
                end[0] - (end[0] - start[0]) / distance * correction,
                end[1] - (end[1] - start[1]) / distance * correction,
            ];
            self.move_segment(eis, start, near, normal, bend, duration.mul_f64(0.85))
                .await?;
            self.move_segment(eis, near, end, normal, 0.0, duration.mul_f64(0.15))
                .await
        } else {
            self.move_segment(eis, start, end, normal, bend, duration)
                .await
        }
    }
    async fn move_segment(
        self,
        eis: &Eis,
        start: [f64; 2],
        end: [f64; 2],
        normal: [f64; 2],
        bend: f64,
        duration: Duration,
    ) -> anyhow::Result<()> {
        let began = tokio::time::Instant::now();
        loop {
            let elapsed = began.elapsed();
            if elapsed >= duration {
                break;
            }
            tokio::time::sleep(POINTER_REPORT_INTERVAL.min(duration - elapsed)).await;
            let t = (began.elapsed().as_secs_f64() / duration.as_secs_f64()).min(1.0);
            // Minimum jerk from zero velocity/acceleration at both endpoints.
            let s = t.powi(3) * (10.0 - 15.0 * t + 6.0 * t * t);
            let curve = bend * 16.0 * s * s * (1.0 - s).powi(2);
            eis.move_abs(
                input_coordinate(start[0] + (end[0] - start[0]) * s + normal[0] * curve),
                input_coordinate(start[1] + (end[1] - start[1]) * s + normal[1] * curve),
            )?;
            if t >= 1.0 {
                return Ok(());
            }
        }
        eis.move_abs(input_coordinate(end[0]), input_coordinate(end[1]))
    }
}
fn capped_key_hold(hold: Duration) -> Duration {
    hold.min(TYPED_KEY_HOLD_MAX)
}
// Shrinks a sampled press-to-press interval so the remaining characters fit the
// remaining budget. Short inputs keep their sampled timing.
fn budgeted_interval(
    sampled: Duration,
    remaining_budget: Duration,
    remaining_characters: usize,
) -> Duration {
    let count = u32::try_from(remaining_characters.max(1)).unwrap_or(u32::MAX);
    sampled.min((remaining_budget / count).max(TYPED_KEY_INTERVAL_FLOOR))
}
#[expect(clippy::as_conversions, reason = "EIS accepts f32 screen coordinates")]
fn input_coordinate(value: f64) -> f32 {
    value as f32
}

// Release only input pressed by this operation on error or cancellation.
struct SentInput<'a> {
    eis: &'a Eis,
    keys: std::collections::BTreeMap<u32, Option<char>>,
    button: Option<u32>,
}
impl<'a> SentInput<'a> {
    fn new(eis: &'a Eis) -> Self {
        Self {
            eis,
            keys: Default::default(),
            button: None,
        }
    }
    fn key(&mut self, code: u32, pressed: bool, character: Option<char>) -> anyhow::Result<()> {
        if pressed {
            self.keys.insert(code, character);
        }
        if character.is_some() {
            self.eis.key_with_character(code, pressed, character)?;
        } else {
            self.eis.key(code, pressed)?;
        }
        if !pressed {
            self.keys.remove(&code);
        }
        Ok(())
    }
    fn tap(&mut self, code: u32, character: char) -> anyhow::Result<()> {
        self.keys.insert(code, Some(character));
        self.eis.tap_with_character(code, Some(character))?;
        self.keys.remove(&code);
        Ok(())
    }
    fn button(&mut self, code: u32, pressed: bool) -> anyhow::Result<()> {
        if pressed {
            self.button = Some(code);
        }
        self.eis.button(code, pressed)?;
        if !pressed {
            self.button = None;
        }
        Ok(())
    }
}
impl Drop for SentInput<'_> {
    fn drop(&mut self) {
        for (code, character) in &self.keys {
            let _ = self.eis.key_with_character(*code, false, *character);
        }
        if let Some(code) = self.button {
            let _ = self.eis.button(code, false);
        }
    }
}

fn typing_log_failure(
    result: Result<CallToolResult, McpError>,
    log_error: std::io::Error,
) -> McpError {
    match result {
        Err(mut error) => {
            error.message = format!(
                "{}. Input log summary also failed: {log_error}",
                error.message
            )
            .into();
            let data = error.data.get_or_insert_with(|| serde_json::json!({}));
            if let Some(data) = data.as_object_mut() {
                data.insert(
                    "input_log_summary_error".to_owned(),
                    serde_json::json!(log_error.to_string()),
                );
            }
            error
        }
        Ok(result) => {
            let input = result
                .structured_content
                .unwrap_or_else(|| serde_json::json!({}));
            McpError::internal_error(
                format!(
                    "Text input submission completed, but its input log summary failed: {log_error}. Inspect the field and retained input_result before sending or retrying; do not retype blindly."
                ),
                Some(serde_json::json!({
                    "submitted_characters": input["submitted_characters"],
                    "clipboard_characters": input["clipboard_characters"],
                    "shortcut_submitted": input["shortcut_submitted"],
                    "submission_complete": true, "retry_safe": false,
                    "compositor_acknowledged": input["delivery"]["compositor_acknowledged"],
                    "input_result": input, "input_log_summary_error": log_error.to_string(),
                })),
            )
        }
    }
}

struct TypingWindow<'a> {
    connection: &'a zbus::Connection,
    kwin_unique: &'a str,
    runtime: &'a Path,
    window_id: &'a str,
    browser_title: Option<&'a str>,
}

fn chromium_resource_class(resource_class: &str) -> bool {
    let class = resource_class.trim().to_ascii_lowercase();
    if matches!(
        class.as_str(),
        "org.chromium.chromium"
            | "com.google.chrome"
            | "com.brave.browser"
            | "com.microsoft.edge"
            | "com.vivaldi.vivaldi"
    ) {
        return true;
    }
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "brave-browser",
        "microsoft-edge",
        "vivaldi",
    ]
    .iter()
    .any(|base| {
        class == *base
            || class.strip_prefix(base).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "-stable" | "-beta" | "-dev" | "-unstable" | "-nightly" | "-snapshot"
                )
            })
    })
}

async fn check_typing_window(
    window: &TypingWindow<'_>,
) -> Result<(), input_bridge::TextFocusError> {
    match tokio::time::timeout(
        Duration::from_millis(400),
        active_window_info(window.connection, window.kwin_unique, window.runtime),
    )
    .await
    {
        Ok(Ok((_, _, observed))) if observed.id != window.window_id => {
            Err(input_bridge::TextFocusError::WindowChanged)
        }
        Ok(Ok((_, _, observed)))
            if window
                .browser_title
                .is_some_and(|title| title != observed.title) =>
        {
            Err(input_bridge::TextFocusError::BrowserContextChanged)
        }
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(input_bridge::TextFocusError::QueryFailed(error.to_string())),
        Err(_) => Err(input_bridge::TextFocusError::QueryTimedOut),
    }
}

/// `check_typing_window`, repeated while KWin does not answer; a changed
/// window is still reported at once.
async fn recheck_typing_window(
    window: &TypingWindow<'_>,
) -> Result<(), input_bridge::TextFocusError> {
    input_bridge::keep_checking(|| check_typing_window(window)).await
}

async fn watch_typing_window(window: &TypingWindow<'_>) -> input_bridge::TextFocusError {
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        if let Err(error) = recheck_typing_window(window).await {
            return error;
        }
    }
}

async fn watch_typing_target(
    target: Option<&input_bridge::TextReceipt>,
    window: &TypingWindow<'_>,
) -> input_bridge::TextFocusError {
    let field = async {
        match target {
            Some(target) => target.watch_focus().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        error = field => error,
        error = watch_typing_window(window) => error,
    }
}

async fn type_with_profile(
    eis: &Eis,
    text: &str,
    keys: &[(u32, bool)],
    pace: InputPace,
    target: Option<&input_bridge::TextReceipt>,
    window: &TypingWindow<'_>,
) -> Result<(), McpError> {
    let characters: Vec<char> = text.chars().collect();
    let began = tokio::time::Instant::now();
    let mut next_down = Duration::ZERO;
    let mut shifted = false;
    let mut submitted = 0;
    let mut attempted = 0;
    let result: anyhow::Result<()> = {
        let typing = async {
            let mut held = SentInput::new(eis);
            for (i, (&character, &(code, shift))) in characters.iter().zip(keys).enumerate() {
                let (_, interval) = pace.key_timing(character, characters.get(i + 1).copied())?;
                let interval = budgeted_interval(
                    interval,
                    TYPING_BUDGET.saturating_sub(began.elapsed()),
                    characters.len() - i,
                );
                // Wait only between complete keystrokes. Scheduler delays must
                // not extend a printable key hold into application autorepeat.
                tokio::time::sleep_until(began + next_down).await;
                if shift != shifted {
                    held.key(LINUX_KEY_LEFTSHIFT, shift, None)?;
                    shifted = shift;
                }
                attempted += 1;
                held.tap(code, character)?;
                submitted += 1;
                // Socket backpressure or scheduler delay must not turn the rest of
                // the text into a burst of overdue events. Pace from the actual send.
                let sent_at = began.elapsed();
                next_down = sent_at + interval;
                tokio::task::yield_now().await;
            }
            if shifted {
                held.key(LINUX_KEY_LEFTSHIFT, false, None)?;
            }
            recheck_typing_window(window).await?;
            if let Some(target) = target {
                target.recheck_focus().await?;
            }
            Ok(())
        };
        tokio::select! {
            biased;
            error = watch_typing_target(target, window) => Err(error.into()),
            result = typing => result,
        }
    };
    result.map_err(|error| McpError::internal_error(
        format!("keyboard_type stopped after completing {submitted} character presses and attempting {attempted} of {} characters: {error}. Some or all attempted text may still arrive. Inspect the field and input log before sending or retrying; do not retype blindly.", characters.len()),
        Some(serde_json::json!({
            "reason": error.downcast_ref::<input_bridge::TextFocusError>().map(input_bridge::TextFocusError::reason).unwrap_or("input_submission_failed"),
            "submitted_characters": submitted, "attempted_characters": attempted,
            "requested_characters": characters.len(), "submission_complete": submitted == characters.len(),
            "compositor_acknowledged": false, "application_text": "unverified", "retry_safe": false,
        })),
    ))
}

// ── EIS input ───────────────────────────────────────────────────────────

#[derive(Clone, Default, Serialize, Deserialize)]
struct InputHistory {
    held: std::collections::BTreeMap<u32, HeldInputKey>,
    last_release_ns: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct HeldInputKey {
    down_ns: u64,
    character: Option<char>,
    gap_before_ns: Option<u64>,
}
struct InputCallLog {
    tool: String,
    request_id: serde_json::Value,
    start_ns: u64,
    origin: [f64; 2],
    window_id: Option<String>,
    pointer_start: Option<[f64; 2]>,
    pointer_start_ns: Option<u64>,
    typing_span: Option<(u64, u64)>,
    scroll_span: Option<(u64, u64)>,
    scroll_mode: Option<String>,
    scroll_unit: Option<String>,
    pointer_last: Option<(u64, [f64; 2])>,
    path_length: f64,
    peak_speed: f64,
    motion_events: u64,
    characters: String,
    keys: Vec<serde_json::Value>,
    scroll_delta: [f64; 2],
    scroll_events: u32,
}
struct InputLogState {
    file: std::fs::File,
    history: InputHistory,
    call: Option<InputCallLog>,
}
struct InputLog {
    path: PathBuf,
    state: std::sync::Mutex<InputLogState>,
}

fn input_clock() -> std::io::Result<(u64, u64)> {
    let mut mono = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { nix::libc::clock_gettime(nix::libc::CLOCK_MONOTONIC, &mut mono) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ns = u64::try_from(mono.tv_sec).map_err(std::io::Error::other)? * 1_000_000_000
        + u64::try_from(mono.tv_nsec).map_err(std::io::Error::other)?;
    let wall = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos(),
    )
    .map_err(std::io::Error::other)?;
    Ok((ns, wall))
}
fn input_log_path(session_id: &str) -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("kwin-mcp/input")
        .join(format!("{session_id}.jsonl"))
}
impl InputLog {
    fn open(
        session_id: &str,
        history: InputHistory,
        saved_path: Option<PathBuf>,
    ) -> std::io::Result<Arc<Self>> {
        use std::os::unix::fs::OpenOptionsExt;
        let resume = saved_path.is_some();
        let mut path = saved_path.unwrap_or_else(|| input_log_path(session_id));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .append(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW);
        options.create_new(!resume);
        let file = match options.open(&path) {
            Err(error) if !resume && error.kind() == std::io::ErrorKind::AlreadyExists => {
                let (_, wall) = input_clock()?;
                path.set_file_name(format!("{session_id}-{wall}.jsonl"));
                options.open(&path)?
            }
            result => result?,
        };
        Ok(Arc::new(Self {
            path,
            state: std::sync::Mutex::new(InputLogState {
                file,
                history,
                call: None,
            }),
        }))
    }
    fn write(
        state: &mut InputLogState,
        mut record: serde_json::Value,
        mono: u64,
        wall: u64,
    ) -> std::io::Result<()> {
        record["monotonic_ns"] = serde_json::json!(mono);
        record["wall_time_unix_ns"] = serde_json::json!(wall);
        if let Some(call) = &state.call {
            record["tool"] = serde_json::json!({"name":call.tool,"request_id":call.request_id});
        }
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        state.file.write_all(&line)
    }
    fn begin(
        self: &Arc<Self>,
        tool: String,
        request_id: serde_json::Value,
    ) -> std::io::Result<InputLogGuard> {
        let (mono, wall) = input_clock()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("input log lock poisoned"))?;
        state.call = Some(InputCallLog {
            tool,
            request_id,
            start_ns: mono,
            origin: [0.0, 0.0],
            window_id: None,
            pointer_start: None,
            pointer_start_ns: None,
            typing_span: None,
            scroll_span: None,
            scroll_mode: None,
            scroll_unit: None,
            pointer_last: None,
            path_length: 0.0,
            peak_speed: 0.0,
            motion_events: 0,
            characters: String::new(),
            keys: Vec::new(),
            scroll_delta: [0.0, 0.0],
            scroll_events: 0,
        });
        Self::write(
            &mut state,
            serde_json::json!({"type":"call_start"}),
            mono,
            wall,
        )?;
        Ok(InputLogGuard {
            log: self.clone(),
            finished: false,
        })
    }
    fn pointer_origin(
        &self,
        origin: [f64; 2],
        position: [f64; 2],
        window: &str,
    ) -> std::io::Result<()> {
        let (mono, _) = input_clock()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("input log lock poisoned"))?;
        if let Some(call) = &mut state.call {
            call.origin = origin;
            call.window_id = Some(window.to_owned());
            call.pointer_start = Some(position);
            call.pointer_start_ns = Some(mono);
            call.pointer_last = Some((mono, position));
        }
        Ok(())
    }
    fn event(
        &self,
        mut event: serde_json::Value,
        delivery: &anyhow::Result<()>,
    ) -> std::io::Result<()> {
        let (mono, wall) = input_clock()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("input log lock poisoned"))?;
        event["delivery"] = serde_json::json!(if delivery.is_ok() {
            "flushed"
        } else {
            "uncertain"
        });
        if let Err(error) = delivery {
            event["error"] = serde_json::json!(error.to_string());
        }
        let mut key_timing = None;
        if event["event"] == "key" {
            let code = event["keycode"]
                .as_u64()
                .and_then(|code| u32::try_from(code).ok())
                .ok_or_else(|| std::io::Error::other("input keycode missing"))?;
            let character = event["character"]
                .as_str()
                .and_then(|text| text.chars().next());
            if event["pressed"] == true {
                let gap = state
                    .history
                    .last_release_ns
                    .map(|release| mono.saturating_sub(release));
                event["since_last_release_ns"] = serde_json::json!(gap);
                state.history.held.entry(code).or_insert(HeldInputKey {
                    down_ns: mono,
                    character,
                    gap_before_ns: gap,
                });
            } else {
                if let Some(held) = state.history.held.remove(&code) {
                    event["character"] = serde_json::json!(held.character);
                    event["hold_ns"] = serde_json::json!(mono.saturating_sub(held.down_ns));
                    key_timing = Some(
                        serde_json::json!({"keycode":code,"character":held.character,"down_ns":held.down_ns,"up_ns":mono,"hold_ns":mono.saturating_sub(held.down_ns),"gap_before_ns":held.gap_before_ns}),
                    );
                }
                state.history.last_release_ns = Some(mono);
            }
        }
        if let Some(call) = &mut state.call {
            if event["event"] == "motion" {
                let position = [
                    event["screen_x"].as_f64().unwrap_or_default(),
                    event["screen_y"].as_f64().unwrap_or_default(),
                ];
                event["window_x"] = serde_json::json!(position[0] - call.origin[0]);
                event["window_y"] = serde_json::json!(position[1] - call.origin[1]);
                event["window_id"] = serde_json::json!(call.window_id);
                event["window_origin"] = serde_json::json!(call.origin);
                if let Some((previous_ns, previous)) = call.pointer_last {
                    let distance = (position[0] - previous[0]).hypot(position[1] - previous[1]);
                    call.path_length += distance;
                    let seconds =
                        Duration::from_nanos(mono.saturating_sub(previous_ns)).as_secs_f64();
                    if seconds > 0.0 {
                        call.peak_speed = call.peak_speed.max(distance / seconds);
                    }
                }
                call.pointer_start.get_or_insert(position);
                call.pointer_start_ns.get_or_insert(mono);
                call.pointer_last = Some((mono, position));
                call.motion_events += 1;
            }
            if event["event"] == "key" {
                call.typing_span.get_or_insert((mono, mono)).1 = mono;
            }
            if event["event"] == "key"
                && event["pressed"] == true
                && let Some(character) = event["character"].as_str()
            {
                call.characters.push_str(character);
            }
            if let Some(timing) = key_timing {
                call.keys.push(timing);
            }
            if event["event"] == "scroll" || event["event"] == "scroll_stop" {
                call.scroll_span.get_or_insert((mono, mono)).1 = mono;
            }
            if event["event"] == "scroll" {
                call.scroll_mode = event["mode"].as_str().map(str::to_owned);
                call.scroll_unit = event["unit"].as_str().map(str::to_owned);
                call.scroll_delta[0] += event["dx"].as_f64().unwrap_or_default();
                call.scroll_delta[1] += event["dy"].as_f64().unwrap_or_default();
                call.scroll_events += 1;
            }
        }
        event["type"] = serde_json::json!("event");
        Self::write(&mut state, event, mono, wall)
    }
    fn finish(&self, outcome: &str) -> std::io::Result<()> {
        let (mono, wall) = input_clock()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("input log lock poisoned"))?;
        let Some(call) = &state.call else {
            return Ok(());
        };
        let duration = mono.saturating_sub(call.start_ns);
        let pointer_duration = call
            .pointer_start_ns
            .zip(call.pointer_last)
            .map(|(start, (end, _))| end.saturating_sub(start))
            .unwrap_or_default();
        let typing_duration = call
            .typing_span
            .map(|(start, end)| end.saturating_sub(start))
            .unwrap_or_default();
        let scroll_duration = call
            .scroll_span
            .map(|(start, end)| end.saturating_sub(start))
            .unwrap_or_default();
        let seconds = Duration::from_nanos(scroll_duration).as_secs_f64();
        let mut keys = call.keys.clone();
        for (code, held) in &state.history.held {
            if held.down_ns >= call.start_ns {
                keys.push(serde_json::json!({"keycode":code,"character":held.character,"down_ns":held.down_ns,"up_ns":null,"hold_ns":null,"gap_before_ns":held.gap_before_ns}));
            }
        }
        keys.sort_by_key(|key| key["down_ns"].as_u64());
        for index in 1..keys.len() {
            if let (Some(down), Some(previous_up)) = (
                keys[index]["down_ns"].as_u64(),
                keys[index - 1]["up_ns"].as_u64(),
            ) {
                keys[index]["gap_before_ns"] =
                    serde_json::json!(i128::from(down) - i128::from(previous_up));
            }
        }
        let summary = serde_json::json!({"type":"call_summary","outcome":outcome,"duration_ns":duration,
            "pointer":{"start":call.pointer_start,"end":call.pointer_last.map(|(_,p)|p),"start_ns":call.pointer_start_ns,"end_ns":call.pointer_last.map(|(ns,_)|ns),"path_length_px":call.path_length,"duration_ns":pointer_duration,"peak_speed_px_s":call.peak_speed,"events":call.motion_events},
            "typing":{"characters":call.characters,"character_count":call.characters.chars().count(),"duration_ns":typing_duration,"keys":keys},
            "scroll":{"total_dx":call.scroll_delta[0],"total_dy":call.scroll_delta[1],"mode":call.scroll_mode,"unit":call.scroll_unit,"duration_ns":scroll_duration,"events":call.scroll_events,"events_per_second":if seconds>0.0 {Some(f64::from(call.scroll_events)/seconds)} else {None}}});
        Self::write(&mut state, summary, mono, wall)?;
        state.file.sync_data()?;
        state.call = None;
        Ok(())
    }
    fn history(&self) -> InputHistory {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .history
            .clone()
    }
}
struct InputLogGuard {
    log: Arc<InputLog>,
    finished: bool,
}
impl InputLogGuard {
    fn finish(mut self, outcome: &str) -> std::io::Result<()> {
        let result = self.log.finish(outcome);
        self.finished = true;
        result
    }
}
impl Drop for InputLogGuard {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.log.finish("cancelled")
        {
            eprintln!("input log summary failed: {error}");
        }
    }
}

struct Eis {
    audit: Option<Arc<InputLog>>,
    context: reis::ei::Context,
    connection: reis::ei::Connection,
    abs_ptr: reis::ei::PointerAbsolute,
    btn: reis::ei::Button,
    scroll: reis::ei::Scroll,
    kbd: reis::ei::Keyboard,
    ptr_dev: reis::ei::Device,
    kbd_dev: reis::ei::Device,
    serial: std::sync::atomic::AtomicU32,
    start: std::time::Instant,
}

impl Eis {
    fn next_serial(&self) -> u32 {
        self.serial
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1)
    }
    fn now_us(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_micros()).unwrap_or(0)
    }

    /// Modifiers a `keyboard_press` left down: text typed now would arrive as shortcuts.
    fn held_modifiers(&self) -> Vec<&'static str> {
        let Some(log) = &self.audit else {
            return Vec::new();
        };
        let history = log.history();
        [
            (KEY_LEFTCTRL, "ctrl"),
            (LINUX_KEY_LEFTSHIFT, "shift"),
            (KEY_LEFTALT, "alt"),
            (KEY_LEFTMETA, "super"),
        ]
        .into_iter()
        .filter(|(code, _)| history.held.contains_key(code))
        .map(|(_, name)| name)
        .collect()
    }

    fn from_fd(fd: std::os::fd::OwnedFd) -> anyhow::Result<Self> {
        let stream = std::os::unix::net::UnixStream::from(fd);
        let context = reis::ei::Context::new(stream)?;
        let resp = reis::handshake::ei_handshake_blocking(
            &context,
            "kwin-mcp",
            reis::ei::handshake::ContextType::Sender,
        )
        .map_err(|e| anyhow::anyhow!("EIS handshake: {e:?}"))?;
        context.flush()?;
        let connection = resp.connection.clone();
        let mut conv = reis::event::EiEventConverter::new(&context, resp);
        let serial = conv.connection().serial();
        let (mut dev, mut kbd_d) = (None, None);
        let (mut abs, mut bt, mut sc, mut kb) = (None, None, None, None);
        let deadline = std::time::Instant::now() + EIS_NEGOTIATION_TIMEOUT;
        loop {
            if dev.is_some() && kb.is_some() {
                break;
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!("EIS negotiation timed out");
            }
            context.read()?;
            while let Some(pending) = context.pending_event() {
                match pending {
                    reis::PendingRequestResult::Request(ev) => {
                        conv.handle_event(ev)
                            .map_err(|e| anyhow::anyhow!("EIS event: {e:?}"))?;
                    }
                    reis::PendingRequestResult::ParseError(e) => {
                        anyhow::bail!("EIS parse: {e}")
                    }
                    reis::PendingRequestResult::InvalidObject(i) => {
                        anyhow::bail!("EIS invalid object: {i}")
                    }
                }
            }
            while let Some(ev) = conv.next_event() {
                match ev {
                    reis::event::EiEvent::SeatAdded(sa) => {
                        sa.seat.bind_capabilities(
                            reis::event::DeviceCapability::Pointer
                                | reis::event::DeviceCapability::PointerAbsolute
                                | reis::event::DeviceCapability::Button
                                | reis::event::DeviceCapability::Scroll
                                | reis::event::DeviceCapability::Keyboard,
                        );
                        context.flush()?;
                    }
                    reis::event::EiEvent::DeviceAdded(da) => {
                        let d = &da.device;
                        match d.has_capability(reis::event::DeviceCapability::PointerAbsolute) {
                            true => {
                                d.device().start_emulating(serial, 0);
                                abs = d.interface::<reis::ei::PointerAbsolute>();
                                bt = d.interface::<reis::ei::Button>();
                                sc = d.interface::<reis::ei::Scroll>();
                                dev = Some(d.device().clone());
                                if let (Some(k), None) = (d.interface::<reis::ei::Keyboard>(), &kb)
                                {
                                    kb = Some(k);
                                    kbd_d = Some(d.device().clone());
                                }
                            }
                            false => {
                                if d.has_capability(reis::event::DeviceCapability::Keyboard)
                                    && kb.is_none()
                                {
                                    d.device().start_emulating(serial, 0);
                                    kb = d.interface::<reis::ei::Keyboard>();
                                    kbd_d = Some(d.device().clone());
                                }
                            }
                        }
                        context.flush()?;
                    }
                    reis::event::EiEvent::Disconnected(_) => anyhow::bail!("EIS disconnected"),
                    reis::event::EiEvent::SeatRemoved(_)
                    | reis::event::EiEvent::DeviceRemoved(_)
                    | reis::event::EiEvent::DevicePaused(_)
                    | reis::event::EiEvent::DeviceResumed(_)
                    | reis::event::EiEvent::KeyboardModifiers(_)
                    | reis::event::EiEvent::Frame(_)
                    | reis::event::EiEvent::DeviceStartEmulating(_)
                    | reis::event::EiEvent::DeviceStopEmulating(_)
                    | reis::event::EiEvent::PointerMotion(_)
                    | reis::event::EiEvent::PointerMotionAbsolute(_)
                    | reis::event::EiEvent::Button(_)
                    | reis::event::EiEvent::ScrollDelta(_)
                    | reis::event::EiEvent::ScrollStop(_)
                    | reis::event::EiEvent::ScrollCancel(_)
                    | reis::event::EiEvent::ScrollDiscrete(_)
                    | reis::event::EiEvent::KeyboardKey(_)
                    | reis::event::EiEvent::TouchDown(_)
                    | reis::event::EiEvent::TouchUp(_)
                    | reis::event::EiEvent::TouchMotion(_)
                    | reis::event::EiEvent::TouchCancel(_) => {}
                }
            }
            std::thread::sleep(EIS_NEGOTIATION_POLL);
        }
        Ok(Self {
            audit: None,
            context,
            connection,
            abs_ptr: abs.ok_or_else(|| anyhow::anyhow!("no EIS pointer"))?,
            btn: bt.ok_or_else(|| anyhow::anyhow!("no EIS button"))?,
            scroll: sc.ok_or_else(|| anyhow::anyhow!("no EIS scroll"))?,
            kbd: kb.ok_or_else(|| anyhow::anyhow!("no EIS keyboard"))?,
            ptr_dev: dev.ok_or_else(|| anyhow::anyhow!("no EIS ptr device"))?,
            kbd_dev: kbd_d.ok_or_else(|| anyhow::anyhow!("no EIS kbd device"))?,
            serial: std::sync::atomic::AtomicU32::new(serial),
            start: std::time::Instant::now(),
        })
    }

    /// Send the queued EIS requests. The socket is non-blocking, so when input
    /// outruns KWin (a long keyboard_type) the kernel buffer fills and the send
    /// fails with EAGAIN while reis keeps the unsent bytes queued. Wait for the
    /// socket to drain and continue, so no input is cut off mid-stream.
    fn flush(&self) -> anyhow::Result<()> {
        use std::os::fd::AsFd;
        let deadline = std::time::Instant::now() + EIS_FLUSH_TIMEOUT;
        loop {
            match self.context.flush() {
                Ok(()) => return Ok(()),
                Err(error)
                    if std::io::Error::from(error).kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                anyhow::bail!(
                    "KWin stopped reading input: the EIS socket stayed full for {}s",
                    EIS_FLUSH_TIMEOUT.as_secs()
                );
            }
            let mut fds = [nix::poll::PollFd::new(
                self.context.as_fd(),
                nix::poll::PollFlags::POLLOUT,
            )];
            let wait = u16::try_from(left.as_millis()).unwrap_or(u16::MAX);
            let _ = nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(wait));
        }
    }

    fn flush_logged(&self, event: serde_json::Value) -> anyhow::Result<()> {
        let delivery = self.flush();
        if let Some(log) = &self.audit {
            log.event(event, &delivery).map_err(|error| {
                anyhow::anyhow!(
                    "input was submitted but log {} could not be appended: {error}",
                    log.path.display()
                )
            })?;
        }
        delivery
    }

    /// A successful socket write only queues input. Wait for KWin to process
    /// all earlier requests before checking what the application received.
    async fn roundtrip(&self) -> anyhow::Result<()> {
        let callback = self.connection.sync(EIS_CALLBACK_VERSION);
        self.flush()?;
        let deadline = tokio::time::Instant::now() + EIS_FLUSH_TIMEOUT;
        loop {
            self.context.read()?;
            while let Some(pending) = self.context.pending_event() {
                let event = match pending {
                    reis::PendingRequestResult::Request(event) => event,
                    reis::PendingRequestResult::ParseError(error) => {
                        anyhow::bail!("EIS parse while confirming input: {error}");
                    }
                    reis::PendingRequestResult::InvalidObject(id) => {
                        anyhow::bail!("EIS invalid object while confirming input: {id}");
                    }
                };
                if let reis::ei::Event::Callback(object, reis::ei::callback::Event::Done { .. }) =
                    &event
                    && *object == callback
                {
                    return Ok(());
                }
                if let reis::ei::Event::Connection(_, reis::ei::connection::Event::Ping { ping }) =
                    &event
                {
                    ping.done(0);
                    self.flush()?;
                }
                if let reis::ei::Event::Connection(
                    _,
                    reis::ei::connection::Event::Disconnected { explanation, .. },
                ) = &event
                {
                    anyhow::bail!(
                        "EIS disconnected while confirming input: {}",
                        explanation
                            .as_deref()
                            .unwrap_or("server provided no explanation")
                    );
                }
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "KWin did not acknowledge submitted input within {}s; text may still arrive. Inspect the field before retrying.",
                    EIS_FLUSH_TIMEOUT.as_secs()
                );
            }
            tokio::time::sleep(EIS_NEGOTIATION_POLL).await;
        }
    }
    fn pointer_origin(&self, wx: i32, wy: i32, geometry: &WindowGeometry) -> anyhow::Result<()> {
        if let Some(log) = &self.audit {
            log.pointer_origin(
                [f64::from(wx), f64::from(wy)],
                [geometry.cx, geometry.cy],
                &geometry.id,
            )?;
        }
        Ok(())
    }
    fn move_abs(&self, x: f32, y: f32) -> anyhow::Result<()> {
        self.abs_ptr.motion_absolute(x, y);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(serde_json::json!({"event":"motion","screen_x":x,"screen_y":y}))
    }

    fn button(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::button::ButtonState::Press,
            false => reis::ei::button::ButtonState::Released,
        };
        self.btn.button(code, st);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(serde_json::json!({"event":"button","button":code,"pressed":pressed}))
    }

    fn scroll_discrete(&self, dx: i32, dy: i32) -> anyhow::Result<()> {
        let notches = dx.unsigned_abs().max(dy.unsigned_abs());
        let (dx, dy) = (
            dx.signum() * SCROLL_VALUE120_PER_NOTCH,
            dy.signum() * SCROLL_VALUE120_PER_NOTCH,
        );
        for _ in 0..notches {
            self.scroll.scroll_discrete(dx, dy);
            self.ptr_dev.frame(self.next_serial(), self.now_us());
            self.flush_logged(serde_json::json!({"event":"scroll","mode":"discrete","unit":"value120","dx":dx,"dy":dy}))?;
        }
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(
            serde_json::json!({"event":"scroll_stop","mode":"discrete","dx":0,"dy":0}),
        )
    }
    fn scroll_smooth(&self, dx: f32, dy: f32) -> anyhow::Result<()> {
        self.scroll.scroll(dx, dy);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(
            serde_json::json!({"event":"scroll","mode":"smooth","unit":"pixels","dx":dx,"dy":dy}),
        )?;
        self.scroll.scroll_stop(0, 0, 0);
        self.ptr_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(serde_json::json!({"event":"scroll_stop","mode":"smooth","dx":0,"dy":0}))
    }
    fn key(&self, code: u32, pressed: bool) -> anyhow::Result<()> {
        let character = self.audit.as_ref().and_then(|log| {
            let history = log.history();
            if [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTMETA]
                .iter()
                .any(|key| history.held.contains_key(key))
            {
                return None;
            }
            let shifted = history.held.contains_key(&LINUX_KEY_LEFTSHIFT);
            (' '..='~').chain(['\t', '\n']).find(|character| {
                char_key(*character).is_ok_and(|(key, shift)| key == code && shift == shifted)
            })
        });
        self.key_with_character(code, pressed, character)
    }
    fn tap_with_character(&self, code: u32, character: Option<char>) -> anyhow::Result<()> {
        let timestamp = self.now_us();
        self.kbd.key(code, reis::ei::keyboard::KeyState::Press);
        self.kbd_dev.frame(self.next_serial(), timestamp);
        self.kbd.key(code, reis::ei::keyboard::KeyState::Released);
        self.kbd_dev.frame(self.next_serial(), timestamp);
        // Queue both transitions before flushing. A paced text key must not
        // remain pressed while the typing task waits for its next timer.
        let delivery = self.flush();
        if let Some(log) = &self.audit {
            for pressed in [true, false] {
                log.event(
                    serde_json::json!({
                        "event": "key", "keycode": code,
                        "pressed": pressed, "character": character
                    }),
                    &delivery,
                )
                .map_err(|error| {
                    anyhow::anyhow!(
                        "input was submitted but log {} could not be appended: {error}",
                        log.path.display()
                    )
                })?;
            }
        }
        delivery
    }

    fn key_with_character(
        &self,
        code: u32,
        pressed: bool,
        character: Option<char>,
    ) -> anyhow::Result<()> {
        let st = match pressed {
            true => reis::ei::keyboard::KeyState::Press,
            false => reis::ei::keyboard::KeyState::Released,
        };
        self.kbd.key(code, st);
        self.kbd_dev.frame(self.next_serial(), self.now_us());
        self.flush_logged(serde_json::json!({"event":"key","keycode":code,"pressed":pressed,"character":character}))
    }
}

/// Preserve the owned startup process's state before cleanup destroys the
/// evidence. Only bounded state and wait-channel fields are read, never its
/// command line, environment, or another session's process inventory.
fn startup_sandbox_process_diagnostic(pid: u32) -> String {
    use std::io::Read;
    let root = PathBuf::from(format!("/proc/{pid}"));
    let read = |name: &str| -> std::io::Result<String> {
        let mut text = String::new();
        std::fs::File::open(root.join(name))?
            .take(4096)
            .read_to_string(&mut text)?;
        Ok(text)
    };
    let state = read("status")
        .map(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("State:"))
                .map(str::trim)
                .unwrap_or("unavailable")
                .chars()
                .take(128)
                .collect::<String>()
        })
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    let wait_channel = read("wchan")
        .map(|text| text.trim().chars().take(128).collect::<String>())
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    format!("startup sandbox process pid={pid} state={state} wait_channel={wait_channel}")
}

/// Wait for `path` to appear inside the starting sandbox. `producer_exit` is
/// a file the producing process's wrapper writes with its exit status, so a
/// producer that died is reported as such rather than as a slow socket.
async fn wait_for_sandbox_path(
    path: &std::path::Path,
    description: &str,
    deadline: std::time::Instant,
    sandbox: &mut Option<std::process::Child>,
    producer_exit: Option<(&std::path::Path, &str)>,
) -> Result<(), String> {
    let child = sandbox
        .as_mut()
        .ok_or_else(|| format!("startup sandbox process is unavailable before {description}"))?;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("check startup sandbox before {description}: {error}"))?
        {
            return Err(format!(
                "startup sandbox exited with {status} before {description}"
            ));
        }
        if path.exists() {
            return Ok(());
        }
        if let Some((exit_file, producer)) = producer_exit
            && let Ok(status) = std::fs::read_to_string(exit_file)
        {
            return Err(format!(
                "{producer} exited with status {} before creating its {description} at {}",
                status.trim(),
                path.display()
            ));
        }
        if std::time::Instant::now() >= deadline {
            let producer_state = producer_exit
                .map(|(_, producer)| format!("{producer} is still running; "))
                .unwrap_or_default();
            return Err(format!(
                "{description} did not appear at {} within {}s; {producer_state}{}",
                path.display(),
                STARTUP_TIMEOUT.as_secs(),
                startup_sandbox_process_diagnostic(child.id())
            ));
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

fn wallet_bus_startup_error(message: String, stderr_path: &Path) -> String {
    use std::io::Read;
    const DIAGNOSTIC_LIMIT: u64 = 16 * 1024;

    // Read a regular file instead of a pipe: a noisy daemon must not block on
    // stderr while we wait for its socket, and a live child must not hold up EOF.
    let diagnostics = (|| -> std::io::Result<String> {
        let mut bytes = Vec::new();
        std::fs::File::open(stderr_path)?
            .take(DIAGNOSTIC_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > DIAGNOSTIC_LIMIT;
        if truncated {
            bytes.pop();
        }
        let text = String::from_utf8_lossy(&bytes);
        Ok(format!(
            "{}{}",
            text.trim(),
            if truncated { " [truncated]" } else { "" }
        ))
    })();
    match diagnostics {
        Ok(text) if text.is_empty() => format!(
            "{message}; dbus-daemon stderr at {} is empty",
            stderr_path.display()
        ),
        Ok(text) => format!(
            "{message}; dbus-daemon stderr at {}: {text}",
            stderr_path.display()
        ),
        Err(error) => format!(
            "{message}; read dbus-daemon stderr at {}: {error}",
            stderr_path.display()
        ),
    }
}

/// What bwrap prints when the kernel refuses it a user namespace or its id map.
const USER_NAMESPACE_REFUSALS: [&str; 3] = [
    "setting up uid map",
    "setting up gid map",
    "No permissions to create new namespace",
];

/// Kernel settings that decide whether this process may create a user
/// namespace, read when the refusal needs explaining.
struct UserNamespaceFacts {
    uid_map: String,
    max_user_namespaces: String,
    no_new_privs: String,
    seccomp: String,
    apparmor_restriction: String,
}

impl UserNamespaceFacts {
    fn read() -> Self {
        let file = |path: &str| {
            std::fs::read_to_string(path)
                .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|_| "unavailable".to_owned())
        };
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .map_or_else(|| "unavailable".to_owned(), |value| value.trim().to_owned())
        };
        Self {
            uid_map: file("/proc/self/uid_map"),
            max_user_namespaces: file("/proc/sys/user/max_user_namespaces"),
            no_new_privs: field("NoNewPrivs:"),
            seccomp: field("Seccomp:"),
            apparmor_restriction: file("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
        }
    }
}

/// Guidance for a startup that bwrap abandoned because the kernel refused its
/// user namespace. The cause is outside the session: this process is itself
/// confined, or the host disables unprivileged user namespaces.
fn user_namespace_guidance(stderr: &str, facts: &UserNamespaceFacts) -> Option<String> {
    USER_NAMESPACE_REFUSALS
        .iter()
        .any(|refusal| stderr.contains(refusal))
        .then(|| {
            format!(
                "bwrap could not create the session's user namespace, so no desktop was started. The kernel refused it for this kwin-mcp process (uid_map: {}; user.max_user_namespaces: {}; NoNewPrivs: {}; Seccomp: {}; kernel.apparmor_restrict_unprivileged_userns: {}). This happens when the server runs inside another sandbox or container that restricts nested user namespaces, or when the host disables unprivileged ones. Start kwin-mcp outside that sandbox, or allow nested user namespaces for it, then call session_start again",
                facts.uid_map,
                facts.max_user_namespaces,
                facts.no_new_privs,
                facts.seccomp,
                facts.apparmor_restriction,
            )
        })
}

fn preserve_startup_sandbox_diagnostics(mut error: McpError, workdir: &Path) -> McpError {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    const DIAGNOSTIC_LIMIT: u64 = 16 * 1024;

    let path = session_disk_path(workdir).join("sandbox.log");
    let diagnostic = (|| -> std::io::Result<(String, bool)> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "sandbox stderr is not a regular file",
            ));
        }
        let length = metadata.len();
        file.seek(SeekFrom::Start(length.saturating_sub(DIAGNOSTIC_LIMIT)))?;
        let mut bytes = Vec::new();
        file.take(DIAGNOSTIC_LIMIT).read_to_end(&mut bytes)?;
        Ok((
            String::from_utf8_lossy(&bytes).into_owned(),
            length > DIAGNOSTIC_LIMIT,
        ))
    })();
    let (tail, truncated) = match diagnostic {
        Ok(diagnostic) => diagnostic,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return error,
        Err(source) => {
            error.message = format!(
                "{}; could not read sandbox stderr before cleanup: {source}",
                error.message
            )
            .into();
            return error;
        }
    };
    error.message = if tail.trim().is_empty() {
        format!("{}; sandbox stderr was empty before cleanup", error.message)
    } else {
        format!(
            "{}; sandbox stderr{}: {}",
            error.message,
            if truncated { " (truncated tail)" } else { "" },
            tail.trim()
        )
    }
    .into();
    let guidance = user_namespace_guidance(&tail, &UserNamespaceFacts::read());
    if let Some(guidance) = &guidance {
        error.message = format!("{}; {guidance}", error.message).into();
    }
    let data = error.data.get_or_insert_with(|| serde_json::json!({}));
    if let Some(data) = data.as_object_mut() {
        if let Some(guidance) = guidance {
            data.insert("guidance".to_owned(), serde_json::json!(guidance));
        }
        data.insert("sandbox_stderr_tail".to_owned(), serde_json::json!(tail));
        data.insert(
            "sandbox_stderr_truncated".to_owned(),
            serde_json::json!(truncated),
        );
        data.insert("sandbox_stderr_path".to_owned(), serde_json::json!(path));
    }
    error
}

async fn wait_for_wallet_bus(
    child: &mut std::process::Child,
    socket: &Path,
    stderr_path: &Path,
) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let deadline = std::time::Instant::now() + DBUS_PROXY_TIMEOUT;
    loop {
        let error = match child.try_wait() {
            Ok(Some(status)) => Some(format!(
                "KWallet bus dbus-daemon exited with {status} before startup completed at {}",
                socket.display()
            )),
            Err(error) => Some(format!("check KWallet bus dbus-daemon: {error}")),
            Ok(None) => None,
        };
        if let Some(error) = error {
            return Err(wallet_bus_startup_error(error, stderr_path));
        }
        if std::fs::metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(wallet_bus_startup_error(
                format!(
                    "KWallet bus socket did not appear at {} within {}s",
                    socket.display(),
                    DBUS_PROXY_TIMEOUT.as_secs()
                ),
                stderr_path,
            ));
        }
        tokio::time::sleep(DBUS_PROXY_POLL).await;
    }
}

async fn connect_session_bus(
    address: &str,
    deadline: std::time::Instant,
) -> Result<zbus::Connection, String> {
    loop {
        let attempt_error = match zbus::connection::Builder::address(address) {
            Ok(builder) => match builder
                .auth_mechanism(zbus::AuthMechanism::Anonymous)
                .build()
                .await
            {
                Ok(conn) => return Ok(conn),
                Err(e) => e.to_string(),
            },
            Err(e) => return Err(format!("invalid D-Bus address '{address}': {e}")),
        };
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "failed to connect to session bus at {address} within {}s: {attempt_error}",
                STARTUP_TIMEOUT.as_secs(),
            ));
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

fn validate_session_socket_path(socket: &Path) -> anyhow::Result<()> {
    // UnixAddr uses the platform's sockaddr_un.sun_path capacity and reserves
    // its terminating NUL. Count filesystem bytes, not Unicode characters.
    nix::sys::socket::UnixAddr::new(socket).map_err(|error| {
        anyhow::anyhow!(
            "Unix socket path {} ({} bytes) cannot fit sockaddr_un.sun_path with a terminating NUL: {error}; choose a shorter TMPDIR before starting kwin-mcp",
            socket.display(),
            socket.as_os_str().len(),
        )
    })?;
    if socket.as_os_str().len() > DBUS_SOCKET_PATH_MAX {
        anyhow::bail!(
            "D-Bus Unix socket path {} ({} bytes) exceeds the libdbus pathname limit of {DBUS_SOCKET_PATH_MAX} bytes; choose a shorter TMPDIR before starting kwin-mcp",
            socket.display(),
            socket.as_os_str().len(),
        );
    }
    Ok(())
}

async fn spawn_dbus_proxy(
    address: &str,
    socket: &Path,
    rules: &[&str],
) -> anyhow::Result<std::process::Child> {
    use std::os::unix::fs::FileTypeExt;
    let _ = std::fs::remove_file(socket);
    let mut command = std::process::Command::new("xdg-dbus-proxy");
    terminate_with_parent(&mut command);
    command.arg(address).arg(socket).arg("--filter").args(rules);
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    let mut child = spawn_session_child(command, false).await?;
    let deadline = std::time::Instant::now() + DBUS_PROXY_TIMEOUT;
    while !std::fs::metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket()) {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("D-Bus proxy socket did not appear at {}", socket.display());
        }
        tokio::time::sleep(DBUS_PROXY_POLL).await;
    }
    Ok(child)
}

// Linux parent-death signals follow the spawning thread. Session children and
// exec therefore belong to the main thread, which survives re-execution.
enum ProcessRequest {
    Spawn {
        command: Box<std::process::Command>,
        process_group: bool,
        reply: tokio::sync::oneshot::Sender<std::io::Result<std::process::Child>>,
    },
    Exec {
        command: Box<std::process::Command>,
        reply: tokio::sync::oneshot::Sender<std::io::Error>,
    },
}
static PROCESS_REQUESTS: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<ProcessRequest>> =
    std::sync::OnceLock::new();
impl ProcessRequest {
    fn execute(self) {
        match self {
            Self::Spawn {
                mut command,
                process_group,
                reply,
            } => {
                if reply.is_closed() {
                    return;
                }
                if let Err(Ok(child)) = reply.send(command.spawn()) {
                    tokio::task::spawn_blocking(move || {
                        terminate_child(child, process_group, "cancelled session child")
                    });
                }
            }
            Self::Exec { mut command, reply } => {
                if reply.is_closed() {
                    return;
                }
                use std::os::unix::process::CommandExt;
                let _ = reply.send(command.exec());
            }
        }
    }
}
async fn spawn_session_child(
    command: std::process::Command,
    process_group: bool,
) -> std::io::Result<std::process::Child> {
    let (reply, response) = tokio::sync::oneshot::channel();
    PROCESS_REQUESTS
        .get()
        .ok_or_else(|| std::io::Error::other("session process owner is unavailable"))?
        .send(ProcessRequest::Spawn {
            command: Box::new(command),
            process_group,
            reply,
        })
        .map_err(|_| std::io::Error::other("session process owner stopped"))?;
    response
        .await
        .map_err(|_| std::io::Error::other("session process owner stopped"))?
}
async fn exec_on_main(command: std::process::Command) -> std::io::Result<std::io::Error> {
    let (reply, response) = tokio::sync::oneshot::channel();
    PROCESS_REQUESTS
        .get()
        .ok_or_else(|| std::io::Error::other("session process owner is unavailable"))?
        .send(ProcessRequest::Exec {
            command: Box::new(command),
            reply,
        })
        .map_err(|_| std::io::Error::other("session process owner stopped"))?;
    response
        .await
        .map_err(|_| std::io::Error::other("session process owner stopped"))
}

fn terminate_with_parent(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)
                .map_err(std::io::Error::other)
        });
    }
}

// ── uinput virtual devices ──────────────────────────────────────────────

const UINPUT_PATH: &str = "/dev/uinput";

fn uinput_error(
    device: &str,
    operation: &str,
    path: &Path,
    error: impl std::fmt::Display,
) -> KwinError {
    // Capture the failing process's credentials and namespaces, not those of
    // the login shell: an enclosing user namespace can change device access.
    let credentials = std::fs::read_to_string("/proc/self/status")
        .map(|status| {
            status
                .lines()
                .filter(|line| {
                    line.starts_with("Uid:")
                        || line.starts_with("Gid:")
                        || line.starts_with("Groups:")
                })
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_else(|error| format!("credentials unavailable: {error}"));
    let namespace = |name| {
        std::fs::read_link(format!("/proc/self/ns/{name}"))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable ({error})"))
    };
    KwinError::Msg(format!(
        "{device}: {operation} {}: {error}; {credentials}; user_namespace={}; mount_namespace={}",
        path.display(),
        namespace("user"),
        namespace("mnt"),
    ))
}

fn uinput_devnode(
    device: &mut evdev::uinput::VirtualDevice,
    name: &str,
) -> Result<PathBuf, KwinError> {
    let syspath = device.get_syspath().map_err(|error| {
        uinput_error(
            name,
            "query sysfs path (UI_GET_SYSNAME) via",
            Path::new(UINPUT_PATH),
            error,
        )
    })?;
    // Match evdev's enumeration, keeping the sysfs path at each fallible step.
    let entries = std::fs::read_dir(&syspath)
        .map_err(|error| uinput_error(name, "read evdev directory", &syspath, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            uinput_error(name, "read evdev directory entry in", &syspath, error)
        })?;
        let filename = entry.file_name();
        if filename.as_encoded_bytes().starts_with(b"event") {
            return Ok(Path::new("/dev/input").join(filename));
        }
    }
    Err(uinput_error(
        name,
        "locate evdev node in",
        &syspath,
        "no event* entry for /dev/input",
    ))
}

fn create_uinput_devices() -> Result<
    (
        evdev::uinput::VirtualDevice,
        std::path::PathBuf,
        evdev::uinput::VirtualDevice,
        std::path::PathBuf,
    ),
    KwinError,
> {
    // Mouse: buttons + relative axes
    let mut mouse_keys = evdev::AttributeSet::<evdev::KeyCode>::new();
    mouse_keys.insert(evdev::KeyCode::BTN_LEFT);
    mouse_keys.insert(evdev::KeyCode::BTN_RIGHT);
    mouse_keys.insert(evdev::KeyCode::BTN_MIDDLE);

    let mut mouse_axes = evdev::AttributeSet::<evdev::RelativeAxisCode>::new();
    mouse_axes.insert(evdev::RelativeAxisCode::REL_X);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_Y);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_WHEEL);
    mouse_axes.insert(evdev::RelativeAxisCode::REL_HWHEEL);

    let mouse_error =
        |operation, error| uinput_error("mouse", operation, Path::new(UINPUT_PATH), error);
    let mut mouse_dev = evdev::uinput::VirtualDevice::builder()
        .map_err(|error| mouse_error("open read/write", error))?
        .name("kwin-mcp-virtual-mouse")
        .with_keys(&mouse_keys)
        .map_err(|error| mouse_error("configure key capabilities on", error))?
        .with_relative_axes(&mouse_axes)
        .map_err(|error| mouse_error("configure relative axes on", error))?
        .build()
        .map_err(|error| {
            mouse_error(
                "create virtual device (UI_DEV_SETUP/UI_DEV_CREATE) via",
                error,
            )
        })?;

    let mouse_path = uinput_devnode(&mut mouse_dev, "mouse")?;

    // Keyboard: all standard keys (KEY_ESC=1 through KEY_MAX=0x2ff)
    let mut kbd_keys = evdev::AttributeSet::<evdev::KeyCode>::new();
    let mut code: u16 = 1;
    loop {
        if code > 0x2ff {
            break;
        }
        kbd_keys.insert(evdev::KeyCode::new(code));
        code = match code.checked_add(1) {
            Some(v) => v,
            None => break,
        };
    }

    let keyboard_error =
        |operation, error| uinput_error("keyboard", operation, Path::new(UINPUT_PATH), error);
    let mut kbd_dev = evdev::uinput::VirtualDevice::builder()
        .map_err(|error| keyboard_error("open read/write", error))?
        .name("kwin-mcp-virtual-keyboard")
        .with_keys(&kbd_keys)
        .map_err(|error| keyboard_error("configure key capabilities on", error))?
        .build()
        .map_err(|error| {
            keyboard_error(
                "create virtual device (UI_DEV_SETUP/UI_DEV_CREATE) via",
                error,
            )
        })?;

    let kbd_path = uinput_devnode(&mut kbd_dev, "keyboard")?;

    Ok((mouse_dev, mouse_path, kbd_dev, kbd_path))
}

// ── Mount-aware overlay ──────────────────────────────────────────────────

const HOST_SOCKET_ROOT: &str = "/run/kwin-mcp-host-sockets";
const HOST_SQLITE_ROOT: &str = "/run/kwin-mcp-host-sqlite";

/// Nested mounts require private copies of files in their ancestor directories.
/// Avoid eager bulk copies unless the caller explicitly requests writability.
const STAGING_COPY_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Entries of the host user runtime directory that a session must never reach:
/// the host session D-Bus (and with it the Secret Service and KWallet),
/// gnome-keyring's control and PKCS#11 sockets, and p11-kit's. Sockets from
/// them are never exposed, and the parent bind that carries sibling sockets
/// masks them.
const HOST_SECRET_ENTRIES: &[&str] = &["bus", "keyring", "p11-kit"];

fn mount_descendants(mounts: &[procfs::process::MountInfo], path: &Path) -> Vec<PathBuf> {
    let mut descendants: Vec<PathBuf> = mounts
        .iter()
        .filter(|mount| mount.mount_point != path && mount.mount_point.starts_with(path))
        .map(|mount| mount.mount_point.clone())
        .collect();
    descendants.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    descendants.dedup();
    descendants
}

struct OverlayMount {
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    destination: PathBuf,
}

#[derive(Default)]
struct SocketLinks(Vec<(PathBuf, PathBuf)>);
impl Drop for SocketLinks {
    fn drop(&mut self) {
        for (path, target) in &self.0 {
            if std::fs::read_link(path).is_ok_and(|link| link == *target) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
#[derive(Default, Serialize)]
struct SqliteAccessReport {
    databases: Vec<serde_json::Value>,
    total_databases: usize,
}

impl SqliteAccessReport {
    fn record(&mut self, path: &Path, mode: &'static str) {
        self.total_databases += 1;
        if self.databases.len() < 128 {
            self.databases
                .push(serde_json::json!({"path": path, "mode": mode}));
        }
    }

    /// A database whose requested access could not be given, with the reason.
    fn record_with_note(&mut self, path: &Path, mode: &'static str, note: String) {
        self.total_databases += 1;
        if self.databases.len() < 128 {
            self.databases
                .push(serde_json::json!({"path": path, "mode": mode, "note": note}));
        }
    }
}

struct OverlayPlan {
    /// Explicitly shared application state; writes reach these host directories.
    host_write_paths: Vec<PathBuf>,
    staging_root: Option<PathBuf>,
    overlays: Vec<OverlayMount>,
    read_only_binds: Vec<PathBuf>,
    socket_binds: Vec<PathBuf>,
    /// Known graphical socket names must stay hidden through aliases and HOME paths.
    graphical_socket_paths: Vec<PathBuf>,
    /// Directory binds preserve SQLite sidecar name lookup across WAL rotation.
    sqlite_binds: Vec<PathBuf>,
    sqlite_access: SqliteAccessReport,
    sqlite_blocked_sockets: Vec<PathBuf>,
    socket_links: SocketLinks,
    /// The host user runtime directory, when sockets from it are exposed. Its
    /// secret-bearing entries (HOST_SECRET_ENTRIES) are masked in the session.
    host_runtime: Option<PathBuf>,
    /// Directories the session sees as empty tmpfs; the host copies stay hidden.
    empty_dirs: Vec<PathBuf>,
    oversized_read_only_files: Vec<PathBuf>,
}

impl OverlayPlan {
    /// Overlay upper-layer path that shadows `path` (under `target`), or None
    /// when the path is not under a writable overlay (read-only bind or a
    /// file copied into the split-overlay staging root).
    fn upper_path(&self, path: &Path) -> Option<(PathBuf, &OverlayMount)> {
        if self
            .read_only_binds
            .iter()
            .any(|mount| path.starts_with(mount))
        {
            return None;
        }
        let overlay = self
            .overlays
            .iter()
            .filter(|overlay| path.starts_with(&overlay.destination))
            .max_by_key(|overlay| overlay.destination.components().count())?;
        Some((
            overlay
                .upper
                .join(path.strip_prefix(&overlay.destination).ok()?),
            overlay,
        ))
    }

    fn add_bwrap_args(
        &self,
        command: &mut std::process::Command,
        target: &Path,
        pre_overlay_binds: &[(PathBuf, PathBuf)],
    ) {
        command.args(["--ro-bind", "/", "/", "--tmpfs", "/run"]);
        // Some bridge sources live beneath HOME, whose overlays below hide the
        // host cache tree. Bind them while the original host paths are visible.
        for (source, destination) in pre_overlay_binds {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
        if let Some(staging_root) = &self.staging_root {
            command.arg("--bind").arg(staging_root).arg(target);
        }
        // The plan is made seconds before bwrap runs. HOME entries that vanished
        // since (lock directories such as ~/.claude.json.lock) would make bwrap
        // fail, so skip them; their staging stub stays an empty directory.
        for overlay in self
            .overlays
            .iter()
            .filter(|overlay| std::fs::symlink_metadata(&overlay.lower).is_ok())
        {
            command
                .arg("--overlay-src")
                .arg(&overlay.lower)
                .arg("--overlay")
                .arg(&overlay.upper)
                .arg(&overlay.work)
                .arg(&overlay.destination);
        }
        for path in &self.read_only_binds {
            command.arg("--ro-bind-try").arg(path).arg(path);
        }
        for path in &self.host_write_paths {
            command.arg("--bind").arg(path).arg(path);
            // Sharing state must not also reconnect host application sockets.
            for socket in &self.sqlite_blocked_sockets {
                if socket.starts_with(path) && std::fs::symlink_metadata(socket).is_ok() {
                    command.arg("--ro-bind").arg("/dev/null").arg(socket);
                }
            }
        }
        for path in &self.empty_dirs {
            command.arg("--tmpfs").arg(path);
        }
        for (index, source) in self.sqlite_binds.iter().enumerate() {
            // A hidden ancestor may have been added after alias discovery.
            // Do not recover its descendants through a separate mount.
            if self
                .empty_dirs
                .iter()
                .any(|hidden| source.starts_with(hidden))
            {
                continue;
            }
            let destination = PathBuf::from(format!("{HOST_SQLITE_ROOT}/{index}"));
            command.arg("--ro-bind").arg(source).arg(&destination);
            for socket in &self.sqlite_blocked_sockets {
                if let Ok(relative) = socket.strip_prefix(source)
                    && std::fs::symlink_metadata(socket).is_ok()
                {
                    command
                        .arg("--ro-bind")
                        .arg("/dev/null")
                        .arg(destination.join(relative));
                }
            }
            // Apply directory masks last: individual socket placeholders must
            // not recreate entries inside an intentionally empty directory.
            for hidden in &self.empty_dirs {
                // Upper-layer directories can be absent from this read-only
                // alias; bwrap cannot mount a tmpfs at a missing destination.
                if let Ok(relative) = hidden.strip_prefix(source)
                    && std::fs::symlink_metadata(hidden).is_ok_and(|metadata| metadata.is_dir())
                {
                    command.arg("--tmpfs").arg(destination.join(relative));
                }
            }
        }
        for (index, source) in self.socket_binds.iter().enumerate() {
            if self
                .empty_dirs
                .iter()
                .any(|hidden| source.starts_with(hidden))
            {
                continue;
            }
            let destination = PathBuf::from(format!("{HOST_SOCKET_ROOT}/{index}"));
            command
                .arg("--dir")
                .arg(&destination)
                .arg("--ro-bind")
                .arg(source)
                .arg(&destination);
            for path in self.graphical_socket_paths.iter().chain(&self.empty_dirs) {
                let Ok(relative) = path.strip_prefix(source) else {
                    continue;
                };
                let Ok(metadata) = std::fs::symlink_metadata(path) else {
                    continue;
                };
                if metadata.is_dir() {
                    command.arg("--tmpfs").arg(destination.join(relative));
                } else {
                    command
                        .arg("--ro-bind")
                        .arg("/dev/null")
                        .arg(destination.join(relative));
                }
            }
            if self
                .host_runtime
                .as_ref()
                .is_some_and(|runtime| runtime == source)
            {
                for name in HOST_SECRET_ENTRIES {
                    let Ok(metadata) = std::fs::symlink_metadata(source.join(name)) else {
                        continue;
                    };
                    if metadata.is_dir() {
                        command.arg("--tmpfs").arg(destination.join(name));
                    } else {
                        command
                            .arg("--ro-bind")
                            .arg("/dev/null")
                            .arg(destination.join(name));
                    }
                }
            }
        }
        // A read-only bind still permits Unix socket IPC. Hide graphical
        // sockets at their ordinary HOME paths as well as in the aliases
        // above; in split overlays those paths may come from the lower layer.
        for path in self
            .graphical_socket_paths
            .iter()
            .filter(|path| path.starts_with(target))
        {
            command.arg("--ro-bind-try").arg("/dev/null").arg(path);
        }
    }

    fn expose_sockets(
        &mut self,
        target: &Path,
        host_runtime: &Path,
        isolated_runtime: &Path,
        inventory: &HostProcessInventory,
        service: &KwinMcp,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let canonical_target = std::fs::canonicalize(target)?;
        let canonical_runtime = std::fs::canonicalize(host_runtime)?;
        let current_uid = procfs::process::Process::myself()?.uid()?;
        self.host_runtime = Some(canonical_runtime.clone());
        let socket_inventory = procfs::net::unix()?;
        self.graphical_socket_paths = socket_inventory
            .iter()
            .filter(|entry| inventory.graphical_sockets.contains(&entry.inode))
            .filter_map(|entry| entry.path.as_ref())
            // Other servers' control sockets come and go while this session
            // starts, and a mask over one that disappears fails startup. They
            // reject peers from another PID namespace, so they need no mask.
            .filter(|path| !is_session_control_socket(path))
            .flat_map(|path| {
                let mut names = vec![path.clone()];
                if let Ok(canonical) = std::fs::canonicalize(path) {
                    names.push(canonical);
                }
                names
            })
            .collect();
        self.graphical_socket_paths.sort();
        self.graphical_socket_paths.dedup();
        self.read_only_binds
            .retain(|mount| !self.graphical_socket_paths.contains(mount));
        // Read-only data mounts still permit socket IPC; hide these names
        // through SQLite aliases and retain the separate helper socket policy.
        self.sqlite_blocked_sockets = socket_inventory
            .iter()
            .filter_map(|entry| entry.path.as_ref().filter(|path| path.is_absolute()))
            .flat_map(|path| {
                let mut names = vec![path.clone()];
                if let Ok(canonical) = std::fs::canonicalize(path)
                    && let Ok(relative) = canonical.strip_prefix(&canonical_target)
                {
                    names.push(target.join(relative));
                }
                names
            })
            .collect();
        self.sqlite_blocked_sockets.sort();
        self.sqlite_blocked_sockets.dedup();
        let mut sockets: Vec<(PathBuf, PathBuf, PathBuf)> = socket_inventory
            .into_iter()
            .filter_map(|entry| {
                if entry.state != procfs::net::UnixState::UNCONNECTED {
                    return None;
                }
                let path = entry.path?;
                // The stat below is the call that stalls on a hung mount, so
                // a blocked start names the socket it was checking.
                service.record_start_stage(format!("checking host socket {}", path.display()));
                let parent = std::fs::canonicalize(path.parent()?).ok()?;
                let metadata = std::fs::metadata(&path).ok()?;
                if metadata.uid() != current_uid
                    || !metadata.file_type().is_socket()
                    || inventory.graphical_sockets.contains(&entry.inode)
                {
                    return None;
                }
                if path.starts_with(target) && parent.starts_with(&canonical_target) {
                    Some((path.clone(), parent, path))
                } else if path.starts_with(host_runtime)
                    && parent.starts_with(&canonical_runtime)
                    && !path
                        .strip_prefix(host_runtime)
                        .ok()
                        .and_then(|relative| relative.components().next())
                        .is_some_and(|first| {
                            HOST_SECRET_ENTRIES
                                .iter()
                                .any(|name| first.as_os_str() == std::ffi::OsStr::new(name))
                        })
                {
                    Some((
                        path.clone(),
                        parent,
                        isolated_runtime.join(path.strip_prefix(host_runtime).ok()?),
                    ))
                } else {
                    None
                }
            })
            .filter(|(path, _, _)| {
                !self
                    .host_write_paths
                    .iter()
                    .any(|shared| path.starts_with(shared))
                    && !self
                        .read_only_binds
                        .iter()
                        .any(|mount| path != mount && path.starts_with(mount))
            })
            .collect();
        sockets.sort();
        sockets.dedup();
        for (path, parent, destination) in sockets {
            self.read_only_binds.retain(|mount| mount != &path);
            let index = self
                .socket_binds
                .iter()
                .position(|bind| bind == &parent)
                .unwrap_or_else(|| {
                    let index = self.socket_binds.len();
                    self.socket_binds.push(parent);
                    index
                });
            let link = if path.starts_with(target) {
                let mount = self
                    .overlays
                    .iter()
                    .filter(|overlay| path.starts_with(&overlay.destination))
                    .max_by_key(|overlay| overlay.destination.components().count());
                if let Some(overlay) = mount {
                    overlay.upper.join(path.strip_prefix(&overlay.destination)?)
                } else {
                    self.staging_root
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("no overlay for {}", path.display()))?
                        .join(path.strip_prefix(target)?)
                }
            } else {
                destination
            };
            std::fs::create_dir_all(
                link.parent()
                    .ok_or_else(|| anyhow::anyhow!("no parent for {}", link.display()))?,
            )?;
            let generated = PathBuf::from(format!("{HOST_SOCKET_ROOT}/{index}")).join(
                path.file_name()
                    .ok_or_else(|| anyhow::anyhow!("unnamed socket {}", path.display()))?,
            );
            match std::fs::symlink_metadata(&link) {
                Ok(_) => anyhow::bail!("refusing to replace {}", link.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            std::os::unix::fs::symlink(&generated, &link)?;
            self.socket_links.0.push((link, generated));
        }
        Ok(())
    }
}

struct HostProcessInventory {
    graphical_sockets: std::collections::HashSet<u64>,
    live_databases: Vec<PathBuf>,
}

/// Link targets of one process's descriptors. procfs' `fd()` also opens and
/// stats every descriptor, which classification never reads; reading only the
/// links cut the scan of a desktop with 21k descriptors by about 40%.
fn descriptor_targets(pid: i32) -> Vec<procfs::process::FDTarget> {
    use std::str::FromStr;
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let link = std::fs::read_link(entry.path()).ok()?;
            procfs::process::FDTarget::from_str(link.to_str()?).ok()
        })
        .collect()
}

/// Read descriptors once for both graphical socket isolation and live WAL files.
fn host_process_inventory(
    target: &Path,
    service: &KwinMcp,
) -> anyhow::Result<HostProcessInventory> {
    use procfs::process::FDTarget;
    let mut graphical = std::collections::HashSet::new();
    let mut databases = Vec::new();
    let uid = procfs::process::Process::myself()?.uid()?;
    let canonical_target = std::fs::canonicalize(target)?;
    for process in procfs::process::all_processes()?.flatten() {
        service.record_start_stage(format!("reading /proc/{}", process.pid));
        let owned = process.uid().ok() == Some(uid);
        // A server that is not root cannot read another user's environment,
        // so that read is skipped for those processes. The read is denied to
        // them anyway, so skipping it changes nothing.
        let environment = if owned || uid == 0 {
            process.environ().unwrap_or_default()
        } else {
            Default::default()
        };
        let cgroup =
            std::fs::read_to_string(format!("/proc/{}/cgroup", process.pid)).unwrap_or_default();
        // The MCP launcher may have no desktop variables. Classify sockets
        // using their owner's environment rather than the launcher's values.
        let display_attached =
            ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
                .iter()
                .any(|name| {
                    environment
                        .get(std::ffi::OsStr::new(name))
                        .is_some_and(|value| !value.is_empty())
                });
        let desktop_scope = cgroup.contains("/session.slice/")
            || (cgroup.contains("/app.slice/") && cgroup.contains(".scope"));
        // Descriptors of another user's process are unreadable to a server
        // that is not root, so those reads are skipped as well.
        let descriptors = if owned || uid == 0 {
            descriptor_targets(process.pid)
        } else {
            Vec::new()
        };
        for descriptor in &descriptors {
            if owned
                && let FDTarget::Path(path) = descriptor
                && let Some(name) = path.file_name().and_then(|name| name.to_str())
                && let Some(database) = name.strip_suffix("-wal")
                && let Ok(relative) = path
                    .with_file_name(database)
                    .strip_prefix(&canonical_target)
            {
                databases.push(target.join(relative));
            }
        }
        let input_attached = descriptors.iter().any(|descriptor| {
            if let FDTarget::Path(path) = descriptor {
                path == Path::new("/dev/uinput") || path.starts_with("/dev/input")
            } else {
                false
            }
        });
        if display_attached || desktop_scope || input_attached {
            graphical.extend(descriptors.into_iter().filter_map(|descriptor| {
                if let FDTarget::Socket(inode) = descriptor {
                    Some(inode)
                } else {
                    None
                }
            }));
        }
    }
    databases.sort();
    databases.dedup();
    Ok(HostProcessInventory {
        graphical_sockets: graphical,
        live_databases: databases,
    })
}

fn create_staging_directory(
    source: &Path,
    destination: &Path,
    initialize: bool,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(destination)?;
    // A source that vanished meanwhile keeps the default mode.
    if initialize && let Ok(meta) = std::fs::metadata(source) {
        std::fs::set_permissions(destination, meta.permissions())?;
    }
    Ok(())
}

struct SplitOverlayContext<'a> {
    target: &'a Path,
    staging_root: &'a Path,
    upper_root: &'a Path,
    work_root: &'a Path,
    mounts: &'a [procfs::process::MountInfo],
    initialize: bool,
    writable_paths: &'a [PathBuf],
    host_write_paths: &'a [PathBuf],
    live_databases: &'a [PathBuf],
}

fn clone_staging_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let source = std::fs::File::open(source)?;
    let destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    // Both descriptors stay open for this ioctl. FICLONE shares data extents;
    // later writes to either file leave the other file's contents unchanged.
    if unsafe {
        nix::libc::ioctl(
            destination.as_raw_fd(),
            nix::libc::FICLONE,
            source.as_raw_fd(),
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    destination.set_permissions(source.metadata()?.permissions())
}

fn prepare_split_overlay_directory(
    source_directory: &Path,
    context: &SplitOverlayContext<'_>,
    overlays: &mut Vec<OverlayMount>,
    read_only_binds: &mut Vec<PathBuf>,
    oversized_read_only_files: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let relative_directory = source_directory.strip_prefix(context.target)?;
    let staged_directory = context.staging_root.join(relative_directory);
    create_staging_directory(source_directory, &staged_directory, context.initialize)?;

    let mut entries: Vec<std::fs::DirEntry> =
        std::fs::read_dir(source_directory)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let source = entry.path();
        let relative = source.strip_prefix(context.target)?;
        let staged = context.staging_root.join(relative);
        let file_type = entry.file_type()?;
        let staged_exists = std::fs::symlink_metadata(&staged).is_ok();

        if context.host_write_paths.contains(&source) {
            // The live directory bind supplies this subtree, including WAL
            // sidecars and nested mounts. Never stage its potentially huge files.
            std::fs::create_dir_all(&staged)?;
            continue;
        }

        if context
            .mounts
            .iter()
            .any(|mount| mount.mount_point == source)
        {
            // Only a stub for bwrap's bind: the mount shadows its mode. Never
            // stat the mount itself; a hung FUSE or network mount would block
            // session_start in the kernel.
            if file_type.is_dir() {
                std::fs::create_dir_all(&staged)?;
            }
            continue;
        }

        if file_type.is_dir() {
            if context
                .mounts
                .iter()
                .any(|mount| mount.mount_point != source && mount.mount_point.starts_with(&source))
            {
                prepare_split_overlay_directory(
                    &source,
                    context,
                    overlays,
                    read_only_binds,
                    oversized_read_only_files,
                )?;
            } else {
                create_staging_directory(&source, &staged, context.initialize)?;
                let upper = context.upper_root.join(relative);
                let work = context.work_root.join(relative);
                std::fs::create_dir_all(&upper)?;
                std::fs::create_dir_all(&work)?;
                overlays.push(OverlayMount {
                    lower: source.clone(),
                    upper,
                    work,
                    destination: source,
                });
            }
            continue;
        }

        if file_type.is_symlink() {
            if context.initialize && !staged_exists {
                std::os::unix::fs::symlink(std::fs::read_link(&source)?, &staged)?;
            }
        } else if file_type.is_file() {
            if context.live_databases.iter().any(|database| {
                !browser_cookie_database(context.target, database)
                    && (source == *database
                        || ["-wal", "-shm"].iter().any(|suffix| {
                            database.file_name().is_some_and(|name| {
                                source
                                    == database.with_file_name(format!(
                                        "{}{suffix}",
                                        name.to_string_lossy()
                                    ))
                            })
                        }))
            }) {
                // These files will resolve through the read-only live directory.
                continue;
            }
            if context.initialize && !staged_exists {
                let requested_writable = context
                    .writable_paths
                    .iter()
                    .any(|path| source.starts_with(path));
                let size = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if size > STAGING_COPY_MAX_BYTES {
                    match clone_staging_file(&source, &staged) {
                        Ok(()) => continue,
                        Err(error) => {
                            match std::fs::remove_file(&staged) {
                                Ok(()) => {}
                                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => {}
                                Err(cleanup) => return Err(cleanup.into()),
                            }
                            if !requested_writable {
                                eprintln!(
                                    "session_start: oversized ancestor file {} ({size} bytes) stays read-only; reflink unavailable: {error}",
                                    source.display()
                                );
                                oversized_read_only_files.push(source.clone());
                                read_only_binds.push(source);
                                continue;
                            }
                        }
                    }
                }
                match std::fs::copy(&source, &staged) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error)
                        if error.kind() == std::io::ErrorKind::PermissionDenied
                            && !requested_writable =>
                    {
                        let _ = std::fs::remove_file(&staged);
                        read_only_binds.push(source);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            read_only_binds.push(source);
        }
    }
    Ok(())
}

fn prepare_overlay_plan(
    target: &Path,
    session_tmp: &Path,
    mounts: &[procfs::process::MountInfo],
    writable_paths: &[PathBuf],
    host_write_paths: &[PathBuf],
    live_databases: &[PathBuf],
) -> anyhow::Result<OverlayPlan> {
    let excluded_mounts = mount_descendants(mounts, target);
    if excluded_mounts.is_empty() {
        let upper = session_tmp.join("overlay-upper");
        let work = session_tmp.join("overlay-work");
        std::fs::create_dir_all(&upper)?;
        std::fs::create_dir_all(&work)?;
        let overlay = OverlayMount {
            lower: target.to_path_buf(),
            upper,
            work,
            destination: target.to_path_buf(),
        };
        return Ok(OverlayPlan {
            host_write_paths: host_write_paths.to_vec(),
            staging_root: None,
            overlays: vec![overlay],
            read_only_binds: Vec::new(),
            socket_binds: Vec::new(),
            graphical_socket_paths: Vec::new(),
            sqlite_binds: Vec::new(),
            sqlite_access: SqliteAccessReport::default(),
            sqlite_blocked_sockets: Vec::new(),
            socket_links: SocketLinks::default(),
            host_runtime: None,
            empty_dirs: Vec::new(),
            oversized_read_only_files: Vec::new(),
        });
    }

    let staging_root = session_tmp.join("overlay-root");
    let initialized_marker = session_tmp.join("overlay-root.initialized");
    let initialize = !initialized_marker.exists();
    let upper_root = session_tmp.join("overlay-upper");
    let work_root = session_tmp.join("overlay-work");
    std::fs::create_dir_all(&upper_root)?;
    std::fs::create_dir_all(&work_root)?;

    let context = SplitOverlayContext {
        target,
        staging_root: &staging_root,
        upper_root: &upper_root,
        work_root: &work_root,
        mounts,
        initialize,
        writable_paths,
        host_write_paths,
        live_databases,
    };
    let mut overlays = Vec::new();
    let mut read_only_binds = excluded_mounts;
    let mut oversized_read_only_files = Vec::new();
    prepare_split_overlay_directory(
        target,
        &context,
        &mut overlays,
        &mut read_only_binds,
        &mut oversized_read_only_files,
    )?;
    read_only_binds.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    read_only_binds.dedup();
    if initialize {
        std::fs::write(&initialized_marker, b"split-overlay-v1\n")?;
    }

    Ok(OverlayPlan {
        host_write_paths: host_write_paths.to_vec(),
        staging_root: Some(staging_root),
        overlays,
        read_only_binds,
        socket_binds: Vec::new(),
        graphical_socket_paths: Vec::new(),
        sqlite_binds: Vec::new(),
        sqlite_access: SqliteAccessReport::default(),
        sqlite_blocked_sockets: Vec::new(),
        socket_links: SocketLinks::default(),
        host_runtime: None,
        empty_dirs: Vec::new(),
        oversized_read_only_files,
    })
}

/// Blocking host scan still owned by an in-flight session_start.
type HostWork = Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;

/// Resolve live SQLite databases through read-only directory binds. SQLite
/// canonicalizes the main database symlink before opening its sidecars, so WAL
/// checkpoint/unlink/recreation uses the host's current directory entries,
/// rather than pinned file mounts. Sibling HOME files remain privately writable.
fn bind_live_sqlite(
    plan: &mut OverlayPlan,
    target: &Path,
    databases: &[PathBuf],
    writable_paths: &[PathBuf],
    service: &KwinMcp,
    browser_profiles: BrowserProfiles,
) -> anyhow::Result<()> {
    let discarded = match browser_profiles {
        BrowserProfiles::Inherit => discarded_session_databases(target, service)?,
        BrowserProfiles::Fresh => Vec::new(),
    };
    for database in databases.iter().filter(|database| {
        !discarded.contains(database) && !browser_cookie_database(target, database)
    }) {
        if plan
            .host_write_paths
            .iter()
            .any(|path| database.starts_with(path))
        {
            plan.sqlite_access.record(database, "shared_host");
            continue;
        }
        let name = database
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("database has no name"))?;
        if !database.is_file() {
            continue;
        }
        if plan
            .empty_dirs
            .iter()
            .any(|hidden| database.starts_with(hidden))
        {
            continue;
        }
        let private_write_requested = writable_paths.iter().any(|path| database.starts_with(path));
        if plan
            .read_only_binds
            .iter()
            .any(|mount| database != mount && database.starts_with(mount))
        {
            anyhow::ensure!(
                !private_write_requested,
                "private SQLite copy is covered by a read-only ancestor: {}",
                database.display()
            );
            // A directory bind already provides fresh read-only name lookup.
            plan.sqlite_access.record(database, "live_read_only");
            continue;
        }
        plan.read_only_binds.retain(|mount| {
            mount != database
                && !["-wal", "-shm", "-journal"].iter().any(|suffix| {
                    mount == &database.with_file_name(format!("{}{suffix}", name.to_string_lossy()))
                })
        });
        let mut snapshot_skipped = None;
        if private_write_requested {
            let destination = if plan.upper_path(database).is_some() {
                upper_file(plan, database)?
            } else {
                plan.staging_root
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("no private layer for writable SQLite database")
                    })?
                    .join(database.strip_prefix(target)?)
            };
            let parent = destination
                .parent()
                .ok_or_else(|| anyhow::anyhow!("private database has no parent"))?;
            std::fs::create_dir_all(parent)?;
            service.host_step(format!(
                "snapshotting private writable SQLite: {}",
                database.display()
            ))?;
            match kwin_mcp::browser_cookies::snapshot_host(database, &destination) {
                Ok(()) => {
                    // A later host rollback journal must not appear beside the private
                    // database through the overlay's lower layer.
                    let mut journal = destination.as_os_str().to_os_string();
                    journal.push("-journal");
                    let journal = PathBuf::from(journal);
                    std::fs::write(&journal, [])?;
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &journal,
                            std::fs::Permissions::from_mode(0o600),
                        )?;
                    }
                    plan.sqlite_access
                        .record(database, "private_writable_snapshot");
                    continue;
                }
                // One database too large to copy must not fail the whole session:
                // it stays a live read-only alias, and the report says why.
                Err(error) if error.is::<kwin_mcp::browser_cookies::SnapshotTooLarge>() => {
                    snapshot_skipped = Some(format!("{error:#}"));
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "private writable SQLite snapshot {}: {error:#}",
                        database.display()
                    ));
                }
            }
        }
        let parent = database
            .parent()
            .ok_or_else(|| anyhow::anyhow!("database has no parent"))?;
        let index = plan
            .sqlite_binds
            .iter()
            .position(|bind| bind == parent)
            .unwrap_or_else(|| {
                let index = plan.sqlite_binds.len();
                plan.sqlite_binds.push(parent.to_path_buf());
                index
            });
        let upper = if plan.upper_path(database).is_some() {
            upper_file(plan, database)?
        } else {
            plan.staging_root
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("no private layer for live database"))?
                .join(database.strip_prefix(target)?)
        };
        std::os::unix::fs::symlink(
            PathBuf::from(format!("{HOST_SQLITE_ROOT}/{index}")).join(name),
            upper,
        )?;
        match snapshot_skipped {
            Some(reason) => {
                plan.sqlite_access.record_with_note(
                    database,
                    "live_read_only",
                    format!("private writable snapshot skipped: {reason}"),
                );
                eprintln!(
                    "session_start: {} is too large for the requested private writable snapshot ({reason}); it stays live read-only",
                    database.display()
                );
            }
            None => plan.sqlite_access.record(database, "live_read_only"),
        }
        eprintln!(
            "session_start: live SQLite database read-only: {}",
            database.display()
        );
    }
    Ok(())
}

/// Browser cookie jars need private writes for refreshed and session cookies.
/// Keep them in the HOME overlay instead of mounting the host jar read-only.
fn browser_cookie_database(target: &Path, database: &Path) -> bool {
    database.file_name().is_some_and(|name| name == "Cookies")
        && BROWSER_CONFIG_DIRS
            .iter()
            .any(|config| database.starts_with(target.join(config)))
}

/// Recreate missing upper parents using the lower directories' permissions.
/// Writes land in the session's disk overlay, never on the host file.
fn upper_file(plan: &OverlayPlan, path: &Path) -> anyhow::Result<PathBuf> {
    let (upper, overlay) = plan
        .upper_path(path)
        .ok_or_else(|| anyhow::anyhow!("not under a writable overlay"))?;
    let parent = upper.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
    let mut missing = Vec::new();
    let mut cursor = parent.to_path_buf();
    while !cursor.exists() && cursor.starts_with(&overlay.upper) && cursor != overlay.upper {
        missing.push(cursor.clone());
        cursor = cursor
            .parent()
            .ok_or_else(|| anyhow::anyhow!("no parent"))?
            .to_path_buf();
    }
    for directory in missing.iter().rev() {
        std::fs::create_dir(directory)?;
        let lower = overlay
            .destination
            .join(directory.strip_prefix(&overlay.upper)?);
        if let Ok(meta) = std::fs::metadata(&lower) {
            std::fs::set_permissions(directory, meta.permissions())?;
        }
    }
    Ok(upper)
}

/// Chromium-family config directories whose profiles are copied into sessions.
const BROWSER_CONFIG_DIRS: &[&str] = &[
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/google-chrome-unstable",
    ".config/chromium",
    ".config/BraveSoftware/Brave-Browser",
    ".config/microsoft-edge",
    ".config/vivaldi",
];

const BROWSER_CACHE_DIR: &str = ".cache/google-chrome";

const EMPTY_BROWSER_DATABASES: &[&str] = &["History", "HistoryEmbeddings", "Favicons"];

fn prepare_browser_cookies(
    plan: &mut OverlayPlan,
    target: &Path,
    baseline_root: &Path,
    service: &KwinMcp,
) -> anyhow::Result<(Vec<kwin_mcp::browser_cookies::CookieRoot>, Vec<String>)> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(baseline_root)?;
    std::fs::set_permissions(baseline_root, std::fs::Permissions::from_mode(0o700))?;
    let state = kwin_mcp::browser_cookies::state_root()?;
    if state.starts_with(target) {
        plan.empty_dirs.push(state.clone());
    }
    let mut roots = Vec::new();
    let mut cookie_errors = Vec::new();
    for config in BROWSER_CONFIG_DIRS {
        let host = target.join(config);
        let saved = state.join(config);
        let baseline = baseline_root.join(config);
        let mut profiles = std::collections::BTreeSet::new();
        for directory in [&host, &saved] {
            service.profile_operation("list cookie profiles", directory)?;
            if let Ok(entries) = std::fs::read_dir(directory) {
                for entry in entries
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                {
                    profiles.insert(entry.file_name());
                }
            }
        }
        for profile in profiles {
            for name in ["Cookies", "Network/Cookies"] {
                let database = host.join(&profile).join(name);
                let previous = saved.join(&profile).join(name);
                service.profile_operation("inspect cookie database", &database)?;
                if !database.is_file() && !previous.is_file() {
                    continue;
                }
                let upper = upper_file(plan, &database)?;
                let mut merge_failed = false;
                let result = if database.is_file() {
                    service.profile_operation("snapshot host cookies", &database)?;
                    kwin_mcp::browser_cookies::snapshot_host(&database, &upper).and_then(|()| {
                        service.profile_operation("merge saved cookies", &previous)?;
                        if let Err(error) = kwin_mcp::browser_cookies::merge(&upper, &previous) {
                            merge_failed = true;
                            let message = format!("{}: {error:#}", database.display());
                            eprintln!(
                                "session_start: preserving host cookie schema for {}: {error:#}",
                                database.display()
                            );
                            cookie_errors.push(message);
                        }
                        Ok(())
                    })
                } else {
                    service.profile_operation("snapshot saved cookies", &previous)?;
                    kwin_mcp::browser_cookies::snapshot(&previous, &upper)
                };
                if let Err(error) = result {
                    // Never expose a live lower-layer WAL after a failed copy.
                    // A saved jar is a safe fallback; without one, fail startup
                    // rather than let the browser open an incoherent profile.
                    if previous.is_file() {
                        let message = format!(
                            "{}: host cookie snapshot failed; using saved cookies instead: {error:#}",
                            database.display()
                        );
                        eprintln!("session_start: {message}");
                        cookie_errors.push(message);
                        service.profile_operation("snapshot saved cookies", &previous)?;
                        kwin_mcp::browser_cookies::snapshot(&previous, &upper)?;
                    } else {
                        return Err(
                            error.context(format!("snapshot cookies for {}", database.display()))
                        );
                    }
                }
                service.profile_operation("inspect private cookie schema", &upper)?;
                if !kwin_mcp::browser_cookies::supports_cookie_jar(&upper)? {
                    eprintln!(
                        "session_start: skipping non-cookie SQLite database {}",
                        database.display()
                    );
                    continue;
                }
                if !merge_failed {
                    service.profile_operation("apply cookie deletions", &previous)?;
                    kwin_mcp::browser_cookies::apply_deletions(&upper, &previous)?;
                }
                let original = baseline.join(&profile).join(name);
                std::fs::create_dir_all(
                    original
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("cookie baseline has no parent"))?,
                )?;
                service.profile_operation("snapshot cookie baseline", &original)?;
                kwin_mcp::browser_cookies::snapshot(&upper, &original)?;
            }
        }
        for overlay in &plan.overlays {
            if let Ok(relative) = host.strip_prefix(&overlay.destination) {
                roots.push(kwin_mcp::browser_cookies::CookieRoot {
                    upper: overlay.upper.join(relative),
                    saved: saved.clone(),
                    baseline: Some(baseline.clone()),
                });
            } else if overlay.destination.starts_with(&host) {
                roots.push(kwin_mcp::browser_cookies::CookieRoot {
                    upper: overlay.upper.clone(),
                    saved: state.join(overlay.destination.strip_prefix(target)?),
                    baseline: Some(baseline_root.join(overlay.destination.strip_prefix(target)?)),
                });
            }
        }
    }
    Ok((roots, cookie_errors))
}

fn discarded_session_databases(target: &Path, service: &KwinMcp) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for config in BROWSER_CONFIG_DIRS {
        service.profile_operation("list profiles", &target.join(config))?;
        let Ok(profiles) = std::fs::read_dir(target.join(config)) else {
            continue;
        };
        for profile in profiles
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        {
            for name in EMPTY_BROWSER_DATABASES {
                let path = profile.path().join(name);
                service.profile_operation("inspect disposable database", &path)?;
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
    }
    Ok(paths)
}

fn empty_session_databases(
    plan: &OverlayPlan,
    databases: &[PathBuf],
    service: &KwinMcp,
) -> anyhow::Result<()> {
    for database in databases {
        service.profile_operation("empty private database", database)?;
        if !database.parent().is_some_and(Path::is_dir) {
            continue;
        }
        for suffix in ["", "-wal", "-shm"] {
            let name = database
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("database has no name"))?;
            let path = database.with_file_name(format!("{}{suffix}", name.to_string_lossy()));
            let upper = upper_file(plan, &path)?;
            std::fs::write(upper, [])?;
        }
    }
    Ok(())
}

/// The session sees the user's browser profiles while the host browser is
/// still running, so their Preferences record an unclean exit and the session
/// browser offers "Restore pages? Chrome didn't shut down correctly". Mark each
/// profile's copy in the session's upper layer as a clean exit. The host
/// Preferences files are only read.
/// Session browsers start on a new tab instead of reopening every tab the user
/// has open on the host: each profile's `Sessions` directory (the saved
/// windows and tabs) is shown to the session as an empty tmpfs. The restore
/// setting itself is left alone, because switching Chrome off "continue where
/// you left off" makes it delete session-only cookies at startup and would
/// drop logins that live in them.
fn hide_browser_sessions(
    plan: &mut OverlayPlan,
    target: &Path,
    service: &KwinMcp,
) -> anyhow::Result<()> {
    for config in BROWSER_CONFIG_DIRS {
        service.profile_operation("list profiles", &target.join(config))?;
        let Ok(profiles) = std::fs::read_dir(target.join(config)) else {
            continue;
        };
        for profile in profiles
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        {
            let sessions = profile.path().join("Sessions");
            service.profile_operation("inspect saved tabs", &sessions)?;
            if sessions.is_dir() {
                plan.empty_dirs.push(sessions);
            }
        }
    }
    Ok(())
}

fn mark_browser_exits_clean(
    plan: &OverlayPlan,
    target: &Path,
    service: &KwinMcp,
) -> anyhow::Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut marked = 0;
    for config in BROWSER_CONFIG_DIRS {
        service.profile_operation("list profiles", &target.join(config))?;
        let Ok(profiles) = std::fs::read_dir(target.join(config)) else {
            continue;
        };
        for profile in profiles
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        {
            let preferences = profile.path().join("Preferences");
            service.profile_operation("read Preferences", &preferences)?;
            let result = (|| -> anyhow::Result<bool> {
                let text = match std::fs::read(&preferences) {
                    Ok(text) => text,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error.into()),
                };
                let mut json: serde_json::Value = serde_json::from_slice(&text)?;
                let profile_prefs = json
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("not a JSON object"))?
                    .entry("profile")
                    .or_insert_with(|| serde_json::json!({}));
                let profile_prefs = profile_prefs
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("profile is not an object"))?;
                if profile_prefs
                    .get("exit_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("Normal")
                    && profile_prefs
                        .get("exited_cleanly")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                {
                    return Ok(false);
                }
                profile_prefs.insert(
                    "exit_type".to_owned(),
                    serde_json::Value::String("Normal".to_owned()),
                );
                profile_prefs.insert("exited_cleanly".to_owned(), serde_json::Value::Bool(true));
                let upper = upper_file(plan, &preferences)?;
                let temporary = upper.with_file_name(".Preferences.kwin-mcp");
                std::fs::write(&temporary, serde_json::to_vec(&json)?)?;
                let mode = std::fs::metadata(&preferences)?.permissions().mode();
                std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))?;
                std::fs::rename(&temporary, &upper)?;
                Ok(true)
            })();
            match result {
                Ok(true) => marked += 1,
                Ok(false) => {}
                Err(error) => eprintln!(
                    "session_start: left {} as is: {error:#}",
                    preferences.display()
                ),
            }
        }
    }
    Ok(marked)
}

/// Host state session_start reads before it spawns the sandbox.
struct HostView {
    overlay_plan: OverlayPlan,
    kdeglobals: String,
    cookie_roots: Vec<kwin_mcp::browser_cookies::CookieRoot>,
    cookie_errors: Vec<String>,
}

/// Validate explicit host writes on the bounded blocking preparation thread.
fn validate_host_write_paths(
    target: &Path,
    host_runtime: &Path,
    writable_paths: &[PathBuf],
    paths: &[PathBuf],
) -> anyhow::Result<Vec<PathBuf>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut host_write_paths = paths.to_vec();
    let canonical_home =
        std::fs::canonicalize(target).map_err(|error| anyhow::anyhow!("resolve HOME: {error}"))?;
    // These paths are hidden by later tmpfs mounts. Sharing any part of them
    // would report live host writes while silently masking the selected state.
    let private_dirs = [
        session_disk_root(),
        host_runtime.to_path_buf(),
        kwin_mcp::browser_cookies::state_root()?,
        target.join(BROWSER_CACHE_DIR),
    ];
    for path in &host_write_paths {
        let relative = path.strip_prefix(target).ok();
        let resolved = std::fs::canonicalize(path).ok();
        if !path.is_absolute()
            || relative.is_none_or(|relative| {
                relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
            })
            || !path.is_dir()
            || resolved.as_ref()
                != relative
                    .map(|relative| canonical_home.join(relative))
                    .as_ref()
            || BROWSER_CONFIG_DIRS.iter().any(|config| {
                let browser = target.join(config);
                path.starts_with(&browser) || browser.starts_with(path)
            })
            || private_dirs
                .iter()
                .any(|private| path.starts_with(private) || private.starts_with(path))
        {
            anyhow::bail!(
                "host_write_paths requires existing directories strictly under HOME, without symlinks, '.', '..', browser profiles/cache, saved browser cookies, session storage or host runtime (including their ancestors and descendants): {}",
                path.display()
            );
        }
        if writable_paths
            .iter()
            .any(|private| private.starts_with(path) || path.starts_with(private))
        {
            anyhow::bail!(
                "host_write_paths conflicts with private writable_paths: {}",
                path.display()
            );
        }
    }
    host_write_paths.sort();
    host_write_paths.dedup();
    let requested = host_write_paths.clone();
    host_write_paths.retain(|path| {
        !requested
            .iter()
            .any(|parent| parent != path && path.starts_with(parent))
    });
    Ok(host_write_paths)
}

/// Blocking host scan for session_start: mount inventory, HOME overlay plan,
/// host socket exposure, and the host kdeglobals.
/// session_start choices that shape the host view.
#[derive(Clone, Copy)]
struct HostViewOptions {
    browser_profiles: BrowserProfiles,
    /// Bytes the disk-backed HOME overlay needs free before startup.
    disk_min_free: u64,
}

fn prepare_host_view(
    target: &Path,
    host_xdg_dir: &Path,
    host_runtime: &Path,
    writable_paths: &[PathBuf],
    host_write_paths: &[PathBuf],
    service: &KwinMcp,
    options: HostViewOptions,
) -> anyhow::Result<HostView> {
    let HostViewOptions {
        browser_profiles,
        disk_min_free,
    } = options;
    service.host_step("validating host_write_paths")?;
    let host_write_paths =
        validate_host_write_paths(target, host_runtime, writable_paths, host_write_paths)?;
    service.host_step("reading the host mount table (/proc/self/mountinfo)")?;
    test_block_host_scan();
    let mount_inventory = procfs::process::Process::myself()
        .and_then(|process| process.mountinfo())
        .map(|mounts| mounts.0)
        .map_err(|e| anyhow::anyhow!("read mount inventory: {e:#}"))?;
    let overlay_exclusions = mount_descendants(&mount_inventory, target);
    eprintln!(
        "session_start: mount inventory={} overlay-exclusions={}",
        mount_inventory.len(),
        overlay_exclusions.len()
    );
    for mount in &overlay_exclusions {
        eprintln!(
            "session_start: excluding mount from overlay: {}",
            mount.display()
        );
    }
    // A leftover twin (a crash, or this pid's run before a reboot) never leaks
    // into the new session.
    let disk = session_disk_path(host_xdg_dir);
    service.host_step(format!(
        "clearing the previous disk overlay {}",
        disk.display()
    ))?;
    remove_verified_workdir(&disk).map_err(|e| anyhow::anyhow!("clear {}: {e}", disk.display()))?;
    std::fs::create_dir_all(&disk)
        .map_err(|e| anyhow::anyhow!("create {}: {e}", disk.display()))?;
    let stat = nix::sys::statvfs::statvfs(&disk)
        .map_err(|e| anyhow::anyhow!("statvfs {}: {e}", disk.display()))?;
    let free = stat.blocks_available().saturating_mul(stat.fragment_size());
    if free < disk_min_free {
        let _ = std::fs::remove_dir(&disk);
        anyhow::bail!("{}", overlay_floor_message(&disk, disk_min_free, free));
    }
    let scripts = disk.join(kwin_script::SCRIPT_DIRECTORY);
    std::fs::create_dir(&scripts)
        .map_err(|e| anyhow::anyhow!("create KWin script storage {}: {e}", scripts.display()))?;
    service.host_step("reading host processes (/proc/<pid>)")?;
    let inventory = host_process_inventory(target, service)?;
    service.host_step(format!(
        "preparing HOME overlays under {}",
        target.display()
    ))?;
    let mut overlay_plan = prepare_overlay_plan(
        target,
        &disk,
        &mount_inventory,
        writable_paths,
        &host_write_paths,
        &inventory.live_databases,
    )
    .map_err(|e| anyhow::anyhow!("prepare overlays: {e:#}"))?;
    // Inside the overlay the layers' own path would loop (ELOOP); the session
    // sees an empty directory there instead.
    let disk_root = session_disk_root();
    if disk_root.starts_with(target) {
        overlay_plan.empty_dirs.push(disk_root);
    }
    service.host_step("reading host unix sockets (/proc/net/unix)")?;
    overlay_plan
        .expose_sockets(target, host_runtime, host_xdg_dir, &inventory, service)
        .map_err(|e| anyhow::anyhow!("expose host sockets: {e:#}"))?;
    service.host_step("preparing live SQLite access modes")?;
    if matches!(browser_profiles, BrowserProfiles::Fresh) {
        // Mask the profile roots before alias mounts are constructed, too.
        overlay_plan
            .empty_dirs
            .extend(BROWSER_CONFIG_DIRS.iter().map(|config| target.join(config)));
    }
    bind_live_sqlite(
        &mut overlay_plan,
        target,
        &inventory.live_databases,
        writable_paths,
        service,
        browser_profiles,
    )?;

    eprintln!(
        "session_start: overlay plan={} overlays={} read-only-mounts={}",
        if overlay_plan.staging_root.is_some() {
            "split"
        } else {
            "whole"
        },
        overlay_plan.overlays.len(),
        overlay_plan.read_only_binds.len()
    );
    let (cookie_roots, cookie_errors) = match browser_profiles {
        BrowserProfiles::Inherit => {
            service.set_start_stage("preparing browser profiles");
            let discarded = discarded_session_databases(target, service)?;
            empty_session_databases(&overlay_plan, &discarded, service)?;
            let cookies = prepare_browser_cookies(
                &mut overlay_plan,
                target,
                &disk.join("cookie-baselines"),
                service,
            )?;
            let marked = mark_browser_exits_clean(&overlay_plan, target, service)?;
            hide_browser_sessions(&mut overlay_plan, target, service)?;
            eprintln!("session_start: marked {marked} browser profile(s) clean");
            cookies
        }
        BrowserProfiles::Fresh => (Vec::new(), Vec::new()),
    };
    let browser_cache = target.join(BROWSER_CACHE_DIR);
    if browser_cache.is_dir() {
        overlay_plan.empty_dirs.push(browser_cache);
    }
    service.host_step("reading desktop settings")?;
    let kdeglobals = std::fs::read_to_string(target.join(".config/kdeglobals")).unwrap_or_default();
    Ok(HostView {
        overlay_plan,
        kdeglobals,
        cookie_roots,
        cookie_errors,
    })
}

// ── Session ──────────────────────────────────────────────────────────────

/// A child of this same PID, including children retained across exec.
fn input_sysname(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

struct SessionProcess {
    pid: u32,
    status: Option<std::process::ExitStatus>,
}
impl From<std::process::Child> for SessionProcess {
    fn from(child: std::process::Child) -> Self {
        Self {
            pid: child.id(),
            status: None,
        }
    }
}
impl SessionProcess {
    fn id(&self) -> u32 {
        self.pid
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if self.status.is_some() {
            return Ok(self.status);
        }
        let pid =
            nix::unistd::Pid::from_raw(i32::try_from(self.pid).map_err(std::io::Error::other)?);
        let mut status = 0;
        let waited = unsafe { nix::libc::waitpid(pid.as_raw(), &mut status, nix::libc::WNOHANG) };
        if waited < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if waited == 0 {
            return Ok(None);
        }
        self.status = Some(std::process::ExitStatus::from_raw(status));
        Ok(self.status)
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn owned_session_scope(unit: &str) -> bool {
    (unit.starts_with("kwin-mcp-") || unit.starts_with("run-p"))
        && unit.ends_with(".scope")
        && unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn stop_session_scope(unit: &str) {
    if unit.is_empty() {
        return;
    }
    if !owned_session_scope(unit) {
        eprintln!("cleanup: invalid session scope name; retaining process-group fallback");
        return;
    }
    let environment = (|| -> anyhow::Result<_> {
        let runtime = host_environment::runtime_dir()?;
        let bus = host_environment::session_bus_address(&runtime)?;
        Ok((runtime, bus))
    })();
    let (runtime, bus) = match environment {
        Ok(values) => values,
        Err(error) => {
            eprintln!("cleanup: cannot contact session scope manager: {error:#}");
            return;
        }
    };
    let run = |args: &[&str]| {
        std::process::Command::new("timeout")
            .args(["3", "systemctl", "--user"])
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("DBUS_SESSION_BUS_ADDRESS", &bus)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !run(&["stop", unit]) {
        eprintln!("cleanup: scope {unit} did not stop promptly; killing its members");
        let _ = run(&["kill", "--kill-whom=all", "--signal=KILL", unit]);
    }
    let _ = run(&["reset-failed", unit]);
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SessionScope {
    unit: String,
    memory_high: Option<u64>,
    memory_max: u64,
    memory_swap_max: u64,
}

const SCOPE_DIAGNOSTIC_TIMEOUT_SECONDS: &str = "2";

impl SessionScope {
    /// Older reload records had no scope fields. Read the sandbox's actual
    /// kernel cgroup instead of substituting the new server's defaults.
    fn recover(mut self, pid: u32) -> Self {
        if !self.unit.is_empty() && self.memory_max != 0 {
            return self;
        }
        let group = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .and_then(|groups| {
                groups
                    .lines()
                    .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
            });
        let Some(group) = group else {
            return self;
        };
        let Some(unit) = Path::new(&group).file_name().and_then(|name| name.to_str()) else {
            return self;
        };
        // These are the generated sandbox scopes from the old and new
        // launchers. Never adopt the surrounding host desktop's app scope.
        if !owned_session_scope(unit) {
            return self;
        }
        self.unit = unit.to_owned();
        let directory = Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
        let read = |name| std::fs::read_to_string(directory.join(name)).ok();
        let (Some(high), Some(max), Some(swap)) = (
            read("memory.high"),
            read("memory.max"),
            read("memory.swap.max"),
        ) else {
            return self;
        };
        let (Ok(max), Ok(swap)) = (max.trim().parse::<u64>(), swap.trim().parse::<u64>()) else {
            return self;
        };
        let high = match high.trim() {
            "max" => None,
            value => match value.parse::<u64>() {
                Ok(bytes) => Some(bytes),
                Err(_) => return self,
            },
        };
        self.memory_high = high;
        self.memory_max = max;
        self.memory_swap_max = swap;
        self
    }

    fn report(&self) -> serde_json::Value {
        let known = self.memory_max != 0;
        serde_json::json!({"unit":(!self.unit.is_empty()).then_some(&self.unit),
            "memory_high":if known { self.memory_high } else { None },
            "memory_max":known.then_some(self.memory_max),
            "memory_swap_max":known.then_some(self.memory_swap_max), "limits_known":known})
    }

    fn new(path: &Path, config: DisplayConfig) -> Self {
        Self {
            unit: format!(
                "{}.scope",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            memory_high: config.memory_high,
            memory_max: config.memory_max,
            memory_swap_max: config.memory_swap_max,
        }
    }

    fn diagnose(&self, status: std::process::ExitStatus) -> serde_json::Value {
        let output = (!self.unit.is_empty())
            .then(|| {
                let runtime = host_environment::runtime_dir().ok()?;
                let bus = host_environment::session_bus_address(&runtime).ok()?;
                Some(
                    std::process::Command::new("timeout")
                        .env("XDG_RUNTIME_DIR", runtime)
                        .env("DBUS_SESSION_BUS_ADDRESS", bus)
                        .args([
                            SCOPE_DIAGNOSTIC_TIMEOUT_SECONDS,
                            "systemctl",
                            "--user",
                            "show",
                            &self.unit,
                            "-p",
                            "Result",
                            "-p",
                            "MemoryPeak",
                            "-p",
                            "MemoryCurrent",
                            "-p",
                            "ControlGroup",
                        ])
                        .output(),
                )
            })
            .flatten()
            .and_then(Result::ok);
        let properties: std::collections::BTreeMap<String, String> = output
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        line.split_once('=')
                            .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let events = properties
            .get("ControlGroup")
            .filter(|path| !path.is_empty())
            .and_then(|path| {
                std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.events")).ok()
            });
        let oom = properties
            .get("Result")
            .is_some_and(|result| result == "oom-kill")
            || events.as_ref().is_some_and(|events| {
                events.lines().any(|line| {
                    line.split_once(' ')
                        .is_some_and(|(key, value)| key == "oom_kill" && value != "0")
                })
            });
        serde_json::json!({
            "status": "dead", "reason": if oom { "oom_kill" } else { "sandbox_exited" },
            "exit_status": status.to_string(),
            "memory_high_bytes": if self.memory_max != 0 { self.memory_high } else { None },
            "memory_max_bytes": (self.memory_max != 0).then_some(self.memory_max),
            "memory_swap_max_bytes": (self.memory_max != 0).then_some(self.memory_swap_max),
            "scope": (!self.unit.is_empty()).then_some(&self.unit), "scope_properties": properties,
            "memory_events": events,
            "resume_guidance": "Call session_stop for this handle, then session_start without session_id to create a new isolated desktop. Relaunch apps and restore your task; the old desktop cannot be resumed."
        })
    }

    fn stop(&self) {
        stop_session_scope(&self.unit);
    }

    fn memory_snapshot(&self, pid: u32) -> Option<SessionMemorySnapshot> {
        let directory = session_scope_cgroup_directory(pid, &self.unit)?;
        let current = std::fs::read_to_string(directory.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let high = read_cgroup_limit(&directory, "memory.high");
        let swap_current = std::fs::read_to_string(directory.join("memory.swap.current"))
            .ok()
            .and_then(|value| value.trim().parse().ok());
        let swap_max = read_cgroup_limit(&directory, "memory.swap.max");
        let pressure_full_avg10 = std::fs::read_to_string(directory.join("memory.pressure"))
            .ok()
            .and_then(|pressure| parse_pressure_avg10(&pressure, "full"));
        let high_events = std::fs::read_to_string(directory.join("memory.events"))
            .ok()
            .and_then(|events| parse_memory_event(&events, "high"));
        Some(SessionMemorySnapshot {
            current,
            high,
            swap_current,
            swap_max,
            pressure_full_avg10,
            high_events,
        })
    }
}

const SESSION_MEMORY_PSI_WARNING_PERCENT: f64 = 10.0;
const SESSION_MEMORY_SWAP_NEAR_FULL_PERCENT: u32 = 99;
const BYTES_PER_GIB: u64 = 1_073_741_824;

/// Limits are fixed when a session starts, so the way out is a new session.
const MEMORY_RAISE_HINT: &str = "limits cannot change in a running session; stop it and call session_start with a larger memory_high, memory_max and memory_swap_max (whole GiB), or start the server with --memory-high, --memory-max and --memory-swap-max";

#[derive(Clone, Copy)]
struct SessionMemorySnapshot {
    current: u64,
    high: Option<u64>,
    swap_current: Option<u64>,
    swap_max: Option<u64>,
    pressure_full_avg10: Option<f64>,
    high_events: Option<u64>,
}

impl SessionMemorySnapshot {
    fn high_reached(self) -> bool {
        self.high
            .is_some_and(|limit| limit > 0 && self.current >= limit)
    }

    fn full_pressure_high(self) -> bool {
        self.pressure_full_avg10
            .is_some_and(|percent| percent >= SESSION_MEMORY_PSI_WARNING_PERCENT)
    }

    fn as_json(self) -> serde_json::Value {
        serde_json::json!({
            "current_bytes": self.current,
            "high_bytes": self.high,
            "swap_current_bytes": self.swap_current,
            "swap_max_bytes": self.swap_max,
            "pressure_full_avg10_percent": self.pressure_full_avg10,
            "high_events": self.high_events,
        })
    }
}

/// A failed call carries the session's memory state even when no limit was
/// reached, so a timeout can be told apart from memory pressure.
fn attach_memory_snapshot_to_failure(
    result: Result<CallToolResult, McpError>,
    snapshot: SessionMemorySnapshot,
) -> Result<CallToolResult, McpError> {
    match result {
        Err(mut error) => {
            let mut data = match error.data.take() {
                Some(serde_json::Value::Object(data)) => data,
                Some(value) => serde_json::Map::from_iter([("original_data".to_owned(), value)]),
                None => serde_json::Map::new(),
            };
            if !data.contains_key("memory_warning") {
                data.insert("memory".to_owned(), snapshot.as_json());
            }
            error.data = Some(serde_json::Value::Object(data));
            Err(error)
        }
        Ok(mut result) => {
            if result.is_error == Some(true)
                && let Some(data) = result
                    .structured_content
                    .as_mut()
                    .and_then(serde_json::Value::as_object_mut)
                && !data.contains_key("memory_warning")
            {
                data.insert("memory".to_owned(), snapshot.as_json());
            }
            Ok(result)
        }
    }
}

struct SessionMemoryWarning {
    message: String,
    reason: &'static str,
    memory_current_bytes: u64,
    memory_high_bytes: Option<u64>,
    memory_swap_current_bytes: Option<u64>,
    memory_swap_max_bytes: Option<u64>,
    memory_pressure_full_avg10_percent: Option<f64>,
    memory_high_events_delta: Option<u64>,
}

impl SessionMemoryWarning {
    fn from_snapshots(
        before: Option<SessionMemorySnapshot>,
        after: Option<SessionMemorySnapshot>,
    ) -> Option<Self> {
        let latest = after.or(before)?;
        let high_events_delta = before
            .and_then(|before| before.high_events)
            .zip(latest.high_events)
            .and_then(|(before, after)| after.checked_sub(before))
            .filter(|delta| *delta > 0);
        let high_reached_before = before.is_some_and(SessionMemorySnapshot::high_reached);
        let throttled_now = latest.high_reached();
        let pressure_high_now = latest.full_pressure_high();
        let pressure_high_before = before.is_some_and(SessionMemorySnapshot::full_pressure_high);
        let full_pressure_high = pressure_high_now || pressure_high_before;
        let throttled = throttled_now || high_reached_before || high_events_delta.is_some();
        if !throttled && !full_pressure_high {
            return None;
        }
        let reported = if throttled_now || pressure_high_now {
            latest
        } else if high_reached_before || pressure_high_before {
            before.unwrap_or(latest)
        } else {
            latest
        };

        let mut details = vec![match reported.high {
            Some(high) => format!(
                "RAM {}/{} GiB",
                format_gib(reported.current),
                format_gib(high)
            ),
            None => format!("RAM {} GiB", format_gib(reported.current)),
        }];
        if let (Some(current), Some(max)) = (reported.swap_current, reported.swap_max) {
            let near_full = max > 0
                && u128::from(current) * 100
                    >= u128::from(max) * u128::from(SESSION_MEMORY_SWAP_NEAR_FULL_PERCENT);
            details.push(format!(
                "swap {}/{} GiB{}",
                format_gib(current),
                format_gib(max),
                if current >= max {
                    " full"
                } else if near_full {
                    " nearly full"
                } else {
                    ""
                }
            ));
        }
        if let Some(percent) = reported.pressure_full_avg10 {
            details.push(format!("memory PSI full avg10={percent:.2}%"));
        }
        if let Some(delta) = high_events_delta {
            details.push(format!("memory.high events +{delta} during this call"));
        }

        let message = if throttled_now {
            "warning: session memory throttled"
        } else if high_events_delta.is_some() {
            "warning: session memory was throttled during this call"
        } else if pressure_high_now {
            "warning: session memory pressure is high"
        } else if high_reached_before {
            "warning: session memory was at memory.high when this call started"
        } else {
            "warning: session memory pressure was high when this call started"
        };

        Some(Self {
            message: format!("{message} ({}); {MEMORY_RAISE_HINT}", details.join(", ")),
            reason: if throttled {
                "memory_high"
            } else {
                "memory_pressure_full"
            },
            memory_current_bytes: reported.current,
            memory_high_bytes: reported.high,
            memory_swap_current_bytes: reported.swap_current,
            memory_swap_max_bytes: reported.swap_max,
            memory_pressure_full_avg10_percent: reported.pressure_full_avg10,
            memory_high_events_delta: high_events_delta,
        })
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "reason": self.reason,
            "message": self.message,
            "memory_current_bytes": self.memory_current_bytes,
            "memory_high_bytes": self.memory_high_bytes,
            "memory_swap_current_bytes": self.memory_swap_current_bytes,
            "memory_swap_max_bytes": self.memory_swap_max_bytes,
            "memory_pressure_full_avg10_percent": self.memory_pressure_full_avg10_percent,
            "memory_high_events_delta": self.memory_high_events_delta,
        })
    }
}

fn screenshot_timeout_error(message: &str) -> bool {
    message.contains("compositor did not finish a frame within 10 seconds")
        || message.starts_with("screenshot: capture exceeded ")
}

fn attach_memory_warning_to_result(
    result: Result<CallToolResult, McpError>,
    warning: SessionMemoryWarning,
    screenshot: bool,
) -> Result<CallToolResult, McpError> {
    match result {
        Ok(mut result) => {
            result.content.push(Content::text(warning.message.clone()));
            if let Some(data) = result
                .structured_content
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                data.insert("memory_warning".to_owned(), warning.as_json());
            }
            Ok(result)
        }
        Err(mut error) => {
            let mut data = match error.data.take() {
                Some(serde_json::Value::Object(data)) => data,
                Some(value) => serde_json::Map::from_iter([("original_data".to_owned(), value)]),
                None => serde_json::Map::new(),
            };
            data.insert("memory_warning".to_owned(), warning.as_json());
            let message = if screenshot && screenshot_timeout_error(&error.message) {
                format!(
                    "{}; likely cause: {}",
                    error.message,
                    warning
                        .message
                        .strip_prefix("warning: ")
                        .unwrap_or(&warning.message)
                )
            } else {
                format!("{}; {}", error.message, warning.message)
            };
            error.message = message.into();
            error.data = Some(serde_json::Value::Object(data));
            Err(error)
        }
    }
}

fn session_scope_cgroup_directory(pid: u32, unit: &str) -> Option<PathBuf> {
    if !owned_session_scope(unit) {
        return None;
    }
    let groups = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let group = groups.lines().find_map(|line| line.strip_prefix("0::"))?;
    let relative = group.strip_prefix('/')?;
    let mut directory = PathBuf::from("/sys/fs/cgroup");
    for component in relative.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        directory.push(component);
        if component == unit {
            return Some(directory);
        }
    }
    None
}

fn read_cgroup_limit(directory: &Path, name: &str) -> Option<u64> {
    let value = std::fs::read_to_string(directory.join(name)).ok()?;
    value.trim().parse().ok()
}

fn parse_pressure_avg10(pressure: &str, kind: &str) -> Option<f64> {
    let line = pressure
        .lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?;
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()?;
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}

fn parse_memory_event(events: &str, name: &str) -> Option<u64> {
    events.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let event = fields.next()?;
        let value = fields.next()?;
        (event == name).then_some(value.parse().ok()).flatten()
    })
}

fn format_gib(bytes: u64) -> String {
    let hundredths =
        (u128::from(bytes) * 100 + u128::from(BYTES_PER_GIB) / 2) / u128::from(BYTES_PER_GIB);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

struct Session {
    host_write_paths: Vec<PathBuf>,
    scope: SessionScope,
    kwin_conn: zbus::Connection, // talks to KWin via its unique name
    _proxy_socket: std::os::fd::OwnedFd,
    _proxy_conn: zbus::Connection, // owns org.kde.KWin, has InputDevice objects (kept alive)
    kwin_unique_name: String,
    service_bus_address: String,
    atspi_bus_address: String,
    eis: Eis,
    sandbox_child: SessionProcess,
    sandbox_stdin: std::os::fd::OwnedFd,
    host_xdg_dir: std::path::PathBuf,
    _uinput_mouse: std::os::fd::OwnedFd,
    _uinput_keyboard: std::os::fd::OwnedFd,
    mouse_sysname: String,
    kbd_sysname: String,
    cdp_browser: Option<Arc<chromiumoxide::Browser>>,
    cdp_forward_port: u16,
    service_proxy_children: Vec<SessionProcess>,
    viewer_child: Option<SessionProcess>,
    /// Why a requested viewer could not start when viewer_child is None.
    viewer_unavailable: Option<String>,
    overlay_work_paths: Vec<PathBuf>,
    cookie_roots: Vec<kwin_mcp::browser_cookies::CookieRoot>,
    cookie_errors: Vec<String>,
    oversized_read_only_files: Vec<PathBuf>,
    _socket_links: SocketLinks,
    screen_width: u32,
    screen_height: u32,
    last_activity: std::time::Instant,
    /// When an input tool last sent events, for screenshot settling.
    last_input: Option<std::time::Instant>,
    /// Mediator between session apps and the host KWallet.
    wallet: Option<wallet_mediator::WalletMediator>,
    /// Why the session has no wallet (host wallet unavailable, locked, ...), or
    /// None when the session-local wallet holds the host copy. launch_app tells
    /// the agent when a browser starts without it.
    wallet_note: Option<String>,
    /// Whether the sandbox has the FUSE bridge; without it launch_app runs
    /// AppImages in extract-and-run mode.
    fuse_enabled: bool,
}

// ── Server ───────────────────────────────────────────────────────────────

/// Virtual display size policy resolved from CLI flags at server launch.
/// `width`/`height` are the session default (compiled constants unless
/// --width/--height overrode them); `locked` (--no-override) makes
/// session_start ignore its optional width/height params entirely.
#[derive(Clone, Copy)]
struct DisplayConfig {
    width: u32,
    height: u32,
    locked: bool,
    autoclean: bool,
    ttl: Option<Duration>,
    /// After the MCP client closes, keep serving running sessions to other
    /// kwin-mcp processes until they have been idle this long; None shuts them
    /// down with the client.
    keep_sessions: Option<Duration>,
    /// cgroup MemoryHigh for each session's sandbox in bytes; None disables throttling.
    memory_high: Option<u64>,
    memory_max: u64,
    memory_swap_max: u64,
}

const STARTUP_CHILD_TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn signal_child(child_id: u32, process_group: bool, signal: nix::sys::signal::Signal) {
    let Ok(pid) = i32::try_from(child_id) else {
        return;
    };
    let target = if process_group { -pid } else { pid };
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(target), signal);
}

/// Terminate a startup or session child without allowing a TERM-resistant
/// process to hold the lifecycle gate forever. The final wait reaps the child
/// after the bounded TERM grace period and SIGKILL escalation.
fn terminate_child(child: impl Into<SessionProcess>, process_group: bool, label: &str) {
    let mut child = child.into();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    signal_child(child.id(), process_group, nix::sys::signal::Signal::SIGTERM);
    let deadline = std::time::Instant::now() + STARTUP_CHILD_TERMINATION_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) | Err(_) => break,
        }
    }
    eprintln!("{label} did not exit after SIGTERM; escalating to SIGKILL");
    signal_child(child.id(), process_group, nix::sys::signal::Signal::SIGKILL);
    let deadline = std::time::Instant::now() + STARTUP_CHILD_TERMINATION_GRACE;
    while std::time::Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // SIGKILL is pending, but the child is blocked in the kernel (D state).
    // Reap it off the lifecycle path so stop and start stay bounded.
    eprintln!("{label} still blocked after SIGKILL; reaping in the background");
    let _ = std::thread::Builder::new()
        .name("kwin-mcp-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// Resources created while session_start is still unpublished. The guard owns
/// them until the Session takes them, so cancelling the startup future cannot
/// detach a process or task that has no reachable cleanup path.
struct StartupResources {
    scope_unit: Option<String>,
    sandbox_child: Option<std::process::Child>,
    sandbox_stdin: Option<std::process::ChildStdin>,
    service_proxy_children: Vec<std::process::Child>,
    /// Disk-backed image staged for the FUSE helper, until Session owns the
    /// whole disk twin or failed startup cleanup removes the image.
    fuse_bridge_exe: Option<PathBuf>,
}

impl StartupResources {
    fn new() -> Self {
        Self {
            scope_unit: None,
            sandbox_child: None,
            sandbox_stdin: None,
            service_proxy_children: Vec::new(),
            fuse_bridge_exe: None,
        }
    }

    fn add_proxy(&mut self, child: std::process::Child) {
        self.service_proxy_children.push(child);
    }

    fn cleanup(&mut self) {
        drop(self.sandbox_stdin.take());
        if let Some(unit) = self.scope_unit.take() {
            stop_session_scope(&unit);
        }
        if let Some(sandbox) = self.sandbox_child.take() {
            terminate_child(sandbox, true, "startup sandbox");
        }
        for proxy in self.service_proxy_children.drain(..) {
            terminate_child(proxy, false, "startup D-Bus proxy");
        }
        if let Some(exe) = self.fuse_bridge_exe.take()
            && let Err(error) = std::fs::remove_file(&exe)
        {
            eprintln!(
                "startup: could not remove staged FUSE bridge {}: {error}",
                exe.display()
            );
        }
    }

    fn into_parts(
        mut self,
    ) -> (
        std::process::Child,
        std::process::ChildStdin,
        Vec<std::process::Child>,
    ) {
        self.scope_unit.take();
        self.fuse_bridge_exe.take();
        let Some(sandbox_child) = self.sandbox_child.take() else {
            panic!("startup sandbox child missing at Session publication");
        };
        let Some(sandbox_stdin) = self.sandbox_stdin.take() else {
            panic!("startup sandbox stdin missing at Session publication");
        };
        (
            sandbox_child,
            sandbox_stdin,
            std::mem::take(&mut self.service_proxy_children),
        )
    }
}

impl Drop for StartupResources {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Debug-only pause used by the production-stdio cancellation regression. It
/// creates a real cancellation point after a selected startup resource exists,
/// allowing the test to prove the owning guards clean it before workdir removal.
async fn test_startup_delay(stage: &str) {
    if !cfg!(debug_assertions)
        || std::env::var("KWIN_MCP_TEST_STARTUP_DELAY_STAGE")
            .ok()
            .as_deref()
            != Some(stage)
    {
        return;
    }
    let millis = std::env::var("KWIN_MCP_TEST_STARTUP_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000);
    if millis == 0 {
        return;
    }
    eprintln!("session_start: test delay at {stage} for {millis}ms");
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

/// Debug-only stand-in for a host scan blocked in the kernel, such as a stat
/// on a hung FUSE mount: a thread sleep that no async timeout can preempt.
fn test_block_host_scan() {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(millis) = std::env::var("KWIN_MCP_TEST_BLOCK_HOST_SCAN_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    eprintln!("session_start: test host scan blocked for {millis}ms");
    std::thread::sleep(std::time::Duration::from_millis(millis));
}

fn test_stop_bwrap(child: &std::process::Child) {
    if !cfg!(debug_assertions)
        || std::env::var("KWIN_MCP_TEST_STOP_BWRAP").ok().as_deref() != Some("1")
    {
        return;
    }
    signal_child(child.id(), false, nix::sys::signal::Signal::SIGSTOP);
    eprintln!("session_start: test stopped bwrap pid={}", child.id());
}

#[derive(Clone)]
struct KwinMcp {
    death: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    clipboards: Arc<std::sync::Mutex<ClipboardStore>>,
    session_id: String,
    input_log: Option<Arc<InputLog>>,
    path: PathBuf,
    input_gate: Arc<tokio::sync::Mutex<()>>,
    input_order: Arc<InputOrder>,
    session: Arc<tokio::sync::RwLock<Option<Session>>>,
    /// Cleanup ownership of the session workdir under --autoclean. Claimed
    /// before session_start creates the directory and released only once the
    /// directory is gone, so every terminal start, stop, and shutdown outcome
    /// either removes it or leaves it owned with a reachable retry.
    workdir: Arc<WorkdirOwnership>,
    /// Held for the whole of session_start, which is the only writer that
    /// populates the workdir. Shutdown takes it before its terminal cleanup, so
    /// a start still running when the transport closes cannot repopulate a
    /// directory that nothing is left to delete.
    start_gate: Arc<tokio::sync::Mutex<()>>,
    /// Last startup checkpoint reached, named in the hard-limit error, with
    /// the moment it was reached so a blocked call's age can be reported.
    start_stage: Arc<std::sync::Mutex<StartStage>>,
    startup_deadline: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Workdir of a timed-out start whose blocked host call still owns the
    /// lifecycle gate. Set when that cleanup is deferred and cleared just
    /// before the gate is released.
    pending_cleanup: Arc<std::sync::Mutex<Option<PathBuf>>>,
    display: DisplayConfig,
}

/// Bound on each session lock the reaper waits for, so one stuck handle cannot
/// stall expiry for the others.
const REAPER_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The terminal status a death record carries. A desktop diagnosis has only `status`.
fn terminal_status(record: &serde_json::Value) -> Option<&str> {
    record["session_status"]
        .as_str()
        .or_else(|| record["status"].as_str())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The death record of a handle that ended without a running desktop.
fn terminal_record(status: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "status": status,
        "session_status": status,
        "reason": reason,
        "session_available_here": false,
        "session_ended_at_unix_seconds": unix_now(),
    })
}

/// The most recent startup checkpoint and when it began.
#[derive(Clone)]
struct StartStage {
    label: String,
    since: std::time::Instant,
}

impl KwinMcp {
    fn death_record(&self) -> Option<serde_json::Value> {
        self.death
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_death(&self, diagnostic: serde_json::Value) {
        *self
            .death
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(diagnostic);
    }

    /// The handle's registry phase. The desktop is read before the terminal
    /// status: a stop, expiry, or death records its status before it releases the
    /// desktop, so a missing desktop always comes with its status.
    async fn registry_phase(&self) -> session_registry::Phase {
        let has_desktop = self.session.read().await.is_some();
        session_registry::phase(
            has_desktop,
            self.death_record().as_ref().and_then(terminal_status),
        )
    }

    /// Records how a failed session_start left its handle, after the start's own
    /// cleanup has run, and returns what the registry keeps.
    fn register_failed_start(&self, error: &McpError) -> session_registry::FailedStart {
        let host_call_blocked = error
            .data
            .as_ref()
            .and_then(|data| data.get("host_call_blocked"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let outcome = session_registry::failed_start(
            error.code == rmcp::model::ErrorCode::INVALID_PARAMS,
            host_call_blocked,
        );
        // A stop that arrived first already recorded the handle's end.
        if outcome == session_registry::FailedStart::Forget || self.death_record().is_some() {
            return outcome;
        }
        match outcome {
            session_registry::FailedStart::Blocked => self.set_death(terminal_record(
                session_registry::STATUS_START_BLOCKED,
                SESSION_LIFECYCLE_REASON_HOST_CALL_BLOCKED,
            )),
            session_registry::FailedStart::Failed => {
                self.set_death(terminal_record(
                    session_registry::STATUS_START_FAILED,
                    SESSION_LIFECYCLE_REASON_START_FAILED,
                ));
                self.record_terminal_cleanup(self.startup_cleanup_label());
            }
            session_registry::FailedStart::Forget => {}
        }
        outcome
    }

    /// The workdir cleanup a start's record names: retained while this server
    /// still owns the workdir, removed when autoclean removed it, otherwise not owned.
    fn startup_cleanup_label(&self) -> &'static str {
        if self.workdir.owned().is_some() {
            "retained"
        } else if self.display.autoclean && !self.path.exists() {
            "removed"
        } else if self.path.exists() {
            "not_owned"
        } else {
            "not_requested"
        }
    }

    /// Expires this handle once its desktop has been idle for the whole TTL, and
    /// records the expiry.
    async fn expire_if_idle(&self) {
        let _ = tokio::time::timeout(REAPER_LOCK_WAIT, self.check_liveness()).await;
        let Ok(_gate) = self.start_gate.try_lock() else {
            return;
        };
        let Ok(mut guard) = tokio::time::timeout(REAPER_LOCK_WAIT, self.session.write()).await
        else {
            eprintln!("ttl: {} is busy; expiry is retried", self.session_id);
            return;
        };
        if let Some(child) = guard
            .as_mut()
            .and_then(|session| session.viewer_child.as_mut())
        {
            let _ = child.try_wait();
        }
        let idle = guard
            .as_ref()
            .map_or(Duration::ZERO, |session| session.last_activity.elapsed());
        let phase = session_registry::phase(
            guard.is_some(),
            self.death_record().as_ref().and_then(terminal_status),
        );
        if !session_registry::idle_expired(&phase, idle, self.display.ttl) {
            return;
        }
        let expired_at = unix_now();
        // The expired status is recorded before the desktop is released.
        self.set_death(serde_json::json!({
            "status": "expired",
            "session_status": "expired",
            "reason": SESSION_LIFECYCLE_REASON_IDLE_TIMEOUT,
            "session_expired_at_unix_seconds": expired_at,
            "session_ended_at_unix_seconds": expired_at,
            "session_available_here": false,
        }));
        let stopped = guard.take();
        drop(guard);
        if let Some(session) = stopped {
            teardown_blocking(session).await;
        }
        let workdir_cleanup = match self.workdir.remove() {
            WorkdirCleanup::Retained { dir, error } => {
                eprintln!("ttl: retained {}: {error}", dir.display());
                "retained"
            }
            WorkdirCleanup::NothingOwned => "not_owned",
            WorkdirCleanup::Removed(_) => "removed",
        };
        self.record_lifecycle(
            SESSION_LIFECYCLE_STATE_SESSION_EXPIRED,
            SESSION_LIFECYCLE_REASON_IDLE_TIMEOUT,
            workdir_cleanup,
        );
    }

    fn record_lifecycle(&self, state: &str, reason: &str, workdir_cleanup: &str) {
        let ended_at = self
            .death_record()
            .and_then(|record| record["session_ended_at_unix_seconds"].as_u64());
        match append_session_lifecycle_event(
            &self.session_id,
            state,
            reason,
            workdir_cleanup,
            ended_at,
        ) {
            Ok(()) => eprintln!(
                "session lifecycle: session_id={} state={state} reason={reason}",
                self.session_id
            ),
            Err(error) => eprintln!(
                "session lifecycle: could not record {state} for {}: {error}",
                self.session_id
            ),
        }
    }

    fn record_terminal_cleanup(&self, cleanup: &str) {
        let Some(terminal) = self.death_record() else {
            return;
        };
        let transition = match terminal_status(&terminal) {
            Some(session_registry::STATUS_STOPPED) => (
                SESSION_LIFECYCLE_STATE_SESSION_STOPPED,
                SESSION_LIFECYCLE_REASON_SESSION_STOP,
            ),
            Some(session_registry::STATUS_EXPIRED) => (
                SESSION_LIFECYCLE_STATE_SESSION_EXPIRED,
                SESSION_LIFECYCLE_REASON_IDLE_TIMEOUT,
            ),
            Some(session_registry::STATUS_DEAD) => (
                SESSION_LIFECYCLE_STATE_SESSION_DIED,
                SESSION_LIFECYCLE_REASON_SANDBOX_EXITED,
            ),
            Some(session_registry::STATUS_START_FAILED) => (
                SESSION_LIFECYCLE_STATE_START_FAILED,
                SESSION_LIFECYCLE_REASON_START_FAILED,
            ),
            Some(session_registry::STATUS_START_BLOCKED) => (
                SESSION_LIFECYCLE_STATE_START_ABANDONED,
                SESSION_LIFECYCLE_REASON_HOST_CALL_BLOCKED,
            ),
            _ => return,
        };
        self.record_lifecycle(transition.0, transition.1, cleanup);
    }

    async fn check_liveness(&self) -> Option<serde_json::Value> {
        if let Some(death) = self.death_record() {
            return Some(death);
        }
        let mut guard = self.session.write().await;
        let Some(session) = guard.as_mut() else {
            return self.death_record();
        };
        let status = match session.sandbox_child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return None,
            Err(error) => {
                eprintln!("session liveness: {}: {error}", self.session_id);
                return None;
            }
        };
        let scope = session.scope.clone();
        let diagnostic = tokio::task::spawn_blocking(move || scope.diagnose(status)).await
            .unwrap_or_else(|error| serde_json::json!({"status":"dead", "reason":"sandbox_exited", "diagnostic_error":error.to_string()}));
        self.set_death(diagnostic.clone());
        let stopped = guard.take();
        drop(guard);
        if let Some(session) = stopped {
            // The sandbox is already gone. No browser can flush to the
            // mediator, and this cleanup must survive cancellation of a tool
            // or of the reaper itself during transport shutdown.
            let _ = tokio::task::spawn_blocking(move || teardown(session)).await;
        }
        eprintln!("session died: {}: {diagnostic}", self.session_id);
        Some(diagnostic)
    }

    async fn refresh_viewer_child(&self) -> Result<(), String> {
        let mut guard = self.session.write().await;
        let Some(session) = guard.as_mut() else {
            return Ok(());
        };
        match session.viewer_child.as_mut().map(SessionProcess::try_wait) {
            None => Ok(()),
            Some(Ok(None)) => {
                let report =
                    viewer_report(&session.host_xdg_dir, session.viewer_child.as_mut(), None);
                if report["state"] == "unavailable" || report["input_activity_reporting"] != true {
                    return Err(format!(
                        "The open viewer cannot report current input activity: {}. Let the user's current interaction finish, then call viewer_open to refresh it before sending agent input.",
                        report["reason"]
                            .as_str()
                            .unwrap_or("viewer status is unavailable")
                    ));
                }
                Ok(())
            }
            Some(Ok(Some(_))) => {
                session.viewer_child.take();
                Ok(())
            }
            Some(Err(error)) => {
                let reason = format!(
                    "check viewer process {}: {error}",
                    session.viewer_child.as_ref().map_or(0, SessionProcess::id)
                );
                eprintln!("viewer liveness: {reason}");
                Err(reason)
            }
        }
    }

    async fn viewer_input_is_recent(&self) -> Result<bool, String> {
        // Do not queue another session read lock while an input operation
        // holds one: a waiting liveness writer would block the activity check
        // that must cancel that operation. The owned workdir path is stable.
        let activity_path = self.path.join(VIEWER_INPUT_ACTIVITY_FILE);
        let modified = match std::fs::metadata(&activity_path) {
            Ok(metadata) => metadata.modified().map_err(|error| {
                format!(
                    "read {} modification time: {error}",
                    activity_path.display()
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "read {} metadata: {error}",
                    activity_path.display()
                ));
            }
        };
        let age = match std::time::SystemTime::now().duration_since(modified) {
            Ok(age) => age,
            Err(error) => error.duration(),
        };
        Ok(age <= VIEWER_INPUT_QUIET_PERIOD)
    }

    async fn viewer_is_active_with_recent_input(&self) -> Result<bool, String> {
        // The child exists only for a viewer on this host. A remote viewer's
        // `--serve` relay writes the same activity file without a local child.
        // Always reap/check a local child when present, then use the file for
        // both local and remote input activity.
        self.refresh_viewer_child().await?;
        self.viewer_input_is_recent().await
    }
    fn new(display: DisplayConfig) -> Self {
        Self {
            death: Arc::new(std::sync::Mutex::new(None)),
            clipboards: Arc::new(std::sync::Mutex::new(ClipboardStore::default())),
            session_id: String::new(),
            input_log: None,
            session: Arc::new(tokio::sync::RwLock::new(None)),
            path: session_workdir_path(),
            input_gate: Arc::new(tokio::sync::Mutex::new(())),
            input_order: Arc::new(InputOrder::default()),
            workdir: Arc::new(WorkdirOwnership::default()),
            start_gate: Arc::new(tokio::sync::Mutex::new(())),
            start_stage: Arc::new(std::sync::Mutex::new(StartStage {
                label: "waiting for the lifecycle gate".to_owned(),
                since: std::time::Instant::now(),
            })),
            startup_deadline: Arc::new(std::sync::Mutex::new(None)),
            pending_cleanup: Arc::new(std::sync::Mutex::new(None)),
            display,
        }
    }
    /// Logs a startup checkpoint that runs once per start.
    fn set_start_stage(&self, stage: impl Into<String>) {
        let stage = stage.into();
        eprintln!("session_start: stage: {stage}");
        self.record_start_stage(stage);
    }
    /// Records a checkpoint without logging it, for per-process and per-socket
    /// steps that would otherwise flood the log.
    fn record_start_stage(&self, stage: impl Into<String>) {
        if let Ok(mut current) = self.start_stage.lock() {
            *current = StartStage {
                label: stage.into(),
                since: std::time::Instant::now(),
            };
        }
    }
    /// host call blocks, nothing moves the checkpoint, so this names the call.
    fn start_stage_report(&self) -> (String, std::time::Duration) {
        self.start_stage
            .lock()
            .map(|stage| (stage.label.clone(), stage.since.elapsed()))
            .unwrap_or_else(|_| ("unknown".to_owned(), std::time::Duration::ZERO))
    }
    /// Checkpoints a host step and refuses to run it once the startup budget is
    /// spent. A scan that is slow but not blocked then stops at the next step
    /// rather than finishing after its caller has already been answered.
    fn host_step(&self, stage: impl Into<String>) -> anyhow::Result<()> {
        let stage = stage.into();
        let elapsed = self
            .startup_deadline
            .lock()
            .ok()
            .and_then(|deadline| *deadline)
            .is_some_and(|deadline| std::time::Instant::now() >= deadline);
        anyhow::ensure!(!elapsed, "startup deadline elapsed before {stage}");
        self.set_start_stage(stage);
        Ok(())
    }
    fn profile_operation(&self, operation: &str, path: &Path) -> anyhow::Result<()> {
        self.host_step(format!(
            "preparing browser profiles: {operation} {}",
            path.display()
        ))
    }
    /// The workdir whose timed-out start still holds the lifecycle gate.
    fn pending_cleanup(&self) -> Option<PathBuf> {
        self.pending_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_pending_cleanup(&self, workdir: Option<PathBuf>) {
        *self
            .pending_cleanup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = workdir;
    }
    /// Wait for the lifecycle gate, bounded so a start, stop, or reaper stuck
    /// behind a blocked host call answers with an error instead of hanging.
    /// The refusal names the blocked call and the pending cleanup when a
    /// timed-out start is what holds the gate.
    async fn lifecycle_gate(
        &self,
        deadline: tokio::time::Instant,
        operation: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, McpError> {
        tokio::time::timeout_at(deadline, self.start_gate.clone().lock_owned())
            .await
            .map_err(|_| {
                let (stage, blocked_for) = self.start_stage_report();
                let detail = match self.pending_cleanup() {
                    Some(workdir) => format!(
                        "a previous start is still blocked: {}",
                        blocked_call_report(&stage, blocked_for, &workdir)
                    ),
                    None => format!(
                        "another session lifecycle operation is still running; its last startup stage is {stage}, current for {}s",
                        blocked_for.as_secs()
                    ),
                };
                lifecycle_busy_error(operation, &stage, &detail)
            })
    }
    async fn mark_input(&self) {
        if let Some(session) = self.session.write().await.as_mut() {
            session.last_input = Some(std::time::Instant::now());
        }
    }
    async fn last_input(&self) -> Option<std::time::Instant> {
        self.session
            .read()
            .await
            .as_ref()
            .and_then(|session| session.last_input)
    }
    async fn touch_activity(&self) {
        if let Some(session) = self.session.write().await.as_mut() {
            session.last_activity = std::time::Instant::now();
        }
    }
    async fn with_session<R>(
        &self,
        f: impl FnOnce(&Session) -> Result<R, McpError>,
    ) -> Result<R, McpError> {
        let guard = self.session.read().await;
        match &*guard {
            Some(s) => f(s),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    async fn session_memory_snapshot(&self) -> Option<SessionMemorySnapshot> {
        let guard = self.session.read().await;
        let session = guard.as_ref()?;
        session.scope.memory_snapshot(session.sandbox_child.id())
    }
    async fn attach_memory_warning(
        &self,
        result: Result<CallToolResult, McpError>,
        before: Option<SessionMemorySnapshot>,
        screenshot: bool,
    ) -> Result<CallToolResult, McpError> {
        let Some(warning) =
            SessionMemoryWarning::from_snapshots(before, self.session_memory_snapshot().await)
        else {
            return result;
        };
        attach_memory_warning_to_result(result, warning, screenshot)
    }
    async fn kwin_conn(&self) -> Result<zbus::Connection, McpError> {
        let guard = self.session.read().await;
        match &*guard {
            Some(s) => Ok(s.kwin_conn.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    async fn kwin_unique_name(&self) -> Result<String, McpError> {
        let guard = self.session.read().await;
        match &*guard {
            Some(s) => Ok(s.kwin_unique_name.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    /// Wait, as a screenshot does, until the screen has stopped changing after
    /// recent input. Chromium updates accessible bounds as it paints, so bounds
    /// read during a scroll animation describe a position the page has left.
    async fn settle_input(&self) -> Result<(), McpError> {
        let Some(last_input) = self
            .last_input()
            .await
            .filter(|at| at.elapsed() < SCREENSHOT_SETTLE_LIMIT)
        else {
            return Ok(());
        };
        let conn = self.kwin_conn().await?;
        let proxy = KWinScreenShot2Proxy::builder(&conn)
            .destination(self.kwin_unique_name().await?)
            .map_err(KwinError::from)?
            .build()
            .await
            .map_err(KwinError::from)?;
        capture_settled_frame(&proxy, Some(last_input), true).await?;
        Ok(())
    }
    async fn host_xdg_dir(&self) -> Result<std::path::PathBuf, McpError> {
        let guard = self.session.read().await;
        match &*guard {
            Some(s) => Ok(s.host_xdg_dir.clone()),
            None => Err(McpError::internal_error(
                "no session — call session_start first",
                None,
            )),
        }
    }
    /// Terminal cleanup for a session_start attempt that published no Session,
    /// covering the failed paths, the cancelled hard-timeout path, and a start
    /// that never created its directory. A start that did publish a Session
    /// keeps its workdir, which session_stop then owns.
    async fn autoclean_unpublished_workdir(&self, error: McpError) -> McpError {
        if self.session.read().await.is_some() {
            return error;
        }
        // Process guards have stopped startup children, but the owned disk
        // twin still exists here. Keep bounded diagnostics in the response
        // before autoclean removes the only copy of their stderr.
        let error = if self.workdir.owned().is_some() {
            preserve_startup_sandbox_diagnostics(error, &self.path)
        } else {
            error
        };
        let removal = self.workdir.remove();
        // The deferred cleanup of a blocked start ends that handle, and its
        // record names what this removal left.
        if self.death_record().as_ref().and_then(terminal_status)
            == Some(session_registry::STATUS_START_BLOCKED)
        {
            self.record_terminal_cleanup(self.startup_cleanup_label());
        }
        match removal {
            WorkdirCleanup::NothingOwned => error,
            WorkdirCleanup::Removed(dir) => {
                eprintln!("session_start: autoclean removed {}", dir.display());
                error
            }
            WorkdirCleanup::Retained {
                dir,
                error: remove_error,
            } => McpError::internal_error(
                format!(
                    "{}; session workdir {} not removed: {remove_error}. Cleanup is still owned, call session_stop to retry.",
                    error.message,
                    dir.display()
                ),
                None,
            ),
        }
    }
    /// Final terminal transition for transport close or a handled signal.
    async fn shutdown_cleanup(&self) {
        let gate = self
            .lifecycle_gate(
                tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT,
                "shutdown cleanup",
            )
            .await;
        if let Err(error) = &gate {
            eprintln!("shutdown: {}; cleaning up without it", error.message);
        }
        if let Some(dir) = self.workdir.owned() {
            eprintln!("shutdown: autoclean owns {}", dir.display());
        }
        let stopped = self.session.write().await.take();
        let session_was_live = stopped.is_some();
        if let Some(sess) = stopped {
            if let Ok(mut clipboards) = self.clipboards.lock() {
                clipboards
                    .entries
                    .remove(&sess.host_xdg_dir.join("wayland-0"));
            }
            teardown_blocking(sess).await;
        }
        let cleanup = self.workdir.remove();
        let cleanup_changed = !matches!(&cleanup, WorkdirCleanup::NothingOwned);
        let workdir_cleanup = match cleanup {
            WorkdirCleanup::NothingOwned => "not_requested",
            WorkdirCleanup::Removed(dir) => {
                eprintln!("shutdown: autoclean removed {}", dir.display());
                "removed"
            }
            WorkdirCleanup::Retained { dir, error } => {
                eprintln!(
                    "shutdown: autoclean could not remove {}: {error}",
                    dir.display()
                );
                "retained"
            }
        };
        let terminal = self.death_record();
        if session_was_live {
            self.record_lifecycle(
                SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED,
                SESSION_LIFECYCLE_REASON_SERVER_SHUTDOWN,
                workdir_cleanup,
            );
        } else if terminal
            .as_ref()
            .and_then(terminal_status)
            .is_some_and(|status| session_registry::clear_writes_record(status, cleanup_changed))
        {
            self.record_terminal_cleanup(workdir_cleanup);
        }
    }
}

struct CursorSprite {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
}

/// Hotspot position within the rasterized cursor PNG — the arrow tip in raw PNG
/// coordinates. Determined once empirically for the fixed render size (see build.rs)
/// so the runtime skips any per-call bbox scanning. If you change the SVG or the
/// rsvg-convert height, rerun the one-shot trim command (`magick cursor.png
/// -alpha extract -threshold 50% -format '%@' info:`) and update these.
const CURSOR_HOTSPOT_X: i32 = 17;
const CURSOR_HOTSPOT_Y: i32 = 10;

/// High-visibility cursor sprite rasterized from cursor_v6_fixed.svg at build time
/// (see build.rs). Lazily decoded once on first use. The tip of the arrow within
/// the returned RGBA buffer is at (CURSOR_HOTSPOT_X, CURSOR_HOTSPOT_Y). Returns
/// None only if the embedded PNG is malformed (shouldn't happen).
fn cursor_sprite() -> Option<&'static CursorSprite> {
    static CACHE: std::sync::OnceLock<Option<CursorSprite>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            const PNG_BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cursor.png"));
            let decoder = png::Decoder::new(PNG_BYTES);
            let mut reader = decoder.read_info().ok()?;
            let mut buf = vec![0u8; reader.output_buffer_size()];
            let info = reader.next_frame(&mut buf).ok()?;
            if info.bit_depth != png::BitDepth::Eight {
                return None;
            }
            let raw = &buf[..info.buffer_size()];
            let rgba: Vec<u8> = match info.color_type {
                png::ColorType::Rgba => raw.to_vec(),
                png::ColorType::Rgb => {
                    let mut out = Vec::with_capacity(raw.len() / 3 * 4);
                    for chunk in raw.as_chunks::<3>().0 {
                        out.extend_from_slice(chunk);
                        out.push(255);
                    }
                    out
                }
                png::ColorType::Grayscale
                | png::ColorType::GrayscaleAlpha
                | png::ColorType::Indexed => return None,
            };
            Some(CursorSprite {
                rgba,
                w: info.width,
                h: info.height,
            })
        })
        .as_ref()
}

async fn structured_result(
    peer: &rmcp::Peer<rmcp::RoleServer>,
    text: impl Into<String>,
    structured: serde_json::Value,
) -> CallToolResult {
    let s: String = text.into();
    let _ = peer
        .notify_logging_message(rmcp::model::LoggingMessageNotificationParam::new(
            rmcp::model::LoggingLevel::Info,
            serde_json::json!(s),
        ))
        .await;
    let mut r = CallToolResult::success(vec![Content::text(s)]);
    r.structured_content = Some(structured);
    r
}

fn cleanup_stale_session_files(dir: &std::path::Path) {
    const STALE_FILES: &[&str] = &[
        "bus",
        "wayland-0",
        "wayland-0.lock",
        "pipewire-0",
        "pipewire-0.lock",
        "pipewire-0-manager",
        "pipewire-0-manager.lock",
        "system_bus_socket",
        "service_bus_socket",
        "kwallet_bus",
        "kwallet-bus.conf",
        "dbus-ready",
        "bridge-ready",
        "screenshot.png",
        "viewer.log",
        VIEWER_STATUS_FILE,
    ];
    const STALE_DIRS: &[&str] = &["at-spi", "browser-bin", "dbus-1", "dconf", "doc"];
    for name in STALE_FILES {
        let _ = std::fs::remove_file(dir.join(name));
    }
    for name in STALE_DIRS {
        let _ = std::fs::remove_dir_all(dir.join(name));
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("script_") && name_str.ends_with(".js") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// ── Session workdir autoclean ────────────────────────────────────────────
//
// --autoclean owns the session workdir and its disk-backed overlay twin, and
// moves through one explicit state machine:
//
//   Idle  --claim (session_start, before the directory is created)--> Owned
//   Owned --remove succeeded (both directories gone)----------------> Idle
//   Owned --remove failed (either directory remains)----------------> Owned
//
// `claim` and `remove` are synchronous and each runs under a single lock
// acquisition, so no await, cancellation, or hard timeout can split a
// transition and strand the directory. Every terminal outcome of session_start,
// session_stop, and server shutdown ends in one of those two transitions, so an
// owned workdir always keeps a reachable retry.

/// Owner read, write, and traverse bits. A directory missing any of them cannot
/// be emptied by its owner, which is what a mode-000 directory in the overlay
/// does to the delete.
const WORKDIR_OWNER_RWX: u32 = 0o700;

/// Depth limit for the permission repair walk inside the owned workdir. Each
/// level holds one open descriptor, so the bound stops a hostile deep tree from
/// exhausting the descriptor limit. A subtree below the limit is left alone; the
/// delete then fails and stays retryable instead of the walk breaking the server.
const WORKDIR_REPAIR_MAX_DEPTH: u32 = 128;
const WORKDIR_LEASE_FILE: &str = ".kwin-mcp-autoclean";
const WORKDIR_LEASE_VERSION: &str = "kwin-mcp-autoclean-v1";

/// The one session workdir this server process owns. `session_start` creates it
/// and cleanup validates against it, so both agree on a single definition.
fn session_workdir_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kwin-mcp-{}", std::process::id()))
}

/// The directory for a new session workdir named `name`: TMPDIR when the
/// workdir's longest socket path fits libdbus's limit there, otherwise
/// `SHORT_RUNTIME_ROOT`. A GPU queue or build staging TMPDIR can be too long.
fn session_runtime_root(name: &str) -> std::path::PathBuf {
    let temp = std::env::temp_dir();
    let longest = temp.join(name).join(WORKDIR_LONGEST_SOCKET);
    if longest.as_os_str().len() <= DBUS_SOCKET_PATH_MAX {
        temp
    } else {
        std::path::PathBuf::from(SHORT_RUNTIME_ROOT)
    }
}

/// Every directory that can hold this user's session workdirs.
fn session_runtime_roots() -> Vec<std::path::PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    let short = std::path::PathBuf::from(SHORT_RUNTIME_ROOT);
    if !roots.contains(&short) {
        roots.push(short);
    }
    roots
}

/// Disk-backed root for session overlay layers.
fn session_disk_root() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("kwin-mcp")
}

/// The disk-backed twin of a session workdir: its overlay upper, work and
/// staging layers, and temporary KWin scripts.
fn session_disk_path(workdir: &Path) -> std::path::PathBuf {
    session_disk_root().join(workdir.file_name().unwrap_or_default())
}

/// Path naming the inode an open descriptor already refers to. Operating through
/// it keeps chmod and directory reads pinned to the descriptor this walk opened
/// and validated, so a name replaced underneath the walk cannot redirect the
/// operation at a different file.
fn proc_fd_path(fd: &std::os::fd::OwnedFd) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    std::path::PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// Grant owner rwx to `dir` and to every directory below it so the delete cannot
/// be blocked by a mode-000 directory a launched command created in the overlay.
///
/// `dir` is a descriptor opened with O_PATH | O_NOFOLLOW, so it names the entry
/// itself and never its symlink target. The walk chmods only after `fstat` on
/// that descriptor proves it is a directory, which excludes symlinks, files, and
/// devices, and it descends only through descriptors opened from the parent
/// descriptor with the same flags. There is no path re-resolution and no
/// check-then-use window, so nothing outside the owned workdir can be reached
/// even while the tree is being rewritten concurrently.
fn repair_workdir_directory_tree(dir: std::os::fd::OwnedFd, depth: u32) {
    let Ok(status) = nix::sys::stat::fstat(&dir) else {
        return;
    };
    if status.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR {
        return;
    }
    let mode = status.st_mode & 0o7777;
    if mode & WORKDIR_OWNER_RWX != WORKDIR_OWNER_RWX {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            proc_fd_path(&dir),
            std::fs::Permissions::from_mode(mode | WORKDIR_OWNER_RWX),
        );
    }
    if depth >= WORKDIR_REPAIR_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(proc_fd_path(&dir)) else {
        return;
    };
    let flags =
        nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_CLOEXEC;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Ok(child) =
            nix::fcntl::openat(&dir, name.as_os_str(), flags, nix::sys::stat::Mode::empty())
        {
            repair_workdir_directory_tree(child, depth + 1);
        }
    }
}

/// Delete the owned session workdir. The path is validated against this
/// process's own workdir first, permissions inside that tree are repaired, and
/// the delete itself never follows symlinks, so every effect stays inside the
/// owned directory. An absent directory is the wanted end state, so a retry
/// after a partial delete converges instead of inventing a new failure.
fn remove_session_workdir(dir: &std::path::Path) -> std::io::Result<()> {
    let expected = session_workdir_path();
    if dir != expected
        && !(dir
            .parent()
            .is_some_and(|parent| session_runtime_roots().iter().any(|root| root == parent))
            && dir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix(&format!("kwin-mcp-{}-", std::process::id()))
                        .is_some_and(|suffix| suffix.parse::<u64>().is_ok())
                }))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not this server's session workdir {}",
                dir.display(),
                expected.display()
            ),
        ));
    }
    // Failed startup may not publish a Session to clean its disk overlay.
    let disk = session_disk_path(dir);
    remove_verified_workdir(&disk).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("disk overlay {} not removed: {error}", disk.display()),
        )
    })?;
    remove_verified_workdir(dir)
}

/// Remove a directory after the caller has proved its ownership. This helper
/// never follows a symlink while repairing nested permissions.
fn remove_verified_workdir(dir: &std::path::Path) -> std::io::Result<()> {
    let flags =
        nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_NOFOLLOW | nix::fcntl::OFlag::O_CLOEXEC;
    if let Ok(root) = nix::fcntl::open(dir, flags, nix::sys::stat::Mode::empty()) {
        repair_workdir_directory_tree(root, 0);
    }
    if let Err(error) = std::fs::remove_dir_all(dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error);
    }
    Ok(())
}

/// Mountinfo escapes whitespace and backslashes with octal bytes. Compare
/// decoded paths, not its printed field, before traversing owned storage.
fn mounted_paths() -> std::io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    std::fs::read("/proc/self/mountinfo")?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut field = line
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty())
                .nth(4)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "mountinfo lacks a mountpoint",
                    )
                })?;
            let mut path = Vec::with_capacity(field.len());
            while !field.is_empty() {
                if let [b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', ..] = field {
                    path.push((*a - b'0') * 64 + (*b - b'0') * 8 + (*c - b'0'));
                    field = &field[4..];
                } else {
                    path.push(field[0]);
                    field = &field[1..];
                }
            }
            Ok(PathBuf::from(std::ffi::OsString::from_vec(path)))
        })
        .collect()
}

/// A previous --autoclean server explicitly marked this workdir and held its
/// lease until exit. The marker records the sandbox process group so a crash
/// cannot cause a later server to delete a still-running container.
fn sweep_orphaned_workdirs(owners: Option<&std::collections::HashSet<u32>>) -> std::io::Result<()> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let uid = std::fs::metadata("/proc/self")?.uid();
    let mounts = mounted_paths()?;
    let entries = session_runtime_roots()
        .iter()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .collect::<std::io::Result<Vec<_>>>()?;
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_text) = name.strip_prefix("kwin-mcp-") else {
            continue;
        };
        let Some(pid_text) = pid_text
            .split('-')
            .next()
            .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            continue;
        };
        if owners.is_some_and(|owners| {
            pid_text
                .parse::<u32>()
                .map_or(true, |pid| !owners.contains(&pid))
        }) {
            continue;
        }
        let dir = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&dir) else {
            continue;
        };
        if !metadata.file_type().is_dir() || metadata.uid() != uid {
            continue;
        }
        let marker_path = dir.join(WORKDIR_LEASE_FILE);
        let Ok(mut marker) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&marker_path)
        else {
            continue;
        };
        let Ok(marker_metadata) = marker.metadata() else {
            continue;
        };
        if !marker_metadata.file_type().is_file()
            || marker_metadata.uid() != uid
            || marker_metadata.len() > 128
            || marker.try_lock().is_err()
        {
            continue;
        }
        let mut contents = String::new();
        if marker.read_to_string(&mut contents).is_err() {
            continue;
        }
        let mut fields = contents.split_whitespace();
        let (Some(version), Some(owner), Some(group), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(group) = group.parse::<i32>() else {
            continue;
        };
        if version != WORKDIR_LEASE_VERSION || owner != pid_text || group <= 1 {
            continue;
        }
        // kill(..., 0) only queries process-group existence. EPERM also means
        // a live group, so only ESRCH authorizes an orphan transition.
        if unsafe { nix::libc::kill(-group, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::ESRCH)
        {
            continue;
        }
        let disk = session_disk_path(&dir);
        let storage_mounted = [&dir, &disk]
            .iter()
            .any(|path| mounts.iter().any(|mount| mount.starts_with(*path)));
        if storage_mounted {
            continue;
        }
        let remove_disk = remove_verified_workdir(&disk).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("disk overlay {} not removed: {error}", disk.display()),
            )
        });
        match remove_disk.and_then(|()| remove_verified_workdir(&dir)) {
            Ok(()) => eprintln!("autoclean: removed orphaned {}", dir.display()),
            Err(error) => eprintln!("autoclean: retained orphaned {}: {error}", dir.display()),
        }
    }
    Ok(())
}

/// Workdirs without an autoclean lease live until their server exits. Once the
/// owning server is gone they are leaked: remove them, unless a live kwin-mcp
/// still has that pid, something is mounted inside, or any process still names
/// the directory on its command line (a surviving sandbox binds it by path).
fn sweep_dead_owner_workdirs(
    owners: Option<&std::collections::HashSet<u32>>,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let runtimes = session_runtime_roots();
    let uid = std::fs::metadata("/proc/self")?.uid();
    let mounts = mounted_paths()?;
    let cmdlines: Vec<Vec<String>> = procfs::process::all_processes()
        .map_err(std::io::Error::other)?
        .flatten()
        .filter_map(|process| process.cmdline().ok())
        .collect();
    // Disk twins (session_disk_root) count too: a twin whose workdir is gone,
    // removed above or cleared from /tmp by a reboot, is leaked the same way.
    for root in runtimes.iter().cloned().chain([session_disk_root()]) {
        let runtime = runtimes.contains(&root);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(pid_text) = name.strip_prefix("kwin-mcp-") else {
                continue;
            };
            let Some(pid_text) = pid_text
                .split('-')
                .next()
                .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            else {
                continue;
            };
            if owners.is_some_and(|owners| {
                pid_text
                    .parse::<u32>()
                    .map_or(true, |pid| !owners.contains(&pid))
            }) {
                continue;
            }
            if !runtime
                && runtimes
                    .iter()
                    .any(|temp| std::fs::symlink_metadata(temp.join(name)).is_ok())
            {
                continue;
            }
            let dir = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&dir) else {
                continue;
            };
            if !metadata.file_type().is_dir() || metadata.uid() != uid {
                continue;
            }
            if std::fs::symlink_metadata(dir.join(WORKDIR_LEASE_FILE)).is_ok() {
                continue;
            }
            let owner_alive = std::fs::read_link(format!("/proc/{pid_text}/exe"))
                .ok()
                .and_then(|exe| {
                    exe.file_name()
                        .map(|file| file.to_string_lossy().starts_with("kwin-mcp"))
                })
                .unwrap_or(false);
            if owner_alive {
                continue;
            }
            let disk = session_disk_path(&dir);
            if mounts
                .iter()
                .any(|mount| mount.starts_with(&dir) || (runtime && mount.starts_with(&disk)))
            {
                continue;
            }
            // Keep argv boundaries: splitting a joined command line loses
            // paths containing whitespace. Conservatively retain references
            // embedded in shell command arguments as well.
            let dir_text = dir.to_string_lossy();
            if cmdlines
                .iter()
                .flatten()
                .any(|arg| arg.contains(dir_text.as_ref()))
            {
                continue;
            }
            match remove_verified_workdir(&dir) {
                Ok(()) => eprintln!("sweep: removed leaked {}", dir.display()),
                Err(error) => eprintln!("sweep: retained leaked {}: {error}", dir.display()),
            }
        }
    }
    Ok(())
}

/// Result of the one terminal cleanup transition.
enum WorkdirCleanup {
    /// Idle: --autoclean is off, or the workdir was already deleted.
    NothingOwned,
    /// Owned to Idle: the directory is gone and nothing is owned any more.
    Removed(std::path::PathBuf),
    /// Owned to Owned: the directory survived, so ownership and the retry stay.
    Retained {
        dir: std::path::PathBuf,
        error: std::io::Error,
    },
}

/// The single authoritative record of the session workdir --autoclean still owns.
struct WorkdirClaim {
    dir: std::path::PathBuf,
    lease: Option<std::fs::File>,
}

#[derive(Default)]
struct WorkdirOwnership {
    owned: std::sync::Mutex<Option<WorkdirClaim>>,
}

impl WorkdirOwnership {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<WorkdirClaim>> {
        self.owned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Idle to Owned. The caller creates the directory just before this call,
    /// with no cancellation point between creation and the claim.
    fn claim(&self, dir: &std::path::Path) {
        *self.lock() = Some(WorkdirClaim {
            dir: dir.to_path_buf(),
            lease: None,
        });
    }
    fn create_lease(&self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut owned = self.lock();
        let claim = owned
            .as_mut()
            .ok_or_else(|| std::io::Error::other("no claimed workdir"))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(claim.dir.join(WORKDIR_LEASE_FILE))?;
        file.try_lock()?;
        claim.lease = Some(file);
        Ok(())
    }
    fn inherit_lease(&self, dir: &Path, descriptor: Option<i32>) -> std::io::Result<()> {
        let lease = descriptor
            .map(adopt_descriptor)
            .transpose()?
            .map(std::fs::File::from);
        *self.lock() = Some(WorkdirClaim {
            dir: dir.to_owned(),
            lease,
        });
        Ok(())
    }
    fn saved_lease(
        &self,
        retained: &mut Vec<std::os::fd::OwnedFd>,
    ) -> std::io::Result<Option<i32>> {
        self.lock()
            .as_ref()
            .and_then(|claim| claim.lease.as_ref())
            .map(|file| retain_descriptor(file, retained))
            .transpose()
    }
    fn mark_sandbox(&self, group: u32) -> std::io::Result<()> {
        use std::io::{Seek, Write};
        let mut owned = self.lock();
        let file = owned
            .as_mut()
            .and_then(|claim| claim.lease.as_mut())
            .ok_or_else(|| std::io::Error::other("no workdir lease"))?;
        file.set_len(0)?;
        file.rewind()?;
        writeln!(
            file,
            "{WORKDIR_LEASE_VERSION} {} {group}",
            std::process::id()
        )?;
        file.sync_data()
    }
    /// The owned path, used by shutdown to decide whether it has work to do.
    fn owned(&self) -> Option<std::path::PathBuf> {
        self.lock().as_ref().map(|claim| claim.dir.clone())
    }
    /// The terminal transition: delete the owned workdir and its disk twin,
    /// releasing ownership only once both are gone. The deletes and release happen
    /// under one lock acquisition, so concurrent callers cannot observe or lose
    /// a half-finished transition.
    fn remove(&self) -> WorkdirCleanup {
        let mut owned = self.lock();
        let Some(dir) = owned.as_ref().map(|claim| claim.dir.clone()) else {
            return WorkdirCleanup::NothingOwned;
        };
        match remove_session_workdir(&dir) {
            Ok(()) => {
                *owned = None;
                WorkdirCleanup::Removed(dir)
            }
            Err(error) => WorkdirCleanup::Retained { dir, error },
        }
    }
}

/// Teardown signals and reaps processes with bounded sleeps; keep that off the
/// async workers that answer other MCP requests. Browsers flush before the
/// session-local wallet and sandbox stop; nothing on the host is touched.
async fn teardown_blocking(mut sess: Session) -> Vec<String> {
    let kwin_windows_closed = request_kwin_browser_close(&sess).await;
    let cdp_close_attempted = request_cdp_browser_close(&sess).await;
    let graceful_close_attempted = kwin_windows_closed > 0 || cdp_close_attempted;
    let sandbox_pid = sess.sandbox_child.id();
    let mut cookie_errors = match tokio::task::spawn_blocking(move || {
        close_session_browsers(sandbox_pid, graceful_close_attempted)
    })
    .await
    {
        Ok(errors) => errors,
        Err(error) => {
            let message = format!("browser shutdown worker failed: {error}");
            eprintln!("teardown: {message}");
            vec![message]
        }
    };
    if let Some(wallet) = sess.wallet.take() {
        let _ = tokio::time::timeout(Duration::from_secs(15), wallet.shutdown()).await;
    }
    match tokio::task::spawn_blocking(move || teardown(sess)).await {
        Ok(mut teardown_errors) => {
            cookie_errors.append(&mut teardown_errors);
            cookie_errors
        }
        Err(error) => {
            let message = format!("session cleanup worker failed: {error}");
            eprintln!("teardown: {message}");
            cookie_errors.push(message);
            cookie_errors
        }
    }
}

const CDP_BROWSER_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const BROWSER_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const BROWSER_SHUTDOWN_POLL: Duration = Duration::from_millis(20);

async fn request_kwin_browser_close(sess: &Session) -> usize {
    let script = r#"
        var browserNames = ["google-chrome-stable", "google-chrome", "chrome",
            "chromium", "chromium-browser", "brave", "brave-browser", "vivaldi",
            "vivaldi-stable", "microsoft-edge", "microsoft-edge-stable", "msedge"];
        var matched = 0;
        var closed = 0;
        var windows = workspace.stackingOrder;
        for (var i = windows.length - 1; i >= 0; --i) {
            var w = windows[i];
            if (!w.managed || w.deleted || w.desktopWindow || w.dock || w.outline) continue;
            var resourceClass = (w.resourceClass || "").toLowerCase();
            var resourceName = (w.resourceName || "").toLowerCase();
            if (browserNames.indexOf(resourceClass) < 0 && browserNames.indexOf(resourceName) < 0) continue;
            matched++;
            if (w.closeable) {
                w.closeWindow();
                closed++;
            }
        }
        var result = {matched: matched, closed: closed};
    "#;
    let report = match run_kwin_script(
        &sess.kwin_conn,
        &sess.kwin_unique_name,
        &sess.host_xdg_dir,
        script,
    )
    .await
    {
        Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("teardown: cannot parse KWin browser-close report: {error}");
                return 0;
            }
        },
        Err(error) => {
            eprintln!("teardown: KWin browser-window close failed: {error}");
            return 0;
        }
    };
    let matched = report["matched"].as_u64().unwrap_or_default();
    let closed = report["closed"].as_u64().unwrap_or_default();
    eprintln!("teardown: KWin close requested for {closed}/{matched} Chromium window(s)");
    usize::try_from(closed).unwrap_or_default()
}

async fn request_cdp_browser_close(sess: &Session) -> bool {
    let Some(browser) = sess.cdp_browser.as_ref() else {
        eprintln!("teardown: no CDP browser handle for graceful close");
        return false;
    };
    use chromiumoxide::cdp::browser_protocol::browser::CloseParams;
    eprintln!("teardown: requesting graceful Chrome close through CDP");
    match tokio::time::timeout(
        CDP_BROWSER_CLOSE_TIMEOUT,
        browser.execute(CloseParams::default()),
    )
    .await
    {
        Ok(Ok(_)) => {
            eprintln!("teardown: CDP graceful browser close accepted");
            true
        }
        Ok(Err(error)) => {
            eprintln!("teardown: CDP graceful browser close failed: {error}");
            true
        }
        Err(_) => {
            eprintln!("teardown: CDP graceful browser close timed out");
            true
        }
    }
}

/// Ask Chromium to close through CDP first, then signal only browser parents
/// descended from this sandbox if any remain. The session's network services,
/// compositor, and wallet stay alive until its cookie flush has completed.
fn close_session_browsers(sandbox_pid: u32, graceful_close_attempted: bool) -> Vec<String> {
    let Ok(sandbox_pid) = i32::try_from(sandbox_pid) else {
        let message = "sandbox PID does not fit the process ID type".to_owned();
        eprintln!("teardown: {message}");
        return vec![message];
    };
    let processes = match procfs::process::all_processes() {
        Ok(processes) => processes,
        Err(error) => {
            let message = format!("cannot enumerate processes for browser shutdown: {error}");
            eprintln!("teardown: {message}");
            return vec![message];
        }
    };
    let processes = processes
        .flatten()
        .filter_map(|process| {
            let stat = process.stat().ok()?;
            Some((process, stat))
        })
        .collect::<Vec<_>>();
    let mut descendants = std::collections::HashSet::from([sandbox_pid]);
    loop {
        let before = descendants.len();
        for (process, stat) in &processes {
            if descendants.contains(&stat.ppid) {
                descendants.insert(process.pid);
            }
        }
        if before == descendants.len() {
            break;
        }
    }
    let parents = processes
        .iter()
        .map(|(process, stat)| (process.pid, stat.ppid))
        .collect::<std::collections::HashMap<_, _>>();
    let mut candidates = Vec::new();
    for (process, stat) in processes {
        if !descendants.contains(&process.pid) || process.pid == sandbox_pid {
            continue;
        }
        let Ok(command) = process.cmdline() else {
            continue;
        };
        let process_type = command.iter().enumerate().find_map(|(index, argument)| {
            if argument == "--type" {
                command.get(index + 1).cloned()
            } else {
                argument.strip_prefix("--type=").map(str::to_owned)
            }
        });
        let Ok(executable) = process.exe() else {
            continue;
        };
        let Some(name) = executable.file_name().and_then(std::ffi::OsStr::to_str) else {
            continue;
        };
        if !BROWSER_COMMANDS.contains(&name)
            || matches!(name, "code" | "codium" | "vscodium" | "electron")
        {
            continue;
        }
        if process
            .stat()
            .is_ok_and(|current| current.starttime == stat.starttime)
        {
            candidates.push((
                process,
                stat.starttime,
                stat.ppid,
                name.to_owned(),
                process_type,
            ));
        }
    }
    let candidate_pids = candidates
        .iter()
        .map(|(process, _, _, _, _)| process.pid)
        .collect::<std::collections::HashSet<_>>();
    let mut main_pids = std::collections::HashSet::new();
    let mut browsers = Vec::new();
    for (process, started, parent, name, process_type) in candidates {
        let mut ancestor = parent;
        let mut seen = std::collections::HashSet::new();
        let mut has_browser_ancestor = false;
        while ancestor > 0 && ancestor != sandbox_pid && seen.insert(ancestor) {
            if candidate_pids.contains(&ancestor) {
                has_browser_ancestor = true;
                break;
            }
            ancestor = *parents.get(&ancestor).unwrap_or(&0);
        }
        let is_main = match process_type.as_deref() {
            Some("browser") => true,
            Some(_) => false,
            None => !has_browser_ancestor,
        };
        eprintln!(
            "teardown: browser process pid={} name={name} type={} main={is_main}",
            process.pid,
            process_type.as_deref().unwrap_or("none")
        );
        if is_main {
            main_pids.insert(process.pid);
        }
        browsers.push((process, started));
    }
    let requested = browsers.len();
    let main_count = main_pids.len();
    let mut cookie_errors = Vec::new();
    eprintln!(
        "teardown: graceful browser shutdown candidates={requested} mains={main_count} graceful_close_attempted={graceful_close_attempted}"
    );
    if graceful_close_attempted && !browsers.is_empty() {
        let deadline = std::time::Instant::now() + CDP_BROWSER_CLOSE_TIMEOUT;
        while !browsers.is_empty() && std::time::Instant::now() < deadline {
            browsers.retain(|(process, started)| {
                process
                    .stat()
                    .is_ok_and(|stat| stat.starttime == *started && stat.state != 'Z')
            });
            if !browsers.is_empty() {
                std::thread::sleep(BROWSER_SHUTDOWN_POLL);
            }
        }
    }
    if browsers.is_empty() {
        if requested > 0 || graceful_close_attempted {
            eprintln!("teardown: graceful browser shutdown completed");
        }
        return cookie_errors;
    }
    for (process, started) in &browsers {
        if !main_pids.contains(&process.pid) {
            continue;
        }
        if process
            .stat()
            .is_ok_and(|current| current.starttime == *started)
            && let Err(error) = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(process.pid),
                nix::sys::signal::Signal::SIGTERM,
            )
        {
            eprintln!(
                "teardown: browser {} shutdown signal failed: {error}",
                process.pid
            );
        }
    }
    let deadline = std::time::Instant::now() + BROWSER_SHUTDOWN_GRACE;
    while !browsers.is_empty() {
        browsers.retain(|(process, started)| {
            process
                .stat()
                .is_ok_and(|stat| stat.starttime == *started && stat.state != 'Z')
        });
        if browsers.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let message = format!(
                "{} browser(s) did not finish cookie flush within {:?}",
                browsers.len(),
                BROWSER_SHUTDOWN_GRACE
            );
            eprintln!("teardown: {message}");
            cookie_errors.push(message);
            break;
        }
        std::thread::sleep(BROWSER_SHUTDOWN_POLL);
    }
    if browsers.is_empty() && (requested > 0 || graceful_close_attempted) {
        eprintln!("teardown: graceful browser shutdown completed");
    }
    cookie_errors
}

fn teardown(mut sess: Session) -> Vec<String> {
    let mut cookie_errors = std::mem::take(&mut sess.cookie_errors);
    drop(sess.cdp_browser);
    // Kill the viewer first so it can flush any pending wayland requests
    // before the container's compositor disappears.
    if let Some(viewer) = sess.viewer_child.take() {
        terminate_child(viewer, false, "viewer");
    }
    drop(sess.sandbox_stdin);
    sess.scope.stop();
    // Kill the pasta and bwrap process group (negative PID = entire group).
    terminate_child(sess.sandbox_child, true, "sandbox");
    for proxy in std::mem::take(&mut sess.service_proxy_children) {
        terminate_child(proxy, false, "session D-Bus proxy");
    }
    if let Err(error) = kwin_mcp::browser_cookies::persist(&sess.cookie_roots) {
        let message = format!("{error:#}");
        eprintln!("teardown: browser cookies were not saved: {error:#}");
        cookie_errors.push(message);
    }
    use std::os::unix::fs::PermissionsExt;
    for path in &sess.overlay_work_paths {
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o700);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
    let _ = std::fs::remove_dir_all(sess.host_xdg_dir.join("tmp"));
    let disk = session_disk_path(&sess.host_xdg_dir);
    if let Err(error) = remove_verified_workdir(&disk) {
        eprintln!("teardown: retained {}: {error}", disk.display());
    }
    cleanup_stale_session_files(&sess.host_xdg_dir);
    cookie_errors
}

/// Linux quotactl command for reading one user's quota (Q_GETQUOTA) and the
/// quota block unit of `struct if_dqblk` (QIF_DQBLKSIZE).
const Q_GETQUOTA: i32 = 0x0080_0007;
const USRQUOTA: i32 = 0;
const QUOTA_BLOCK_BYTES: u64 = 1024;
const GIB: f64 = 1_073_741_824.0;

#[repr(C)]
#[derive(Default)]
struct IfDqblk {
    dqb_bhardlimit: u64,
    dqb_bsoftlimit: u64,
    dqb_curspace: u64,
    dqb_ihardlimit: u64,
    dqb_isoftlimit: u64,
    dqb_curinodes: u64,
    dqb_btime: u64,
    dqb_itime: u64,
    dqb_valid: u32,
}

fn gib(bytes: u64) -> String {
    #[expect(clippy::as_conversions)]
    let value = bytes as f64 / GIB;
    format!("{value:.2} GiB")
}

/// The refusal when the HOME overlay's filesystem has less than the floor free,
/// with the exact retry a caller can make: half of what is free, in MiB.
fn overlay_floor_message(disk: &Path, floor: u64, free: u64) -> String {
    let retry = ((free >> 20) / 2).max(1);
    format!(
        "the session's HOME overlay lives in {} and needs {} free there; that filesystem has {} free. Free space there, or retry session_start with disk_min_free_mib={retry} (MiB; the session then writes into what is left)",
        disk.display(),
        gib(floor),
        gib(free)
    )
}

/// Explain a failed write: for EDQUOT/ENOSPC, name the per-user quota (systemd
/// mounts /tmp as tmpfs with usrquota, so a user can hit its limit while df
/// still shows free space), the filesystem it applies to, and the filesystem's
/// own free space.
fn describe_write_failure(path: &Path, error: &std::io::Error) -> String {
    let mut message = format!("writing {} failed: {error}", path.display());
    let Some(code) = error.raw_os_error() else {
        return message;
    };
    if code != nix::libc::EDQUOT && code != nix::libc::ENOSPC {
        return message;
    }
    let directory = path.parent().unwrap_or(path);
    let filesystem = nix::sys::statfs::statfs(directory)
        .map(|stat| filesystem_name(stat.filesystem_type()))
        .unwrap_or_else(|_| "unidentified".to_owned());
    if let Ok(dir) = std::fs::File::open(directory) {
        use std::os::fd::AsRawFd;
        let mut quota = IfDqblk::default();
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata("/proc/self")
            .map(|meta| meta.uid())
            .unwrap_or_default();
        // SAFETY: quotactl_fd writes one struct if_dqblk into `quota`, which is
        // repr(C) with the kernel layout and outlives the call.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_quotactl_fd,
                dir.as_raw_fd(),
                (Q_GETQUOTA << 8) | USRQUOTA,
                uid,
                std::ptr::from_mut(&mut quota),
            )
        };
        if result == 0
            && let Some(summary) = quota_summary(&quota)
        {
            message.push_str(&format!(
                ". Per-user disk quota for uid {uid} on the {filesystem} filesystem: {summary}"
            ));
        }
    }
    if let Ok(stat) = nix::sys::statvfs::statvfs(directory) {
        let free = stat.blocks_available().saturating_mul(stat.fragment_size());
        message.push_str(&format!(
            "; the filesystem itself has {} free. Files owned by this user on this filesystem count against its limit; free space owned by this user on that filesystem (or stop idle sessions) and retry",
            gib(free)
        ));
    }
    message
}

/// The name of the filesystem a write failed on: /tmp is tmpfs, and the HOME
/// overlay's upper layer is overlayfs.
fn filesystem_name(kind: nix::sys::statfs::FsType) -> String {
    if kind == nix::sys::statfs::TMPFS_MAGIC {
        "tmpfs".to_owned()
    } else if kind == nix::sys::statfs::OVERLAYFS_SUPER_MAGIC {
        "overlayfs".to_owned()
    } else {
        format!("type {:#x}", kind.0)
    }
}

/// The block and inode usage of a per-user quota, marking each limit that is
/// reached, or `None` when the quota sets neither limit.
fn quota_summary(quota: &IfDqblk) -> Option<String> {
    let mut parts = Vec::new();
    if quota.dqb_bhardlimit > 0 {
        let limit = quota.dqb_bhardlimit.saturating_mul(QUOTA_BLOCK_BYTES);
        let at_limit = if quota.dqb_curspace >= limit {
            " (at its limit)"
        } else {
            ""
        };
        parts.push(format!(
            "{} used of a {} block limit{at_limit}",
            gib(quota.dqb_curspace),
            gib(limit)
        ));
    }
    if quota.dqb_ihardlimit > 0 {
        let at_limit = if quota.dqb_curinodes >= quota.dqb_ihardlimit {
            " (at its limit)"
        } else {
            ""
        };
        parts.push(format!(
            "{} of {} inodes used{at_limit}",
            quota.dqb_curinodes, quota.dqb_ihardlimit
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// A launch whose command script cannot be written fails before anything is
/// submitted, and names what ran out.
fn launch_script_write_error(path: &Path, error: &std::io::Error) -> McpError {
    McpError::internal_error(
        format!(
            "launch_app could not write its command script: {}. Nothing was submitted.",
            describe_write_failure(path, error)
        ),
        Some(serde_json::json!({
            "reason": "write_failed",
            "stage": "writing the launch script",
            "command_submitted": false,
            "path": path.display().to_string(),
            "os_error": error.raw_os_error(),
        })),
    )
}

/// Write `bytes` to `path` via a sibling temporary file and rename, so a
/// failed write never leaves a truncated file at `path`.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("partial");
    let written =
        std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Root directory of the sandbox's mount namespace as seen from the host:
/// /proc/<pid>/root of bwrap's inner child, two levels below pasta.
fn sandbox_root(sandbox_pid: u32) -> Option<PathBuf> {
    let children = |parent: i32| -> Vec<i32> {
        procfs::process::all_processes()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|process| process.stat().is_ok_and(|stat| stat.ppid == parent))
            .map(|process| process.pid)
            .collect()
    };
    let pasta = i32::try_from(sandbox_pid).ok()?;
    let outer = *children(pasta).first()?;
    let inner = *children(outer).first()?;
    let root = PathBuf::from(format!("/proc/{inner}/root"));
    std::fs::read_dir(&root).ok().map(|_| root)
}

/// Stream-compare two files byte for byte.
fn same_contents(left: &Path, right: &Path) -> std::io::Result<bool> {
    let (mut a, mut b) = (std::fs::File::open(left)?, std::fs::File::open(right)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    same_reader_contents(&mut a, &mut b)
}

fn same_reader_contents(
    a: &mut impl std::io::Read,
    b: &mut impl std::io::Read,
) -> std::io::Result<bool> {
    let (mut left_buf, mut right_buf) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let read = a.read(&mut left_buf)?;
        if read == 0 {
            // The source can shrink after the initial file-size comparison.
            // A matching prefix is insufficient if the copy still has bytes.
            return Ok(b.read(&mut right_buf[..1])? == 0);
        }
        b.read_exact(&mut right_buf[..read])?;
        if left_buf[..read] != right_buf[..read] {
            return Ok(false);
        }
    }
}

/// Copy a session file (read through the sandbox root) to a host path with a
/// temporary file and rename, then verify the host copy byte for byte.
fn export_session_file(source: &Path, destination: &Path, overwrite: bool) -> Result<u64, String> {
    let metadata =
        std::fs::metadata(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", source.display()));
    }
    if !overwrite && std::fs::symlink_metadata(destination).is_ok() {
        return Err(format!(
            "{} already exists on the host; pass overwrite=true to replace it",
            destination.display()
        ));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    if !parent.is_dir() {
        return Err(format!(
            "host directory {} does not exist; create it or pass create_parents=true",
            parent.display()
        ));
    }
    let temporary = parent.join(format!(
        ".{}.kwin-mcp-export",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
    ));
    let copied = std::fs::copy(source, &temporary)
        .map_err(|error| describe_write_failure(&temporary, &error))
        .and_then(|bytes| match same_contents(source, &temporary) {
            Ok(true) => Ok(bytes),
            Ok(false) => Err(
                "host copy differs from the session file (was it still being written?)".to_owned(),
            ),
            Err(error) => Err(format!("verify host copy: {error}")),
        })
        .and_then(|bytes| {
            std::fs::rename(&temporary, destination)
                .map(|()| bytes)
                .map_err(|error| format!("rename into {}: {error}", destination.display()))
        });
    if copied.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    copied
}

/// Screenshot settling after recent input; None when no input was recent.
struct SettleReport {
    settled: bool,
    waited: Duration,
}

type Frame = (u32, u32, u32, Vec<u8>);

const SCREENSHOT_RETRY_AFTER_MS: u64 = 1_000;

fn screenshot_timeout(stage: &'static str, timeout: Duration) -> McpError {
    McpError::internal_error(
        format!(
            "screenshot: frame unavailable after {} seconds during {stage}. The compositor may be busy or a window query may be stalled. Wait at least one second, then retry screenshot. This timeout does not stop the session.",
            timeout.as_secs(),
        ),
        Some(serde_json::json!({
            "reason": "frame_unavailable",
            "stage": stage,
            "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "retry_safe": true,
            "retry_after_ms": SCREENSHOT_RETRY_AFTER_MS,
            "possible_causes": ["compositor_busy", "capture_or_window_query_stalled"],
            "guidance": "Wait at least one second, then retry screenshot. If the problem persists, inspect window_list and session_list to distinguish a capture failure from a wider session problem. The timeout does not identify the underlying cause.",
        })),
    )
}

/// One complete CaptureScreen frame.
async fn capture_frame(
    proxy: &KWinScreenShot2Proxy<'_>,
    include_cursor: bool,
) -> Result<Frame, McpError> {
    tokio::time::timeout(
        SCREENSHOT_FRAME_TIMEOUT,
        capture_frame_inner(proxy, include_cursor),
    )
    .await
    .map_err(|_| screenshot_timeout("capture_frame", SCREENSHOT_FRAME_TIMEOUT))?
}

/// The frame KWin writes into `frame`, once all `expected` bytes are there.
async fn read_frame_file(frame: &std::fs::File, expected: usize) -> Result<Vec<u8>, McpError> {
    use std::os::unix::fs::FileExt;
    let expected_len = u64::try_from(expected).map_err(KwinError::from)?;
    while frame.metadata().map_err(KwinError::from)?.len() < expected_len {
        tokio::time::sleep(SCREENSHOT_FILL_POLL).await;
    }
    let mut pixels = vec![0; expected];
    frame
        .read_exact_at(&mut pixels, 0)
        .map_err(KwinError::from)?;
    Ok(pixels)
}

async fn capture_frame_inner(
    proxy: &KWinScreenShot2Proxy<'_>,
    include_cursor: bool,
) -> Result<Frame, McpError> {
    capture_source_inner(proxy, None, include_cursor).await
}

async fn capture_source_inner(
    proxy: &KWinScreenShot2Proxy<'_>,
    window: Option<&str>,
    include_cursor: bool,
) -> Result<Frame, McpError> {
    let frame = std::fs::File::from(
        nix::sys::memfd::memfd_create("kwin-mcp-frame", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)
            .map_err(KwinError::from)?,
    );
    let writer = std::os::fd::OwnedFd::from(frame.try_clone().map_err(KwinError::from)?);
    let mut opts = std::collections::HashMap::new();
    opts.insert(
        "include-cursor",
        zbus::zvariant::Value::from(window.is_none() && include_cursor),
    );
    opts.insert(
        "include-decoration",
        zbus::zvariant::Value::from(window.is_none()),
    );
    opts.insert("include-shadow", zbus::zvariant::Value::from(false));
    opts.insert("hide-caller-windows", zbus::zvariant::Value::from(false));
    // CaptureScreen composites all surfaces including popups (xdg_popup menus);
    // CaptureWindow only grabs the toplevel's own framebuffer and misses popups.
    let meta = match window {
        Some(handle) => {
            proxy
                .capture_window(handle, opts, zbus::zvariant::OwnedFd::from(writer))
                .await
        }
        None => {
            proxy
                .capture_screen("Virtual-0", opts, zbus::zvariant::OwnedFd::from(writer))
                .await
        }
    }
    .map_err(KwinError::from)?;
    let get_u32 = |k: &str| -> Result<u32, McpError> {
        let val = meta
            .get(k)
            .ok_or_else(|| McpError::internal_error(format!("screenshot: no {k}"), None))?;
        let n: u32 = val.try_into().map_err(KwinError::from)?;
        Ok(n)
    };
    let (width, height, stride) = (get_u32("width")?, get_u32("height")?, get_u32("stride")?);
    let format = get_u32("format")?;
    // Qt's RGB32/ARGB32 formats are native-endian words (BGRA bytes on
    // little-endian hosts); RGBX/RGBA8888 are byte-ordered RGBA on every host.
    // Normalize both capture paths before comparing their opaque interiors.
    if !matches!(format, 16..=18) && !(cfg!(target_endian = "little") && matches!(format, 4..=6)) {
        return Err(McpError::internal_error(
            format!("unsupported screenshot QImage format {format}"),
            None,
        ));
    }
    if width == 0 || height == 0 || u64::from(stride) < u64::from(width) * 4 {
        return Err(McpError::internal_error(
            "invalid screenshot dimensions or stride",
            None,
        ));
    }

    let expected =
        usize::try_from(u64::from(stride) * u64::from(height)).map_err(KwinError::from)?;
    let mut pixels = read_frame_file(&frame, expected).await?;
    if matches!(format, 16..=18) {
        let row_bytes = usize::try_from(u64::from(width) * 4).map_err(KwinError::from)?;
        for row in pixels.chunks_exact_mut(usize::try_from(stride).map_err(KwinError::from)?) {
            for pixel in row[..row_bytes].chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }
    }
    Ok((width, height, stride, pixels))
}

fn transient_capture_error(window_id: &str, detail: impl Into<String>) -> McpError {
    let detail = detail.into();
    McpError::internal_error(
        format!(
            "screenshot: active dialog pixels could not be verified: {detail}. Wait for the dialog to finish drawing and retry."
        ),
        Some(serde_json::json!({
            "reason":"transient_capture_unverified", "window_id":window_id,
            "detail":detail, "retry_safe":true, "retry_after_ms":200,
        })),
    )
}

fn frame_pixel(frame: &Frame, x: u32, y: u32) -> Result<&[u8], String> {
    if x >= frame.0 || y >= frame.1 {
        return Err("pixel lies outside its frame".to_owned());
    }
    let start = usize::try_from(u64::from(y) * u64::from(frame.2) + u64::from(x) * 4)
        .map_err(|_| "pixel offset overflow".to_owned())?;
    let end = start.checked_add(4).ok_or("pixel end overflow")?;
    frame
        .3
        .get(start..end)
        .ok_or_else(|| "capture buffer is incomplete".to_owned())
}

/// Compare only the opaque client interior. A native window capture excludes
/// separately composited popups; it is a verification reference, not our output.
fn compare_transient_capture(
    composite: &Frame,
    reference: &Frame,
    client_origin: (i32, i32),
    client_size: (u32, u32),
    cursor_positions: &[(i32, i32)],
) -> Result<serde_json::Value, String> {
    if (reference.0, reference.1) != client_size {
        return Err(format!(
            "window reference is {}x{}, expected client {}x{}",
            reference.0, reference.1, client_size.0, client_size.1
        ));
    }
    let inset_x = 8_u32.min(reference.0 / 4);
    let inset_y = 8_u32.min(reference.1 / 4);
    let mut visible = 0_u64;
    let mut excluded = 0_u64;
    let mut compared = 0_u64;
    let mut mismatches = 0_u64;
    let mut screen_translucent = 0_u64;
    let mut reference_translucent = 0_u64;
    let mut samples = Vec::new();
    for y in inset_y..reference.1.saturating_sub(inset_y) {
        for x in inset_x..reference.0.saturating_sub(inset_x) {
            let sx = i64::from(client_origin.0) + i64::from(x);
            let sy = i64::from(client_origin.1) + i64::from(y);
            if sx < 0 || sy < 0 || sx >= i64::from(composite.0) || sy >= i64::from(composite.1) {
                continue;
            }
            visible += 1;
            if cursor_positions.iter().any(|(cx, cy)| {
                (sx - i64::from(*cx)).abs() <= 48 && (sy - i64::from(*cy)).abs() <= 48
            }) {
                excluded += 1;
                continue;
            }
            let a = frame_pixel(
                composite,
                u32::try_from(sx).map_err(|_| "invalid screen x")?,
                u32::try_from(sy).map_err(|_| "invalid screen y")?,
            )?;
            let b = frame_pixel(reference, x, y)?;
            // The composited screen is the output and is opaque however its
            // alpha byte was blended; only the window's own alpha marks
            // translucent pixels that cannot be compared.
            if a[3] != 255 {
                screen_translucent += 1;
            }
            if b[3] != 255 {
                reference_translucent += 1;
                continue;
            }
            compared += 1;
            if a[..3] != b[..3] {
                mismatches += 1;
                if samples.len() < 4 {
                    samples.push(serde_json::json!({"x": x, "y": y, "screen": [a[0], a[1], a[2], a[3]], "window": [b[0], b[1], b[2], b[3]]}));
                }
            }
        }
    }
    let counts = serde_json::json!({
        "reference_method":"CaptureWindow", "compared_pixels":compared,
        "mismatching_pixels":mismatches, "visible_interior_pixels":visible,
        "cursor_excluded_pixels":excluded, "screen_alpha_below_255":screen_translucent,
        "window_alpha_below_255":reference_translucent,
        "mismatch_samples":samples, "client_origin":[client_origin.0, client_origin.1], "client_size":[client_size.0, client_size.1],
    });
    let eligible = visible.saturating_sub(excluded);
    let required = (eligible - eligible / 5).max(1_024.min(visible));
    if compared < required {
        // A translucent window cannot be checked this way; the target identity
        // check still applies, so return the image and say it is unverified.
        let mut evidence = counts;
        evidence["status"] = serde_json::json!("not_comparable");
        evidence["required_pixels"] = serde_json::json!(required);
        return Ok(evidence);
    }
    // A caret or animation can change a few pixels between the two captures;
    // a parent window's pixels under a dialog's identity differ almost entirely.
    if mismatches.saturating_mul(100) > compared.saturating_mul(TRANSIENT_MISMATCH_PERCENT) {
        return Err(format!(
            "{mismatches} of {compared} opaque dialog pixels differ from the independent window capture ({counts})"
        ));
    }
    let mut evidence = counts;
    evidence["status"] = serde_json::json!("matches_window_capture");
    Ok(evidence)
}

/// Capture a frame that reflects recent input. Input tools return once their
/// events are flushed, before the app has handled them and repainted, so a
/// capture shortly after input polls until the screen has been unchanged for
/// SCREENSHOT_SETTLE_QUIET, bounded by SCREENSHOT_SETTLE_LIMIT after the input.
async fn capture_settled_frame(
    proxy: &KWinScreenShot2Proxy<'_>,
    last_input: Option<std::time::Instant>,
    include_cursor: bool,
) -> Result<(u32, u32, u32, Vec<u8>, Option<SettleReport>), McpError> {
    let started = std::time::Instant::now();
    let mut frame = capture_frame(proxy, include_cursor).await?;
    let Some(input_at) = last_input.filter(|at| at.elapsed() < SCREENSHOT_SETTLE_LIMIT) else {
        let (width, height, stride, pixels) = frame;
        return Ok((width, height, stride, pixels, None));
    };
    let deadline = input_at + SCREENSHOT_SETTLE_LIMIT;
    let mut unchanged_since = std::time::Instant::now();
    let settled = loop {
        let now = std::time::Instant::now();
        if now.duration_since(unchanged_since) >= SCREENSHOT_SETTLE_QUIET
            && now.duration_since(input_at) >= SCREENSHOT_SETTLE_QUIET
        {
            break true;
        }
        if now >= deadline {
            break false;
        }
        tokio::time::sleep(SCREENSHOT_SETTLE_POLL).await;
        let next = capture_frame(proxy, include_cursor).await?;
        if next.3 != frame.3 {
            unchanged_since = std::time::Instant::now();
        }
        frame = next;
    };
    let (width, height, stride, pixels) = frame;
    Ok((
        width,
        height,
        stride,
        pixels,
        Some(SettleReport {
            settled,
            waited: started.elapsed(),
        }),
    ))
}

/// FUSE support for the sandbox, when the host allows it: /dev/fuse exists,
/// bwrap is not setuid (so it may grant capabilities in its user namespace),
/// setpriv can drop them for everything but the helper, and at least one real
/// fusermount binary exists. Returns (host binary, program name) pairs.
fn fuse_support() -> Option<Vec<(PathBuf, &'static str)>> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::PermissionsExt;
    let find = |name: &str| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
    };
    if !std::fs::metadata("/dev/fuse").is_ok_and(|meta| meta.file_type().is_char_device()) {
        return None;
    }
    let bwrap = find("bwrap")?;
    if std::fs::metadata(&bwrap).ok()?.permissions().mode() & 0o4000 != 0 {
        return None;
    }
    find("setpriv")?;
    let binaries: Vec<(PathBuf, &'static str)> = fuse_bridge::PROGRAMS
        .into_iter()
        .filter_map(|program| Some((std::fs::canonicalize(find(program)?).ok()?, program)))
        .collect();
    (!binaries.is_empty()).then_some(binaries)
}

/// Find the AT-SPI bus launcher in common system locations or on the host PATH.
fn resolve_at_spi_bus_launcher() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let mut candidates = vec![
        PathBuf::from("/usr/lib/at-spi-bus-launcher"),
        PathBuf::from("/usr/lib/at-spi2-core/at-spi-bus-launcher"),
        PathBuf::from("/usr/libexec/at-spi-bus-launcher"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path).map(|directory| directory.join("at-spi-bus-launcher")),
        );
    }
    candidates.into_iter().find_map(|candidate| {
        let metadata = std::fs::metadata(&candidate).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| std::fs::canonicalize(candidate).ok())
            .flatten()
    })
}

/// Resolve the kwin-viewer binary by replacing the basename of our own
/// current_exe. Returns None if the binary doesn't exist — the viewer is
/// an optional convenience, not required for MCP tool operation.
fn resolve_viewer_binary() -> Option<std::path::PathBuf> {
    let me = std::env::current_exe().ok()?;
    let dir = me.parent()?;
    let candidate = dir.join("kwin-viewer");
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// OpenSSH rejects an included config file unless root or the running user
/// owns it and no group or other write bit is set. The sandbox maps only the
/// session user, so a host root-owned drop-in reaches it with an unmapped owner.
/// Preserve rejection of configs already unsafe on the host: copying must not
/// turn group/other-writable or foreign-user-owned input into trusted config.
fn ssh_config_needs_copy(owner: u32, mode: u32, session_uid: u32) -> bool {
    owner == 0 && owner != session_uid && mode & 0o022 == 0
}

/// Session copies of host root-owned, non-writable OpenSSH drop-ins, each
/// owned by the session user with mode 600, paired with the host path they
/// replace. The copies live in the workdir and are bound over those paths.
fn prepare_ssh_config_overrides(workdir: &Path, session_uid: u32) -> Vec<(PathBuf, PathBuf)> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let Ok(entries) = std::fs::read_dir("/etc/ssh/ssh_config.d") else {
        return Vec::new();
    };
    let mut overrides = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "conf") {
            continue;
        }
        let copy = workdir.join(format!("ssh-{}", entry.file_name().to_string_lossy()));
        let written = std::fs::canonicalize(&path).and_then(|target| {
            // Validate the same opened file whose bytes are copied. Replacing
            // a symlink between path lookups must not sanitize an unsafe file.
            let mut source = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&target)?;
            let metadata = source.metadata()?;
            if !metadata.is_file()
                || !ssh_config_needs_copy(metadata.uid(), metadata.mode(), session_uid)
            {
                return Ok(None);
            }
            let mut destination = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&copy)?;
            std::io::copy(&mut source, &mut destination)?;
            destination.flush()?;
            Ok(Some(target))
        });
        match written {
            Ok(Some(target)) => overrides.push((copy, target)),
            Ok(None) => {}
            Err(error) => eprintln!(
                "session_start: cannot copy {} for the sandbox: {error}",
                path.display()
            ),
        }
    }
    overrides
}

/// Quote one complete shell expression as a single argument to `bash -c`.
/// This keeps control operators inside the child shell so the required
/// environment reaches every simple command in the expression.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Absolute path arguments of a launch command, each once. A `NAME=/path` or
/// `--option=/path` word contributes its value.
fn launch_path_words(command: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for word in shlex::split(command).unwrap_or_default() {
        let value = if word.starts_with('/') {
            word.as_str()
        } else {
            word.split_once('=')
                .map_or(word.as_str(), |(_, value)| value)
        };
        let path = Path::new(value);
        if path.is_absolute() && !paths.iter().any(|saved| saved == path) {
            paths.push(path.to_owned());
        }
    }
    paths
}

/// Files a launch argument names, as host paths: absolute words and option
/// values, and relative words that name an existing host file in `cwd`.
fn launch_file_words(command: &str, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = launch_path_words(command);
    for word in shlex::split(command).unwrap_or_default() {
        if word.starts_with(['/', '-']) || word.contains('=') {
            continue;
        }
        let Some(path) = cwd.map(|cwd| cwd.join(&word)) else {
            continue;
        };
        if std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file())
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

// The session sees HOME through an overlay (#611): writes stay in the
// session, and a host file replaced after session_start can stay the old copy.
const LAUNCH_OVERLAY_CHECK_LIMIT: usize = 32;
// Same-size files whose mtimes differ are compared byte for byte up to this.
const LAUNCH_OVERLAY_COMPARE_BYTES: u64 = 16 << 20;

/// Whether the session's regular file at `path` has the host file's bytes.
fn same_session_contents(root: &Path, path: &Path) -> std::io::Result<bool> {
    let mut session = file_import::session_file(root, path, true)?;
    if !session.metadata()?.is_file() {
        return Ok(false);
    }
    let mut host = std::fs::File::open(path)?;
    same_reader_contents(&mut host, &mut session)
}

#[derive(Default)]
struct HomeOverlayReport {
    /// Absolute arguments under HOME whose writes stay in the session.
    overlay_paths: Vec<PathBuf>,
    /// The command's working directory, when it is in the overlay.
    working_directory: Option<PathBuf>,
    /// Host files the session sees a different copy of, or none at all.
    different_copies: Vec<serde_json::Value>,
}

impl HomeOverlayReport {
    fn is_empty(&self) -> bool {
        self.overlay_paths.is_empty()
            && self.working_directory.is_none()
            && self.different_copies.is_empty()
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "overlay_paths": self.overlay_paths,
            "working_directory": self.working_directory,
            "different_copies": self.different_copies,
        })
    }

    /// Sentences for the result text; the working directory alone is not
    /// worth one, since nearly every command runs in HOME.
    fn notes(&self) -> Vec<String> {
        let list = |paths: &mut dyn Iterator<Item = String>| paths.collect::<Vec<_>>().join(", ");
        let mut notes = Vec::new();
        if !self.different_copies.is_empty() {
            notes.push(format!(
                "The session sees a different copy of {} than the host (a host edit after session_start, or a session-side change); import_file with overwrite=true copies the host file in.",
                list(&mut self.different_copies.iter().filter_map(|copy| copy["path"].as_str().map(str::to_owned)))
            ));
        }
        if !self.overlay_paths.is_empty() {
            notes.push(format!(
                "{} {} in the session's HOME overlay: files written there stay in the session. Write outputs to host_shared_dir, pass the directory in host_write_paths to session_start, or copy them out with export_file.",
                list(&mut self.overlay_paths.iter().map(|path| path.display().to_string())),
                if self.overlay_paths.len() == 1 { "is" } else { "are" }
            ));
        }
        notes
    }
}

fn modified_unix(metadata: &std::fs::Metadata) -> Option<f64> {
    let modified = metadata.modified().ok()?;
    Some(
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64(),
    )
}

/// What a launch's file arguments look like through the session's HOME
/// overlay. `root` is the session's root seen from the host; without it only
/// the overlay paths are reported.
fn home_overlay_report(
    command: &str,
    home: &Path,
    cwd: Option<&Path>,
    host_write_paths: &[PathBuf],
    root: Option<&Path>,
) -> HomeOverlayReport {
    let in_overlay = |path: &Path| {
        path.starts_with(home)
            && path != home
            && !host_write_paths
                .iter()
                .any(|shared| path.starts_with(shared))
    };
    let mut report = HomeOverlayReport {
        overlay_paths: launch_path_words(command)
            .into_iter()
            .filter(|path| in_overlay(path))
            .collect(),
        working_directory: cwd.filter(|cwd| in_overlay(cwd)).map(Path::to_owned),
        different_copies: Vec::new(),
    };
    let Some(root) = root else {
        return report;
    };
    for path in launch_file_words(command, cwd)
        .into_iter()
        .filter(|path| in_overlay(path))
        .take(LAUNCH_OVERLAY_CHECK_LIMIT)
    {
        let Ok(host) = std::fs::metadata(&path) else {
            continue;
        };
        if !host.is_file() {
            continue;
        }
        let session =
            file_import::session_file(root, &path, false).and_then(|file| file.metadata());
        let same = match &session {
            Ok(session) if session.len() == host.len() => {
                session.modified().ok() == host.modified().ok()
                    // An imported copy has the host's bytes and a new mtime.
                    || (host.len() <= LAUNCH_OVERLAY_COMPARE_BYTES
                        && same_session_contents(root, &path).unwrap_or(false))
            }
            _ => false,
        };
        if !same {
            report.different_copies.push(serde_json::json!({
                "path": path,
                "host_bytes": host.len(),
                "host_modified_unix": modified_unix(&host),
                "session_bytes": session.as_ref().ok().map(std::fs::Metadata::len),
                "session_modified_unix": session.as_ref().ok().and_then(modified_unix),
            }));
        }
    }
    report
}

/// Adds the overlay report to a launch result or error.
fn attach_home_overlay_report(
    result: Result<CallToolResult, McpError>,
    report: &HomeOverlayReport,
) -> Result<CallToolResult, McpError> {
    if report.is_empty() {
        return result;
    }
    let notes = report.notes();
    match result {
        Err(mut error) => {
            let mut data = match error.data.take() {
                Some(serde_json::Value::Object(data)) => data,
                Some(value) => serde_json::Map::from_iter([("original_data".to_owned(), value)]),
                None => serde_json::Map::new(),
            };
            data.insert("home_overlay".to_owned(), report.as_json());
            error.data = Some(serde_json::Value::Object(data));
            if !notes.is_empty() {
                error.message = format!("{} {}", error.message, notes.join(" ")).into();
            }
            Err(error)
        }
        Ok(mut result) => {
            if let Some(data) = result
                .structured_content
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                data.insert("home_overlay".to_owned(), report.as_json());
            }
            if !notes.is_empty() {
                result.content.push(Content::text(notes.join(" ")));
            }
            Ok(result)
        }
    }
}

/// The session path each absolute launch argument must reach before the program
/// starts, paired with the argument it reports. The session shell checks these
/// because mounts such as /tmp can hide host files even when the spelling is
/// unchanged. An argument that exists on the host is probed as itself; one the
/// command may create is probed through its parent directory. An argument whose
/// host parent is also missing is not probed, since the command may create that.
fn launch_path_probes(command: &str, host_has: impl Fn(&Path) -> bool) -> Vec<(PathBuf, PathBuf)> {
    let mut probes = Vec::new();
    for path in launch_path_words(command) {
        let probe = if host_has(&path) {
            path.clone()
        } else {
            match path.parent() {
                Some(parent) if host_has(parent) => parent.to_owned(),
                _ => continue,
            }
        };
        probes.push((path, probe));
    }
    probes
}

/// What a launch that exits without a window left in the session's stderr: the
/// last lines it wrote and the sandbox.log line that names one of its paths.
#[derive(Debug, PartialEq, Eq)]
struct ExitDiagnostics {
    stderr: Vec<String>,
    sandbox_line: Option<String>,
    unavailable: Option<String>,
}

/// The session's stderr from `offset` on: the bytes sandbox.log gained since the
/// command was submitted, at most the last LAUNCH_STDERR_READ_BYTES of them.
fn sandbox_log_since(path: &Path, offset: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = offset.max(length.saturating_sub(LAUNCH_STDERR_READ_BYTES));
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > offset {
        // Reading from inside the output starts mid-line, so drop that partial line.
        text = match text.find('\n') {
            Some(newline) => text.split_off(newline + 1),
            None => String::new(),
        };
    }
    Ok(text)
}

/// The newest line that names one of the command's paths or reports a missing or
/// denied file, so a path the session cannot see is named in the result.
fn relevant_sandbox_line(output: &str, command_paths: &[PathBuf]) -> Option<String> {
    let names: Vec<String> = command_paths
        .iter()
        .filter(|path| path.as_path() != Path::new("/"))
        .map(|path| path.display().to_string())
        .collect();
    output
        .lines()
        .rev()
        .find(|line| {
            line.contains("No such file or directory")
                || line.contains("Permission denied")
                || names.iter().any(|name| line.contains(name.as_str()))
        })
        .map(str::to_owned)
}

fn exit_diagnostics(output: std::io::Result<String>, command_paths: &[PathBuf]) -> ExitDiagnostics {
    match output {
        Ok(text) => {
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            let shown = lines.len().saturating_sub(LAUNCH_STDERR_TAIL_LINES);
            ExitDiagnostics {
                stderr: lines[shown..]
                    .iter()
                    .map(|line| (*line).to_owned())
                    .collect(),
                sandbox_line: relevant_sandbox_line(&text, command_paths),
                unavailable: None,
            }
        }
        Err(error) => ExitDiagnostics {
            stderr: Vec::new(),
            sandbox_line: None,
            unavailable: Some(error.to_string()),
        },
    }
}

/// The lines a launch that exited without a window adds to its result.
fn exit_diagnostics_text(diagnostics: &ExitDiagnostics) -> String {
    let mut text = String::new();
    if let Some(error) = &diagnostics.unavailable {
        text.push_str(&format!("\nstderr unavailable: {error}"));
    } else if diagnostics.stderr.is_empty() {
        text.push_str("\nno stderr was written");
    } else {
        let count = diagnostics.stderr.len();
        let noun = if count == 1 { "line" } else { "lines" };
        text.push_str(&format!(
            "\nstderr (last {count} {noun}):\n{}",
            diagnostics.stderr.join("\n")
        ));
    }
    // A relevant line already in the tail needs no second copy.
    if let Some(line) = diagnostics
        .sandbox_line
        .as_ref()
        .filter(|line| !diagnostics.stderr.contains(*line))
    {
        text.push_str(&format!("\nsandbox.log: {line}"));
    }
    text
}

/// The DRM device nodes to show the session when the host's NVIDIA nodes are
/// useless to it, or `None` when `/dev/dri` and `/dev/nvidia*` should be bound
/// whole. Mesa cannot drive a node owned by the proprietary `nvidia` kernel
/// driver unless the NVIDIA GBM backend is installed; KWin would probe every
/// such node for seconds and still composite with llvmpipe. Without a render
/// node KWin falls back to QPainter, which cannot serve screenshots, so one
/// NVIDIA render node stays when the host has no other.
fn usable_dri_nodes(nvidia_gbm_installed: bool) -> Option<Vec<PathBuf>> {
    if nvidia_gbm_installed {
        return None;
    }
    let entries = std::fs::read_dir("/dev/dri").ok()?;
    let mut other = Vec::new();
    let mut nvidia = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        if is_nvidia_dri_node(&entry.path()) {
            nvidia.push(entry.path())
        } else {
            other.push(entry.path())
        }
    }
    if nvidia.is_empty() {
        return None;
    }
    let is_render = |node: &PathBuf| {
        node.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
    };
    if !other.iter().any(is_render) {
        nvidia.sort();
        other.extend(nvidia.into_iter().find(is_render));
    }
    Some(other)
}

fn is_nvidia_dri_node(node: &Path) -> bool {
    let Some(name) = node.file_name() else {
        return false;
    };
    std::fs::read_link(Path::new("/sys/class/drm").join(name).join("device/driver"))
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .is_some_and(|driver| driver == "nvidia")
}

fn nvidia_gbm_installed() -> bool {
    [
        "/usr/lib/gbm",
        "/usr/lib64/gbm",
        "/usr/lib/x86_64-linux-gnu/gbm",
    ]
    .iter()
    .any(|dir| Path::new(dir).join("nvidia-drm_gbm.so").exists())
}

/// Whether `command` starts a Chromium-family browser (leading VAR=value words
/// skipped); editors built on Electron do not count.
fn launches_chromium(command: &str) -> bool {
    command
        .split_whitespace()
        .find(|word| !word.contains('='))
        .and_then(|program| Path::new(program).file_name())
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| {
            BROWSER_COMMANDS.contains(&name)
                && !matches!(name, "code" | "codium" | "vscodium" | "electron")
        })
}

/// Whether the command asks branded Google Chrome to load an unpacked
/// extension. Current Google Chrome builds ignore this switch even when the
/// DisableLoadExtensionCommandLineSwitch feature is disabled.
fn requests_blocked_google_chrome_extension_load(command: &str) -> bool {
    let is_google_chrome = command
        .split_whitespace()
        .find(|word| !word.contains('='))
        .map(|program| program.trim_matches(|character| character == '\'' || character == '"'))
        .and_then(|program| Path::new(program).file_name())
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| matches!(name, "google-chrome" | "google-chrome-stable"));
    let requests_extension_load = command.split_whitespace().any(|argument| {
        let argument = argument.trim_matches(|character| character == '\'' || character == '"');
        argument == "--load-extension" || argument.starts_with("--load-extension=")
    });
    is_google_chrome && requests_extension_load
}

const BROWSER_COMMANDS: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chrome",
    "chromium",
    "chromium-browser",
    "brave",
    "brave-browser",
    "vivaldi",
    "vivaldi-stable",
    "microsoft-edge",
    "microsoft-edge-stable",
    "msedge",
    "code",
    "codium",
    "vscodium",
    "electron",
];

// Bash resolves the browser itself, so compound commands and wrappers need no
// second shell parser in the MCP server.
const BROWSER_WRAPPER: &str = r#"#!/usr/bin/env bash
set -e
name=${0##*/}
real=$(PATH="${PATH#*:}" command -v "$name") || real=
if [[ -z $real ]]; then
  # The same browser often installs under only one of its names.
  case "$name" in
    google-chrome|google-chrome-stable) alts="google-chrome-stable google-chrome" ;;
    chromium|chromium-browser) alts="chromium chromium-browser" ;;
    *) alts= ;;
  esac
  for alt in $alts; do real=$(PATH="${PATH#*:}" command -v "$alt") && break || real=; done
fi
[[ -n $real ]] || exit 127
ozone=0 wallet=0 a11y=0 silent_debugger=0 test_type=0 custom_profile=0 profile_value=0
first_run=0 default_check=0
for arg in "$@"; do
  if (( profile_value )); then
    [[ -n "$arg" ]] && custom_profile=1
    profile_value=0
  fi
  case "$arg" in
    --ozone-platform|--ozone-platform=*) ozone=1 ;;
    --password-store|--password-store=*) wallet=1 ;;
    --force-renderer-accessibility|--force-renderer-accessibility=*) a11y=1 ;;
    --user-data-dir) profile_value=1 ;;
    --user-data-dir=*) [[ -n "${arg#*=}" ]] && custom_profile=1 ;;
    --silent-debugger-extension-api) silent_debugger=1 ;;
    --test-type) test_type=1 ;;
    --no-first-run) first_run=1 ;;
    --no-default-browser-check) default_check=1 ;;
  esac
done
flags=()
(( ozone )) || flags+=(--ozone-platform=wayland)
(( wallet )) || flags+=(--password-store=kwallet6)
(( a11y )) || flags+=(--force-renderer-accessibility)
# Extensions using chrome.debugger (Claude in Chrome, bridges) otherwise pin a
# "started debugging this browser" infobar on every window, and flags Chrome
# calls unsupported (--no-sandbox) pin another; --test-type silences it.
case "$name" in
  code|codium|vscodium|electron) ;;
  *)
    (( silent_debugger )) || flags+=(--silent-debugger-extension-api)
    (( test_type )) || flags+=(--test-type)
    # A fresh profile otherwise opens Chrome's first-run terms page in place of
    # the requested URL, and its default-browser infobar shifts page content.
    (( first_run )) || flags+=(--no-first-run)
    (( default_check )) || flags+=(--no-default-browser-check)
    ;;
esac
cdp=1
case "$name" in
  google-chrome*|chrome|microsoft-edge*|msedge*) (( custom_profile )) || cdp=0 ;;
esac
if [[ -n ${KWIN_MCP_CDP_PORT:-} ]] && (( cdp )); then
  # The server connects through the port reserved and forwarded for this
  # session. Replace caller-supplied ports so the browser and server agree.
  browser_args=()
  skip_debug_port_value=0
  for arg in "$@"; do
    if (( skip_debug_port_value )); then
      skip_debug_port_value=0
      continue
    fi
    case "$arg" in
      --remote-debugging-port) skip_debug_port_value=1 ;;
      --remote-debugging-port=*) ;;
      *) browser_args+=("$arg") ;;
    esac
  done
  flags+=("--remote-debugging-port=$KWIN_MCP_CDP_PORT")
  [[ -z ${KWIN_MCP_BROWSER_MARKER:-} ]] || printf cdp > "$KWIN_MCP_BROWSER_MARKER"
  set -- "${browser_args[@]}"
fi
exec "$real" "${flags[@]}" "$@"
"#;

fn create_browser_wrappers(workdir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let bin = workdir.join("browser-bin");
    std::fs::create_dir_all(&bin)?;
    let wrapper = bin.join("kwin-browser");
    std::fs::write(&wrapper, BROWSER_WRAPPER)?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    for name in BROWSER_COMMANDS {
        symlink("kwin-browser", bin.join(name))?;
    }
    Ok(bin)
}

static NEXT_BROWSER_LAUNCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Browser launch commands we know how to detect. The container mounts the host
/// root read-only, so anything on the host's PATH is runnable inside the session
/// by the same command. Ordered most- to least-common so the injected hint reads
/// sensibly. `chromium` is called out separately by callers because it's the only
/// one that exposes CDP on its default profile.
const KNOWN_BROWSERS: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "brave",
    "brave-browser",
    "firefox",
    "firefox-esr",
    "vivaldi-stable",
    "vivaldi",
    "microsoft-edge-stable",
    "microsoft-edge",
    "opera",
];

/// Scan the host PATH for known browser commands. Returns the runnable command
/// names in KNOWN_BROWSERS order. Deduplicates so a name reachable from two PATH
/// entries is reported once. Runs once at server launch (see `main`).
fn detect_browsers() -> Vec<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    KNOWN_BROWSERS
        .iter()
        .filter(|name| {
            dirs.iter().any(|dir| {
                let candidate = dir.join(name);
                std::fs::metadata(&candidate)
                    .is_ok_and(|m| m.is_file() || m.file_type().is_symlink())
            })
        })
        .map(|name| (*name).to_owned())
        .collect()
}

/// The first `wayland-N` socket in `runtime` that still accepts a connection.
fn live_wayland_socket(runtime: &Path) -> Option<std::ffi::OsString> {
    let mut numbered: Vec<(u32, std::ffi::OsString)> = std::fs::read_dir(runtime)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let number = name.to_str()?.strip_prefix("wayland-")?.parse::<u32>().ok()?;
            Some((number, name))
        })
        .collect();
    numbered.sort();
    numbered
        .into_iter()
        .map(|(_, name)| name)
        .find(|name| std::os::unix::net::UnixStream::connect(runtime.join(name)).is_ok())
}

/// The host desktop's Wayland socket. logind says which session is the user's
/// active desktop, but it is a hint: a worker in a confined environment cannot
/// reach it, and a desktop started outside a logind graphical session is not
/// listed as one. Either way the desktop's socket sits in the runtime directory,
/// so look there before giving up.
async fn host_wayland() -> anyhow::Result<(PathBuf, std::ffi::OsString)> {
    use std::os::unix::fs::MetadataExt;
    let logind_error = match host_wayland_via_logind().await {
        Ok(found) => return Ok(found),
        Err(error) => error,
    };
    let runtime = match std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(runtime) => PathBuf::from(runtime),
        None => PathBuf::from(format!(
            "/run/user/{}",
            std::fs::metadata("/proc/self")?.uid()
        )),
    };
    match live_wayland_socket(&runtime) {
        Some(display) => {
            eprintln!(
                "viewer: {logind_error:#}; using {} in {} instead",
                display.to_string_lossy(),
                runtime.display()
            );
            Ok((runtime, display))
        }
        None => Err(logind_error.context(format!(
            "{} has no live wayland-N socket either",
            runtime.display()
        ))),
    }
}

async fn host_wayland_via_logind() -> anyhow::Result<(PathBuf, std::ffi::OsString)> {
    use std::os::unix::fs::FileTypeExt;
    let (inherited_display, inherited_runtime) = (
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
    );
    let socket = |runtime: &Path, display: &std::ffi::OsStr| {
        if Path::new(display).is_absolute() {
            PathBuf::from(display)
        } else {
            runtime.join(display)
        }
    };
    if let (Some(runtime), Some(display)) = (&inherited_runtime, &inherited_display) {
        let path = socket(runtime, display);
        anyhow::ensure!(
            std::fs::metadata(&path)?.file_type().is_socket(),
            "{} is not a Unix socket",
            path.display()
        );
        return Ok((runtime.clone(), display.clone()));
    }
    let system = zbus::Connection::system().await?;
    let login = zbus::Proxy::new(
        &system,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let (user_path,): (zbus::zvariant::OwnedObjectPath,) =
        login.call("GetUserByPID", &(std::process::id(),)).await?;
    let user = zbus::Proxy::new(
        &system,
        "org.freedesktop.login1",
        user_path,
        "org.freedesktop.login1.User",
    )
    .await?;
    let runtime = inherited_runtime.unwrap_or(PathBuf::from(
        user.get_property::<String>("RuntimePath").await?,
    ));
    let display = if let Some(display) = inherited_display {
        display
    } else {
        let (_, session_path): (String, zbus::zvariant::OwnedObjectPath) =
            user.get_property("Display").await?;
        let session = zbus::Proxy::new(
            &system,
            "org.freedesktop.login1",
            session_path,
            "org.freedesktop.login1.Session",
        )
        .await?;
        anyhow::ensure!(
            session.get_property::<bool>("Active").await?
                && session.get_property::<String>("State").await? == "active"
                && session.get_property::<String>("Type").await? == "wayland",
            "no active logind Wayland session"
        );
        let managed = async {
            let address = format!("unix:path={}/bus", runtime.display());
            let user_bus = zbus::connection::Builder::address(address.as_str())?
                .build()
                .await?;
            let manager = zbus::Proxy::new(
                &user_bus,
                "org.freedesktop.systemd1",
                "/org/freedesktop/systemd1",
                "org.freedesktop.systemd1.Manager",
            )
            .await?;
            anyhow::Ok(
                manager
                    .get_property::<Vec<String>>("Environment")
                    .await?
                    .into_iter()
                    .find_map(|entry| {
                        entry
                            .strip_prefix("WAYLAND_DISPLAY=")
                            .map(std::ffi::OsString::from)
                    })
                    .filter(|display| {
                        std::fs::metadata(socket(&runtime, display))
                            .is_ok_and(|meta| meta.file_type().is_socket())
                    }),
            )
        }
        .await
        .ok()
        .flatten();
        managed
            .or_else(|| {
                std::fs::read_dir(&runtime)
                    .ok()?
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name();
                        let number = name
                            .to_str()?
                            .strip_prefix("wayland-")?
                            .parse::<u32>()
                            .ok()?;
                        entry
                            .file_type()
                            .ok()?
                            .is_socket()
                            .then_some((number, name))
                    })
                    .min_by_key(|entry| entry.0)
                    .map(|entry| entry.1)
            })
            .ok_or_else(|| anyhow::anyhow!("no Wayland socket in {}", runtime.display()))?
    };
    let socket = socket(&runtime, &display);
    anyhow::ensure!(
        std::fs::metadata(&socket)?.file_type().is_socket(),
        "{} is not a Unix socket",
        socket.display()
    );
    Ok((runtime, display))
}

/// Status file the viewer writes (see src/bin/kwin-viewer.rs).
const VIEWER_STATUS_FILE: &str = "viewer-status.json";
const VIEWER_INPUT_ACTIVITY_FILE: &str = "viewer-input-activity";
const VIEWER_INPUT_QUIET_PERIOD: Duration = Duration::from_secs(1);
const VIEWER_INPUT_CHECK_INTERVAL: Duration = Duration::from_millis(20);
/// How long viewer_open waits for the first successful host render submission
/// before reporting the viewer as still starting.
const VIEWER_READY_WAIT: Duration = Duration::from_secs(6);
const VIEWER_READY_POLL: Duration = Duration::from_millis(50);
const VIEWER_STATUS_STALE_AFTER: Duration = Duration::from_secs(3);

/// Spawn the viewer as a sibling host-side process. Intentionally non-fatal:
/// if anything goes wrong the agent's MCP tools still work, and the error is
/// the reason reported in the viewer outcome. Stderr lands in
/// {session_dir}/viewer.log so crashes and input-forwarding diagnostics
/// survive past the spawn.
async fn spawn_viewer(
    host_xdg_dir: &Path,
    width: u32,
    height: u32,
) -> Result<SessionProcess, String> {
    let log_path = host_xdg_dir.join("viewer.log");
    let _ = std::fs::remove_file(host_xdg_dir.join(VIEWER_STATUS_FILE));
    let _ = std::fs::remove_file(host_xdg_dir.join("viewer-pipewire.json"));
    let mut log_file = std::fs::File::create(&log_path)
        .map_err(|error| format!("create {}: {error}", log_path.display()))?;
    let note = |log_file: &mut std::fs::File, reason: String| {
        let _ = std::io::Write::write_all(log_file, format!("kwin-viewer: {reason}\n").as_bytes());
        eprintln!("session viewer unavailable: {reason}");
        reason
    };
    let Some(bin) = resolve_viewer_binary() else {
        return Err(note(
            &mut log_file,
            "kwin-viewer binary not found next to kwin-mcp".to_owned(),
        ));
    };
    let (runtime, display) = match host_wayland().await {
        Ok(found) => found,
        Err(error) => {
            // No desktop here (a headless host): the session can still be shown
            // on another machine's screen. The command runs there, and starts
            // `kwin-viewer --serve` on this host over ssh.
            let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
            let remote = format!(
                "run on the machine with the screen: KWIN_VIEWER_REMOTE_BIN={bin} {bin} --remote {host} {dir} {width} {height}",
                bin = bin.display(),
                host = host.trim(),
                dir = host_xdg_dir.display(),
            );
            return Err(note(
                &mut log_file,
                format!("host Wayland resolution failed: {error:#}; {remote}"),
            ));
        }
    };
    let mut command = std::process::Command::new(&bin);
    command
        .arg(host_xdg_dir)
        .arg(width.to_string())
        .arg(height.to_string())
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", display)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));
    terminate_with_parent(&mut command);
    match spawn_session_child(command, false).await {
        Ok(child) => {
            eprintln!("viewer_open: spawned viewer pid={}", child.id());
            Ok(child.into())
        }
        Err(e) => {
            let reason = format!("spawn failed: {e}");
            let _ = std::fs::write(&log_path, format!("kwin-viewer: {reason}\n"));
            eprintln!("session viewer unavailable: {reason}");
            Err(reason)
        }
    }
}

/// The viewer outcome: ready (the first session frame was submitted by the
/// host renderer), starting, unavailable with a reason, or closed.
fn viewer_report(
    host_xdg_dir: &Path,
    child: Option<&mut SessionProcess>,
    unavailable: Option<&str>,
) -> serde_json::Value {
    let log = host_xdg_dir.join("viewer.log").display().to_string();
    let status: Option<serde_json::Value> =
        std::fs::read_to_string(host_xdg_dir.join(VIEWER_STATUS_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
    let field = |name: &str| {
        status
            .as_ref()
            .and_then(|value| value[name].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let Some(child) = child else {
        let reason = unavailable.unwrap_or("viewer not open");
        let state = if unavailable.is_some() {
            "unavailable"
        } else {
            "closed"
        };
        return serde_json::json!({"state": state, "reason": reason, "log": log});
    };
    if let Ok(Some(exit)) = child.try_wait() {
        let detail = field("detail");
        let reason = if detail.is_empty() {
            format!("viewer exited ({exit})")
        } else {
            format!("viewer exited ({exit}): {detail}")
        };
        return serde_json::json!({"state": "unavailable", "reason": reason, "log": log});
    }
    let phase = field("state");
    let input_activity_reporting = status
        .as_ref()
        .and_then(|value| value["input_activity_reporting"].as_bool())
        .unwrap_or(false);
    if let Some(status_pid) = status.as_ref().and_then(|value| value["pid"].as_u64())
        && status_pid != u64::from(child.id())
    {
        return serde_json::json!({
            "state": "unavailable",
            "reason": "viewer status belongs to another process",
            "pid": child.id(),
            "log": log,
        });
    }
    if phase == "ready" && !input_activity_reporting {
        return serde_json::json!({
            "state": "unavailable",
            "reason": "viewer uses an older input-reporting protocol; refresh it with viewer_open after the user's current interaction finishes",
            "pid": child.id(), "log": log,
        });
    }
    if phase == "ready"
        && (status.as_ref().and_then(|value| value["pid"].as_u64()) != Some(u64::from(child.id()))
            || !std::fs::metadata(host_xdg_dir.join(VIEWER_STATUS_FILE))
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age <= VIEWER_STATUS_STALE_AFTER))
    {
        return serde_json::json!({
            "state": "unavailable",
            "reason": "viewer window status is stale",
            "pid": child.id(),
            "log": log,
        });
    }
    match phase.as_str() {
        "ready" => {
            serde_json::json!({"state": "ready", "pid": child.id(), "log": log, "input_activity_reporting": true})
        }
        "failed" | "closed" => {
            serde_json::json!({"state": "unavailable", "reason": field("detail"), "pid": child.id(), "log": log})
        }
        phase => serde_json::json!({
            "state": "starting",
            "phase": if phase.is_empty() { "launching" } else { phase },
            "pid": child.id(),
            "log": log,
            "input_activity_reporting": input_activity_reporting,
        }),
    }
}

/// Poll the viewer until it is ready, has failed, or VIEWER_READY_WAIT passes.
async fn wait_for_viewer(host_xdg_dir: &Path, child: &mut SessionProcess) -> serde_json::Value {
    let deadline = std::time::Instant::now() + VIEWER_READY_WAIT;
    loop {
        let report = viewer_report(host_xdg_dir, Some(child), None);
        if report["state"] != "starting" || std::time::Instant::now() >= deadline {
            return report;
        }
        tokio::time::sleep(VIEWER_READY_POLL).await;
    }
}

/// One-line summary of a viewer report for tool text output.
fn viewer_summary(report: &serde_json::Value) -> String {
    match report["state"].as_str().unwrap_or_default() {
        "ready" => "viewer=ready".to_owned(),
        "closed" => "viewer=closed".to_owned(),
        "starting" => format!(
            "viewer=starting ({})",
            report["phase"].as_str().unwrap_or_default()
        ),
        state => format!(
            "viewer={state}: {}",
            report["reason"].as_str().unwrap_or_default()
        ),
    }
}

async fn active_window_info(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &std::path::Path,
) -> Result<(i32, i32, WindowGeometry), KwinError> {
    let json = run_kwin_script(
        conn,
        kwin_unique,
        host_xdg_dir,
        "var w = workspace.activeWindow;\
         var c = workspace.cursorPos;\
         var result = w ? {x:w.clientGeometry.x,y:w.clientGeometry.y,\
         w:w.clientGeometry.width,h:w.clientGeometry.height,\
         title:w.caption,id:w.internalId.toString(),\
         cx:c.x,cy:c.y} : null;",
    )
    .await?;
    if json == "null" {
        return Err(KwinError::Msg(NO_ACTIVE_WINDOW.to_owned()));
    }
    let info: WindowGeometry = serde_json::from_str(&json)?;
    #[expect(clippy::as_conversions)]
    let (x, y) = (info.x.round() as i32, info.y.round() as i32);
    Ok((x, y, info))
}

/// The active window, waiting up to `ACTIVE_WINDOW_WAIT` while focus is
/// between windows, e.g. after a click closes one dialog and opens another.
async fn awaited_active_window_info(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &std::path::Path,
) -> Result<(i32, i32, WindowGeometry), KwinError> {
    let deadline = tokio::time::Instant::now() + ACTIVE_WINDOW_WAIT;
    loop {
        match active_window_info(conn, kwin_unique, host_xdg_dir).await {
            Err(KwinError::Msg(message)) if message == NO_ACTIVE_WINDOW => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(KwinError::Msg(format!(
                        "{NO_ACTIVE_WINDOW} after waiting {} seconds; inspect window_list and call window_activate for the intended window",
                        ACTIVE_WINDOW_WAIT.as_secs()
                    )));
                }
                tokio::time::sleep(ACTIVE_WINDOW_POLL).await;
            }
            result => return result,
        }
    }
}

// KDE's show desktop (super+d) hides windows without minimizing them, and
// KWin scripts cannot see it; its D-Bus interface can.
const SHOWING_DESKTOP_WARNING: &str = "KDE show desktop (super+d) is on: windows are hidden without being minimized, so screenshots show the desktop while keys still reach the active window. window_activate or super+d again shows them.";

async fn showing_desktop(conn: &zbus::Connection, kwin_unique: &str) -> Result<bool, KwinError> {
    Ok(zbus::Proxy::new(conn, kwin_unique, "/KWin", "org.kde.KWin")
        .await?
        .get_property::<bool>("showingDesktop")
        .await?)
}

#[derive(Clone, Deserialize, Serialize)]
struct WindowSnapshot {
    id: String,
    title: String,
    #[serde(rename = "resourceClass")]
    resource_class: String,
    #[serde(rename = "resourceName")]
    resource_name: String,
    pid: i32,
    active: bool,
    minimized: bool,
    /// Not shown: KWin hid it (an auto-hide panel), or show desktop is on.
    #[serde(default)]
    hidden: bool,
    modal: bool,
    transient: bool,
    dialog: bool,
    popup: bool,
    #[serde(rename = "transientFor")]
    transient_for: Option<String>,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    #[serde(rename = "stackingOrder")]
    stacking_order: i32,
}

impl WindowSnapshot {
    fn summary(&self) -> String {
        let mut flags = Vec::new();
        if self.active {
            flags.push("active");
        }
        if self.minimized {
            flags.push("minimized");
        }
        if self.hidden {
            flags.push("hidden");
        }
        if self.modal {
            flags.push("modal");
        }
        if self.transient {
            flags.push("transient");
        }
        if self.dialog {
            flags.push("dialog");
        }
        if self.popup {
            flags.push("popup");
        }
        let flags = if flags.is_empty() {
            "normal".to_owned()
        } else {
            flags.join(",")
        };
        let transient_for = self
            .transient_for
            .as_deref()
            .map(|id| format!(" transient_for={id}"))
            .unwrap_or_default();
        format!(
            "[{flags}] id={} title={:?} app={}/{} pid={} geometry=({:.0},{:.0} {:.0}x{:.0}) stack={}{}",
            self.id,
            self.title,
            self.resource_class,
            self.resource_name,
            self.pid,
            self.x,
            self.y,
            self.w,
            self.h,
            self.stacking_order,
            transient_for,
        )
    }
}

async fn window_snapshots(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
) -> Result<Vec<WindowSnapshot>, KwinError> {
    let json = run_kwin_script(
        conn,
        kwin_unique,
        host_xdg_dir,
        "var result = [];\
         var windows = workspace.stackingOrder;\
         for (var i = windows.length - 1; i >= 0; --i) {\
           var w = windows[i];\
           if (!w.managed || w.deleted || w.desktopWindow || w.dock || w.outline) continue;\
           var g = w.clientGeometry;\
           result.push({\
             id:w.internalId.toString(),title:w.caption,\
             resourceClass:w.resourceClass,resourceName:w.resourceName,pid:w.pid,\
             active:w.active,minimized:w.minimized,hidden:w.hidden === true,\
             modal:w.modal,transient:w.transient,\
             dialog:w.dialog,popup:w.popupWindow,\
             transientFor:w.transientFor ? w.transientFor.internalId.toString() : null,\
             x:g.x,y:g.y,w:g.width,h:g.height,stackingOrder:w.stackingOrder\
           });\
         }",
    )
    .await?;
    Ok(serde_json::from_str(&json)?)
}

async fn request_window_activation(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
    window_id: &str,
) -> Result<bool, KwinError> {
    let window_id_json = serde_json::to_string(window_id)?;

    // Search the managed-window set, not the stacking order that window_list reads,
    // so the two lookups cannot disagree about whether a window is present.
    let script = format!(
        "var targetId = {window_id_json};\
         var result = false;\
         var windows = workspace.windowList();\
         for (var i = 0; i < windows.length; ++i) {{\
           var w = windows[i];\
           if (w.internalId.toString() !== targetId) continue;\
           w.minimized = false;\
           workspace.raiseWindow(w);\
           workspace.activeWindow = w;\
           result = true;\
           break;\
         }}"
    );
    Ok(serde_json::from_str(
        &run_kwin_script(conn, kwin_unique, host_xdg_dir, &script).await?,
    )?)
}

/// The UUID form KWin reports for a window: lowercase, without braces. A caller
/// may name a window with or without braces, or in another letter case.
fn canonical_window_id(window_id: &str) -> String {
    window_id
        .trim()
        .trim_matches(['{', '}'])
        .to_ascii_lowercase()
}

/// The listed window that `window_id` names.
fn find_window<'a>(windows: &'a [WindowSnapshot], window_id: &str) -> Option<&'a WindowSnapshot> {
    let wanted = canonical_window_id(window_id);
    windows
        .iter()
        .find(|window| canonical_window_id(&window.id) == wanted)
}

fn window_closed(window_id: &str) -> KwinError {
    KwinError::Msg(format!(
        "window {window_id} closed: window_list no longer lists it in this session"
    ))
}

/// The window to focus for a request that names `window`: its topmost visible,
/// non-popup transient descendant, or `window` itself when there is none.
fn activation_target<'a>(
    windows: &'a [WindowSnapshot],
    window: &'a WindowSnapshot,
) -> &'a WindowSnapshot {
    let wanted = canonical_window_id(&window.id);
    windows
        .iter()
        .find(|candidate| {
            if candidate.minimized || candidate.popup {
                return false;
            }
            let mut ancestor = candidate.transient_for.as_deref();
            let mut visited = std::collections::HashSet::new();
            while let Some(id) = ancestor {
                if canonical_window_id(id) == wanted {
                    return true;
                }
                if !visited.insert(canonical_window_id(id)) {
                    break;
                }
                ancestor =
                    find_window(windows, id).and_then(|parent| parent.transient_for.as_deref());
            }
            false
        })
        .unwrap_or(window)
}

/// A fresh listing that includes `window_id`. KWin can omit a window it just
/// listed while it restacks, so a missing window is looked for again before it
/// is reported closed.
async fn listing_with_window(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
    window_id: &str,
) -> Result<Vec<WindowSnapshot>, KwinError> {
    let mut lookups = 1;
    loop {
        let windows = window_snapshots(conn, kwin_unique, host_xdg_dir).await?;
        if find_window(&windows, window_id).is_some() {
            return Ok(windows);
        }
        if lookups >= WINDOW_LOOKUP_ATTEMPTS {
            return Err(window_closed(window_id));
        }
        lookups += 1;
        tokio::time::sleep(WINDOW_LOOKUP_RETRY).await;
    }
}

async fn activate_window(
    conn: &zbus::Connection,
    kwin_unique: &str,
    host_xdg_dir: &Path,
    requested_id: &str,
) -> Result<WindowSnapshot, KwinError> {
    // Resolve the requested ID against a listing, then activate the ID that the
    // listing reports, so one string names the window on both sides.
    let windows = listing_with_window(conn, kwin_unique, host_xdg_dir, requested_id).await?;
    let window_id = find_window(&windows, requested_id)
        .map(|window| window.id.clone())
        .ok_or_else(|| window_closed(requested_id))?;
    // Restore the parent first: KWin can also minimize its transient dialogs.
    if !request_window_activation(conn, kwin_unique, host_xdg_dir, &window_id).await? {
        return Err(window_closed(requested_id));
    }
    tokio::time::sleep(WINDOW_ACTIVATION_SETTLE).await;
    // KWin can raise a chooser along with its parent while leaving keyboard
    // focus on the parent. Resolve the topmost visible dialog descendant before
    // activation so the returned target agrees with what the user will see.
    let windows = listing_with_window(conn, kwin_unique, host_xdg_dir, &window_id).await?;
    let listed = find_window(&windows, &window_id).ok_or_else(|| window_closed(&window_id))?;
    let target_id = activation_target(&windows, listed).id.clone();
    if target_id != window_id {
        if !request_window_activation(conn, kwin_unique, host_xdg_dir, &target_id).await? {
            return Err(window_closed(&target_id));
        }
        tokio::time::sleep(WINDOW_ACTIVATION_SETTLE).await;
    }
    let windows = listing_with_window(conn, kwin_unique, host_xdg_dir, &target_id).await?;
    let window = windows
        .into_iter()
        .find(|window| window.id == target_id)
        .ok_or_else(|| window_closed(&target_id))?;
    if !window.active {
        return Err(KwinError::Msg(format!(
            "KWin did not activate window {target_id}"
        )));
    }
    Ok(window)
}

#[derive(Deserialize)]
struct WindowGeometry {
    x: f64,
    y: f64,
    #[serde(default)]
    w: f64,
    #[serde(default)]
    h: f64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    cx: f64,
    #[serde(default)]
    cy: f64,
}

fn capture_target_metadata(window: &WindowGeometry) -> serde_json::Value {
    serde_json::json!({
        "id": window.id,
        "title": window.title,
        "x": window.x,
        "y": window.y,
        "width": window.w,
        "height": window.h,
    })
}

struct AtspiNode {
    name: String,
    role: String,
    states: Vec<String>,
    bounds: (i32, i32, i32, i32),
}

impl AtspiNode {
    fn text(&self) -> String {
        format!(
            "{}\t{}\t{}\t{:?}",
            tree_field(&self.role),
            tree_field(&self.name),
            self.states.join("|"),
            self.bounds
        )
    }

    fn is_useful(&self) -> bool {
        let (x, y, w, h) = self.bounds;
        w > 1
            && h > 1
            && x > -1000000
            && y > -1000000
            && (!self.name.is_empty() || ATSPI_CONTROL_ROLES.contains(&self.role.as_str()))
    }

    /// Whether any part of the node lies on the virtual screen. Pages expose
    /// every offscreen element too (a long article is thousands of nodes).
    fn on_screen(&self, width: u32, height: u32) -> bool {
        let (x, y, w, h) = self.bounds;
        let (x, y, w, h) = (i64::from(x), i64::from(y), i64::from(w), i64::from(h));
        x + w > 0 && y + h > 0 && x < i64::from(width) && y < i64::from(height)
    }
}

fn state_labels(states: &[String]) -> Vec<String> {
    let has = |want: &str| states.iter().any(|s| s == want);
    [
        (
            has("Active") || has("Editable") || has("Checked"),
            "current",
        ),
        (has("Enabled") || has("Sensitive"), "enabled"),
        (has("Focused"), "focused"),
        (has("Focusable"), "focusable"),
        (has("ReadOnly"), "readonly"),
        (has("Transient"), "transient"),
        (has("Checkable"), "checkable"),
        (has("Showing") || has("Visible"), "visible"),
    ]
    .into_iter()
    .filter_map(|(yes, label)| yes.then_some(label.to_owned()))
    .collect()
}

/// AT-SPI role names of controls that take input, which can be disabled and
/// can be named by a placeholder alone.
const ATSPI_CONTROL_ROLES: [&str; 12] = [
    "entry",
    "password text",
    "spin button",
    "text",
    "combo box",
    "push button",
    "toggle button",
    "check box",
    "radio button",
    "slider",
    "link",
    "menu item",
];

async fn atspi_node(
    acc: &atspi::proxy::accessible::AccessibleProxy<'_>,
) -> Result<AtspiNode, KwinError> {
    use atspi::proxy::proxy_ext::ProxyExt;
    let mut name = acc.name().await.unwrap_or_default();
    let role = acc.get_role_name().await.unwrap_or_default();
    let raw_states = acc
        .get_state()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>();
    let mut states = state_labels(&raw_states);
    let control = ATSPI_CONTROL_ROLES.contains(&role.as_str());
    // A field labelled only by its placeholder has no accessible name in Chromium,
    // which exposes the placeholder as an object attribute instead.
    if name.is_empty() && control {
        name = acc
            .get_attributes()
            .await
            .unwrap_or_default()
            .remove("placeholder-text")
            .unwrap_or_default();
    }
    if control && !states.iter().any(|state| state == "enabled") {
        states.push("disabled".to_owned());
    }
    let bounds = match acc.proxies().await?.component().await {
        Ok(c) => c
            .get_extents(atspi::CoordType::Screen)
            .await
            .unwrap_or_default(),
        Err(_) => (0, 0, 0, 0),
    };
    Ok(AtspiNode {
        name,
        role,
        states,
        bounds,
    })
}

/// AT-SPI application names of Chromium-family browsers.
const CHROMIUM_APP_NAMES: [&str; 4] = ["chrom", "edge", "brave", "vivaldi"];

/// DevTools shortcuts are handled by the browser, not necessarily its web
/// page. A successful compositor receipt cannot establish that DevTools opened.
fn is_chromium_devtools_shortcut(title: &str, mods: &[u32], key: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    if !CHROMIUM_APP_NAMES
        .iter()
        .any(|browser| lower.contains(browser))
    {
        return false;
    }
    let main = key.rsplit('+').next().unwrap_or("").trim();
    (mods.is_empty() && main.eq_ignore_ascii_case("f12"))
        || (mods.len() == 2
            && mods.contains(&KEY_LEFTCTRL)
            && mods.contains(&LINUX_KEY_LEFTSHIFT)
            && (main.eq_ignore_ascii_case("i") || main.eq_ignore_ascii_case("j")))
}

#[cfg(test)]
mod shortcut_receipt_tests {
    use super::{KEY_LEFTCTRL, LINUX_KEY_LEFTSHIFT, is_chromium_devtools_shortcut};

    #[test]
    fn only_chromium_devtools_chords_need_specific_feedback() {
        let chord = [KEY_LEFTCTRL, LINUX_KEY_LEFTSHIFT];
        assert!(is_chromium_devtools_shortcut(
            "Qwen - Google Chrome",
            &chord,
            "ctrl+shift+i"
        ));
        assert!(is_chromium_devtools_shortcut(
            "Qwen - Google Chrome",
            &chord,
            "shift+ctrl+J"
        ));
        assert!(is_chromium_devtools_shortcut(
            "Qwen - Google Chrome",
            &[],
            "F12"
        ));
        assert!(!is_chromium_devtools_shortcut(
            "Qwen - Firefox",
            &chord,
            "ctrl+shift+i"
        ));
        assert!(!is_chromium_devtools_shortcut(
            "Qwen - Google Chrome",
            &chord,
            "ctrl+shift+k"
        ));
    }
}

/// True when the active window (KWin caption `active_title`) is a
/// Chromium-family browser whose page has no keyboard focus: the browser UI
/// around the showing page (DocumentWeb) of that window's frame holds a Focused
/// object, or the page holds none. Such a browser can move focus to its own UI
/// while its window stays active, and keys then never reach the page (#176).
/// False when the page has focus, the window is not such a browser, or it
/// cannot be told within KEY_FOCUS_CHECK_TIMEOUT and KEY_FOCUS_CHECK_NODES.
async fn browser_page_unfocused(conn: &zbus::Connection, active_title: &str) -> bool {
    if active_title.is_empty() {
        return false;
    }
    let check = async {
        use atspi::proxy::accessible::ObjectRefExt;
        let address = atspi::proxy::bus::BusProxy::new(conn)
            .await
            .ok()?
            .get_address()
            .await
            .ok()?;
        let bus = connect_session_bus(
            &address,
            std::time::Instant::now() + KEY_FOCUS_CHECK_TIMEOUT,
        )
        .await
        .ok()?;
        let root = atspi::proxy::accessible::AccessibleProxy::builder(&bus)
            .destination("org.a11y.atspi.Registry")
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        let mut visited = 0usize;
        for app in root.get_children().await.ok()? {
            let Ok(app) = app.as_accessible_proxy(&bus).await else {
                continue;
            };
            let name = app.name().await.unwrap_or_default().to_lowercase();
            if !CHROMIUM_APP_NAMES
                .iter()
                .any(|browser| name.contains(browser))
            {
                continue;
            }
            for frame in app.get_children().await.unwrap_or_default() {
                let Ok(frame) = frame.as_accessible_proxy(&bus).await else {
                    continue;
                };
                // Chromium sets no Active state on its frames; the KWin caption of the
                // active window starts its frame name (which adds the profile name).
                if !input_bridge::caption_starts_with(
                    &frame.name().await.unwrap_or_default(),
                    active_title,
                ) {
                    continue;
                }
                let mut stack: Vec<(atspi::ObjectRefOwned, bool)> = frame
                    .get_children()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|child| (child, false))
                    .collect();
                let (mut page_seen, mut focused_in_page, mut focused_outside) =
                    (false, false, false);
                while let Some((object, in_page)) = stack.pop() {
                    visited += 1;
                    if visited > KEY_FOCUS_CHECK_NODES {
                        return None;
                    }
                    let Ok(node) = object.as_accessible_proxy(&bus).await else {
                        continue;
                    };
                    let states = node.get_state().await.unwrap_or_default();
                    let page = node
                        .get_role()
                        .await
                        .is_ok_and(|role| role == atspi::Role::DocumentWeb)
                        && states.contains(atspi::State::Showing);
                    let in_page = in_page || page;
                    page_seen |= page;
                    if states.contains(atspi::State::Focused) {
                        if in_page {
                            focused_in_page = true
                        } else {
                            focused_outside = true
                        }
                    }
                    for child in node.get_children().await.unwrap_or_default() {
                        stack.push((child, in_page));
                    }
                }
                // Chromium leaves Focused on the page while its own UI (the address bar)
                // holds focus, so focus outside the page wins.
                return Some(page_seen && (focused_outside || !focused_in_page));
            }
        }
        Some(false)
    };
    matches!(
        tokio::time::timeout(KEY_FOCUS_CHECK_TIMEOUT, check).await,
        Ok(Some(true))
    )
}

fn tree_field(value: &str) -> String {
    value
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// An accessibility tree gathered breadth-first, rendered as whole levels.
#[derive(Default)]
struct LevelTree {
    nodes: Vec<LevelNode>,
}

struct LevelNode {
    text: String,
    id: String,
    depth: usize,
    children: Vec<usize>,
}

impl LevelTree {
    fn push(&mut self, text: String, id: String, parent: Option<usize>) -> usize {
        let at = self.nodes.len();
        let depth = parent
            .and_then(|p| self.nodes.get(p))
            .map_or(0, |p| p.depth + 1);
        self.nodes.push(LevelNode {
            text,
            id,
            depth,
            children: Vec::new(),
        });
        if let Some(parent) = parent.and_then(|p| self.nodes.get_mut(p)) {
            parent.children.push(at);
        }
        at
    }

    fn levels(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| node.depth + 1)
            .max()
            .unwrap_or(0)
    }

    fn line_cost(node: &LevelNode) -> usize {
        node.depth + node.text.chars().count() + 1
    }

    fn suffix(node: &LevelNode) -> String {
        format!("\t+{} children\troot={}", node.children.len(), node.id)
    }

    /// How many whole levels fit in A11Y_TREE_MAX_CHARS, counting the child
    /// notes the last one carries. An oversized root level does not fit.
    fn fitting_levels(&self, max_levels: usize) -> usize {
        self.fitting_partial_levels(max_levels, false)
    }

    fn fitting_partial_levels(&self, max_levels: usize, incomplete: bool) -> usize {
        let mut cost = vec![0usize; self.levels()];
        let mut notes = vec![0usize; self.levels()];
        for node in &self.nodes {
            cost[node.depth] += Self::line_cost(node);
            if incomplete {
                cost[node.depth] += format!("\tmore may follow\troot={}", node.id)
                    .chars()
                    .count();
            }
            if !node.children.is_empty() {
                notes[node.depth] += Self::suffix(node).chars().count();
            }
        }
        let mut total = 0;
        let mut fit = 0;
        for level in 0..cost.len().min(max_levels) {
            total += cost[level];
            if total + notes[level] > A11Y_TREE_MAX_CHARS {
                break;
            }
            fit = level + 1;
        }
        fit
    }

    /// Whether every level gathered so far still fits, so one more is worth
    /// reading (it either fits too or gives the last level its child counts).
    fn open_level(&self) -> bool {
        self.fitting_levels(usize::MAX) == self.levels()
    }

    fn render(&self, max_levels: usize) -> (String, serde_json::Value) {
        self.render_partial(max_levels, false)
    }

    fn render_partial(&self, max_levels: usize, incomplete: bool) -> (String, serde_json::Value) {
        let fit = self.fitting_partial_levels(max_levels, incomplete);
        let mut out = String::new();
        let mut shown = 0usize;
        let roots: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].depth == 0)
            .collect();
        let mut stack: Vec<usize> = roots.into_iter().rev().collect();
        while let Some(at) = stack.pop() {
            let node = &self.nodes[at];
            if node.depth >= fit {
                continue;
            }
            out.push_str(&"\t".repeat(node.depth));
            out.push_str(&node.text);
            if incomplete {
                out.push_str(&format!("\tmore may follow\troot={}", node.id));
            } else if node.depth + 1 == fit && !node.children.is_empty() {
                out.push_str(&Self::suffix(node));
            }
            out.push('\n');
            shown += 1;
            stack.extend(node.children.iter().rev());
        }
        let deeper = incomplete || self.levels() > fit;
        let chars = out.chars().count();
        // Some MCP clients show only structuredContent and drop the text
        // content, so the tree rides in both.
        let info = serde_json::json!({
            "levels": fit, "nodes": shown, "chars": chars, "more_below": deeper, "tree": out,
        });
        (out, info)
    }
}

// ── Tool parameter structs ──────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ScreenshotParams {
    /// Crop region [x1, y1, x2, y2] in window-relative pixels (the same space
    /// as mouse input). It may extend past the window edges, e.g. negative
    /// values to include a popup; it is clamped to the screen. Omit to capture
    /// the whole active window.
    #[serde(default)]
    region: Option<[i32; 4]>,
    /// When true, return a 10x-upscaled crop centered on the current cursor
    /// position. Useful for confirming exactly what the cursor is hovering or
    /// where a click would land. Mutually exclusive with `region`.
    #[serde(default)]
    cursor: bool,
    /// Whether to draw the pointer in the image. Defaults to true. Set false
    /// for pixel comparisons or clean captures without moving the pointer.
    /// Independent of `cursor`, which selects a crop around its position.
    #[serde(default)]
    include_cursor: Option<bool>,
    /// When true, attach the PNG inline (base64) to the tool result so the
    /// calling model can see the image directly without a follow-up read.
    /// The file is still written to disk either way.
    #[serde(default)]
    inline: bool,
    /// For inline captures, also expose metadata in structuredContent. Opt-in:
    /// some clients hide image content when structuredContent is present.
    /// Metadata remains in a JSON text block either way. File captures always
    /// include structuredContent, regardless of this option.
    #[serde(default)]
    structured_metadata: bool,
    /// Absolute host path to also save this PNG to, so the next screenshot
    /// does not replace it. The file must not exist; its directory must.
    #[serde(default)]
    save_as: Option<String>,
}

const GENERATED_SECRET_MIME: &str = kwin_mcp::clipboard::GENERATED_SECRET_MIME;
const CLIPBOARD_TEXT_MIME: &str = "text/plain;charset=utf-8";

#[derive(Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ClipboardTarget {
    Session,
    Host,
}
impl ClipboardTarget {
    fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Host => "host",
        }
    }
}
#[derive(Clone, Copy, Deserialize, schemars::JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum SecretCharacterClass {
    Lowercase,
    Uppercase,
    Digits,
    Symbols,
}
impl SecretCharacterClass {
    fn alphabet(self) -> &'static [u8] {
        match self {
            Self::Lowercase => b"abcdefghijklmnopqrstuvwxyz",
            Self::Uppercase => b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            Self::Digits => b"0123456789",
            Self::Symbols => b"!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
        }
    }
}
#[derive(Deserialize, schemars::JsonSchema)]
struct GenerateSecretParams {
    /// Number of characters, from 1 to 4096. Must cover every selected class.
    #[schemars(range(min = 1, max = 4096))]
    length: usize,
    /// Every selected class appears at least once. Duplicate classes are ignored.
    character_classes: Vec<SecretCharacterClass>,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct ClipboardWriteParams {
    /// Both targets require a live isolated session. Session is its private
    /// clipboard; host is the server desktop clipboard used by local viewer
    /// copy/paste, not a remote viewer client's clipboard.
    target: ClipboardTarget,
    /// Plain UTF-8 text. Specify exactly one of text and generate_secret.
    text: Option<String>,
    /// Generate on the server without returning the value to the agent.
    generate_secret: Option<GenerateSecretParams>,
}

fn secret_index(upper: usize) -> Result<usize, McpError> {
    use rand::TryRngCore;
    let upper = u32::try_from(upper).map_err(KwinError::from)?;
    let limit = u32::MAX - u32::MAX % upper;
    loop {
        let value = rand::rngs::OsRng.try_next_u32().map_err(|error| {
            McpError::internal_error(format!("system randomness unavailable: {error}"), None)
        })?;
        if value < limit {
            return usize::try_from(value % upper).map_err(|error| KwinError::from(error).into());
        }
    }
}
fn generate_secret(params: GenerateSecretParams) -> Result<String, McpError> {
    let classes: std::collections::BTreeSet<_> = params.character_classes.into_iter().collect();
    if classes.is_empty() || params.length < classes.len() || params.length > 4096 {
        return Err(McpError::invalid_params(
            "secret length must be 1..4096 and include at least one character from every selected class",
            None,
        ));
    }
    let mut alphabet = Vec::new();
    let mut bytes = Vec::with_capacity(params.length);
    for class in classes {
        let characters = class.alphabet();
        bytes.push(characters[secret_index(characters.len())?]);
        alphabet.extend_from_slice(characters);
    }
    while bytes.len() < params.length {
        bytes.push(alphabet[secret_index(alphabet.len())?]);
    }
    for end in (1..bytes.len()).rev() {
        let other = secret_index(end + 1)?;
        bytes.swap(end, other);
    }
    Ok(bytes.into_iter().map(char::from).collect())
}

#[derive(Clone, Serialize, Deserialize)]
struct ClipboardRestore {
    socket: PathBuf,
    text: String,
    secret: bool,
    #[serde(default)]
    primary: bool,
}
struct ClipboardEntry {
    clipboard: kwin_mcp::clipboard::Clipboard,
    saved: Option<(u64, Option<u64>, ClipboardRestore)>,
}
#[derive(Default)]
struct ClipboardStore {
    entries: std::collections::BTreeMap<PathBuf, ClipboardEntry>,
}
impl ClipboardStore {
    fn entry(&mut self, socket: &Path, label: &'static str) -> anyhow::Result<&mut ClipboardEntry> {
        use std::collections::btree_map::Entry;
        Ok(match self.entries.entry(socket.to_owned()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let stream = std::os::unix::net::UnixStream::connect(socket)?;
                let conn = wayland_client::Connection::from_socket(stream)?;
                let clipboard = kwin_mcp::clipboard::Clipboard::spawn(
                    conn,
                    label,
                    &kwin_mcp::clipboard::Shutdown::default(),
                )?;
                entry.insert(ClipboardEntry {
                    clipboard,
                    saved: None,
                })
            }
        })
    }
    fn write(
        &mut self,
        socket: PathBuf,
        label: &'static str,
        text: String,
        secret: bool,
        primary: bool,
    ) -> anyhow::Result<usize> {
        let length = text.chars().count();
        let mut contents = vec![(CLIPBOARD_TEXT_MIME.to_owned(), text.as_bytes().to_vec())];
        if secret {
            contents.push((
                GENERATED_SECRET_MIME.to_owned(),
                length.to_string().into_bytes(),
            ));
        }
        let entry = self.entry(&socket, label)?;
        let contents = Arc::new(contents);
        entry.clipboard.write(Arc::clone(&contents))?;
        if primary {
            entry.clipboard.write_primary(contents)?;
        }
        let generation = entry.clipboard.generation()?;
        let primary_generation = primary
            .then(|| entry.clipboard.primary_generation())
            .transpose()?;
        entry.saved = Some((
            generation,
            primary_generation,
            ClipboardRestore {
                socket,
                text,
                secret,
                primary,
            },
        ));
        Ok(length)
    }
    fn saved(&self) -> anyhow::Result<Vec<ClipboardRestore>> {
        let mut saved = Vec::new();
        for entry in self.entries.values() {
            if let Some((generation, primary_generation, contents)) = &entry.saved {
                let clipboard_unchanged = entry.clipboard.generation()? == *generation;
                let primary_unchanged = match primary_generation {
                    Some(generation) => entry.clipboard.primary_generation()? == *generation,
                    None => true,
                };
                if clipboard_unchanged && primary_unchanged {
                    saved.push(contents.clone());
                }
            }
        }
        Ok(saved)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ViewerOpenParams {
    /// A short instruction for the user. Displayed only in the viewer title.
    reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseClickParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    /// Smaller target dimension in pixels for movement timing. Default 20.
    target_width: Option<f64>,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
    button: Option<String>,
    double: Option<bool>,
    triple: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseMoveParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    /// Smaller target dimension in pixels for movement timing. Default 20.
    target_width: Option<f64>,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseScrollParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    /// Smaller target dimension in pixels for movement timing. Default 20.
    target_width: Option<f64>,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    y: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    delta: i32,
    horizontal: Option<bool>,
    discrete: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MouseDragParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    /// Smaller target dimension in pixels for movement timing. Default 20.
    target_width: Option<f64>,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    from_x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    from_y: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    to_x: i32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    to_y: i32,
    button: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyboardTypeParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyboardPasteParams {
    text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KeyboardKeyParams {
    /// Timing multiplier. Default 1.0; larger values send input faster.
    speed: Option<f64>,
    key: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExportFileParams {
    /// Absolute path of one regular file as the session sees it, e.g. a finished
    /// download (/home/<user>/Downloads/report.pdf). Archive directories inside
    /// the session first, or export their files individually.
    session_path: String,
    /// Absolute host path to write, or an existing host directory to write
    /// into under the same file name. Omit to use session_path itself, which
    /// hands a download over to the real directory it names.
    host_path: Option<String>,
    /// Replace an existing host file. Default false: an existing file is left
    /// alone and the export fails.
    #[serde(default)]
    overwrite: bool,
    /// Create missing host parent directories of host_path. Default false: a
    /// missing directory fails the export without writing anything.
    #[serde(default)]
    create_parents: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ImportFileParams {
    /// Absolute path of a regular file on the host.
    host_path: String,
    /// Absolute session file path. Defaults to host_path. Its parent directory
    /// must already exist in the session and be writable there.
    session_path: Option<String>,
    /// Replace an existing session file, including private edits. Default false.
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WaitForFileParams {
    /// Absolute path of the file as the session sees it.
    session_path: String,
    /// Maximum wait in seconds. Defaults to 30; accepted range is 1–300.
    timeout_seconds: Option<u64>,
    /// Require the file's inode, size, and modification time to remain unchanged
    /// for this many milliseconds. Defaults to 500; accepted range is 100–5000.
    stable_for_ms: Option<u64>,
}

#[derive(Clone, Copy)]
struct FileReadinessObservation {
    ready: bool,
    exists: bool,
    size_bytes: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileRevision {
    inode: u64,
    size_bytes: u64,
    modified: Option<std::time::SystemTime>,
}

enum FileReadinessError {
    NotRegularFile,
    Inspect(String),
}

async fn wait_for_file_stability(
    path: &Path,
    timeout: Duration,
    stable_for: Duration,
) -> Result<FileReadinessObservation, FileReadinessError> {
    use std::os::unix::fs::MetadataExt;

    let poll_interval = Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut previous_revision = None;
    let mut stable_since = None;
    let mut exists;
    let mut size_bytes;

    loop {
        match tokio::fs::metadata(path).await {
            Ok(metadata) if !metadata.is_file() => {
                return Err(FileReadinessError::NotRegularFile);
            }
            Ok(metadata) => {
                let now = tokio::time::Instant::now();
                exists = true;
                size_bytes = Some(metadata.len());
                let revision = FileRevision {
                    inode: metadata.ino(),
                    size_bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                };
                if previous_revision == Some(revision) {
                    if now < deadline
                        && stable_since.is_some_and(|started| {
                            now.saturating_duration_since(started) >= stable_for
                        })
                    {
                        return Ok(FileReadinessObservation {
                            ready: true,
                            exists,
                            size_bytes,
                        });
                    }
                } else {
                    previous_revision = Some(revision);
                    stable_since = Some(now);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let now = tokio::time::Instant::now();
                exists = false;
                size_bytes = None;
                previous_revision = None;
                stable_since = None;
                if now >= deadline {
                    return Ok(FileReadinessObservation {
                        ready: false,
                        exists,
                        size_bytes,
                    });
                }
            }
            Err(error) => {
                return Err(FileReadinessError::Inspect(format!(
                    "cannot inspect the session file: {error}"
                )));
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(FileReadinessObservation {
                ready: false,
                exists,
                size_bytes,
            });
        }
        tokio::time::sleep(poll_interval.min(deadline.saturating_duration_since(now))).await;
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct LaunchAppParams {
    command: String,
    /// Seconds to wait for a new window. Defaults to 15; accepted range is 1–120.
    window_timeout_seconds: Option<u64>,
    /// Set false for a command that backgrounds itself or never maps a window.
    /// Once its PID is recorded, the call waits up to one second for an exit
    /// status, then returns promptly with the PID if it is still running.
    /// Defaults to true.
    expect_window: Option<bool>,
}

/// The PID the command's shell wrote to its launch file. Zero and text that is
/// not a number, such as a partially written file, are not PIDs.
fn parse_launch_pid(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|pid| *pid != 0)
}

fn newly_mapped_window<'a>(
    previous: &[WindowSnapshot],
    current: &'a [WindowSnapshot],
) -> Option<&'a WindowSnapshot> {
    current
        .iter()
        .filter(|window| !previous.iter().any(|before| before.id == window.id))
        .max_by_key(|window| (window.active, window.stacking_order))
}

fn launch_window_snapshot(windows: &[WindowSnapshot]) -> serde_json::Value {
    serde_json::json!(windows)
}

/// Waits up to LAUNCH_PID_TIMEOUT for the command shell to record its PID. The
/// shell writes it before running the command, so this normally returns within
/// one poll interval.
async fn poll_launch_pid(path: &Path, path_error: &Path, exit_status: &Path) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + LAUNCH_PID_TIMEOUT;
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_launch_pid(&text))
        {
            return Some(pid);
        }
        if path_error.exists() || exit_status.exists() || tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
    }
}

/// Read a fast command's exit status when it becomes available. A command that
/// keeps running still returns promptly after the short grace period.
async fn poll_launch_exit_status(path: &Path, timeout: Duration) -> Option<i32> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            return Some(status);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        tokio::time::sleep(LAUNCH_POLL_INTERVAL.min(deadline - now)).await;
    }
}

/// Warning for a Chromium-family launch without the session wallet, which
/// cannot decrypt the copied profile's cookies and saved logins.
fn launch_wallet_warning(wallet_note: Option<String>, command: &str) -> Option<String> {
    wallet_note.filter(|_| launches_chromium(command)).map(|reason| {
        format!("warning: the session has no wallet ({reason}); this browser cannot decrypt the copied profile, so sites may show signed out. To use the saved logins, start and unlock the host's KWallet before session_start (kwin-mcp never starts or unlocks it); otherwise sign in inside the session")
    })
}

/// The error for a command whose host-side path the session cannot see. The
/// session's shell records such a path before it runs the command.
fn invisible_launch_path(path_error_file: &Path) -> Option<McpError> {
    let path = std::fs::read_to_string(path_error_file).ok();
    let _ = std::fs::remove_file(path_error_file);
    let path = path?;
    // The marker lives in the session workdir, which is host_shared_dir: the
    // one host directory the session sees at the same path.
    let shared = path_error_file.parent();
    let detail = match shared {
        Some(shared) if Path::new(&path).starts_with("/tmp") => format!(
            "/tmp is private to the session; copy what the command needs into host_shared_dir {}, which the host and the session see at the same path, or copy a host file into the session with import_file",
            shared.display()
        ),
        Some(shared) => format!(
            "use a path available inside the session, such as one under host_shared_dir {}, or copy the host file into the session with import_file (session_path defaults to the same path)",
            shared.display()
        ),
        None => {
            "use a path available inside the session, or copy the host file in with import_file"
                .to_owned()
        }
    };
    Some(McpError::invalid_params(
        format!(
            "{path} is not visible in the session, so the command did not start; {detail}. A path the command creates inside its own namespace (bwrap --tmpfs, --dir) is checked as written: launch a script file under host_shared_dir instead"
        ),
        Some(serde_json::json!({
            "reason": "path_not_visible", "path": path, "command_submitted": true,
            "command_started": false,
            "host_shared_dir": shared, "import_tool": "import_file",
        })),
    ))
}

async fn poll_launch_window(
    conn: zbus::Connection,
    kwin_unique: String,
    xdg: PathBuf,
    previous: Option<String>,
    previous_windows: Option<Vec<WindowSnapshot>>,
    chromium_launch: bool,
    exit_file: PathBuf,
) -> Result<Option<LaunchWindow>, McpError> {
    let mut pending_reused: Option<(String, String)> = None;
    loop {
        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
        let exit_status = std::fs::read_to_string(&exit_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        // An existing window is the launch's window only when the command has
        // already handed off to it and exited. A command that is still running
        // has not opened that window, so its activation for another reason
        // (focus returning, another client raising it) says nothing about it.
        let handed_off = exit_status == Some(0);
        let current_windows = window_snapshots(&conn, &kwin_unique, &xdg).await;
        if let Ok(windows) = &current_windows {
            if let Some(window) = previous_windows
                .as_deref()
                .and_then(|before| newly_mapped_window(before, windows))
            {
                return Ok(Some(LaunchWindow {
                    geometry: launch_window_geometry(window),
                    reused: false,
                }));
            }
            if let Some(active) = windows.iter().find(|window| window.active)
                && previous.as_deref() != Some(&active.id)
            {
                let reused = previous_windows
                    .as_ref()
                    .is_some_and(|before| before.iter().any(|window| window.id == active.id));
                let current = (active.id.clone(), active.title.clone());
                if !reused {
                    return Ok(Some(LaunchWindow {
                        geometry: launch_window_geometry(active),
                        reused: false,
                    }));
                }
                if !chromium_launch && handed_off {
                    if pending_reused.as_ref() == Some(&current) {
                        return Ok(Some(LaunchWindow {
                            geometry: launch_window_geometry(active),
                            reused: true,
                        }));
                    }
                    pending_reused = Some(current);
                }
            }
        } else if let Ok((_, _, geometry)) = active_window_info(&conn, &kwin_unique, &xdg).await
            && previous.as_deref() != Some(&geometry.id)
        {
            let reused = previous_windows
                .as_ref()
                .is_some_and(|before| before.iter().any(|window| window.id == geometry.id));
            let current = (geometry.id.clone(), geometry.title.clone());
            if !reused {
                return Ok(Some(LaunchWindow {
                    geometry,
                    reused: false,
                }));
            }
            if !chromium_launch && handed_off {
                if pending_reused.as_ref() == Some(&current) {
                    return Ok(Some(LaunchWindow {
                        geometry,
                        reused: true,
                    }));
                }
                pending_reused = Some(current);
            }
        }
        if exit_status.is_some_and(|status| status != 0) {
            return Ok(None);
        }
        if chromium_launch
            && exit_status == Some(0)
            && let Some(previous_windows) = previous_windows.as_deref()
            && let Ok(current_windows) = current_windows
        {
            let titled = changed_existing_browser_window_titles(previous_windows, &current_windows);
            if let [window] = titled.as_slice() {
                let current = (window.id.clone(), window.title.clone());
                if pending_reused.as_ref() == Some(&current) {
                    return Ok(Some(LaunchWindow {
                        geometry: launch_window_geometry(window),
                        reused: true,
                    }));
                }
                pending_reused = Some(current);
            } else {
                pending_reused = None;
            }
        }
    }
}

struct LaunchWindow {
    geometry: WindowGeometry,
    reused: bool,
}

fn is_chromium_window(window: &WindowSnapshot) -> bool {
    let app = format!("{} {}", window.resource_class, window.resource_name).to_lowercase();
    ["chrome", "chromium", "brave", "vivaldi", "edge", "msedge"]
        .iter()
        .any(|name| app.contains(name))
}

fn changed_existing_browser_windows<'a>(
    previous: &[WindowSnapshot],
    current: &'a [WindowSnapshot],
) -> Vec<&'a WindowSnapshot> {
    current
        .iter()
        .filter(|window| {
            is_chromium_window(window)
                && previous
                    .iter()
                    .find(|before| before.id == window.id)
                    .is_some_and(|before| {
                        before.title != window.title
                            || (!before.active && window.active)
                            || (before.minimized && !window.minimized)
                    })
        })
        .collect()
}

fn changed_existing_browser_window_titles<'a>(
    previous: &[WindowSnapshot],
    current: &'a [WindowSnapshot],
) -> Vec<&'a WindowSnapshot> {
    current
        .iter()
        .filter(|window| {
            is_chromium_window(window)
                && previous
                    .iter()
                    .find(|before| before.id == window.id)
                    .is_some_and(|before| before.title != window.title)
        })
        .collect()
}

/// The active window's id and the full window list, read before a launch so a
/// window that appears afterwards can be told apart from the ones already open.
async fn window_baseline(
    conn: &zbus::Connection,
    kwin_unique: &str,
    xdg: &Path,
) -> Result<(Option<String>, Vec<WindowSnapshot>), String> {
    let windows = tokio::time::timeout(
        LAUNCH_BASELINE_TIMEOUT,
        window_snapshots(conn, kwin_unique, xdg),
    )
    .await
    .map_err(|_| {
        format!(
            "KWin {kwin_unique} did not answer the window inventory within {} seconds",
            LAUNCH_BASELINE_TIMEOUT.as_secs()
        )
    })?
    .map_err(|error| format!("KWin {kwin_unique} window inventory failed: {error}"))?;
    // A complete empty inventory is authoritative in a newly started desktop.
    // Do not make a second active-window request when there is no active window.
    let active = windows
        .iter()
        .find(|window| window.active)
        .map(|window| window.id.clone());
    Ok((active, windows))
}

/// When the window list a launch compares against was read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowBaseline {
    /// Read before the command was submitted.
    PreLaunch,
    /// KWin did not answer before submission, so it was read right after.
    AfterSubmit,
    /// KWin did not answer either time, so a window found may predate the command.
    Unavailable,
}

impl WindowBaseline {
    fn as_str(self) -> &'static str {
        match self {
            Self::PreLaunch => "pre_launch",
            Self::AfterSubmit => "after_submit",
            Self::Unavailable => "unavailable",
        }
    }
}

fn launch_window_geometry(window: &WindowSnapshot) -> WindowGeometry {
    WindowGeometry {
        x: window.x,
        y: window.y,
        w: window.w,
        h: window.h,
        title: window.title.clone(),
        id: window.id.clone(),
        cx: 0.0,
        cy: 0.0,
    }
}

// Resolve only a literal leading command word. Assignment prefixes and shell
// expressions need evaluation to identify their executable; leave those unknown
// instead of reporting a guessed PATH result.
fn launch_program_name(command: &str) -> Option<String> {
    let word = shlex::split(command)?.into_iter().next()?;
    if word.is_empty()
        || word.contains(['=', '$', '`', ';', '|', '&', '(', ')', '<', '>', '{', '}'])
    {
        return None;
    }
    Some(word)
}

struct LaunchResolutionFile(PathBuf);

impl Drop for LaunchResolutionFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct LaunchProgress {
    stage: &'static str,
    command_submitted: bool,
    window_baseline_errors: Vec<serde_json::Value>,
}

struct PendingLaunchFile {
    path: PathBuf,
    submitted: bool,
}

impl Drop for PendingLaunchFile {
    fn drop(&mut self) {
        if !self.submitted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Every launch failure retains its exact submission stage, including errors
/// outside command-script writes. Never turn a submitted command into safe replay.
fn launch_failure(mut error: McpError, progress: &LaunchProgress) -> McpError {
    let mut data = error.data.take().unwrap_or_else(|| serde_json::json!({}));
    if !data.is_object() {
        data = serde_json::json!({"original_data": data});
    }
    let submitted = progress.command_submitted || data["command_submitted"] == true;
    if data["stage"].is_null() {
        data["stage"] = serde_json::json!(progress.stage);
    }
    data["operation"] = serde_json::json!("launch_app");
    data["command_submitted"] = serde_json::json!(submitted);
    let started = data["command_started"] != false;
    data["retry_safe"] = serde_json::json!(!submitted || !started);
    let action = if !started {
        "The command did not start: the session shell stopped before running it. Correct what the message names, then retry; nothing is running."
    } else if submitted {
        "The command may still be running. Inspect window_list and screenshot in the same session before any retry; do not launch a duplicate command."
    } else {
        "No command was submitted. Inspect the reported path and its filesystem/quota (free blocks alone do not establish available user quota), then retry only after the underlying error is corrected. The session remains open."
    };
    data["recovery_action"] = serde_json::json!(action);
    error.message = format!(
        "{} [launch stage: {}; command submitted: {submitted}]. {action}",
        error.message,
        data["stage"].as_str().unwrap_or(progress.stage)
    )
    .into();
    error.data = Some(data);
    error
}

fn launch_timeout(progress: &LaunchProgress, timeout: Duration) -> McpError {
    McpError::internal_error(
        format!(
            "launch_app exceeded {} seconds while {}. Command submitted: {}. The session remains open; use window_list before retrying.",
            timeout.as_secs(),
            progress.stage,
            progress.command_submitted,
        ),
        Some(serde_json::json!({
            "reason": "launch_timeout", "stage": progress.stage,
            "window_baseline_errors": progress.window_baseline_errors,
            "command_submitted": progress.command_submitted,
            "timeout_seconds": timeout.as_secs(),
        })),
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
struct AccessibilityTreeParams {
    app_name: Option<String>,
    max_depth: Option<u32>,
    role: Option<String>,
    show_elements: Option<bool>,
    /// Traversal time budget in seconds, from 1 to 30. Defaults to 5.
    #[schemars(range(min = 1, max = 30))]
    timeout_seconds: Option<u32>,
    /// Node id from a previous reply's "+N children root=ID" note; the tree
    /// starts at that node.
    root: Option<String>,
}

const FIND_UI_MAX_MATCHES: usize = 200;
// The five-second ATSPI_TRAVERSAL_TIMEOUT bounds how long a search runs and keeps
// the matches already found; this only caps runaway trees. It was 2000, which
// Chrome's own toolbar and a modest page exhaust before the page's tail.
const FIND_UI_MAX_NODES: usize = 100_000;
const FIND_UI_MAX_PAGES: usize = 10;

fn find_ui_search_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn find_ui_role_matches(wanted: Option<&str>, actual: &str) -> bool {
    let Some(wanted) = wanted else {
        return true;
    };
    let wanted = wanted.trim().to_lowercase();
    let actual = actual.trim().to_lowercase();
    wanted == actual
        || wanted == "button" && matches!(actual.as_str(), "push button" | "toggle button")
}

fn find_ui_frame_window<'a>(
    windows: &'a [WindowSnapshot],
    name: &str,
    _bounds: (i32, i32, i32, i32),
) -> Option<&'a WindowSnapshot> {
    if name.is_empty() {
        return None;
    }
    let candidates = windows
        .iter()
        .filter(|window| {
            !window.title.is_empty()
                && (input_bridge::caption_starts_with(&window.title, name)
                    || input_bridge::caption_starts_with(name, &window.title))
        })
        .collect::<Vec<_>>();
    // A provider can report a window-local origin even for CoordType::Screen.
    // Do not use that origin to disambiguate equal-titled windows.
    if let [window] = candidates.as_slice() {
        Some(*window)
    } else {
        None
    }
}

fn find_ui_frame_aligned(window: &WindowSnapshot, bounds: (i32, i32, i32, i32)) -> bool {
    (f64::from(bounds.2) - window.w).abs() <= 2.0 && (f64::from(bounds.3) - window.h).abs() <= 2.0
}

fn find_ui_native_match(
    node: &AtspiNode,
    window: Option<&WindowSnapshot>,
    windows: &[WindowSnapshot],
    frame: (i32, i32, i32, i32),
) -> serde_json::Value {
    let (x, y, w, h) = node.bounds;
    // Subtract the provider's own frame origin, not the compositor origin. This
    // works for both real screen-relative extents and Qt's local-origin extents.
    let relative_x = f64::from(x) - f64::from(frame.0);
    let relative_y = f64::from(y) - f64::from(frame.1);
    let cx = relative_x + f64::from(w) / 2.0;
    let cy = relative_y + f64::from(h) / 2.0;
    let in_frame = cx >= 0.0 && cy >= 0.0 && cx < f64::from(frame.2) && cy < f64::from(frame.3);
    let provider_visible = node.states.iter().any(|state| state == "visible");
    let visible = provider_visible && in_frame && window.is_none_or(|window| !window.minimized);
    let aligned = window.is_some_and(|window| find_ui_frame_aligned(window, frame));
    let covered = aligned
        && window.is_some_and(|matched| {
            windows.iter().any(|other| {
                other.id != matched.id
                    && !other.minimized
                    && other.stacking_order > matched.stacking_order
                    && matched.x + cx >= other.x
                    && matched.y + cy >= other.y
                    && matched.x + cx < other.x + other.w
                    && matched.y + cy < other.y + other.h
            })
        });
    let enabled = !node.states.iter().any(|state| state == "disabled");
    let compatible = aligned && window.is_some_and(|window| window.active);
    let bounds = if compatible {
        serde_json::json!({"x":relative_x,"y":relative_y,"w":w,"h":h})
    } else {
        serde_json::json!({"x":x,"y":y,"w":w,"h":h})
    };
    let mapped = window.filter(|_| aligned).map(|window| {
        serde_json::json!({
        "x":window.x+relative_x,"y":window.y+relative_y,"w":w,"h":h})
    });
    serde_json::json!({"backend":"atspi","role":node.role,"name":node.name,"states":node.states,
        "bounds":bounds,"raw_atspi_bounds":{"x":x,"y":y,"w":w,"h":h},"mapped_screen_bounds":mapped,"window":window,
        "coordinate_space":if compatible {"active_window_pixels"} else {"atspi_screen_pixels"},
        "mouse_coordinates_usable":compatible,"window_association":if aligned {"unique_title_and_frame_size"} else if window.is_some() {"title_match_geometry_unverified"} else {"unverified"},
        "visible":visible,"provider_visible":provider_visible,"enabled":enabled,
        "occluded":if covered {serde_json::json!(true)} else {serde_json::Value::Null},
        "clickable":if !visible || !enabled || covered || !compatible {serde_json::json!(false)} else {serde_json::Value::Null},
        "center_hit":serde_json::Value::Null,"occlusion_note":"Native center hit test unavailable; visibility and KWin geometry do not exclude an in-app overlay."})
}

#[derive(Deserialize, schemars::JsonSchema)]
struct FindUiElementsParams {
    query: String,
    /// Semantic role filter; "button" includes native push/toggle buttons.
    role: Option<String>,
    /// Keep only matches reported visible; visibility alone does not prove clickability.
    visible_only: Option<bool>,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BrowserProfiles {
    #[default]
    Inherit,
    Fresh,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WindowMode {
    /// Every new window maximizes when it maps (default).
    #[default]
    Maximized,
    /// New windows map at the size their app requests, including on relaunch.
    Native,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SessionStartParams {
    /// Startup wall-clock limit in seconds (1..300). Default 20; includes host preparation.
    startup_timeout_seconds: Option<u64>,
    /// inherit (default) prepares saved Chromium profiles and cookies. fresh
    /// starts those browser config directories empty, skipping profile copying
    /// and cookie persistence; useful for terminal-only work.
    #[serde(default)]
    browser_profiles: BrowserProfiles,
    /// maximized (default) maximizes every new window when it maps. native
    /// leaves windows at the size their app requests, so a relaunched app
    /// reopens at its own size. Both modes disable window decorations.
    #[serde(default)]
    window_mode: WindowMode,
    /// Session RAM throttle in GiB. Zero disables throttling; omit for the server default.
    memory_high: Option<u64>,
    /// Session RAM hard limit in GiB. Must be positive; omit for the server default.
    memory_max: Option<u64>,
    /// Session swap hard limit in GiB. Zero disables swap; omit for the server default.
    memory_swap_max: Option<u64>,
    /// Virtual display width in pixels for this session. Omit to use the
    /// server default. Ignored when the server was launched with --no-override.
    width: Option<u32>,
    /// Virtual display height in pixels for this session. Omit to use the
    /// server default. Ignored when the server was launched with --no-override.
    height: Option<u32>,
    /// Absolute HOME paths requiring private writable copies. Live SQLite
    /// databases in these paths use consistent private snapshots (database plus
    /// WAL must fit 64 MiB) instead of live read-only aliases. Oversized ancestor
    /// files are also copied. Shares startup_timeout_seconds (default 20).
    writable_paths: Option<Vec<PathBuf>>,
    /// Existing application state directories under HOME to share with the host.
    /// Writes persist on the host, including SQLite databases and WAL sidecars.
    /// For live Codex state, pass the absolute path to ~/.codex. Omit for isolation.
    /// HOME itself and symlinked paths are rejected. Selections cannot overlap
    /// browser profiles/cache, saved browser cookies, session storage or host runtime.
    /// Existing host Unix sockets in these directories remain blocked.
    host_write_paths: Option<Vec<PathBuf>>,
    /// Free space in MiB the disk-backed HOME overlay needs under
    /// ${XDG_CACHE_HOME:-~/.cache}/kwin-mcp before startup. Default 2048. A lower
    /// value starts on a fuller disk, but session HOME writes fail once it fills.
    disk_min_free_mib: Option<u64>,
}

impl SessionStartParams {
    fn memory_config(&self, mut config: DisplayConfig) -> Result<DisplayConfig, McpError> {
        let bytes = |name: &str, value: u64| {
            value
                .checked_mul(GIB_BYTES)
                .ok_or_else(|| McpError::invalid_params(format!("{name} is too large"), None))
        };
        if let Some(value) = self.memory_high {
            config.memory_high = if value == 0 {
                None
            } else {
                Some(bytes("memory_high", value)?)
            };
        }
        if let Some(value) = self.memory_max {
            config.memory_max = bytes("memory_max", value)?;
        }
        if let Some(value) = self.memory_swap_max {
            config.memory_swap_max = bytes("memory_swap_max", value)?;
        }
        if config.memory_max == 0
            || config
                .memory_high
                .is_some_and(|high| high > config.memory_max)
        {
            return Err(McpError::invalid_params(
                "memory_max must be positive and memory_high must not exceed memory_max; set both when lowering the limit",
                None,
            ));
        }
        Ok(config)
    }
}

/// The session's KWin window rule. Maximize is applied only at window
/// creation (rule type 3), so KWin doesn't force the window to stay maximized
/// after later resize requests. Both modes disable window decorations.
fn kwinrulesrc_contents(window_mode: WindowMode) -> String {
    let (description, maximize) = match window_mode {
        WindowMode::Maximized => (
            "No decorations, initially maximized",
            "maximizehoriz=true\nmaximizehorizrule=3\nmaximizevert=true\nmaximizevertrule=3\n",
        ),
        WindowMode::Native => ("No decorations", ""),
    };
    format!(
        "[1]\nDescription={description}\nnoborder=true\nnoborderrule=2\n{maximize}\
         wmclassmatch=0\n\n[General]\ncount=1\nrules=1\n"
    )
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WindowActivateParams {
    /// Exact window ID returned by window_list.
    window_id: String,
}

// ── Tool implementations ────────────────────────────────────────────────

fn session_start_log_path() -> Option<PathBuf> {
    Some(
        host_environment::runtime_dir()
            .ok()?
            .join("kwin-mcp/session-starts.log"),
    )
}

/// Persist completed server-shutdown cleanup so a replacement process can
/// explain why a saved session ID is no longer available.
fn session_lifecycle_log_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("kwin-mcp/session-lifecycle.jsonl")
}

const SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED: &str = "shutdown_cleanup_finished";
const SESSION_LIFECYCLE_REASON_SERVER_SHUTDOWN: &str = "server_shutdown";
const SESSION_LIFECYCLE_STATE_SESSION_STOPPED: &str = "session_stopped";
const SESSION_LIFECYCLE_REASON_SESSION_STOP: &str = "session_stop";
const SESSION_LIFECYCLE_STATE_SESSION_EXPIRED: &str = "session_expired";
const SESSION_LIFECYCLE_REASON_IDLE_TIMEOUT: &str = "idle_timeout";
const SESSION_LIFECYCLE_STATE_SESSION_DIED: &str = "session_dead";
const SESSION_LIFECYCLE_REASON_SANDBOX_EXITED: &str = "sandbox_exited";
const SESSION_LIFECYCLE_STATE_START_FAILED: &str = "session_start_failed";
const SESSION_LIFECYCLE_REASON_START_FAILED: &str = "start_failed";
const SESSION_LIFECYCLE_STATE_START_ABANDONED: &str = "session_start_abandoned";
const SESSION_LIFECYCLE_REASON_HOST_CALL_BLOCKED: &str = "host_call_blocked";
const SESSION_LIFECYCLE_LOG_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
struct SessionLifecycleRecord {
    session_id: String,
    server_pid: u32,
    state: String,
    reason: String,
    finished_at_unix_seconds: u64,
    workdir_cleanup: String,
}

impl SessionLifecycleRecord {
    fn validate(self) -> std::io::Result<Self> {
        self.validate_with_future_states(false)
    }

    fn validate_with_future_states(self, future: bool) -> std::io::Result<Self> {
        let valid_session_id = self
            .session_id
            .strip_prefix('s')
            .and_then(|id| id.split_once('-'))
            .is_some_and(|(pid, number)| {
                pid.parse::<u32>().ok() == Some(self.server_pid)
                    && number.parse::<u64>().is_ok_and(|number| number > 0)
            });
        let valid_transition = matches!(
            (self.state.as_str(), self.reason.as_str()),
            (
                SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED,
                SESSION_LIFECYCLE_REASON_SERVER_SHUTDOWN
            ) | (
                SESSION_LIFECYCLE_STATE_SESSION_STOPPED,
                SESSION_LIFECYCLE_REASON_SESSION_STOP
            ) | (
                SESSION_LIFECYCLE_STATE_SESSION_EXPIRED,
                SESSION_LIFECYCLE_REASON_IDLE_TIMEOUT
            ) | (
                SESSION_LIFECYCLE_STATE_SESSION_DIED,
                SESSION_LIFECYCLE_REASON_SANDBOX_EXITED
            ) | (
                SESSION_LIFECYCLE_STATE_START_FAILED,
                SESSION_LIFECYCLE_REASON_START_FAILED
            ) | (
                SESSION_LIFECYCLE_STATE_START_ABANDONED,
                SESSION_LIFECYCLE_REASON_HOST_CALL_BLOCKED
            )
        );
        let valid_transition =
            valid_transition || (future && !self.state.is_empty() && !self.reason.is_empty());
        let valid_cleanup = matches!(
            self.workdir_cleanup.as_str(),
            "not_requested" | "not_owned" | "removed" | "retained"
        ) || (future && !self.workdir_cleanup.is_empty());
        if self.server_pid == 0
            || self.finished_at_unix_seconds == 0
            || !valid_session_id
            || !valid_transition
            || !valid_cleanup
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid session-lifecycle record fields",
            ));
        }
        Ok(self)
    }

    fn legacy_compatible(&self) -> bool {
        self.state == SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED
            && self.reason == SESSION_LIFECYCLE_REASON_SERVER_SHUTDOWN
            && matches!(
                self.workdir_cleanup.as_str(),
                "not_requested" | "removed" | "retained"
            )
    }

    fn supersedes(&self, previous: &Self) -> bool {
        if self.finished_at_unix_seconds != previous.finished_at_unix_seconds {
            return self.finished_at_unix_seconds > previous.finished_at_unix_seconds;
        }
        // A retry preserves its original end time. Migrating an older retained
        // row must never undo the later confirmation that storage was removed.
        !(self.workdir_cleanup == "retained" && previous.workdir_cleanup == "removed")
    }

    fn status(&self) -> &'static str {
        match self.state.as_str() {
            SESSION_LIFECYCLE_STATE_SESSION_EXPIRED => session_registry::STATUS_EXPIRED,
            SESSION_LIFECYCLE_STATE_SESSION_DIED => session_registry::STATUS_DEAD,
            SESSION_LIFECYCLE_STATE_START_FAILED => session_registry::STATUS_START_FAILED,
            SESSION_LIFECYCLE_STATE_START_ABANDONED => session_registry::STATUS_START_BLOCKED,
            SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED | SESSION_LIFECYCLE_STATE_SESSION_STOPPED => {
                "stopped"
            }
            _ => "unknown",
        }
    }
}

fn current_session_lifecycle_log_path() -> PathBuf {
    session_lifecycle_log_path().with_file_name("session-lifecycle-v2.jsonl")
}

fn append_session_lifecycle_event(
    session_id: &str,
    state: &str,
    reason: &str,
    workdir_cleanup: &str,
    ended_at: Option<u64>,
) -> std::io::Result<()> {
    let finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs();
    let record = SessionLifecycleRecord {
        session_id: session_id.to_owned(),
        server_pid: std::process::id(),
        state: state.to_owned(),
        reason: reason.to_owned(),
        finished_at_unix_seconds: ended_at.unwrap_or(finished_at),
        workdir_cleanup: workdir_cleanup.to_owned(),
    }
    .validate()?;
    migrate_legacy_lifecycle_records()?;
    append_lifecycle_record(&current_session_lifecycle_log_path(), &record)?;
    if record.legacy_compatible() {
        append_lifecycle_record(&session_lifecycle_log_path(), &record)?;
    }
    Ok(())
}

fn append_lifecycle_record(path: &Path, record: &SessionLifecycleRecord) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.lock()?;

    let line_len = u64::try_from(line.len()).map_err(std::io::Error::other)?;
    if line_len > SESSION_LIFECYCLE_LOG_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session-lifecycle record exceeds the log limit",
        ));
    }
    let file_len = file.metadata()?.len();
    if file_len.saturating_add(line_len) > SESSION_LIFECYCLE_LOG_MAX_BYTES {
        let retain_limit = SESSION_LIFECYCLE_LOG_MAX_BYTES - line_len;
        let start = file_len.saturating_sub(retain_limit);
        file.seek(SeekFrom::Start(start))?;
        let mut retained = Vec::new();
        (&mut file).take(retain_limit).read_to_end(&mut retained)?;
        if start > 0 {
            if let Some(first_line_end) = retained.iter().position(|byte| *byte == b'\n') {
                retained.drain(..=first_line_end);
            } else {
                retained.clear();
            }
        }
        if let Some(last_line_end) = retained.iter().rposition(|byte| *byte == b'\n') {
            retained.truncate(last_line_end + 1);
        } else {
            retained.clear();
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&retained)?;
    } else {
        file.seek(SeekFrom::End(0))?;
    }
    file.write_all(&line)?;
    Ok(())
}

fn read_lifecycle_tail(file: &mut std::fs::File) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let file_len = file.metadata()?.len();
    let log_was_over_limit = file_len > SESSION_LIFECYCLE_LOG_MAX_BYTES;
    let start = file_len.saturating_sub(SESSION_LIFECYCLE_LOG_MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut contents = Vec::new();
    (&mut *file)
        .take(SESSION_LIFECYCLE_LOG_MAX_BYTES)
        .read_to_end(&mut contents)?;
    if start > 0 {
        if let Some(first_line_end) = contents.iter().position(|byte| *byte == b'\n') {
            contents.drain(..=first_line_end);
        } else {
            file.set_len(0)?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "session-lifecycle log tail contains no complete record",
            ));
        }
    }
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        if log_was_over_limit {
            if let Some(last_line_end) = contents.iter().rposition(|byte| *byte == b'\n') {
                contents.truncate(last_line_end + 1);
            } else {
                contents.clear();
            }
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&contents)?;
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "session-lifecycle log ends with an incomplete record",
        ));
    }
    if log_was_over_limit {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&contents)?;
    }

    Ok(contents)
}

fn lifecycle_record_from_path(
    path: &Path,
    session_id: &str,
    future: bool,
) -> std::io::Result<Option<SessionLifecycleRecord>> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    file.lock()?;
    let contents = read_lifecycle_tail(&mut file)?;
    let mut latest = None;
    if contents.is_empty() {
        return Ok(None);
    }
    let complete_records = &contents[..contents.len() - 1];
    for (line_number, line) in complete_records.split(|byte| *byte == b'\n').enumerate() {
        let record = parse_lifecycle_record(line, line_number, future)?;
        if record.session_id == session_id
            && latest
                .as_ref()
                .is_none_or(|previous| record.supersedes(previous))
        {
            latest = Some(record);
        }
    }
    Ok(latest)
}

fn parse_lifecycle_record(
    line: &[u8],
    number: usize,
    future: bool,
) -> std::io::Result<SessionLifecycleRecord> {
    let parsed = serde_json::from_slice::<SessionLifecycleRecord>(line).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed session-lifecycle record at retained line {number}: {error}"),
        )
    })?;
    parsed.validate_with_future_states(future).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed session-lifecycle record at retained line {number}: {error}"),
        )
    })
}

fn migrate_legacy_lifecycle_records() -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let legacy_path = session_lifecycle_log_path();
    let mut legacy = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&legacy_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    legacy.lock()?;
    let contents = read_lifecycle_tail(&mut legacy)?;
    if contents.is_empty() {
        return Ok(());
    }
    let mut retained = Vec::new();
    let mut migrated = Vec::new();
    for (number, line) in contents[..contents.len() - 1]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        let record = parse_lifecycle_record(line, number, false)?;
        if record.legacy_compatible() {
            retained.extend_from_slice(line);
            retained.push(b'\n');
        } else {
            migrated.push(record);
        }
    }
    if migrated.is_empty() {
        return Ok(());
    }
    // Keep a bounded recovery copy before changing the stable legacy inode.
    // Old writers may already have that inode open and be waiting on its lock.
    let backup_path = legacy_path.with_file_name("session-lifecycle-v1-migration.jsonl");
    let mut backup = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&backup_path)?;
    backup.write_all(&contents)?;
    backup.sync_all()?;
    let current_path = current_session_lifecycle_log_path();
    for record in migrated {
        append_lifecycle_record(&current_path, &record)?;
    }
    let current = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&current_path)?;
    current.lock()?;
    current.sync_all()?;
    if let Some(parent) = legacy_path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    legacy.set_len(0)?;
    legacy.seek(SeekFrom::Start(0))?;
    legacy.write_all(&retained)?;
    legacy.sync_all()?;
    Ok(())
}

fn previous_session_lifecycle(session_id: &str) -> std::io::Result<Option<SessionLifecycleRecord>> {
    migrate_legacy_lifecycle_records()?;
    let legacy = lifecycle_record_from_path(&session_lifecycle_log_path(), session_id, false)?;
    let current =
        lifecycle_record_from_path(&current_session_lifecycle_log_path(), session_id, true)?;
    Ok(match (legacy, current) {
        (Some(legacy), Some(current)) => Some(if current.supersedes(&legacy) {
            current
        } else {
            legacy
        }),
        (Some(record), None) | (None, Some(record)) => Some(record),
        (None, None) => None,
    })
}

fn append_session_start_outcome(
    started: std::time::Instant,
    error: Option<&str>,
) -> std::io::Result<()> {
    let Some(path) = session_start_log_path() else {
        return Ok(());
    };
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let status = if error.is_some() { "fail" } else { "ok" };
    let reason = error.unwrap_or_default().replace(['\t', '\n', '\r'], " ");
    let line = format!(
        "{seconds}\t{status}\t{}\t{}\t{}\t{reason}\n",
        started.elapsed().as_millis(),
        std::process::id(),
        env!("GIT_HASH"),
    );
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // One O_APPEND write keeps records from concurrent server processes together.
    if file.write(line.as_bytes())? != line.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "short session-start log write",
        ));
    }
    Ok(())
}

/// Names the host call a start is blocked in, how long it has been blocked,
/// and the cleanup that waits for it to return.
fn blocked_call_report(stage: &str, blocked_for: std::time::Duration, workdir: &Path) -> String {
    format!(
        "{stage} has not returned after {}s; cleanup of {} and its disk overlay runs when it does",
        blocked_for.as_secs(),
        workdir.display()
    )
}

/// The refusal for a lifecycle call that could not take the start gate within
/// its budget. The detail names a timed-out start's blocked call when that is
/// what holds the gate.
fn lifecycle_busy_error(operation: &str, stage: &str, detail: &str) -> McpError {
    McpError::internal_error(
        format!("{operation} could not start: {detail}. Retry shortly."),
        Some(serde_json::json!({"reason": "lifecycle_busy", "stage": stage, "detail": detail})),
    )
}

/// The hard-limit error for a start that ran out of budget. A blocked call is
/// reported with its operation, its age, and the cleanup it holds.
fn hard_timeout_error(
    budget: std::time::Duration,
    stage: &str,
    blocked_for: std::time::Duration,
    blocked: bool,
    workdir: &Path,
) -> McpError {
    let message = format!(
        "session_start exceeded {}s hard limit while {stage}",
        budget.as_secs()
    );
    if !blocked {
        return McpError::internal_error(
            message,
            Some(
                serde_json::json!({"reason": "hard_timeout", "startup_timeout_seconds": budget.as_secs(), "stage": stage, "host_call_blocked": false}),
            ),
        );
    }
    let pending = blocked_call_report(stage, blocked_for, workdir);
    McpError::internal_error(
        format!(
            "session_start exceeded {}s hard limit: {stage} has not returned after {}s. A host filesystem or /proc call is blocked (check for hung FUSE or network mounts under $HOME and processes stuck in D state); cleanup of {} and its disk overlay runs when it does, and lifecycle calls wait for it and then report this status.",
            budget.as_secs(),
            blocked_for.as_secs(),
            workdir.display()
        ),
        Some(serde_json::json!({
            "reason": "hard_timeout",
            "startup_timeout_seconds": budget.as_secs(),
            "stage": stage,
            "host_call_blocked": true,
            "blocked_seconds": blocked_for.as_secs(),
            "pending_cleanup": pending,
        })),
    )
}

struct SessionStartAttempt {
    started: std::time::Instant,
    recorded: bool,
}

impl SessionStartAttempt {
    fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            recorded: false,
        }
    }

    fn finish(&mut self, error: Option<&str>) {
        self.recorded = true;
        let _ = append_session_start_outcome(self.started, error);
    }
}

impl Drop for SessionStartAttempt {
    fn drop(&mut self) {
        if !self.recorded {
            let _ = append_session_start_outcome(self.started, Some("cancelled"));
        }
    }
}

fn print_session_start_stats(seconds: u64) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut successes = 0_u64;
    let mut attempts = 0_u64;
    let mut reasons = std::collections::HashMap::<String, u64>::new();
    if let Some(path) = session_start_log_path() {
        let file = match std::fs::File::open(path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(file) = file {
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                let mut fields = line.splitn(6, '\t');
                let (
                    Some(timestamp),
                    Some(status),
                    Some(_duration),
                    Some(_pid),
                    Some(_commit),
                    Some(error),
                ) = (
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                    fields.next(),
                )
                else {
                    continue;
                };
                let Ok(timestamp) = timestamp.parse::<u64>() else {
                    continue;
                };
                if timestamp < now.saturating_sub(seconds) || timestamp > now {
                    continue;
                }
                match status {
                    "ok" => {
                        successes += 1;
                        attempts += 1;
                    }
                    "fail" => {
                        attempts += 1;
                        *reasons.entry(error.to_owned()).or_default() += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    print!("session_start {successes}/{attempts} last {seconds}s");
    if !reasons.is_empty() {
        let top = reasons
            .into_iter()
            .take(5)
            .map(|(reason, count)| format!("{count}x {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        print!("; failures: {top}");
    }
    println!();
    Ok(())
}

impl rmcp::ServerHandler for KwinMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_logging().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "KDE Wayland automation in an isolated desktop. Flow: session_start → launch_app → screenshot → input → screenshot to verify. \
                Prefer find_ui_elements for named controls, accessibility_tree for structure, screenshots otherwise. Use window_list and window_activate for hidden windows. \
                Work without a viewer until the user must see or act (password, OTP, Duo, CAPTCHA, choice, result). Show the relevant entry, then call viewer_open; never assume it is open or stop the session for a handoff. Poll screenshots and continue when the page advances. The viewer may stay open; the user can close it. \
                At browser logins, select saved-credential autofill without reading or typing its value; otherwise show the entry and open the viewer. \
                Clipboards are separate: local viewer Ctrl+C/Ctrl+X copies to the server host; Ctrl+V/Shift+Insert pastes from it. clipboard_write target=host prepares viewer paste; target=session prepares agent input and X11 PRIMARY. clipboard_read reads session text only and withholds generated secrets. \
                Chrome file chooser: click the file input, ctrl+l, type the absolute session path, alt+o; for folders click its top-right button, not alt+o. Enter in the location bar cancels. \
                Mouse/screenshot coordinates are 1:1 pixels from the active window's top-left. For crop region=[x1,y1,x2,y2], image (px,py) maps to mouse (px+x1,py+y1). \
                {size_line} Windows auto-maximize unless window_mode=native. Results report memory_warning when the session is throttled.",
                size_line = if self.display.locked {
                    format!("The virtual display is fixed at {}x{} (server launched with --no-override; session_start size params are ignored).", self.display.width, self.display.height)
                } else {
                    format!("The virtual display defaults to {}x{}; session_start accepts optional width/height to override it for that session.", self.display.width, self.display.height)
                },
            ))
    }
}

#[rmcp::tool_router]
impl KwinMcp {
    #[rmcp::tool(
        name = "session_start",
        description = "Create an isolated desktop and return its session_id; use IDs from your own session_start calls, or session_attach to an existing session the user explicitly authorized. Calls may forward to that ID's live owner. This tool takes no session_id. The viewer starts closed; use viewer_open when the user needs to see or interact, and keep the session alive for handoff. startup_timeout_seconds is 1-300 (default 20). browser_profiles=fresh skips inherited Chromium profiles and cookie persistence; inherit is default. window_mode=maximized is default; native uses app-requested sizes. Both remove decorations. Optional width/height override server defaults unless --no-override; results report actual dimensions and viewer state. Optional memory_high/memory_max/memory_swap_max are whole GiB; zero disables throttling or swap, and memory_max must be positive and at least memory_high. HOME writes normally stay in a per-session disk overlay and are discarded on stop; export_file copies them to the host. Host edits after start are not reliably visible; import_file refreshes one file, then reload it in the app. Explicit host_write_paths binds existing HOME state directories writable (for Codex, an absolute path to ~/.codex): writes and deletions persist on the host. Results and session_list report those paths. host_shared_dir is the session workdir and XDG_RUNTIME_DIR, visible at the same absolute path on both sides: put host-facing sockets, FIFOs and output there. When the result has workdir_removed_on_stop=true (server --autoclean or --ttl), session_stop deletes it; copy outputs out first. Other /tmp paths are private, and HOME-overlay sockets cannot be reached through their host upper paths. /sys/fs/cgroup is read-only; run helpers needing delegated cgroups on the host. App stdout is discarded; stderr goes to the disk-backed sandbox.log under the session's cache directory."
    )]
    async fn session_start(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<SessionStartParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        // The hard limit covers the gate wait too, so a start queued behind a
        // blocked lifecycle operation still answers within it.
        let mut attempt = SessionStartAttempt::new();
        let startup_timeout = params
            .startup_timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(SESSION_START_HARD_TIMEOUT);
        if !(1..=SESSION_START_MAX_SECONDS).contains(&startup_timeout.as_secs()) {
            return Err(McpError::invalid_params(
                "startup_timeout_seconds must be from 1 to 300",
                None,
            ));
        }
        let deadline = tokio::time::Instant::now() + startup_timeout;
        let gate = match self.lifecycle_gate(deadline, "session_start").await {
            Ok(gate) => gate,
            Err(error) => {
                attempt.finish(Some(&error.message));
                return Err(error);
            }
        };
        if self.death_record().is_some() {
            // session_stop abandoned this handle while this start waited for the gate.
            drop(gate);
            let error = McpError::invalid_params(
                "session_start: this session was stopped before its startup began",
                None,
            );
            attempt.finish(Some(&error.message));
            return Err(error);
        }
        self.set_start_stage("preparing the session workdir");
        *self
            .startup_deadline
            .lock()
            .map_err(|_| McpError::internal_error("startup deadline lock poisoned", None))? =
            Some(deadline.into_std());
        let host_work: HostWork = Arc::new(std::sync::Mutex::new(None));
        let outcome = match tokio::time::timeout_at(
            deadline,
            self.session_start_inner(peer, params, host_work.clone()),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => {
                let (stage, blocked_for) = self.start_stage_report();
                eprintln!(
                    "session_start: exceeded {}s hard limit while {stage}",
                    startup_timeout.as_secs()
                );
                let blocked = host_work
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                    .filter(|handle| !handle.is_finished());
                if let Some(handle) = blocked {
                    // A host call is still blocked in the kernel and may yet
                    // write into the workdir. Keep the gate and the workdir
                    // until it returns, then run the usual failed-start cleanup.
                    self.set_pending_cleanup(Some(self.path.clone()));
                    let this = self.clone();
                    tokio::spawn(async move {
                        let _ = handle.await;
                        let error = this
                            .autoclean_unpublished_workdir(McpError::internal_error(
                                "deferred startup cleanup",
                                None,
                            ))
                            .await;
                        eprintln!(
                            "session_start: blocked host call returned; {}",
                            error.message
                        );
                        // Clear the record before the gate opens, so a lifecycle
                        // call never sees neither the record nor the gate.
                        this.set_pending_cleanup(None);
                        drop(gate);
                    });
                    let error =
                        hard_timeout_error(startup_timeout, &stage, blocked_for, true, &self.path);
                    attempt.finish(Some(&error.message));
                    return Err(error);
                }
                Err(hard_timeout_error(
                    startup_timeout,
                    &stage,
                    blocked_for,
                    false,
                    &self.path,
                ))
            }
        };
        let result = match outcome {
            Ok(result) => Ok(result),
            Err(error) => Err(self.autoclean_unpublished_workdir(error).await),
        };
        drop(gate);
        attempt.finish(result.as_ref().err().map(|error| error.message.as_ref()));
        result
    }

    async fn session_start_inner(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        params: SessionStartParams,
        host_work: HostWork,
    ) -> Result<CallToolResult, McpError> {
        let memory = params.memory_config(self.display)?;
        let browser_profiles = params.browser_profiles;
        let window_mode = params.window_mode;
        eprintln!(
            "kwin-mcp v{}.{} ({}) session_start",
            env!("CARGO_PKG_VERSION"),
            env!("BUILD_NUMBER"),
            env!("GIT_HASH")
        );
        let version_stamp = format!(
            "kwin-mcp v{}.{} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("BUILD_NUMBER"),
            env!("GIT_HASH")
        );
        let ver_err = |e: String| McpError::internal_error(format!("{version_stamp} — {e}"), None);
        let at_spi_bus_launcher = resolve_at_spi_bus_launcher().ok_or_else(|| {
            ver_err(
                "required at-spi-bus-launcher was not found in /usr/lib, /usr/lib/at-spi2-core, /usr/libexec, or PATH".to_owned(),
            )
        })?;
        // Resolve the launcher's missing login environment without mutating
        // global variables used by concurrent sessions or the strict wrapper.
        let host_runtime = host_environment::runtime_dir()
            .map_err(|error| ver_err(format!("host runtime directory: {error:#}")))?;
        let host_session_bus = host_environment::session_bus_address(&host_runtime)
            .map_err(|error| ver_err(format!("host session D-Bus address: {error:#}")))?;
        // Resolve the virtual display size: tool params > CLI flags > compiled
        // defaults — unless --no-override locked it at the CLI/compiled value.
        let (screen_w, screen_h) = if self.display.locked {
            if params.width.is_some() || params.height.is_some() {
                eprintln!(
                    "session_start: size params ignored (--no-override), using {}x{}",
                    self.display.width, self.display.height
                );
            }
            (self.display.width, self.display.height)
        } else {
            let w = params.width.unwrap_or(self.display.width);
            let h = params.height.unwrap_or(self.display.height);
            for (name, v) in [("width", w), ("height", h)] {
                if !(MIN_SCREEN_DIM..=MAX_SCREEN_DIM).contains(&v) {
                    return Err(McpError::invalid_params(
                        format!("{name} {v} out of range {MIN_SCREEN_DIM}..={MAX_SCREEN_DIM}"),
                        None,
                    ));
                }
            }
            (w, h)
        };
        eprintln!("session_start: virtual display {screen_w}x{screen_h}");
        let host_xdg_dir = self.path.clone();
        let system_proxy_socket = host_xdg_dir.join("system_bus_socket");
        let wallet_bus_socket = host_xdg_dir.join("kwallet_bus");
        let service_proxy_socket = host_xdg_dir.join("service_bus_socket");
        // Validate before creating the workdir or acquiring devices/processes.
        // The service proxy basename also bounds the shorter session bus,
        // Wayland, PipeWire and generated dbus-* socket names in this directory.
        for socket in [
            &system_proxy_socket,
            &wallet_bus_socket,
            &service_proxy_socket,
        ] {
            validate_session_socket_path(socket).map_err(|error| ver_err(error.to_string()))?;
        }
        // Creation and the ownership claim are synchronous with no cancellation
        // point between them. Never claim an existing path from another run.
        std::fs::create_dir(&host_xdg_dir).map_err(|e| ver_err(e.to_string()))?;
        if self.display.autoclean {
            self.workdir.claim(&host_xdg_dir);
            self.workdir
                .create_lease()
                .map_err(|e| ver_err(format!("create workdir lease: {e}")))?;
        }
        cleanup_stale_session_files(&host_xdg_dir);
        std::fs::create_dir_all(host_xdg_dir.join("tmp")).map_err(|e| ver_err(e.to_string()))?;
        create_browser_wrappers(&host_xdg_dir)
            .map_err(|e| ver_err(format!("browser wrappers: {e}")))?;
        eprintln!(
            "session_start: host_xdg_dir ready path={}",
            host_xdg_dir.display()
        );
        let xdg_dir_str = host_xdg_dir.display().to_string();
        // Write AT-SPI dbus config with ANONYMOUS auth for cross-namespace access
        let atspi_conf_path = host_xdg_dir.join("accessibility.conf");
        std::fs::write(&atspi_conf_path, format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
            \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
            <busconfig><type>accessibility</type>\
            <servicedir>/usr/share/dbus-1/accessibility-services</servicedir>\
            <auth>EXTERNAL</auth><auth>ANONYMOUS</auth><allow_anonymous/>\
            <listen>unix:dir={xdg_dir_str}</listen>\
            <policy context=\"default\"><allow user=\"root\"/>\
            <allow send_destination=\"*\"/><allow receive_type=\"method_call\"/>\
            <allow receive_type=\"method_return\"/><allow receive_type=\"error\"/>\
            <allow receive_type=\"signal\"/><allow own=\"*\"/></policy>\
            </busconfig>"
        )).map_err(|e| ver_err(format!("write atspi config: {e}")))?;
        // Write kwin-mcp display config files to host_xdg_dir for --ro-bind mounting.
        // Protected from agent writes: the ro-bind shadows the overlay entry.
        let kwinrc_path = host_xdg_dir.join("kwinrc");
        std::fs::write(
            &kwinrc_path,
            "[org.kde.kdecoration2]\nBorderSize=None\nShadowSize=0\n\n\
             [Compositing]\nLockScreenAutoLockEnabled=false\n",
        )
        .map_err(|e| ver_err(format!("write kwinrc: {e}")))?;
        let kwinrulesrc_path = host_xdg_dir.join("kwinrulesrc");
        std::fs::write(&kwinrulesrc_path, kwinrulesrc_contents(window_mode))
            .map_err(|e| ver_err(format!("write kwinrulesrc: {e}")))?;
        let kscreenlockerrc_path = host_xdg_dir.join("kscreenlockerrc");
        std::fs::write(
            &kscreenlockerrc_path,
            "[Daemon]\nAutolock=false\nLockOnResume=false\nTimeout=0\n",
        )
        .map_err(|e| ver_err(format!("write kscreenlockerrc: {e}")))?;
        let kcmfonts_path = host_xdg_dir.join("kcmfonts");
        std::fs::write(
            &kcmfonts_path,
            format!("[General]\nforceFontDPI={KDE_FORCE_FONT_DPI}\n"),
        )
        .map_err(|e| ver_err(format!("write kcmfonts: {e}")))?;
        let fonts_conf_path = host_xdg_dir.join("fonts.conf");
        std::fs::write(
            &fonts_conf_path,
            format!(
                "<?xml version=\"1.0\"?>\n\
             <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
             <fontconfig>\n\
             <match target=\"font\">\n\
             <edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\n\
             <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit>\n\
             <edit name=\"antialias\" mode=\"assign\"><bool>true</bool></edit>\n\
             <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit>\n\
             </match>\n\
             </fontconfig>\n"
            ),
        )
        .map_err(|e| ver_err(format!("write fonts.conf: {e}")))?;
        // Read host kdeglobals and patch display settings for the virtual session
        let home = std::env::var("HOME").map_err(|e| ver_err(e.to_string()))?;
        let overlay_target = PathBuf::from(&home);
        if !overlay_target.is_absolute() {
            return Err(ver_err(format!(
                "overlay target must be absolute: {}",
                overlay_target.display()
            )));
        }
        let writable_paths = params.writable_paths.unwrap_or_default();
        for path in &writable_paths {
            if !path.is_absolute()
                || !path.starts_with(&overlay_target)
                || path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return Err(ver_err(format!(
                    "writable_paths must contain absolute paths under HOME without '..': {}",
                    path.display()
                )));
            }
        }
        let host_write_paths = params.host_write_paths.unwrap_or_default();
        let disk_min_free = match params.disk_min_free_mib {
            None => SESSION_DISK_MIN_FREE,
            Some(mib) => mib
                .checked_mul(1 << 20)
                .ok_or_else(|| McpError::invalid_params("disk_min_free_mib is too large", None))?,
        };
        // Mount, socket, and /proc scans can block in the kernel on a hung
        // FUSE or network mount, or on a process stuck in D state. Run them on
        // a blocking thread so the hard limit can still answer; the handle
        // stays in `host_work` so a timed-out start defers workdir cleanup
        // until this thread has stopped writing into it.
        self.set_start_stage("scanning host mounts, sockets, and processes");
        let (view_tx, view_rx) = tokio::sync::oneshot::channel();
        let view_target = overlay_target.clone();
        let view_xdg = host_xdg_dir.clone();
        let view_runtime = host_runtime.clone();
        let view_service = self.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let _ = view_tx.send(prepare_host_view(
                &view_target,
                &view_xdg,
                &view_runtime,
                &writable_paths,
                &host_write_paths,
                &view_service,
                HostViewOptions {
                    browser_profiles,
                    disk_min_free,
                },
            ));
        });
        if let Ok(mut slot) = host_work.lock() {
            *slot = Some(handle);
        }
        let host_view = view_rx
            .await
            .map_err(|error| ver_err(format!("host scan thread: {error}")))?
            .map_err(|error| ver_err(format!("{error:#}")))?;
        if let Ok(mut slot) = host_work.lock() {
            slot.take();
        }
        let HostView {
            mut overlay_plan,
            kdeglobals: mut kdeglobals_content,
            cookie_roots,
            cookie_errors,
        } = host_view;
        let ui_regular = qt_font_spec(UI_FONT_FAMILY, UI_FONT_SIZE, FONT_WEIGHT_REGULAR, false);
        let ui_small = qt_font_spec(
            UI_FONT_FAMILY,
            UI_FONT_SIZE_SMALL,
            FONT_WEIGHT_REGULAR,
            false,
        );
        let ui_bold = qt_font_spec(UI_FONT_FAMILY, UI_FONT_SIZE, FONT_WEIGHT_BOLD, true);
        let fixed = qt_font_spec(
            FIXED_FONT_FAMILY,
            FIXED_FONT_SIZE,
            FONT_WEIGHT_REGULAR,
            false,
        );
        let replacements: [(&str, String); 10] = [
            ("ScaleFactor=", format!("ScaleFactor={KDE_SCALE_FACTOR}")),
            ("ScreenScaleFactors=", "ScreenScaleFactors=".to_owned()),
            ("XftHintStyle=", format!("XftHintStyle={KDE_HINT_STYLE}")),
            ("XftSubPixel=", format!("XftSubPixel={KDE_SUB_PIXEL}")),
            ("font=", format!("font={ui_regular}")),
            ("menuFont=", format!("menuFont={ui_regular}")),
            (
                "smallestReadableFont=",
                format!("smallestReadableFont={ui_small}"),
            ),
            ("toolBarFont=", format!("toolBarFont={ui_regular}")),
            ("activeFont=", format!("activeFont={ui_bold}")),
            ("fixed=", format!("fixed={fixed}")),
        ];
        for (prefix, replacement) in &replacements {
            kdeglobals_content = kdeglobals_content
                .lines()
                .map(|line| {
                    if line.starts_with(prefix) {
                        replacement.clone()
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        let kdeglobals_path = host_xdg_dir.join("kdeglobals");
        std::fs::write(&kdeglobals_path, &kdeglobals_content)
            .map_err(|e| ver_err(format!("write kdeglobals: {e}")))?;
        // Write fontconfig system overrides to bind-mount over files that force hinting/subpixel
        let fc_hinting_path = host_xdg_dir.join("10-hinting-none.conf");
        std::fs::write(&fc_hinting_path, format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\
            <edit name=\"hintstyle\" mode=\"assign\"><const>{KDE_HINT_STYLE}</const></edit></match>\n\
            </fontconfig>\n"
        )).map_err(|e| ver_err(format!("write fontconfig hinting: {e}")))?;
        let fc_lcd_path = host_xdg_dir.join("11-lcdfilter-none.conf");
        std::fs::write(&fc_lcd_path, format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
            <fontconfig>\n\
            <match target=\"font\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit></match>\n\
            <match target=\"pattern\"><edit name=\"lcdfilter\" mode=\"assign\"><const>lcdnone</const></edit>\
            <edit name=\"rgba\" mode=\"assign\"><const>{KDE_SUB_PIXEL}</const></edit></match>\n\
            </fontconfig>\n"
        )).map_err(|e| ver_err(format!("write fontconfig lcd: {e}")))?;
        let fc_hinting_str = fc_hinting_path.display().to_string();
        let fc_lcd_str = fc_lcd_path.display().to_string();
        let fc_hinting_dest =
            std::fs::canonicalize("/usr/share/fontconfig/conf.default/10-hinting-slight.conf")
                .map_err(|e| ver_err(format!("resolve fontconfig hinting destination: {e}")))?;
        let fc_lcd_dest =
            std::fs::canonicalize("/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf")
                .map_err(|e| ver_err(format!("resolve fontconfig LCD destination: {e}")))?;
        let fc_hinting_dest_str = fc_hinting_dest.display().to_string();
        let fc_lcd_dest_str = fc_lcd_dest.display().to_string();
        // Inline entrypoint: starts dbus/kwin/services, reads stdin for launch_app
        // The host TMPDIR names the workdir's parent, which is read-only inside
        // bwrap. Keep session applications' temporary files in private /tmp.
        let media_wait_attempts = STARTUP_TIMEOUT.as_millis() / STARTUP_POLL.as_millis();
        let media_poll_seconds = STARTUP_POLL.as_secs_f64();
        let at_spi_bus_launcher = shell_quote(&at_spi_bus_launcher.display().to_string());
        let entrypoint = format!(
            "set -u\n\
            export TMPDIR=/tmp\n\
            export XDG_RUNTIME_DIR={xdg_dir_str}\n\
            export WAYLAND_DISPLAY=wayland-0\n\
            export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1\n\
            export QT_SCALE_FACTOR={KDE_SCALE_FACTOR}\n\
            export GDK_SCALE={KDE_SCALE_FACTOR}\n\
            export FREETYPE_PROPERTIES=truetype:interpreter-version=35\n\
            export ATSPI_DBUS_IMPLEMENTATION=dbus-daemon\n\
            mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix\n\
            printf '<busconfig><include>/usr/share/dbus-1/session.conf</include><auth>ANONYMOUS</auth><allow_anonymous/></busconfig>' > /tmp/mcp-dbus.conf\n\
            dbus-daemon --config-file=/tmp/mcp-dbus.conf --address='unix:path={xdg_dir_str}/bus' --nofork &\n\
            dbus_pid=$!\n\
            n=0; while [ ! -S '{xdg_dir_str}/bus' ] && kill -0 \"$dbus_pid\" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            test -S '{xdg_dir_str}/bus' || {{ echo 'container D-Bus daemon did not create its socket' >&2; exit 1; }}\n\
            export DBUS_SESSION_BUS_ADDRESS='unix:path={xdg_dir_str}/bus'\n\
            touch '{xdg_dir_str}/dbus-ready'\n\
            n=0; while [ ! -f '{xdg_dir_str}/bridge-ready' ] && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done\n\
            {{ KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 \
            kwin_wayland --virtual --xwayland --no-lockscreen --width {screen_w} --height {screen_h}; \
            echo $? > '{xdg_dir_str}/{KWIN_EXIT_FILE}'; }} &\n\
            sleep 0.3\n\
            dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR QT_QPA_PLATFORM PATH HOME USER TMPDIR ATSPI_DBUS_IMPLEMENTATION\n\
            {at_spi_bus_launcher} --launch-immediately &\n\
            pipewire &\n\
            media_pid=$!\n\
            for ((attempt=0; attempt<{media_wait_attempts}; attempt++)); do\n\
                test -S \"$XDG_RUNTIME_DIR/pipewire-0\" && break\n\
                kill -0 \"$media_pid\" 2>/dev/null || {{ echo 'private PipeWire exited before its socket was ready' >&2; exit 1; }}\n\
                sleep {media_poll_seconds}\n\
            done\n\
            test -S \"$XDG_RUNTIME_DIR/pipewire-0\" || {{ echo 'private PipeWire socket startup timed out' >&2; exit 1; }}\n\
            wireplumber &\n\
            while read -r cmd; do\n\
                eval \"$cmd\" &\n\
            done\n"
        );
        // Create uinput virtual devices before bwrap so we can bind-mount them
        let (uinput_mouse, mouse_evdev, uinput_keyboard, kbd_evdev) =
            create_uinput_devices().map_err(|e| ver_err(format!("uinput: {e}")))?;
        let mouse_evdev_str = mouse_evdev.display().to_string();
        let kbd_evdev_str = kbd_evdev.display().to_string();
        eprintln!("session_start: uinput mouse={mouse_evdev_str} keyboard={kbd_evdev_str}");

        let cleanup_err = |message: String, startup: &mut StartupResources| {
            eprintln!("session_start: startup error: {message}");
            startup.cleanup();
            Err(ver_err(message))
        };
        // Begin ownership before the first proxy is spawned. Any failure or
        // cancellation between proxy acquisitions is therefore still covered.
        let mut startup = StartupResources::new();
        self.set_start_stage("starting host D-Bus proxies");
        match spawn_dbus_proxy(
            "unix:path=/run/dbus/system_bus_socket",
            &system_proxy_socket,
            &[
                "--call=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.Get@/*",
                "--call=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.GetAll@/*",
                "--broadcast=org.freedesktop.NetworkManager=org.freedesktop.DBus.Properties.PropertiesChanged@/*",
            ],
        ).await {
            Ok(child) => {
                if cfg!(debug_assertions)
                    && std::env::var("KWIN_MCP_TEST_FAIL_AFTER_FIRST_PROXY").ok().as_deref()
                        == Some("1")
                {
                    eprintln!("session_start: test first proxy registered pid={}", child.id());
                }
                startup.add_proxy(child);
                if cfg!(debug_assertions)
                    && std::env::var("KWIN_MCP_TEST_FAIL_AFTER_FIRST_PROXY").ok().as_deref()
                        == Some("1")
                {
                    return cleanup_err("test failure after first proxy".to_owned(), &mut startup);
                }
            }
            Err(error) => return cleanup_err(format!("system D-Bus proxy: {error:#}"), &mut startup),
        }
        // Session apps reach KWallet through a session-local service on a private
        // bus (see wallet_mediator): a one-time guarded snapshot of the host
        // wallet, then answers from memory; the host is never contacted again.
        self.set_start_stage("starting the KWallet mediator");
        let wallet_bus_config = host_xdg_dir.join("kwallet-bus.conf");
        if let Err(error) = std::fs::write(
            &wallet_bus_config,
            format!(
                "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>\n",
                wallet_bus_socket.display()
            ),
        ) {
            return cleanup_err(format!("write KWallet bus config: {error}"), &mut startup);
        }
        let wallet_bus_stderr_path = host_xdg_dir.join("kwallet-bus.stderr");
        let wallet_bus_stderr = match std::fs::File::create(&wallet_bus_stderr_path) {
            Ok(file) => file,
            Err(error) => {
                return cleanup_err(format!("create KWallet bus stderr: {error}"), &mut startup);
            }
        };
        let mut wallet_bus = std::process::Command::new("dbus-daemon");
        wallet_bus
            .arg(format!("--config-file={}", wallet_bus_config.display()))
            .args(["--nofork", "--nopidfile"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(wallet_bus_stderr));
        terminate_with_parent(&mut wallet_bus);
        let wallet_bus_index = startup.service_proxy_children.len();
        match spawn_session_child(wallet_bus, false).await {
            Ok(child) => startup.add_proxy(child),
            Err(error) => return cleanup_err(format!("start KWallet bus: {error}"), &mut startup),
        }
        if let Err(error) = wait_for_wallet_bus(
            &mut startup.service_proxy_children[wallet_bus_index],
            &wallet_bus_socket,
            &wallet_bus_stderr_path,
        )
        .await
        {
            return cleanup_err(error, &mut startup);
        }
        let wallet_bus_address = format!("unix:path={}", wallet_bus_socket.display());
        let host_bus = match zbus::connection::Builder::address(host_session_bus.as_str()) {
            Ok(builder) => builder.build().await,
            Err(error) => Err(error),
        };
        // The one and only host contact: a guarded read-only snapshot. The
        // session is then served from memory and never reaches the host wallet.
        let wallet_snapshot = match host_bus {
            Ok(host_bus) => wallet_mediator::snapshot(&host_bus, &host_runtime).await,
            Err(error) => wallet_mediator::Snapshot {
                wallet: None,
                reason: format!("host session bus unavailable: {error}"),
            },
        };
        eprintln!("session_start: KWallet: {}", wallet_snapshot.reason);
        let wallet = match wallet_mediator::WalletMediator::start(
            &wallet_bus_address,
            wallet_snapshot.wallet.clone(),
        )
        .await
        {
            Ok(mediator) => mediator,
            Err(error) => return cleanup_err(format!("KWallet service: {error}"), &mut startup),
        };
        match spawn_dbus_proxy(
            &wallet_bus_address,
            &service_proxy_socket,
            &[
                // Upstream is the session-local wallet service on a private bus
                // (wallet_mediator); these rules keep the session to KWallet
                // calls on that bus.
                "--see=org.kde.kwalletd6",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Introspectable.Introspect@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Peer.GetMachineId@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Peer.Ping@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Properties.Get@/*",
                "--call=org.kde.kwalletd6=org.freedesktop.DBus.Properties.GetAll@/*",
                "--call=org.kde.kwalletd6=org.kde.KWallet.isEnabled@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.networkWallet@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.localWallet@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.wallets@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.isOpen@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.open@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openAsync@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openPath@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.openPathAsync@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.folderList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.hasFolder@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entryList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entriesList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.hasEntry@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.entryType@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readEntry@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readMap@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.readPassword@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.mapList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.passwordList@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.users@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.close@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.createFolder@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.removeFolder@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.writePassword@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.writeEntry@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.writeMap@/modules/kwalletd6",
                "--call=org.kde.kwalletd6=org.kde.KWallet.removeEntry@/modules/kwalletd6",
                "--broadcast=org.kde.kwalletd6=org.kde.KWallet.walletAsyncOpened@/modules/kwalletd6",
                "--broadcast=org.kde.kwalletd6=org.kde.KWallet.walletOpened@/modules/kwalletd6",
            ],
        ).await {
            Ok(child) => startup.add_proxy(child),
            Err(error) => return cleanup_err(format!("session D-Bus proxy: {error:#}"), &mut startup),
        }
        let service_bus_address = format!("unix:path={}", service_proxy_socket.display());
        let proxy_sock_str = system_proxy_socket.display().to_string();

        let cdp_forward_port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr().map(|address| address.port()))
            .map_err(|error| ver_err(format!("reserve CDP forwarding port: {error}")))?;
        let host_name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map_err(|error| ver_err(format!("read host name: {error}")))?;
        let host_name = host_name.trim();
        if host_name.is_empty() {
            return Err(ver_err("host name is empty".to_owned()));
        }
        use std::os::unix::fs::MetadataExt;
        let identity = std::fs::metadata("/proc/self")
            .map_err(|error| ver_err(format!("read current UID and GID: {error}")))?;
        let uid = identity.uid().to_string();
        let gid = identity.gid().to_string();
        let ssh_config_overrides = prepare_ssh_config_overrides(&host_xdg_dir, identity.uid());
        // pasta gives the session its own loopback. Into the host it forwards
        // only the session's CDP port. Host listeners are reachable from the
        // session via --tcp-ns, but pasta's default `auto` ranges omit the host's
        // ephemeral port range. Scan every usable port with auto enabled so
        // active listeners are forwarded while unbound ports remain
        // private. The CDP port is excluded so the session's browser can bind it.
        let scope_unit = format!(
            "{}.scope",
            self.path.file_name().unwrap_or_default().to_string_lossy()
        );
        let scope = SessionScope::new(&self.path, memory);
        startup.scope_unit = Some(scope_unit.clone());
        let mut cmd =
            match sandbox_launcher(memory, &host_runtime, &host_session_bus, Some(&scope_unit)) {
                Ok(command) => command,
                Err(error) => return cleanup_err(error, &mut startup),
            };
        let cdp_forward_spec = format!("127.0.0.1/{cdp_forward_port}");
        // With auto, an explicit range defines the complete scan set, not
        // an addition to the default non-ephemeral range. Include all usable
        // ports while still forwarding only bound host listeners.
        let host_ports_spec = format!("auto,1-{},~{cdp_forward_port}", u16::MAX);
        let host_ipv4 = host_default_ipv4().unwrap_or_else(|error| {
            eprintln!(
                "session_start: host IPv4 lookup failed ({error:#}); pasta picks the address itself"
            );
            None
        });
        // A host address marked noprefixroute leaves pasta without a connected
        // route. Assign the default-route interface's IPv4 address and prefix explicitly.
        if let Some((address, prefix)) = &host_ipv4 {
            cmd.args([
                "--address",
                &address.to_string(),
                "--netmask",
                &prefix.to_string(),
            ]);
        }
        cmd.args([
            "--quiet",
            "--config-net",
            "--no-map-gw",
            "--host-lo-to-ns-lo",
            "--tcp-ports",
            &cdp_forward_spec,
            "--udp-ports",
            "none",
            "--tcp-ns",
            &host_ports_spec,
            "--udp-ns",
            &host_ports_spec,
            "--",
            "bwrap",
        ]);
        // FUSE needs a process holding CAP_SYS_ADMIN in the user namespace that
        // owns the sandbox's mount namespace. With --uid other than 0, bwrap
        // (needing root for devpts) nests a second user namespace, where the
        // capability cannot mount. So with FUSE, bwrap stays at uid 0 in one
        // namespace for the helper, and the entrypoint nests the namespace
        // that maps the real uid for everything else.
        let fuse = fuse_support();
        let (sandbox_uid, sandbox_gid) = if fuse.is_some() {
            ("0", "0")
        } else {
            (uid.as_str(), gid.as_str())
        };
        cmd.args([
            "--die-with-parent",
            "--unshare-user",
            "--uid",
            sandbox_uid,
            "--gid",
            sandbox_gid,
            "--unshare-pid",
            "--unshare-uts",
            "--hostname",
            host_name,
            "--unshare-ipc",
        ]);
        let fuse_exe = if fuse.is_some() {
            // Keep this large copy on the disk-backed session twin. Putting it
            // in host_xdg_dir charges it to /tmp's per-user tmpfs quota and can
            // leave later session operations unable to write there.
            let exe = session_disk_path(&host_xdg_dir).join("kwin-mcp-exe");
            let temporary = exe.with_extension("partial");
            let copied = std::fs::copy("/proc/self/exe", &temporary)
                .and_then(|_| std::fs::rename(&temporary, &exe));
            if let Err(error) = copied {
                let _ = std::fs::remove_file(&temporary);
                return Err(ver_err(format!(
                    "copy running binary for the FUSE bridge: {}",
                    describe_write_failure(&temporary, &error)
                )));
            }
            startup.fuse_bridge_exe = Some(exe.clone());
            Some(exe)
        } else {
            None
        };
        let mut pre_overlay_binds = Vec::new();
        // Keep the real agent inside the same filtered bus and sandbox. Only
        // its known missing-PolicyKit registration diagnostic moves to a log;
        // startup status and unrelated stderr remain visible to applications.
        if let Some(agent) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("pkttyagent"))
                .find(|candidate| candidate.is_file())
        }) {
            use std::os::unix::fs::PermissionsExt;
            let agent = std::fs::canonicalize(agent)
                .map_err(|error| ver_err(format!("resolve session pkttyagent: {error}")))?;
            let shim = host_xdg_dir.join("pkttyagent-session.sh");
            std::fs::write(&shim, include_str!("../data/pkttyagent-session.sh"))
                .and_then(|_| {
                    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                })
                .map_err(|error| ver_err(format!("write session pkttyagent wrapper: {error}")))?;
            pre_overlay_binds.push((
                agent.clone(),
                PathBuf::from("/run/kwin-mcp-pkttyagent/real"),
            ));
            pre_overlay_binds.push((shim, agent));
        }
        if let (Some(binaries), Some(exe)) = (&fuse, &fuse_exe) {
            pre_overlay_binds.push((exe.clone(), PathBuf::from(fuse_bridge::BRIDGE_EXECUTABLE)));
            for (real, program) in binaries {
                let hidden = PathBuf::from(format!("{}/{program}", fuse_bridge::REAL_BINARY_DIR));
                pre_overlay_binds.push((real.clone(), hidden));
            }
            // Preserve all real helpers before replacing any PATH entry with
            // the bridge; the host may resolve multiple names to one binary.
            for (real, _) in binaries {
                pre_overlay_binds.push((exe.clone(), real.clone()));
            }
        }
        overlay_plan.add_bwrap_args(&mut cmd, &overlay_target, &pre_overlay_binds);
        let kwinrc_str = kwinrc_path.display().to_string();
        let kdeglobals_str = kdeglobals_path.display().to_string();
        let kwinrulesrc_str = kwinrulesrc_path.display().to_string();
        let kscreenlockerrc_str = kscreenlockerrc_path.display().to_string();
        let kcmfonts_str = kcmfonts_path.display().to_string();
        let fonts_conf_str = fonts_conf_path.display().to_string();
        let home_kwinrc = format!("{home}/.config/kwinrc");
        let home_kdeglobals = format!("{home}/.config/kdeglobals");
        let home_kwinrulesrc = format!("{home}/.config/kwinrulesrc");
        let home_kscreenlockerrc = format!("{home}/.config/kscreenlockerrc");
        let home_kcmfonts = format!("{home}/.config/kcmfonts");
        let home_fonts_conf = format!("{home}/.config/fontconfig/fonts.conf");
        let script_directory = session_disk_path(&host_xdg_dir).join(kwin_script::SCRIPT_DIRECTORY);
        let container_script_directory = host_xdg_dir.join(kwin_script::SCRIPT_DIRECTORY);
        cmd.args(["--dev", "/dev"]);
        let trimmed_dri = usable_dri_nodes(nvidia_gbm_installed());
        match &trimmed_dri {
            None => {
                cmd.args(["--dev-bind", "/dev/dri", "/dev/dri"]);
            }
            Some(nodes) => {
                eprintln!(
                    "session_start: /dev/dri nodes shown to the session: {}",
                    nodes.len()
                );
                cmd.args(["--dir", "/dev/dri"]);
                for node in nodes {
                    cmd.arg("--dev-bind").arg(node).arg(node);
                }
            }
        }
        cmd.args([
            "--dev-bind",
            "/dev/uinput",
            "/dev/uinput",
            "--dev-bind",
            &mouse_evdev_str,
            &mouse_evdev_str,
            "--dev-bind",
            &kbd_evdev_str,
            &kbd_evdev_str,
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--ro-bind-try",
            &proxy_sock_str,
            "/run/dbus/system_bus_socket",
            "--bind",
            &xdg_dir_str,
            &xdg_dir_str,
            // Geometry/window inspection must not need a free page on /tmp
            // before screenshot's PNG-save error or inline fallback can run.
            "--ro-bind",
            &script_directory.display().to_string(),
            &container_script_directory.display().to_string(),
            // System config overrides (read-only)
            "--ro-bind",
            &atspi_conf_path.display().to_string(),
            "/usr/share/defaults/at-spi2/accessibility.conf",
            "--ro-bind",
            &fc_hinting_str,
            &fc_hinting_dest_str,
            "--ro-bind",
            &fc_lcd_str,
            &fc_lcd_dest_str,
            // Mask dbus service files so the container's dbus-daemon doesn't auto-activate
            // $HOME config overrides (read-only — protects display settings from agent writes)
            "--ro-bind",
            &kwinrc_str,
            &home_kwinrc,
            "--ro-bind",
            &kdeglobals_str,
            &home_kdeglobals,
            "--ro-bind",
            &kwinrulesrc_str,
            &home_kwinrulesrc,
            "--ro-bind",
            &kscreenlockerrc_str,
            &home_kscreenlockerrc,
            "--ro-bind",
            &kcmfonts_str,
            &home_kcmfonts,
            "--ro-bind",
            &fonts_conf_str,
            &home_fonts_conf,
        ]);
        for (copy, destination) in &ssh_config_overrides {
            cmd.arg("--ro-bind").arg(copy).arg(destination);
        }
        // NVIDIA Vulkan clients need character devices outside /dev/dri. Keep them
        // available when all DRM nodes are exposed or a selected node is NVIDIA-backed,
        // without granting them to sessions selecting only another renderer.
        let nvidia_dri_exposed = match &trimmed_dri {
            None => true,
            Some(nodes) => nodes.iter().any(|node| is_nvidia_dri_node(node)),
        };
        if nvidia_dri_exposed {
            for path in glob::glob("/dev/nvidia*").into_iter().flatten().flatten() {
                cmd.arg("--dev-bind-try").arg(&path).arg(&path);
            }
        }
        // FUSE: /dev/fuse, CAP_SYS_ADMIN in the sandbox's user namespace for
        // the helper only, and fusermount shims over the real binaries.
        let sandbox_command = match &fuse {
            Some(binaries) => {
                // Bind a copy of the running image, never the path it was
                // started from: once the binary is rebuilt that path names a
                // different or missing file ("<path> (deleted)"), bwrap cannot
                // bind it, and the sandbox died before D-Bus came up.
                // /proc/self/exe stays readable after deletion; bwrap will not
                // bind it directly, so it is copied into the disk-backed twin
                // and bound before HOME overlays hide that host path.
                cmd.args([
                    "--dev-bind",
                    "/dev/fuse",
                    "/dev/fuse",
                    "--cap-add",
                    "CAP_SYS_ADMIN",
                ]);
                let entrypoint_path = host_xdg_dir.join("entrypoint.sh");
                std::fs::write(&entrypoint_path, &entrypoint)
                    .map_err(|e| ver_err(format!("write entrypoint: {e}")))?;
                eprintln!(
                    "session_start: FUSE bridge enabled ({})",
                    binaries
                        .iter()
                        .map(|(_, program)| *program)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                // The AppImage runtime only accepts a setuid-root fusermount
                // from PATH, which no binary is inside a user namespace; it
                // takes FUSERMOUNT_PROG as given.
                let prog = binaries
                    .iter()
                    .find(|(_, program)| *program == "fusermount3")
                    .map(|(real, _)| {
                        format!(
                            "export FUSERMOUNT_PROG={}\n",
                            shell_quote(&real.display().to_string())
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{prog}{} --fuse-helper {} </dev/null >/dev/null &\nexec unshare --user --map-user={uid} --map-group={gid} setpriv --inh-caps=-all --ambient-caps=-all bash {}",
                    shell_quote(fuse_bridge::BRIDGE_EXECUTABLE),
                    fuse_bridge::HELPER_SOCKET,
                    shell_quote(&entrypoint_path.display().to_string()),
                )
            }
            None => {
                eprintln!(
                    "session_start: FUSE unavailable on this host (needs /dev/fuse, non-setuid bwrap, setpriv, fusermount)"
                );
                entrypoint.clone()
            }
        };
        cmd.args(["--", "bash", "-c", &sandbox_command]);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::null());
        // Applications can write arbitrary bytes (including crash diagnostics).
        // Keep them off MCP stderr: clients may stop draining it on invalid UTF-8.
        let sandbox_log_path = session_disk_path(&host_xdg_dir).join("sandbox.log");
        let sandbox_log = match std::fs::File::create(&sandbox_log_path) {
            Ok(file) => file,
            Err(error) => return cleanup_err(format!("create sandbox log: {error}"), &mut startup),
        };
        cmd.stderr(std::process::Stdio::from(sandbox_log));
        eprintln!(
            "session_start: sandbox stderr: {}",
            sandbox_log_path.display()
        );
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        terminate_with_parent(&mut cmd);
        self.set_start_stage("spawning the pasta and bwrap sandbox");
        let sandbox_child = match spawn_session_child(cmd, true).await {
            Ok(child) => child,
            Err(error) => {
                return cleanup_err(
                    format!("start pasta: {error} (install passt)"),
                    &mut startup,
                );
            }
        };
        startup.sandbox_child = Some(sandbox_child);
        let sandbox_pid = startup
            .sandbox_child
            .as_ref()
            .map(std::process::Child::id)
            .unwrap_or_default();
        if self.display.autoclean
            && let Err(error) = self.workdir.mark_sandbox(sandbox_pid)
        {
            return cleanup_err(format!("record sandbox owner: {error}"), &mut startup);
        }
        eprintln!("session_start: pasta spawned pid={sandbox_pid:?}");
        if let Some(child) = startup.sandbox_child.as_ref() {
            test_stop_bwrap(child);
        }
        let sandbox_stdin = startup
            .sandbox_child
            .as_mut()
            .and_then(|child| child.stdin.take());
        match sandbox_stdin {
            Some(stdin) => startup.sandbox_stdin = Some(stdin),
            None => return cleanup_err("sandbox stdin not available".to_owned(), &mut startup),
        }
        test_startup_delay("after-bwrap").await;
        // Wait for dbus-ready marker (entrypoint touches it after dbus-daemon starts)
        let dbus_ready_path = host_xdg_dir.join("dbus-ready");
        self.set_start_stage("waiting for the container D-Bus daemon");
        if let Err(e) = wait_for_sandbox_path(
            &dbus_ready_path,
            "dbus-ready marker",
            std::time::Instant::now() + STARTUP_TIMEOUT,
            &mut startup.sandbox_child,
            None,
        )
        .await
        {
            return cleanup_err(e, &mut startup);
        }
        eprintln!("session_start: dbus-ready");
        let bus_addr = format!("unix:path={xdg_dir_str}/bus");

        // Create proxy_conn: claims org.kde.KWin, registers InputDevice objects
        // This must happen BEFORE KWin starts so we own the well-known name
        eprintln!("session_start: creating proxy_conn");
        let proxy_stream = std::os::unix::net::UnixStream::connect(host_xdg_dir.join("bus"))
            .map_err(KwinError::from)?;
        let proxy_socket: std::os::fd::OwnedFd =
            proxy_stream.try_clone().map_err(KwinError::from)?.into();
        proxy_stream
            .set_nonblocking(true)
            .map_err(KwinError::from)?;
        let proxy_stream =
            tokio::net::UnixStream::from_std(proxy_stream).map_err(KwinError::from)?;
        let proxy_conn = zbus::connection::Builder::unix_stream(proxy_stream)
            .build()
            .await
            .map_err(KwinError::from)?;
        // Claim org.kde.KWin on proxy_conn (before KWin starts, so we get it first)
        if let Err(e) = proxy_conn.request_name("org.kde.KWin").await {
            return cleanup_err(format!("claim org.kde.KWin: {e}"), &mut startup);
        }
        eprintln!("session_start: proxy_conn owns org.kde.KWin");

        // Register InputDevice objects on proxy_conn
        let mouse_sysname = mouse_evdev
            .file_name()
            .ok_or_else(|| ver_err("no mouse sysname".to_owned()))?
            .to_string_lossy()
            .to_string();
        let kbd_sysname = kbd_evdev
            .file_name()
            .ok_or_else(|| ver_err("no keyboard sysname".to_owned()))?
            .to_string_lossy()
            .to_string();
        let mouse_dev = input_bridge::InputDevice::new_pointer(mouse_sysname);
        let kbd_dev = input_bridge::InputDevice::new_keyboard(kbd_sysname);
        if let Err(e) = input_bridge::register_devices(&proxy_conn, vec![mouse_dev, kbd_dev]).await
        {
            return cleanup_err(format!("register input devices: {e}"), &mut startup);
        }
        eprintln!("session_start: input devices registered on proxy_conn");

        // Signal bridge-ready so the entrypoint starts KWin
        let bridge_ready_path = host_xdg_dir.join("bridge-ready");
        std::fs::write(&bridge_ready_path, "")
            .map_err(|e| ver_err(format!("write bridge-ready: {e}")))?;
        eprintln!("session_start: bridge-ready signaled, KWin starting");

        // Create kwin_conn: separate connection for talking to KWin
        let kwin_conn =
            match connect_session_bus(&bus_addr, std::time::Instant::now() + STARTUP_TIMEOUT).await
            {
                Ok(conn) => conn,
                Err(e) => return cleanup_err(e, &mut startup),
            };

        // Wait for KWin's wayland-0 socket to appear (proves KWin is running)
        let wayland_socket = host_xdg_dir.join("wayland-0");
        self.set_start_stage("waiting for KWin's Wayland socket");
        if let Err(e) = wait_for_sandbox_path(
            &wayland_socket,
            "wayland-0 socket",
            std::time::Instant::now() + STARTUP_TIMEOUT,
            &mut startup.sandbox_child,
            Some((&host_xdg_dir.join(KWIN_EXIT_FILE), "kwin_wayland")),
        )
        .await
        {
            return cleanup_err(e, &mut startup);
        }
        eprintln!("session_start: wayland-0 ready");

        // Discover KWin's unique bus name — try each unique name for EIS interface
        eprintln!("session_start: discovering KWin unique name");
        let dbus_proxy = zbus::fdo::DBusProxy::new(&kwin_conn)
            .await
            .map_err(|e| ver_err(format!("DBus proxy: {e}")))?;
        let kwin_unique_name;
        let kwin_deadline = std::time::Instant::now() + STARTUP_TIMEOUT;
        // Skip our own connections
        let proxy_unique = proxy_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let kwin_conn_unique = kwin_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        loop {
            let names = dbus_proxy
                .list_names()
                .await
                .map_err(|e| ver_err(format!("ListNames: {e}")))?;
            let mut found = None;
            for name in &names {
                let name_str = name.as_str();
                if !name_str.starts_with(':') {
                    continue;
                }
                if name_str == proxy_unique || name_str == kwin_conn_unique {
                    continue;
                }
                // Quick probe with timeout — Introspect the EIS path
                let probe_result = tokio::time::timeout(KWIN_NAME_PROBE_TIMEOUT, async {
                    let p: zbus::Proxy = zbus::proxy::Builder::new(&kwin_conn)
                        .destination(name_str)?
                        .path("/org/kde/KWin/EIS/RemoteDesktop")?
                        .interface("org.freedesktop.DBus.Introspectable")?
                        .build()
                        .await?;
                    let r: (String,) = p.call("Introspect", &()).await?;
                    Ok::<String, zbus::Error>(r.0)
                })
                .await;
                if let Ok(Ok(xml)) = probe_result
                    && xml.contains("connectToEIS")
                {
                    found = Some(name_str.to_owned());
                    break;
                }
            }
            if let Some(name) = found {
                kwin_unique_name = name;
                break;
            }
            if std::time::Instant::now() >= kwin_deadline {
                return cleanup_err(
                    "could not discover KWin unique name".to_owned(),
                    &mut startup,
                );
            }
            tokio::time::sleep(STARTUP_POLL).await;
        }
        eprintln!("session_start: KWin unique name = {kwin_unique_name}");

        // Connect to KWin EIS using its unique name
        self.set_start_stage("connecting to KWin EIS input");
        let eis_builder = KWinEisProxy::builder(&kwin_conn)
            .destination(kwin_unique_name.as_str())
            .map_err(|e| ver_err(format!("EIS proxy builder: {e}")))?;
        let eis_proxy = match eis_builder.build().await {
            Ok(p) => p,
            Err(e) => return cleanup_err(format!("KWin EIS proxy: {e}"), &mut startup),
        };
        let (eis_fd, _cookie) = match eis_proxy.connect_to_eis(EIS_CAPS_KBD_POINTER).await {
            Ok(r) => r,
            Err(e) => return cleanup_err(format!("connectToEIS: {e}"), &mut startup),
        };
        eprintln!("session_start: EIS fd received, negotiating");
        let eis_owned_fd = std::os::fd::OwnedFd::from(eis_fd);
        let mut eis = match tokio::task::spawn_blocking(move || Eis::from_fd(eis_owned_fd)).await {
            Ok(Ok(eis)) => eis,
            Ok(Err(e)) => return cleanup_err(format!("EIS negotiation: {e}"), &mut startup),
            Err(e) => return cleanup_err(format!("EIS task: {e}"), &mut startup),
        };
        eis.audit = self.input_log.clone();
        eprintln!("session_start: EIS ready");

        let atspi_bus_address = atspi::proxy::bus::BusProxy::new(&kwin_conn)
            .await
            .map_err(KwinError::from)?
            .get_address()
            .await
            .map_err(KwinError::from)?;

        // Chromium's ATK bridge stays dormant while org.a11y.Status reports
        // accessibility disabled — Chrome then registers on the AT-SPI bus but
        // exposes zero children. Non-fatal: Qt apps are unaffected either way
        // (QT_LINUX_ACCESSIBILITY_ALWAYS_ON is exported in the entrypoint).
        if let Err(e) = kwin_conn
            .call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.freedesktop.DBus.Properties"),
                "Set",
                &(
                    "org.a11y.Status",
                    "IsEnabled",
                    zbus::zvariant::Value::from(true),
                ),
            )
            .await
        {
            eprintln!(
                "session_start: enabling org.a11y.Status failed (Chromium AT-SPI trees will be empty): {e}"
            );
        }

        let bus_name = kwin_conn
            .unique_name()
            .map(|n| n.to_string())
            .unwrap_or_default();
        let workdir = host_xdg_dir.display().to_string();
        let temp = std::env::temp_dir();
        let runtime_root_fallback = (host_xdg_dir.parent() != Some(temp.as_path())).then(|| {
            serde_json::json!({
                "tmpdir": temp,
                "reason": format!(
                    "session socket paths under TMPDIR would exceed the libdbus limit of {DBUS_SOCKET_PATH_MAX} bytes"
                ),
            })
        });
        let msg = format!(
            "{version_stamp} — session started bus={bus_name} kwin={kwin_unique_name} display={screen_w}x{screen_h}"
        );
        let viewer = viewer_report(&host_xdg_dir, None, None);
        let msg = format!("{msg} {}", viewer_summary(&viewer));
        let host_write_paths = std::mem::take(&mut overlay_plan.host_write_paths);
        let msg = if host_write_paths.is_empty() {
            msg
        } else {
            format!(
                "{msg}; writes reach the host in host_write_paths: {}",
                host_write_paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let oversized_read_only_files = std::mem::take(&mut overlay_plan.oversized_read_only_files);
        let sqlite_access = std::mem::take(&mut overlay_plan.sqlite_access);
        let msg = if sqlite_access.total_databases == 0 {
            msg
        } else {
            format!(
                "{msg}; SQLite access modes are listed in sqlite_access. live_read_only databases cannot support application writes; request writable_paths on a new session for private writable snapshots."
            )
        };
        let msg = if oversized_read_only_files.is_empty() {
            msg
        } else {
            format!(
                "{msg}; {} oversized ancestor file(s) are read-only; see oversized_read_only_files. Request writable_paths on a new session for private writable copies.",
                oversized_read_only_files.len()
            )
        };
        let socket_links = std::mem::take(&mut overlay_plan.socket_links);
        let overlay_work_paths = overlay_plan
            .overlays
            .iter()
            .map(|overlay| overlay.work.join("work"))
            .collect();
        // Keep the startup guard responsible for the sandbox and staged FUSE
        // image until every fallible session resource has been acquired.
        let uinput_mouse_fd = std::os::fd::AsFd::as_fd(&uinput_mouse)
            .try_clone_to_owned()
            .map_err(KwinError::from)?;
        let uinput_keyboard_fd = std::os::fd::AsFd::as_fd(&uinput_keyboard)
            .try_clone_to_owned()
            .map_err(KwinError::from)?;
        let mut guard = self.session.write().await;
        let (sandbox_child, sandbox_stdin, service_proxy_children) = startup.into_parts();
        *guard = Some(Session {
            host_write_paths: host_write_paths.clone(),
            scope: scope.clone(),
            kwin_conn,
            _proxy_conn: proxy_conn,
            _proxy_socket: proxy_socket,
            kwin_unique_name: kwin_unique_name.clone(),
            service_bus_address,
            atspi_bus_address,
            eis,
            sandbox_child: sandbox_child.into(),
            sandbox_stdin: sandbox_stdin.into(),
            host_xdg_dir,
            _uinput_mouse: uinput_mouse_fd,
            _uinput_keyboard: uinput_keyboard_fd,
            cdp_browser: None,
            cdp_forward_port,
            service_proxy_children: service_proxy_children
                .into_iter()
                .map(SessionProcess::from)
                .collect(),
            mouse_sysname: input_sysname(&mouse_evdev),
            kbd_sysname: input_sysname(&kbd_evdev),
            viewer_child: None,
            viewer_unavailable: None,
            overlay_work_paths,
            cookie_roots,
            cookie_errors: cookie_errors.clone(),
            oversized_read_only_files: oversized_read_only_files.clone(),
            _socket_links: socket_links,
            screen_width: screen_w,
            screen_height: screen_h,
            last_activity: std::time::Instant::now(),
            last_input: None,
            fuse_enabled: fuse.is_some(),
            wallet: Some(wallet),
            wallet_note: wallet_snapshot
                .wallet
                .is_none()
                .then(|| wallet_snapshot.reason.clone()),
        });
        Ok(structured_result(&peer, msg, serde_json::json!({
            "status": "started",
            "browser_profiles": browser_profiles,
            "startup_timeout_seconds": params.startup_timeout_seconds.unwrap_or(SESSION_START_HARD_TIMEOUT.as_secs()),
            "memory_limits": scope.report(),
            "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
            "commit": env!("GIT_HASH"),
            "bus": bus_name,
            "kwin_unique": kwin_unique_name,
            "workdir": workdir,
            "host_shared_dir": workdir,
            // Server flags, not session_start parameters, decide this.
            "workdir_removed_on_stop": self.display.autoclean,
            "ttl_minutes": self.display.ttl.map(|ttl| ttl.as_secs() / 60),
            "runtime_root_fallback": runtime_root_fallback,
            "width": screen_w,
            "height": screen_h,
            "viewer": viewer,
            "cookie_errors": cookie_errors,
            "oversized_read_only_files": oversized_read_only_files,
            "sqlite_access": sqlite_access,
            "host_write_paths": host_write_paths,
            "wallet": {
                "mode": if wallet_snapshot.wallet.is_some() { "session-local copy" } else { "disabled" },
                "entries": wallet_snapshot.wallet.as_ref().map_or(0, wallet_mediator::Wallet::entry_count),
                "reason": wallet_snapshot.reason,
            },
        })).await)
    }

    #[rmcp::tool(
        name = "clipboard_write",
        description = "Write UTF-8 text with a live isolated KWin MCP session, even for target=host. This server controls its own isolated desktop, not the user's active desktop session. target=session writes that session's CLIPBOARD and PRIMARY for agent input, including X11 Shift+Insert. target=host writes the server host's Wayland clipboard used by the local viewer: viewer Ctrl+V/Shift+Insert pastes from it, and Ctrl+C/Ctrl+X copies to it. Host does not select a remote viewer client's clipboard or an arbitrary user desktop. For a request to change the user's active desktop clipboard outside this session/viewer workflow, use the appropriate separate desktop mechanism. Specify target and exactly one of text or generate_secret. generate_secret uses operating-system randomness, length 1..4096, and includes every selected character class (lowercase, uppercase, digits, symbols). The result returns only target and character length; clipboard_read withholds generated values."
    )]
    async fn clipboard_write(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ClipboardWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let (text, secret) = match (params.text, params.generate_secret) {
            (Some(text), None) => (text, false),
            (None, Some(generate)) => (generate_secret(generate)?, true),
            _ => {
                return Err(McpError::invalid_params(
                    "specify exactly one of text or generate_secret",
                    None,
                ));
            }
        };
        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session; call session_start first", None)
        })?;
        let socket = match params.target {
            ClipboardTarget::Session => session.host_xdg_dir.join("wayland-0"),
            ClipboardTarget::Host => {
                let (runtime, display) = host_wayland().await.map_err(KwinError::from)?;
                if Path::new(&display).is_absolute() {
                    PathBuf::from(display)
                } else {
                    runtime.join(display)
                }
            }
        };
        let store = self.clipboards.clone();
        let label = params.target.label();
        let primary = matches!(params.target, ClipboardTarget::Session);
        let length = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
                .write(socket, label, text, secret, primary)
        })
        .await
        .map_err(|error| KwinError::Msg(format!("clipboard task failed: {error}")))?
        .map_err(KwinError::from)?;
        drop(guard);
        Ok(structured_result(
            &peer,
            format!("wrote {length} characters to the {label} clipboard"),
            serde_json::json!({"target":params.target,"length":length}),
        )
        .await)
    }

    #[rmcp::tool(
        name = "clipboard_read",
        description = "Read UTF-8 text from the selected live isolated KWin MCP session's clipboard. Requires a live session, just like clipboard_write with either target. This is the isolated desktop's selection, not the server host's or remote viewer client's clipboard. Local viewer Ctrl+V/Shift+Insert reads the server host clipboard instead. This tool does not read the user's active desktop clipboard; use an appropriate separate desktop mechanism for that request. A generated secret returns only length and protected=true, with text=null.",
        annotations(read_only_hint = true)
    )]
    async fn clipboard_read(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let guard = self.session.read().await;
        let session = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session; call session_start first", None)
        })?;
        let socket = session.host_xdg_dir.join("wayland-0");
        let store = self.clipboards.clone();
        let contents = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
                .entry(&socket, "session")?
                .clipboard
                .read_text()
        })
        .await
        .map_err(|error| KwinError::Msg(format!("clipboard task failed: {error}")))?
        .map_err(KwinError::from)?;
        drop(guard);
        use kwin_mcp::clipboard::ClipboardText;
        let (message, data) = match contents {
            ClipboardText::Empty => (
                "session clipboard has no text".to_owned(),
                serde_json::json!({"target":"session","text":null,"length":0,"protected":false}),
            ),
            ClipboardText::Secret(length) => (
                "session clipboard contains a generated secret; value withheld".to_owned(),
                serde_json::json!({"target":"session","text":null,"length":length,"protected":true}),
            ),
            ClipboardText::Text(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| {
                    McpError::internal_error("session clipboard text is not UTF-8", None)
                })?;
                let length = text.chars().count();
                (
                    text.clone(),
                    serde_json::json!({"target":"session","text":text,"length":length,"protected":false}),
                )
            }
        };
        Ok(structured_result(&peer, message, data).await)
    }

    #[rmcp::tool(
        name = "viewer_open",
        description = "Open or reuse the live viewer for this isolated session on the server's desktop. Use it when the user asks to watch or must see or act on a password, OTP, Duo, CAPTCHA, choice or result. First navigate to the relevant form and show the entry; pass reason with a short instruction for the window title. Never assume the viewer is open or stop the session for a handoff. Keep the page open and poll screenshots; continue when the step completes. If Duo expires, leave the resend option visible and tell the user. The viewer may stay open; the user can close it. Viewer input is forwarded immediately and cancels remaining in-flight agent events with retry_safe=false: inspect before retrying. New agent input receives a retry-safe error for one second after viewer input; resume after a quiet second. Returns ready after the host renderer submits its first PipeWire frame, or unavailable with the startup failure and last PipeWire state. On a headless host, the reason gives a kwin-viewer --remote HOST DIR command to show this session elsewhere without restarting it. Works with --no-viewer. In the local viewer Ctrl+C/Ctrl+X copies to the server host clipboard and Ctrl+V/Shift+Insert pastes from it. clipboard_write target=host prepares local viewer paste; target=session prepares agent input. clipboard_read reads session text only and withholds generated secrets."
    )]
    async fn viewer_open(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ViewerOpenParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let _gate = self
            .lifecycle_gate(
                tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT,
                "viewer_open",
            )
            .await?;
        let _input = self.input_gate.clone().lock_owned().await;
        let reason = params
            .reason
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let title = if reason.is_empty() {
            format!("kwin-viewer {}", self.session_id)
        } else {
            format!("kwin-viewer {}: {reason}", self.session_id)
        };
        let (host_xdg_dir, width, height, existing, stale) = {
            let mut guard = self.session.write().await;
            let sess = guard.as_mut().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            let title_path = sess.host_xdg_dir.join("viewer-title");
            let temporary = title_path.with_extension("tmp");
            std::fs::write(&temporary, &title).map_err(KwinError::from)?;
            std::fs::rename(&temporary, &title_path).map_err(KwinError::from)?;
            let running = match sess.viewer_child.as_mut() {
                Some(child) => matches!(child.try_wait(), Ok(None)),
                None => false,
            };
            let viewer = if running {
                sess.viewer_child
                    .as_mut()
                    .map(|child| viewer_report(&sess.host_xdg_dir, Some(child), None))
            } else {
                None
            };
            if let Some(viewer) = viewer.as_ref()
                && viewer["state"] == "ready"
            {
                let message = format!("viewer already open: {}", viewer_summary(viewer));
                drop(guard);
                return Ok(structured_result(
                    &peer,
                    message,
                    serde_json::json!({"status": "already_open", "viewer": viewer,"title":title}),
                )
                .await);
            }
            let reuse = viewer
                .as_ref()
                .is_some_and(|viewer| viewer["state"] == "starting");
            let (existing, stale) = if running {
                let child = sess.viewer_child.take();
                if reuse { (child, None) } else { (None, child) }
            } else {
                (None, None)
            };
            (
                sess.host_xdg_dir.clone(),
                sess.screen_width,
                sess.screen_height,
                existing,
                stale,
            )
        };
        if let Some(stale) = stale {
            tokio::task::spawn_blocking(move || terminate_child(stale, false, "stale viewer"))
                .await
                .map_err(|error| KwinError::Msg(format!("viewer cleanup task failed: {error}")))?;
        }
        let reused = existing.is_some();
        let spawned = match existing {
            Some(child) => Ok(child),
            None => spawn_viewer(&host_xdg_dir, width, height).await,
        };
        let mut guard = self.session.write().await;
        let Some(sess) = guard
            .as_mut()
            .filter(|sess| sess.host_xdg_dir == host_xdg_dir)
        else {
            // The session stopped while the viewer was starting.
            if let Ok(child) = spawned {
                terminate_child(child, false, "orphaned viewer");
            }
            return Err(McpError::internal_error(
                "session stopped while opening the viewer",
                None,
            ));
        };
        match spawned {
            Ok(child) => {
                if let Some(old) = sess.viewer_child.replace(child) {
                    terminate_child(old, false, "exited viewer");
                }
                sess.viewer_unavailable = None;
            }
            Err(reason) => sess.viewer_unavailable = Some(reason),
        }
        let mut child = sess.viewer_child.take();
        let unavailable = sess.viewer_unavailable.clone();
        drop(guard);
        let mut viewer = match child.as_mut() {
            Some(child) if unavailable.is_none() => wait_for_viewer(&host_xdg_dir, child).await,
            _ => viewer_report(&host_xdg_dir, None, unavailable.as_deref()),
        };
        if viewer["state"] == "starting" {
            let pipewire = std::fs::read_to_string(host_xdg_dir.join("viewer-pipewire.json"))
                .unwrap_or_else(|_| "not connected".to_owned());
            let reason = format!(
                "viewer did not submit a frame within {} seconds; last PipeWire state: {}",
                VIEWER_READY_WAIT.as_secs(),
                pipewire.trim()
            );
            if let Some(stalled) = child.take() {
                tokio::task::spawn_blocking(move || {
                    terminate_child(stalled, false, "stalled viewer")
                })
                .await
                .map_err(|error| KwinError::Msg(format!("viewer cleanup task failed: {error}")))?;
            }
            std::fs::write(
                host_xdg_dir.join(VIEWER_STATUS_FILE),
                serde_json::json!({"state":"failed","detail":reason}).to_string(),
            )
            .map_err(KwinError::from)?;
            viewer = serde_json::json!({"state":"unavailable","reason":reason,"log":host_xdg_dir.join("viewer.log")});
        }
        if viewer["state"] == "unavailable"
            && let Some(mut failed) = child.take()
            && matches!(failed.try_wait(), Ok(None))
        {
            tokio::task::spawn_blocking(move || terminate_child(failed, false, "failed viewer"))
                .await
                .map_err(|error| KwinError::Msg(format!("viewer cleanup task failed: {error}")))?;
        }
        let mut guard = self.session.write().await;
        match guard
            .as_mut()
            .filter(|sess| sess.host_xdg_dir == host_xdg_dir)
        {
            Some(sess) => {
                if viewer["state"] == "unavailable" {
                    sess.viewer_unavailable = viewer["reason"].as_str().map(str::to_owned);
                }
                sess.viewer_child = child;
            }
            None => {
                if let Some(child) = child {
                    terminate_child(child, false, "orphaned viewer");
                }
            }
        }
        drop(guard);
        let status = if viewer["state"] == "unavailable" {
            "unavailable"
        } else if reused {
            "already_open"
        } else {
            "opened"
        };
        Ok(structured_result(
            &peer,
            format!("viewer {status}: {}", viewer_summary(&viewer)),
            serde_json::json!({"status": status, "viewer": viewer,"title":title}),
        )
        .await)
    }

    #[rmcp::tool(
        name = "viewer_close",
        description = "Close the host viewer window when the user asks, without stopping the isolated session. Completing a user-facing step does not require closing the viewer; the user may keep watching or close it directly. No-op if the viewer is already closed. Call viewer_open again if the user needs to act or asks to watch later."
    )]
    async fn viewer_close(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let _gate = self
            .lifecycle_gate(
                tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT,
                "viewer_close",
            )
            .await?;
        let _input = self.input_gate.clone().lock_owned().await;
        let mut guard = self.session.write().await;
        let sess = guard.as_mut().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let child = sess.viewer_child.take();
        sess.viewer_unavailable = None;
        let host_xdg_dir = sess.host_xdg_dir.clone();
        drop(guard);
        let status = if let Some(child) = child {
            terminate_child(child, false, "viewer");
            "closed"
        } else {
            "already_closed"
        };
        let viewer = viewer_report(&host_xdg_dir, None, None);
        Ok(structured_result(
            &peer,
            format!("viewer {status}"),
            serde_json::json!({"status": status, "viewer": viewer}),
        )
        .await)
    }

    #[rmcp::tool(
        name = "session_stop",
        description = "Tear down the current session and its container processes. With --autoclean or --ttl (session_start reports workdir_removed_on_stop), remove the session workdir, including files the host wrote to host_shared_dir; if removal fails, calling session_stop again retries it. Transport shutdown also cleans up. No-op if no session is running and nothing is left to clean.",
        annotations(destructive_hint = true)
    )]
    async fn session_stop(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        // A start whose host call is still blocked holds the lifecycle gate until
        // that call returns. Its stop is recorded now: the handle stays until the
        // cleanup it owns runs, and a later session_stop clears it.
        let gate_busy = self.start_gate.try_lock().is_err();
        if session_registry::stop_is_pending(&self.registry_phase().await, gate_busy) {
            return Ok(structured_result(
                &peer,
                "session stop recorded; this session's startup is blocked in a host call, and its cleanup runs when that call returns. Call session_stop again then to clear it.",
                serde_json::json!({
                    "status": "cleanup_pending",
                    "session_status": session_registry::STATUS_START_BLOCKED,
                    "cookie_errors": [],
                }),
            )
            .await);
        }
        // Serialize stop with the entire start attempt, including workdir
        // creation and failed-start cleanup. Without this gate, stop can see no
        // published Session, delete a workdir that startup is still using, and
        // release cleanup ownership while start continues against the path.
        let _start_gate = self
            .lifecycle_gate(
                tokio::time::Instant::now() + SESSION_START_HARD_TIMEOUT,
                "session_stop",
            )
            .await?;
        let plan = session_registry::stop_plan(&self.registry_phase().await);
        let mut cookie_errors = Vec::new();
        match plan {
            session_registry::StopPlan::Teardown => {
                // The stopped status is recorded before the desktop is released,
                // so a reader sees the desktop or its status, never neither.
                let mut guard = self.session.write().await;
                if guard.is_some() {
                    self.set_death(serde_json::json!({
                        "status": "stopped",
                        "session_status": "stopped",
                        "session_available_here": false,
                        "reason": SESSION_LIFECYCLE_REASON_SESSION_STOP,
                        "session_end_reason": SESSION_LIFECYCLE_REASON_SESSION_STOP,
                        "session_ended_at_unix_seconds": unix_now(),
                    }));
                }
                let stopped = guard.take();
                drop(guard);
                if let Some(sess) = stopped {
                    if let Ok(mut clipboards) = self.clipboards.lock() {
                        clipboards
                            .entries
                            .remove(&sess.host_xdg_dir.join("wayland-0"));
                    }
                    cookie_errors = teardown_blocking(sess).await;
                }
            }
            session_registry::StopPlan::Clear => {}
            session_registry::StopPlan::Abandon => self.set_death(terminal_record(
                session_registry::STATUS_START_FAILED,
                SESSION_LIFECYCLE_REASON_START_FAILED,
            )),
        }
        let teardown = matches!(plan, session_registry::StopPlan::Teardown);
        let removed = match self.workdir.remove() {
            WorkdirCleanup::NothingOwned => None,
            WorkdirCleanup::Removed(dir) => Some(dir),
            WorkdirCleanup::Retained { dir, error } => {
                self.record_terminal_cleanup("retained");
                let terminal = self.death_record();
                // The Session is already torn down, so name that too: cleanup
                // ownership is the only state left, and another session_stop is
                // its retry.
                let stopped_note = if teardown {
                    "session stopped, but "
                } else {
                    ""
                };
                let cookie_note = if cookie_errors.is_empty() {
                    String::new()
                } else {
                    format!(" Cookie save errors: {}.", cookie_errors.join("; "))
                };
                return Err(McpError::internal_error(
                    format!(
                        "{stopped_note}session workdir {} not removed: {error}. Cleanup is still owned, call session_stop again to retry.{cookie_note}",
                        dir.display()
                    ),
                    Some(serde_json::json!({
                        "cookie_errors": cookie_errors,
                        "session_status": terminal.as_ref().and_then(|record| record["session_status"].as_str()).unwrap_or("none_running"),
                        "session_available_here": false,
                        "session_end_reason": terminal.as_ref().and_then(|record| record["reason"].as_str()),
                        "session_ended_at_unix_seconds": terminal.as_ref().and_then(|record| record["session_ended_at_unix_seconds"].as_u64()),
                    })),
                ));
            }
        };
        // The cleanup record is written unless a clear removed nothing and the
        // handle's end is already recorded. Teardown always records its stop.
        let recorded = self
            .death_record()
            .as_ref()
            .and_then(terminal_status)
            .map(str::to_owned)
            .unwrap_or_default();
        let cleanup = if removed.is_some() {
            "removed"
        } else {
            "not_requested"
        };
        if teardown
            || matches!(plan, session_registry::StopPlan::Abandon)
            || session_registry::clear_writes_record(&recorded, removed.is_some())
        {
            self.record_terminal_cleanup(cleanup);
        }
        let workdir_removed = removed.map(|dir| dir.display().to_string());
        let (status, subject) = if teardown {
            ("stopped", "session stopped".to_owned())
        } else {
            ("cleaned", format!("cleared {recorded} session"))
        };
        let message = match &workdir_removed {
            Some(workdir) => format!("{subject}, workdir {workdir} removed"),
            None => subject,
        };
        let lifecycle_record = previous_session_lifecycle(&self.session_id).ok().flatten();
        Ok(structured_result(
            &peer,
            message,
            serde_json::json!({
                "status": status,
                "workdir_removed": workdir_removed,
                "cookie_errors": cookie_errors,
                "session_lifecycle_record": lifecycle_record,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "window_list",
        description = "List every managed app window in the isolated session, ordered from topmost to bottommost. Includes IDs, titles, app classes, geometry, active/minimized/hidden state, and modal/transient relationships; showing_desktop=true means KDE show desktop (super+d) hides every listed window without minimizing it. Call this whenever an expected prompt or transition is not visible in screenshot; a fully covered window still appears here. Use window_activate with the returned ID to reveal and inspect it.",
        annotations(read_only_hint = true)
    )]
    async fn window_list(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        let mut windows = window_snapshots(&conn, &kwin_unique, &xdg).await?;
        // Listed windows exclude the desktop and docks, which show desktop keeps.
        // null when KWin does not answer; the list itself is still useful.
        let showing = showing_desktop(&conn, &kwin_unique).await.ok();
        let desktop_shown = showing == Some(true);
        if desktop_shown {
            for window in &mut windows {
                window.hidden = true;
            }
        }
        let mut text = if windows.is_empty() {
            "no managed windows".to_owned()
        } else {
            windows
                .iter()
                .map(WindowSnapshot::summary)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let count = windows.len();
        let mut data = serde_json::json!({
            "count": count, "showing_desktop": showing, "windows": windows,
        });
        if desktop_shown {
            text = format!("{SHOWING_DESKTOP_WARNING}\n{text}");
            data["warning"] = serde_json::json!(SHOWING_DESKTOP_WARNING);
        }
        Ok(structured_result(&peer, text, data).await)
    }

    #[rmcp::tool(
        name = "window_activate",
        description = "Reveal a specific isolated-session window by exact ID from window_list: unminimize it, raise it above covering windows, and give it focus. If a visible non-popup transient descendant belongs to that window, activate the topmost such descendant instead so keyboard input reaches the displayed chooser. The returned window identifies the actual focused target; requested_window_id identifies the request. Then call screenshot to render it and use the normal input tools. This replaces blind Alt+Tab cycling when a modal, prompt, or app window is hidden."
    )]
    async fn window_activate(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<WindowActivateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        let window = activate_window(&conn, &kwin_unique, &xdg, &params.window_id).await?;
        self.mark_input().await;
        let text = format!("activated {}", window.summary());
        Ok(structured_result(
            &peer,
            text,
            serde_json::json!({"status": "activated", "requested_window_id": params.window_id, "window": window}),
        )
        .await)
    }

    // Alpha-blends the high-visibility cursor sprite onto an RGBA buffer so that
    // the arrow tip lands exactly at (cx, cy). The sprite is the raw rasterized
    // PNG (with transparent padding and soft shadow); we shift its origin by the
    // hardcoded CURSOR_HOTSPOT_* offsets instead of cropping at runtime.
    fn overlay_cursor(rgba: &mut [u8], img_w: u32, img_h: u32, cx: i32, cy: i32) {
        let Some(sprite) = cursor_sprite() else {
            return;
        };
        let origin_x = cx - CURSOR_HOTSPOT_X;
        let origin_y = cy - CURSOR_HOTSPOT_Y;
        for dy in 0..sprite.h {
            let Ok(dy_i) = i32::try_from(dy) else {
                continue;
            };
            let py = origin_y + dy_i;
            if py < 0 {
                continue;
            }
            let Ok(py_u) = u32::try_from(py) else {
                continue;
            };
            if py_u >= img_h {
                continue;
            }
            for dx in 0..sprite.w {
                let Ok(dx_i) = i32::try_from(dx) else {
                    continue;
                };
                let px = origin_x + dx_i;
                if px < 0 {
                    continue;
                }
                let Ok(px_u) = u32::try_from(px) else {
                    continue;
                };
                if px_u >= img_w {
                    continue;
                }
                let Some(s_lin) = dy
                    .checked_mul(sprite.w)
                    .and_then(|v| v.checked_add(dx))
                    .and_then(|v| v.checked_mul(4))
                else {
                    continue;
                };
                let Some(d_lin) = py_u
                    .checked_mul(img_w)
                    .and_then(|v| v.checked_add(px_u))
                    .and_then(|v| v.checked_mul(4))
                else {
                    continue;
                };
                let Ok(s) = usize::try_from(s_lin) else {
                    continue;
                };
                let Ok(d) = usize::try_from(d_lin) else {
                    continue;
                };
                if s + 3 >= sprite.rgba.len() || d + 3 >= rgba.len() {
                    continue;
                }
                let a = u32::from(sprite.rgba[s + 3]);
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                for c in 0..3 {
                    let src_c = u32::from(sprite.rgba[s + c]);
                    let dst_c = u32::from(rgba[d + c]);
                    rgba[d + c] = u8::try_from((src_c * a + dst_c * inv) / 255).unwrap_or(255);
                }
                rgba[d + 3] = 255;
            }
        }
    }

    #[rmcp::tool(
        name = "screenshot",
        description = "Capture the active window as a PNG written to the session workdir. Pixels are 1:1 and window-relative, the same space as mouse input: pixel (x,y) of a default screenshot is mouse_click (x,y), also for a non-maximized dialog. The active window identity and geometry are checked before and after capture; a changed or unavailable target returns a retryable error without an image, and success reports target_verification. The metadata names the captured window (id, title, display x/y, size) and region=[x1,y1,x2,y2], the window-relative rectangle the image covers; for any crop, image pixel (px,py) is mouse_click (px+x1, py+y1). Pass cursor=true for an image centered on the cursor. include_cursor=false (default true) omits both pointer layers without moving it, also for cursor crops. A screenshot within 1.5s of an input tool waits until the screen has been unchanged for 200ms, so it reflects input the app has already handled (settle.settled=false means the screen was still changing, e.g. an animation). Pass region=[x1,y1,x2,y2] in window-relative pixels to crop (it may extend past the window, e.g. negative values to include a popup outside a small dialog); prefer cropping when you know which area matters. region and cursor are mutually exclusive. inline=true returns the PNG in the tool result (base64); the file on disk is written either way; save_as keeps a copy at a new absolute host path. Inline metadata is always included as JSON text; structured_metadata=true also exposes structuredContent, only for clients that keep displaying image content alongside structured results. With no active window, it waits up to 2 seconds for focus to land. Requires an open app (call launch_app first if needed).",
        annotations(read_only_hint = true)
    )]
    async fn screenshot(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        tokio::time::timeout(
            SCREENSHOT_TOOL_TIMEOUT,
            self.screenshot_result(peer, params),
        )
        .await
        .map_err(|_| screenshot_timeout("screenshot", SCREENSHOT_TOOL_TIMEOUT))?
    }

    async fn screenshot_result(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        params: ScreenshotParams,
    ) -> Result<CallToolResult, McpError> {
        let conn = self.kwin_conn().await?;
        let kwin_unique = self.kwin_unique_name().await?;
        let xdg = self.host_xdg_dir().await?;
        if params.cursor && params.region.is_some() {
            return Err(McpError::invalid_params(
                "region and cursor are mutually exclusive",
                None,
            ));
        }
        let (win_x, win_y, win_geo) = awaited_active_window_info(&conn, &kwin_unique, &xdg).await?;
        let windows = window_snapshots(&conn, &kwin_unique, &xdg).await?;
        let observed = windows
            .iter()
            .find(|window| window.id == win_geo.id)
            .filter(|window| window.active && !window.minimized)
            .ok_or_else(|| {
                transient_capture_error(
                    &win_geo.id,
                    "capture target changed before dialog classification",
                )
            })?;
        if [observed.x, observed.y, observed.w, observed.h]
            != [win_geo.x, win_geo.y, win_geo.w, win_geo.h]
        {
            return Err(transient_capture_error(
                &win_geo.id,
                "capture target geometry changed before dialog classification",
            ));
        }
        let transient = observed.dialog || observed.modal || observed.transient;

        #[expect(clippy::as_conversions)]
        let (win_w, win_h) = (win_geo.w.round() as i32, win_geo.h.round() as i32);
        // Every coordinate here is window-relative, matching mouse input: by
        // default the image is the active window itself, so image pixel (x,y)
        // is mouse_click (x,y). A region or cursor crop may extend past the
        // window edges (e.g. to include a popup) and is clamped to the screen.
        let region = if params.cursor {
            #[expect(clippy::as_conversions)]
            let (cx, cy) = (
                win_geo.cx.round() as i32 - win_x,
                win_geo.cy.round() as i32 - win_y,
            );
            [
                cx - CURSOR_ZOOM_HALF_EDGE,
                cy - CURSOR_ZOOM_HALF_EDGE,
                cx + CURSOR_ZOOM_HALF_EDGE,
                cy + CURSOR_ZOOM_HALF_EDGE,
            ]
        } else {
            params.region.unwrap_or([0, 0, win_w, win_h])
        };
        let proxy = KWinScreenShot2Proxy::builder(&conn)
            .destination(kwin_unique.as_str())
            .map_err(KwinError::from)?
            .build()
            .await
            .map_err(KwinError::from)?;
        let include_cursor = params.include_cursor.unwrap_or(true);
        let (mut width, mut height, mut stride, mut pixels, settle) =
            capture_settled_frame(&proxy, self.last_input().await, include_cursor).await?;
        // CaptureScreen returns the composited display, which can change focus
        // while we wait for it to settle. Do not crop or label another window's
        // pixels using the identity and geometry sampled before that wait.
        let (_, _, observed_window) = active_window_info(&conn, &kwin_unique, &xdg)
            .await
            .map_err(|error| {
                McpError::internal_error(
                    format!(
                        "screenshot could not recheck its active window after capture: {error}. No image was returned or saved; inspect window_list and capture again."
                    ),
                    Some(serde_json::json!({
                        "reason": "capture_target_unavailable",
                        "stage": "checking the capture target",
                        "retry_safe": true,
                        "expected_window": capture_target_metadata(&win_geo),
                    })),
                )
            })?;
        if win_geo.id != observed_window.id
            || win_geo.x != observed_window.x
            || win_geo.y != observed_window.y
            || win_geo.w != observed_window.w
            || win_geo.h != observed_window.h
        {
            return Err(McpError::internal_error(
                "screenshot target changed or moved during capture. No image was returned or saved; inspect window_list, activate the intended window, and capture again.",
                Some(serde_json::json!({
                    "reason": "capture_target_changed",
                    "stage": "checking the capture target",
                    "retry_safe": true,
                    "expected_window": capture_target_metadata(&win_geo),
                    "observed_window": capture_target_metadata(&observed_window),
                })),
            ));
        }
        let transient_pixel_check = if transient {
            // A dialog that has just mapped can be missing from the composited
            // screen until KWin renders its next frame: recapture until the
            // screen and the window capture agree, or the wait runs out.
            let deadline = tokio::time::Instant::now() + TRANSIENT_CAPTURE_WAIT;
            let mut attempts = 0_u32;
            loop {
                attempts += 1;
                let reference = tokio::time::timeout(
                    SCREENSHOT_FRAME_TIMEOUT,
                    capture_source_inner(&proxy, Some(&win_geo.id), false),
                )
                .await
                .map_err(|_| {
                    transient_capture_error(&win_geo.id, "independent dialog capture timed out")
                })?
                .map_err(|error| transient_capture_error(&win_geo.id, error.message.to_string()))?;
                let (_, _, after) = active_window_info(&conn, &kwin_unique, &xdg)
                    .await
                    .map_err(|error| transient_capture_error(&win_geo.id, error.to_string()))?;
                if after.id != win_geo.id
                    || [after.x, after.y, after.w, after.h]
                        != [win_geo.x, win_geo.y, win_geo.w, win_geo.h]
                {
                    return Err(transient_capture_error(
                        &win_geo.id,
                        "the active dialog or its client geometry changed during capture",
                    ));
                }
                #[expect(clippy::as_conversions)]
                let cursors = [
                    (win_geo.cx.round() as i32, win_geo.cy.round() as i32),
                    (after.cx.round() as i32, after.cy.round() as i32),
                ];
                let composite = (width, height, stride, pixels);
                let compared = compare_transient_capture(
                    &composite,
                    &reference,
                    (win_x, win_y),
                    (
                        u32::try_from(win_w).map_err(KwinError::from)?,
                        u32::try_from(win_h).map_err(KwinError::from)?,
                    ),
                    &cursors,
                );
                pixels = composite.3;
                match compared {
                    Ok(mut evidence) => {
                        evidence["reference_window_id"] = serde_json::json!(win_geo.id);
                        evidence["captures"] = serde_json::json!(attempts);
                        break Some(evidence);
                    }
                    Err(detail) if tokio::time::Instant::now() >= deadline => {
                        return Err(transient_capture_error(
                            &win_geo.id,
                            format!("{detail}; still differing after {attempts} captures"),
                        ));
                    }
                    Err(_) => {
                        tokio::time::sleep(SCREENSHOT_SETTLE_POLL).await;
                        (width, height, stride, pixels) =
                            capture_frame(&proxy, include_cursor).await?;
                    }
                }
            }
        } else {
            None
        };
        // The composited screen is opaque; convert normalized BGRA to RGBA.
        let px = usize::try_from(width * height).map_err(KwinError::from)?;
        let mut rgba = vec![0u8; px * 4];
        for row in 0..height {
            for col in 0..width {
                let si = usize::try_from(row * stride + col * 4).map_err(KwinError::from)?;
                let di = usize::try_from((row * width + col) * 4).map_err(KwinError::from)?;
                rgba[di] = pixels[si + 2];
                rgba[di + 1] = pixels[si + 1];
                rgba[di + 2] = pixels[si];
                rgba[di + 3] = pixels[si + 3];
            }
        }
        // Window-relative region -> clamped display rectangle.
        let [x1, y1, x2, y2] = region;
        let clamp_x = |v: i32| {
            u32::try_from(v.saturating_add(win_x).max(0))
                .unwrap_or(0)
                .min(width)
        };
        let clamp_y = |v: i32| {
            u32::try_from(v.saturating_add(win_y).max(0))
                .unwrap_or(0)
                .min(height)
        };
        let (cx1, cy1, cx2, cy2) = (clamp_x(x1), clamp_y(y1), clamp_x(x2), clamp_y(y2));
        let cw = cx2.saturating_sub(cx1);
        let ch = cy2.saturating_sub(cy1);
        if cw == 0 || ch == 0 {
            return Err(McpError::invalid_params(
                format!("region [{x1},{y1},{x2},{y2}] (window-relative) has zero area on screen"),
                None,
            ));
        }
        let mut out_rgba = vec![0u8; usize::try_from(cw * ch * 4).map_err(KwinError::from)?];
        for row in 0..ch {
            let src =
                usize::try_from((cy1 + row) * width * 4 + cx1 * 4).map_err(KwinError::from)?;
            let dst = usize::try_from(row * cw * 4).map_err(KwinError::from)?;
            let len = usize::try_from(cw * 4).map_err(KwinError::from)?;
            out_rgba[dst..dst + len].copy_from_slice(&rgba[src..src + len]);
        }
        let (out_w, out_h) = (cw, ch);
        // Image pixel (0,0) in window-relative input coordinates.
        let origin_x = i32::try_from(cx1).map_err(KwinError::from)? - win_x;
        let origin_y = i32::try_from(cy1).map_err(KwinError::from)? - win_y;
        let out_region = [
            origin_x,
            origin_y,
            origin_x + i32::try_from(cw).map_err(KwinError::from)?,
            origin_y + i32::try_from(ch).map_err(KwinError::from)?,
        ];
        // Overlay the high-visibility cursor; its position is absolute on the
        // screen, so shift it into the output frame by the crop's display origin.
        #[expect(clippy::as_conversions)]
        let cursor_abs_x = win_geo.cx.round() as i32;
        #[expect(clippy::as_conversions)]
        let cursor_abs_y = win_geo.cy.round() as i32;
        let crop_ox_i = i32::try_from(cx1).unwrap_or(0);
        let crop_oy_i = i32::try_from(cy1).unwrap_or(0);
        if include_cursor {
            Self::overlay_cursor(
                &mut out_rgba,
                out_w,
                out_h,
                cursor_abs_x - crop_ox_i,
                cursor_abs_y - crop_oy_i,
            );
        }
        let path = xdg.join("screenshot.png");
        let mut png_bytes: Vec<u8> = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, out_w, out_h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().map_err(KwinError::from)?;
            writer
                .write_image_data(&out_rgba)
                .map_err(KwinError::from)?;
        }
        // Never leave a partial or stale capture at the path: on failure the
        // previous screenshot is removed too, so the file is always this call's.
        let write_error = write_atomically(&path, &png_bytes).err().map(|error| {
            let _ = std::fs::remove_file(&path);
            describe_write_failure(&path, &error)
        });
        if let Some(error) = &write_error {
            eprintln!("screenshot: {error}");
            if !params.inline {
                return Err(McpError::internal_error(
                    format!(
                        "screenshot captured but not saved: {error}. Pass inline=true to receive the image without writing it."
                    ),
                    Some(serde_json::json!({"reason": "write_failed"})),
                ));
            }
        }
        // A kept copy: the workdir's screenshot.png is replaced on every call.
        let saved_as = match params.save_as.as_deref().map(Path::new) {
            None => None,
            Some(target) => {
                if !target.is_absolute() {
                    return Err(McpError::invalid_params(
                        "save_as must be an absolute host path",
                        None,
                    ));
                }
                if std::fs::symlink_metadata(target).is_ok() {
                    return Err(McpError::invalid_params(
                        format!(
                            "screenshot captured but not saved: {} already exists; choose a new save_as path",
                            target.display()
                        ),
                        Some(serde_json::json!({"reason": "save_as_exists"})),
                    ));
                }
                write_atomically(target, &png_bytes).map_err(|error| {
                    McpError::internal_error(
                        format!(
                            "screenshot captured but not saved to save_as: {}",
                            describe_write_failure(target, &error)
                        ),
                        Some(serde_json::json!({"reason": "save_as_failed"})),
                    )
                })?;
                Some(target.display().to_string())
            }
        };
        let path_str = path.to_string_lossy().to_string();
        let mut payload = serde_json::json!({
            "path": if write_error.is_some() { serde_json::Value::Null } else { serde_json::json!(path_str) },
            "saved_as": saved_as,
            "width": out_w,
            "height": out_h,
            "include_cursor": include_cursor,
            // Window-relative rectangle the image covers: image pixel (px,py)
            // is mouse_click (px + region[0], py + region[1]).
            "region": out_region,
            "window": {
                "id": win_geo.id,
                "title": observed_window.title,
                "x": win_x,
                "y": win_y,
                "width": win_w,
                "height": win_h,
            },
            "target_verification": {
                "method": "active_window_before_and_after",
                "matched": true,
                "after": capture_target_metadata(&observed_window),
            },
        });
        if let Some(evidence) = transient_pixel_check {
            payload["transient_pixel_check"] = evidence;
        }
        if let Some(settle) = &settle {
            payload["settle"] = serde_json::json!({
                "settled": settle.settled,
                "waited_ms": u64::try_from(settle.waited.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if let Some(error) = &write_error {
            payload["write_error"] = serde_json::json!(error);
        }
        let mut text = match &write_error {
            None => format!("{path_str} size={out_w}x{out_h}"),
            Some(error) => format!("inline only, file not saved ({error}) size={out_w}x{out_h}"),
        };
        // The capture matched the active window, but show desktop hides it.
        if showing_desktop(&conn, &kwin_unique).await.unwrap_or(false) {
            payload["showing_desktop"] = serde_json::json!(true);
            payload["warning"] = serde_json::json!(SHOWING_DESKTOP_WARNING);
            text = format!("{text}. {SHOWING_DESKTOP_WARNING}");
        }
        if params.inline {
            // Claude Code can hide content[] when structuredContent is present.
            // Preserve image rendering by default and let clients explicitly
            // opt into the same structured metadata as file captures.
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
            let mut image = Content::image(b64, "image/png");
            if let rmcp::model::RawContent::Image(raw) = &mut image.raw {
                let mut meta = rmcp::model::Meta::new();
                meta.0.insert(
                    "codex/imageDetail".to_owned(),
                    serde_json::json!("original"),
                );
                raw.meta = Some(meta);
            }
            let payload_text = serde_json::to_string(&payload).unwrap_or_else(|_| text.clone());
            let _ = peer
                .notify_logging_message(rmcp::model::LoggingMessageNotificationParam::new(
                    rmcp::model::LoggingLevel::Info,
                    serde_json::json!(text),
                ))
                .await;
            let mut result = CallToolResult::success(vec![
                Content::text(text),
                Content::text(payload_text),
                image,
            ]);
            if params.structured_metadata {
                result.structured_content = Some(payload);
            }
            Ok(result)
        } else {
            Ok(structured_result(&peer, text, payload).await)
        }
    }
    #[rmcp::tool(
        name = "accessibility_tree",
        description = "Dump the active app's widget hierarchy when structural context is needed. Trees can be large or empty if an app exposes no accessibility nodes; use screenshot and input in that case. Prefer find_ui_elements for one named control. The default traversal budget is 5 seconds; set timeout_seconds from 1 to 30 for a shorter or longer bounded call. If time runs out, gathered nodes are returned with incomplete=true and root=ID continuations; the interrupted level may contain only some nodes. Completed traversals return whole levels read breadth-first, as deep as fits in 30,000 characters, one tab of indent per level; the same text is also in structuredContent.tree for clients that show only structured results. A node on the last level that has more below ends in '+N children root=ID'; call again with root=ID to open that subtree. app_name filters top-level apps; role filters role names; max_depth caps the levels returned. show_elements=true keeps zero-rect, unnamed and offscreen nodes; default false trims them out.",
        annotations(read_only_hint = true)
    )]
    async fn accessibility_tree(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<AccessibilityTreeParams>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = match params.timeout_seconds {
            Some(seconds)
                if (ACCESSIBILITY_TREE_TIMEOUT_MIN_SECONDS
                    ..=ACCESSIBILITY_TREE_TIMEOUT_MAX_SECONDS)
                    .contains(&seconds) =>
            {
                Duration::from_secs(u64::from(seconds))
            }
            Some(_) => {
                return Err(McpError::invalid_params(
                    format!(
                        "timeout_seconds must be between {ACCESSIBILITY_TREE_TIMEOUT_MIN_SECONDS} and {ACCESSIBILITY_TREE_TIMEOUT_MAX_SECONDS}"
                    ),
                    None,
                ));
            }
            None => ATSPI_TRAVERSAL_TIMEOUT,
        };
        self.touch_activity().await;
        self.settle_input().await?;
        let mut tree = LevelTree::default();
        let max_levels = params.max_depth.map_or(usize::MAX, |depth| {
            usize::try_from(depth)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
        });
        match tokio::time::timeout(
            timeout,
            self.accessibility_tree_result(peer.clone(), params, &mut tree),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let (text, mut info) = tree.render_partial(max_levels, true);
                info["incomplete"] = serde_json::json!(true);
                info["timeout_seconds"] = serde_json::json!(timeout.as_secs());
                info["reason"] = serde_json::json!(format!(
                    "traversal reached the {} second limit",
                    timeout.as_secs()
                ));
                info["continuation"] = serde_json::json!(format!(
                    "Call accessibility_tree with a root=ID shown in this reply and a smaller max_depth to continue that subtree. Set timeout_seconds from {ACCESSIBILITY_TREE_TIMEOUT_MIN_SECONDS} to {ACCESSIBILITY_TREE_TIMEOUT_MAX_SECONDS} to adjust the time budget. If no nodes were read, use find_ui_elements for a named control or screenshot."
                ));
                Ok(structured_result(&peer, text, info).await)
            }
        }
    }

    async fn accessibility_tree_result(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        params: AccessibilityTreeParams,
        tree: &mut LevelTree,
    ) -> Result<CallToolResult, McpError> {
        let max_levels = params.max_depth.map_or(usize::MAX, |depth| {
            usize::try_from(depth)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
        });
        let show_elements = params.show_elements.unwrap_or(false);
        let role = params.role.map(|s| s.to_lowercase());
        let role_ok = |name: &str| {
            role.as_ref()
                .is_none_or(|needle| name.to_lowercase().contains(needle))
        };
        // CDP path for Chromium/Electron apps
        let cdp_browser = self
            .session
            .read()
            .await
            .as_ref()
            .and_then(|s| s.cdp_browser.clone());
        if let Some(browser) = cdp_browser
            && let Ok(pages) = browser.pages().await
        {
            for page in &pages {
                let url = page.url().await.ok().flatten().unwrap_or_default();
                if url.starts_with("chrome://") || url.starts_with("chrome-extension://") {
                    continue;
                }
                use chromiumoxide::cdp::browser_protocol::accessibility::{
                    GetFullAxTreeParams, GetFullAxTreeReturns,
                };
                let Ok(result) = page.execute(GetFullAxTreeParams::builder().build()).await else {
                    continue;
                };
                let returns: &GetFullAxTreeReturns = &result;
                let index: std::collections::HashMap<String, usize> = returns
                    .nodes
                    .iter()
                    .enumerate()
                    .map(|(i, node)| (node.node_id.inner().to_string(), i))
                    .collect();
                let text = |i: usize| -> Option<String> {
                    let node = &returns.nodes[i];
                    let value = |v: &Option<
                        chromiumoxide::cdp::browser_protocol::accessibility::AxValue,
                    >| {
                        v.as_ref()
                            .and_then(|v| v.value.as_ref())
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned()
                    };
                    let (role, name) = (value(&node.role), value(&node.name));
                    let shown = (!node.ignored || show_elements)
                        && (!name.is_empty() || show_elements)
                        && role_ok(&role);
                    shown.then(|| {
                        format!(
                            "{}\t{}",
                            tree_field(if role.is_empty() { "none" } else { &role }),
                            tree_field(&name)
                        )
                    })
                };
                // Map the AX tree onto shown nodes; hidden ones pass their children up.
                let starts: Vec<usize> = match &params.root {
                    Some(root) if !root.contains('/') => {
                        let Some(&at) = index.get(root) else { continue };
                        vec![at]
                    }
                    Some(_) => continue,
                    None => returns
                        .nodes
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| n.parent_id.is_none())
                        .map(|(i, _)| i)
                        .collect(),
                };
                let mut pending: Vec<(usize, Option<usize>)> =
                    starts.iter().rev().map(|&i| (i, None)).collect();
                let mut visited = std::collections::HashSet::new();
                loop {
                    let mut level = Vec::new();
                    while let Some((i, parent)) = pending.pop() {
                        if !visited.insert(i) {
                            continue;
                        }
                        let forced = params.root.is_some() && parent.is_none();
                        if let Some(line) = text(i).or_else(|| forced.then(|| "root".to_owned())) {
                            let at = tree.push(
                                line,
                                returns.nodes[i].node_id.inner().to_string(),
                                parent,
                            );
                            level.push((i, at));
                        } else {
                            for child in returns.nodes[i].child_ids.iter().flatten().rev() {
                                if let Some(&c) = index.get(child.inner()) {
                                    pending.push((c, parent));
                                }
                            }
                        }
                    }
                    if level.is_empty() || !tree.open_level() || tree.levels() > max_levels {
                        break;
                    }
                    for (i, at) in level.into_iter().rev() {
                        for child in returns.nodes[i].child_ids.iter().flatten().rev() {
                            if let Some(&c) = index.get(child.inner()) {
                                pending.push((c, Some(at)));
                            }
                        }
                    }
                }
                let (tree, mut info) = tree.render(max_levels);
                info["source"] = serde_json::json!("cdp");
                return Ok(structured_result(&peer, tree, info).await);
            }
        }
        // AT-SPI path for native apps
        if let Some(root) = &params.root
            && !root.contains('/')
        {
            return Err(McpError::invalid_params(
                format!("root '{root}' is not present in the browser accessibility tree"),
                None,
            ));
        }
        let (zbus_conn, screen) = self
            .with_session(|s| Ok((s.kwin_conn.clone(), (s.screen_width, s.screen_height))))
            .await?;
        let a11y_addr: String = atspi::proxy::bus::BusProxy::new(&zbus_conn)
            .await
            .map_err(KwinError::from)?
            .get_address()
            .await
            .map_err(KwinError::from)?;
        let a11y_bus = connect_session_bus(&a11y_addr, std::time::Instant::now() + STARTUP_TIMEOUT)
            .await
            .map_err(|e| McpError::internal_error(format!("AT-SPI bus: {e}"), None))?;
        let proxy_at = |dest: String, path: String| {
            let bus = a11y_bus.clone();
            async move {
                atspi::proxy::accessible::AccessibleProxy::builder(&bus)
                    .destination(dest)
                    .map_err(KwinError::from)?
                    .path(path)
                    .map_err(KwinError::from)?
                    .cache_properties(zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .map_err(KwinError::from)
            }
        };
        let app_name = params.app_name.map(|s| s.to_lowercase());
        let shown = |node: &AtspiNode| {
            role_ok(&node.role)
                && (show_elements || (node.is_useful() && node.on_screen(screen.0, screen.1)))
        };
        // Level 0: the requested root, or the first shown nodes of each matching app.
        let mut level: Vec<(usize, atspi::proxy::accessible::AccessibleProxy<'static>)> =
            Vec::new();
        let mut pending: Vec<(
            atspi::proxy::accessible::AccessibleProxy<'static>,
            Option<usize>,
        )> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        match &params.root {
            Some(root) => {
                let (dest, path) = root.find('/').map(|at| root.split_at(at)).ok_or_else(|| {
                    McpError::invalid_params(
                        format!("root '{root}' is not a node id from a previous reply"),
                        None,
                    )
                })?;
                let acc = proxy_at(dest.to_owned(), path.to_owned()).await?;
                let node = atspi_node(&acc)
                    .await
                    .map_err(|e| McpError::invalid_params(format!("root '{root}': {e}"), None))?;
                visited.insert(root.clone());
                let id = tree.push(node.text(), root.clone(), None);
                level.push((id, acc));
            }
            None => {
                let root = proxy_at(
                    "org.a11y.atspi.Registry".to_owned(),
                    "/org/a11y/atspi/accessible/root".to_owned(),
                )
                .await?;
                for app in root.get_children().await.map_err(KwinError::from)? {
                    let Some(dest) = app.name() else { continue };
                    let Ok(acc) = proxy_at(dest.to_string(), app.path().to_string()).await else {
                        continue;
                    };
                    let name = acc.name().await.unwrap_or_default().to_lowercase();
                    if app_name.as_ref().is_none_or(|needle| name.contains(needle)) {
                        pending.push((acc, None));
                    }
                }
            }
        }
        // Expand breadth-first: each round collects the next level's shown
        // nodes, passing through hidden ones, until the levels run out.
        loop {
            while let Some((acc, parent)) = pending.pop() {
                let identity = format!("{}{}", acc.inner().destination(), acc.inner().path());
                if !visited.insert(identity.clone()) {
                    continue;
                }
                let Ok(node) = atspi_node(&acc).await else {
                    continue;
                };
                if shown(&node) {
                    let at = tree.push(node.text(), identity, parent);
                    level.push((at, acc));
                } else {
                    for child in acc
                        .get_children()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .rev()
                    {
                        if let Some(dest) = child.name()
                            && let Ok(child) =
                                proxy_at(dest.to_string(), child.path().to_string()).await
                        {
                            pending.push((child, parent));
                        }
                    }
                }
            }
            if level.is_empty() || !tree.open_level() || tree.levels() > max_levels {
                break;
            }
            for (at, acc) in std::mem::take(&mut level).into_iter().rev() {
                for child in acc
                    .get_children()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                {
                    if let Some(dest) = child.name()
                        && let Ok(child) =
                            proxy_at(dest.to_string(), child.path().to_string()).await
                    {
                        pending.push((child, Some(at)));
                    }
                }
            }
        }
        let (tree, info) = tree.render(max_levels);
        Ok(structured_result(&peer, tree, info).await)
    }

    #[rmcp::tool(
        name = "find_ui_elements",
        description = "Find widgets by case-insensitive name/role text with whitespace collapsed. Optional role filters semantic roles (button includes push button); visible_only removes reported hidden/offscreen matches. Results identify their backend, application/page/frame, matched KWin window, visibility and coordinate space. Only bounds with mouse_coordinates_usable=true use the active-window pixel frame accepted by mouse_click. CDP bounds are CSS viewport pixels, not browser-window pixels. A visible match is not proof that it is clickable: CDP reports a center hit test, while native occlusion can remain unknown. Re-query after scrolling, navigation or switching windows; inspect screenshot for unknown or obscured matches.",
        annotations(read_only_hint = true)
    )]
    async fn find_ui_elements(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<FindUiElementsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        self.settle_input().await?;
        let query = find_ui_search_text(&params.query);
        let visible_only = params.visible_only.unwrap_or(false);
        let mut elements = Vec::new();
        let mut errors = Vec::new();
        let windows = match window_snapshots(
            &self.kwin_conn().await?,
            &self.kwin_unique_name().await?,
            &self.host_xdg_dir().await?,
        )
        .await
        {
            Ok(windows) => windows,
            Err(error) => {
                errors.push(format!("window association unavailable: {error}"));
                Vec::new()
            }
        };
        let active_id = windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.id.clone());
        let cdp_browser = self
            .session
            .read()
            .await
            .as_ref()
            .and_then(|s| s.cdp_browser.clone());
        if let Some(browser) = cdp_browser {
            let cdp = tokio::time::timeout(ATSPI_TRAVERSAL_TIMEOUT, async {
                let pages = browser.pages().await.map_err(|error| error.to_string())?;
                for page in pages.iter().take(FIND_UI_MAX_PAGES) {
                    let url = page.url().await.ok().flatten().unwrap_or_default();
                    if url.starts_with("chrome://") || url.starts_with("chrome-extension://") { continue; }
                    let result = page.evaluate(include_str!("find_ui_dom.js")).await.map_err(|error| error.to_string())?;
                    let text = result.value().and_then(serde_json::Value::as_str).ok_or("CDP search did not return JSON")?;
                    let snapshot: serde_json::Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
                    if snapshot["truncated"] == true { errors.push(format!("CDP DOM element limit reached on {url}")); }
                    for element in snapshot["elements"].as_array().into_iter().flatten() {
                        if elements.len() >= FIND_UI_MAX_MATCHES { return Err("match limit reached".to_owned()); }
                        let name = element["name"].as_str().unwrap_or_default();
                        let role = element["role"].as_str().unwrap_or_default();
                        if !(find_ui_search_text(name).contains(&query) || find_ui_search_text(role).contains(&query))
                            || !find_ui_role_matches(params.role.as_deref(), role)
                            || visible_only && element["visible"] != true { continue; }
                        let mut matched = element.clone();
                        matched["backend"] = serde_json::json!("cdp");
                        matched["surface"] = serde_json::json!({"url":url,"title":snapshot["title"],
                            "page_focused":snapshot["page_focused"],"viewport":snapshot["viewport"]});
                        matched["coordinate_space"] = serde_json::json!("css_viewport_pixels");
                        matched["mouse_coordinates_usable"] = serde_json::json!(false);
                        matched["window"] = serde_json::Value::Null;
                        matched["window_association"] = serde_json::json!("unverified_cdp_target");
                        elements.push(matched);
                    }
                }
                if pages.len() > FIND_UI_MAX_PAGES { return Err("page limit reached".to_owned()); }
                Ok::<(), String>(())
            }).await;
            match cdp {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(format!("CDP search incomplete: {error}")),
                Err(_) => errors.push("CDP search reached its five-second limit".to_owned()),
            }
        }

        // Native accessibility also exposes browser chrome, which a page DOM cannot describe.
        // Keep frame provenance with every descendant instead of flattening all apps together.
        let native = tokio::time::timeout(ATSPI_TRAVERSAL_TIMEOUT, async {
            use atspi::proxy::accessible::ObjectRefExt;
            let conn = self.with_session(|s| Ok(s.kwin_conn.clone())).await?;
            let address = atspi::proxy::bus::BusProxy::new(&conn).await.map_err(KwinError::from)?
                .get_address().await.map_err(KwinError::from)?;
            let bus = connect_session_bus(&address, std::time::Instant::now() + ATSPI_TRAVERSAL_TIMEOUT)
                .await.map_err(|error| McpError::internal_error(format!("AT-SPI bus: {error}"), None))?;
            let root = atspi::proxy::accessible::AccessibleProxy::builder(&bus)
                .destination("org.a11y.atspi.Registry").map_err(KwinError::from)?
                .cache_properties(zbus::proxy::CacheProperties::No).build().await.map_err(KwinError::from)?;
            let mut visited = 0;
            for app_obj in root.get_children().await.map_err(KwinError::from)? {
                let Ok(app) = app_obj.as_accessible_proxy(&bus).await else { continue; };
                let app_name = app.name().await.unwrap_or_default();
                for frame_obj in app.get_children().await.unwrap_or_default() {
                    let Ok(frame) = frame_obj.as_accessible_proxy(&bus).await else { continue; };
                    let Ok(frame_node) = atspi_node(&frame).await else { continue; };
                    let associated = find_ui_frame_window(&windows, &frame_node.name, frame_node.bounds);
                    let surface = serde_json::json!({"application":app_name,"frame_title":frame_node.name,
                        "frame_screen_bounds":frame_node.bounds,"bus":frame.inner().destination().to_string(),
                        "frame_path":frame.inner().path().to_string()});
                    let mut stack = vec![frame_obj];
                    while let Some(obj) = stack.pop() {
                        visited += 1;
                        if visited > FIND_UI_MAX_NODES || elements.len() >= FIND_UI_MAX_MATCHES {
                            return Err(McpError::internal_error("native search reached its node/match limit", None));
                        }
                        let Ok(acc) = obj.as_accessible_proxy(&bus).await else { continue; };
                        let Ok(node) = atspi_node(&acc).await else { continue; };
                        if node.is_useful()
                            && (find_ui_search_text(&node.name).contains(&query) || find_ui_search_text(&node.role).contains(&query))
                            && find_ui_role_matches(params.role.as_deref(), &node.role) {
                            let mut matched = find_ui_native_match(&node, associated, &windows, frame_node.bounds);
                            if !visible_only || matched["visible"] == true {
                                matched["surface"] = surface.clone();
                                matched["object_path"] = serde_json::json!(acc.inner().path().to_string());
                                elements.push(matched);
                            }
                        }
                        stack.extend(acc.get_children().await.unwrap_or_default().into_iter().rev());
                    }
                }
            }
            Ok::<(), McpError>(())
        }).await;
        match native {
            Ok(Ok(())) => {},
            Ok(Err(error)) => errors.push(format!("AT-SPI search incomplete: {error}")),
            Err(_) => errors.push("AT-SPI search reached its five-second limit; already collected matches are retained".to_owned()),
        }
        let lines = elements.iter().map(|element| format!("{}\t{}\t{}\t{}\tvisible={} occluded={} mouse_coordinates_usable={}\tsurface={}",
            element["role"].as_str().unwrap_or_default(), tree_field(element["name"].as_str().unwrap_or_default()),
            element["bounds"], element["coordinate_space"].as_str().unwrap_or_default(), element["visible"],
            element["occluded"], element["mouse_coordinates_usable"], element["surface"])).collect::<Vec<_>>();
        let results = if lines.is_empty() {
            format!("no observed elements matching '{}'", params.query)
        } else {
            lines.join("\n")
        };
        let sampled_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        Ok(structured_result(&peer, results.clone(), serde_json::json!({
            "matches":elements.len(),"query":params.query,"role":params.role,"visible_only":visible_only,
            "results":results,"elements":elements,"active_window_id":active_id,"server_pid":std::process::id(),
            "sampled_at_milliseconds":sampled_at,"incomplete":!errors.is_empty(),"errors":errors,
            "continuation":"Re-query after navigation, scroll or window changes. Only mouse_coordinates_usable bounds share mouse_click's active-window pixel frame; use screenshot for unknown or obscured matches."
        })).await)
    }

    #[rmcp::tool(
        name = "mouse_click",
        description = "Move the cursor to (x,y) and click. Coordinates are pixels relative to the active window's top-left. For find_ui_elements use only bounds marked mouse_coordinates_usable=true; CDP CSS-viewport bounds are not in this frame. button defaults to left (use right for context menus, middle rarely). double=true for file-manager-style open, triple=true to select a whole paragraph. No need to call mouse_move first; the click already positions the cursor."
    )]
    async fn mouse_click(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseClickParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, params.target_width)?;
        self.touch_activity().await;
        let x = params.x;
        let y = params.y;
        let (wx, wy, geometry) = active_window_info(
            &self.kwin_conn().await?,
            &self.kwin_unique_name().await?,
            &self.host_xdg_dir().await?,
        )
        .await?;
        let code = btn_code(params.button.as_deref())?;
        let count = match (params.triple, params.double) {
            (Some(true), _) => 3,
            (_, Some(true)) => 2,
            (Some(false) | None, Some(false) | None) => 1,
        };
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (
            f32::from(i16::try_from(wx + x).map_err(KwinError::from)?),
            f32::from(i16::try_from(wy + y).map_err(KwinError::from)?),
        );
        sess.eis
            .pointer_origin(wx, wy, &geometry)
            .map_err(KwinError::from)?;
        pace.move_pointer(
            &sess.eis,
            [geometry.cx, geometry.cy],
            [f64::from(ax), f64::from(ay)],
        )
        .await
        .map_err(KwinError::from)?;
        let mut sent = SentInput::new(&sess.eis);
        for n in 0..count {
            if n > 0 {
                tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
            }
            sent.button(code, true).map_err(KwinError::from)?;
            tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
            sent.button(code, false).map_err(KwinError::from)?;
        }
        drop(sent);
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!("clicked ({x},{y}) x{count}"),
            serde_json::json!({
                "action": "click", "x": x, "y": y, "count": count,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "mouse_move",
        description = "Move the cursor to (x,y) in window-relative pixels without clicking. Use only when you need to trigger a hover effect (tooltip, CSS :hover, menu reveal). For clicks, call mouse_click directly — it already moves the cursor.",
        annotations(read_only_hint = true)
    )]
    async fn mouse_move(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseMoveParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, params.target_width)?;
        self.touch_activity().await;
        let x = params.x;
        let y = params.y;
        let (wx, wy, geometry) = active_window_info(
            &self.kwin_conn().await?,
            &self.kwin_unique_name().await?,
            &self.host_xdg_dir().await?,
        )
        .await?;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (
            f32::from(i16::try_from(wx + x).map_err(KwinError::from)?),
            f32::from(i16::try_from(wy + y).map_err(KwinError::from)?),
        );
        sess.eis
            .pointer_origin(wx, wy, &geometry)
            .map_err(KwinError::from)?;
        pace.move_pointer(
            &sess.eis,
            [geometry.cx, geometry.cy],
            [f64::from(ax), f64::from(ay)],
        )
        .await
        .map_err(KwinError::from)?;
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!("moved ({x},{y})"),
            serde_json::json!({
                "action": "move", "x": x, "y": y,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "mouse_scroll",
        description = "Scroll at (x,y) in window-relative pixels — the cursor moves there first, then wheel events fire. delta is signed: positive = down (or right, with horizontal=true); negative = up/left. Default is smooth scroll, where delta is pixels in Chromium-family browsers and 120 is one wheel notch in other apps (good for documents and web); set discrete=true and delta counts wheel notches (better for lists, dropdowns, sliders). Choose (x,y) inside the element you want to scroll, not just anywhere in the window."
    )]
    async fn mouse_scroll(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseScrollParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, params.target_width)?;
        self.touch_activity().await;
        let x = params.x;
        let y = params.y;
        let delta = params.delta;
        let (wx, wy, geometry) = active_window_info(
            &self.kwin_conn().await?,
            &self.kwin_unique_name().await?,
            &self.host_xdg_dir().await?,
        )
        .await?;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (ax, ay) = (
            f32::from(i16::try_from(wx + x).map_err(KwinError::from)?),
            f32::from(i16::try_from(wy + y).map_err(KwinError::from)?),
        );
        sess.eis
            .pointer_origin(wx, wy, &geometry)
            .map_err(KwinError::from)?;
        pace.move_pointer(
            &sess.eis,
            [geometry.cx, geometry.cy],
            [f64::from(ax), f64::from(ay)],
        )
        .await
        .map_err(KwinError::from)?;
        let horiz = params.horizontal.unwrap_or_default();
        let discrete = params.discrete.unwrap_or_default();
        let reports = if discrete {
            delta.unsigned_abs()
        } else {
            delta.unsigned_abs().div_ceil(SCROLL_PIXELS_PER_NOTCH)
        }
        .min(SCROLL_MAX_REPORTS);
        let interval = pace
            .delay(SCROLL_REPORT_INTERVAL_MS)
            .map_err(KwinError::from)?;
        let mut sent = 0;
        for index in 1..=reports {
            if index > 1 {
                tokio::time::sleep(interval).await;
            }
            // Cumulative rounding keeps the reports within one unit of each other and their sum exact.
            let total = i32::try_from(i64::from(delta) * i64::from(index) / i64::from(reports))
                .map_err(KwinError::from)?;
            let part = total - sent;
            sent = total;
            let (dx, dy) = if horiz { (part, 0) } else { (0, part) };
            if discrete {
                sess.eis.scroll_discrete(dx, dy).map_err(KwinError::from)?;
            } else {
                sess.eis
                    .scroll_smooth(
                        f32::from(i16::try_from(dx).map_err(KwinError::from)?)
                            / SCROLL_PIXELS_PER_AXIS_UNIT,
                        f32::from(i16::try_from(dy).map_err(KwinError::from)?)
                            / SCROLL_PIXELS_PER_AXIS_UNIT,
                    )
                    .map_err(KwinError::from)?;
            }
        }
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!("scrolled {delta} at ({x},{y})"),
            serde_json::json!({
                "action": "scroll", "x": x, "y": y, "delta": delta,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "mouse_drag",
        description = "Press button at (from_x, from_y), smoothly move to (to_x, to_y), release. Use for text selection, window dragging, drag-and-drop, and slider adjustments — a plain mouse_click followed by mouse_move will NOT trigger drag handlers, because the button is already released by then. button defaults to left. All coords are window-relative pixels."
    )]
    async fn mouse_drag(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<MouseDragParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, params.target_width)?;
        self.touch_activity().await;
        let from_x = params.from_x;
        let from_y = params.from_y;
        let to_x = params.to_x;
        let to_y = params.to_y;
        let (wx, wy, geometry) = active_window_info(
            &self.kwin_conn().await?,
            &self.kwin_unique_name().await?,
            &self.host_xdg_dir().await?,
        )
        .await?;
        let code = btn_code(params.button.as_deref())?;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let ax = f32::from(i16::try_from(wx + from_x).map_err(KwinError::from)?);
        let ay = f32::from(i16::try_from(wy + from_y).map_err(KwinError::from)?);
        sess.eis
            .pointer_origin(wx, wy, &geometry)
            .map_err(KwinError::from)?;
        pace.move_pointer(
            &sess.eis,
            [geometry.cx, geometry.cy],
            [f64::from(ax), f64::from(ay)],
        )
        .await
        .map_err(KwinError::from)?;
        let end = [
            f64::from(i16::try_from(wx + to_x).map_err(KwinError::from)?),
            f64::from(i16::try_from(wy + to_y).map_err(KwinError::from)?),
        ];
        let mut sent = SentInput::new(&sess.eis);
        sent.button(code, true).map_err(KwinError::from)?;
        pace.move_pointer(&sess.eis, [f64::from(ax), f64::from(ay)], end)
            .await
            .map_err(KwinError::from)?;
        sent.button(code, false).map_err(KwinError::from)?;
        drop(sent);
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!("dragged ({from_x},{from_y})->({to_x},{to_y})"),
            serde_json::json!({
                "action": "drag", "from_x": from_x, "from_y": from_y, "to_x": to_x, "to_y": to_y,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "keyboard_type",
        description = "Type printable ASCII (letters, digits, standard punctuation, space, tab, newline) with paced key events into the focused field. Click or Tab into the target field first. Waits for KWin to acknowledge all events, then verifies the complete expected text through focused-field accessibility or rich-text descendants when available. A rich-text value that AT-SPI cannot reconstruct is reported as unverified. unverified or pending_or_mismatched requires inspecting the field before sending or retrying, because retyping can duplicate delayed text. If keys were submitted but accessibility focus is on a non-editable control, the result is unverified because the keys may have reached the application. not_in_field is returned only when no characters were submitted; its warning names the focused control. Single-line browser text requires an observable editable target before sending; terminals and protected fields remain unverified. During typing, a changed or unobservable original focus/window cancels remaining keys and reports partial counts with retry_safe:false. Inspect both surfaces before retrying. Polling cannot prevent every key from reaching a newly focused surface. Tab/Return cannot be field-verified. For key combinations use keyboard_key. Non-ASCII input errors before any keys are sent. For fast UTF-8 or long input use keyboard_paste. keyboard_type is paced like a person, about 0.1 s per character and 45 s at most (timings_ms reports it); raise speed to type faster. A terminal's text is unreadable through AT-SPI, so the result stays unverified: screenshot the terminal before pressing Return."
    )]
    async fn keyboard_type(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardTypeParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, None)?;
        self.touch_activity().await;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let held = sess.eis.held_modifiers();
        if !held.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "Typing was not started: {} is still held from keyboard_press, so each character would arrive as a shortcut. Call keyboard_release with the same combo first.",
                    held.join("+")
                ),
                Some(serde_json::json!({
                    "reason":"modifier_held", "held_modifiers":held, "submitted_characters":0,
                    "submission_complete":false, "retry_safe":true,
                })),
            ));
        }
        // Resolve every character before sending anything, so an unsupported
        // character never leaves half the text typed.
        let keys = params
            .text
            .chars()
            .map(char_key)
            .collect::<Result<Vec<_>, _>>()?;
        let preflight_error = |reason: &str, detail: String| {
            McpError::invalid_params(
                format!(
                    "Typing was not started: {detail}. Click the intended field and inspect it before retrying."
                ),
                Some(serde_json::json!({
                    "reason":reason, "submitted_characters":0, "attempted_characters":0,
                    "requested_characters":keys.len(), "submission_complete":false,
                    "retry_safe":true,
                })),
            )
        };
        let active =
            active_window_info(&sess.kwin_conn, &sess.kwin_unique_name, &sess.host_xdg_dir)
                .await
                .map(|(_, _, geometry)| geometry)
                .map_err(|error| preflight_error("target_unverifiable", error.to_string()))?;
        let snapshot =
            window_snapshots(&sess.kwin_conn, &sess.kwin_unique_name, &sess.host_xdg_dir)
                .await
                .map_err(|error| preflight_error("target_unverifiable", error.to_string()))?
                .into_iter()
                .find(|window| window.id == active.id && window.active && !window.minimized)
                .ok_or_else(|| {
                    preflight_error(
                        "target_changed",
                        "the original window is no longer active".to_owned(),
                    )
                })?;
        if snapshot.resource_class.is_empty() {
            return Err(preflight_error(
                "target_unverifiable",
                "the active application's identity is unavailable".to_owned(),
            ));
        }
        let chrome = chromium_resource_class(&snapshot.resource_class);
        let receipt = input_bridge::TextReceipt::capture(
            &sess.atspi_bus_address,
            &active.title,
            [active.x, active.y, active.w, active.h],
            Some([active.cx, active.cy]),
            &params.text,
        )
        .await;
        // A transient window of the browser (its GTK file chooser) is not web
        // content and exposes nothing to AT-SPI; it types unverified like any app.
        if !params.text.is_empty()
            && !params.text.contains(['\t', '\n'])
            && chrome
            && !snapshot.transient
            && let Err(error) = &receipt
            && !error.is::<input_bridge::TerminalFocus>()
            && !error.is::<input_bridge::ProtectedFocus>()
        {
            // Slowness is not an unobservable field: say so, so the caller retries
            // instead of hunting for a different field.
            let (message, reason) = if error.is::<input_bridge::TextDiscoveryTimedOut>() {
                (
                    "Typing was not started because reading the browser's focused field timed out; the host or session is slow. Nothing was typed, so retrying is safe; if it keeps timing out, use keyboard_paste.",
                    "text_discovery_timed_out",
                )
            } else {
                (
                    "Typing was not started because the browser's focused editable target could not be verified. Click the intended field and inspect it before retrying.",
                    "text_target_unavailable",
                )
            };
            return Err(McpError::invalid_params(
                message,
                Some(serde_json::json!({
                    "reason":reason, "submitted_characters":0,
                    "attempted_characters":0, "requested_characters":keys.len(),
                    "submission_complete":false, "retry_safe":true,
                    "target_observation":error.to_string(),
                })),
            ));
        }
        if let Ok(target) = &receipt {
            target.check_focus().await.map_err(|error| {
                McpError::invalid_params(
                    error.to_string(),
                    Some(serde_json::json!({
                        "reason":error.reason(), "submitted_characters":0,
                        "attempted_characters":0, "requested_characters":keys.len(),
                        "submission_complete":false, "retry_safe":true,
                    })),
                )
            })?;
        }
        let window = TypingWindow {
            connection: &sess.kwin_conn,
            kwin_unique: &sess.kwin_unique_name,
            runtime: &sess.host_xdg_dir,
            window_id: &active.id,
            // Chromium can retain Focused/Showing on a background tab's field.
            // A caption change is an independent page-context signal. Tab and
            // Return requests may intentionally navigate, so retain their path.
            browser_title: (chrome && !params.text.contains(['\t', '\n']))
                .then_some(active.title.as_str()),
        };
        check_typing_window(&window)
            .await
            .map_err(|error| preflight_error(error.reason(), error.to_string()))?;
        let started = std::time::Instant::now();
        if let Err(error) = type_with_profile(
            &sess.eis,
            &params.text,
            &keys,
            pace,
            receipt.as_ref().ok(),
            &window,
        )
        .await
        {
            drop(guard);
            self.mark_input().await;
            return Err(error);
        }
        let typing_elapsed_ms = started.elapsed().as_millis();
        let acknowledgment_started = std::time::Instant::now();
        sess.eis.roundtrip().await.map_err(|error| {
            McpError::internal_error(
                format!("All {} characters were submitted, but compositor acknowledgement failed: {error}. Text may still arrive; inspect the field before sending or retrying, and do not retype blindly.", keys.len()),
                Some(serde_json::json!({
                    "submitted_characters": keys.len(), "submission_complete": true,
                    "compositor_acknowledged": false,
                    "application_text": "unverified", "retry_safe": false,
                })),
            )
        })?;
        let acknowledgment_elapsed_ms = acknowledgment_started.elapsed().as_millis();
        let verification_started = std::time::Instant::now();
        let delivery = tokio::time::timeout(TYPING_VERIFICATION_TIMEOUT, async {
            match receipt {
                Ok(receipt) => receipt.confirm().await,
                // A terminal or other inaccessible target can receive these key
                // events even when AT-SPI reports focus on a non-editable control.
                // That focus report cannot prove that submitted text missed the app.
                Err(error)
                    if error.is::<input_bridge::FocusNotEditable>()
                        || error.is::<input_bridge::TerminalFocus>() =>
                {
                    let after = input_bridge::TextReceipt::capture(
                        &sess.atspi_bus_address,
                        &active.title,
                        [active.x, active.y, active.w, active.h],
                        None,
                        "",
                    )
                    .await;
                    match after {
                        Err(after) => input_bridge::TextDelivery::unreadable_field_delivery(
                            keys.len(),
                            &error,
                            &after,
                        ),
                        Ok(_) => input_bridge::TextDelivery::unavailable(
                            "keyboard focus moved into an editable field while the keys were sent"
                                .to_owned(),
                        ),
                    }
                }
                Err(error) => input_bridge::TextDelivery::unavailable(error.to_string()),
            }
        })
        .await
        .unwrap_or_else(|_| {
            input_bridge::TextDelivery::verification_timed_out(TYPING_VERIFICATION_TIMEOUT)
        });
        let verification_elapsed_ms = verification_started.elapsed().as_millis();
        let verification_context = delivery.verification_context();
        let retry_safe = keys.is_empty() && delivery.status == "not_in_field";
        // Names where the keys went, so a caller can tell a terminal's other
        // window or a hidden window from lost input.
        let desktop_shown = showing_desktop(&sess.kwin_conn, &sess.kwin_unique_name)
            .await
            .unwrap_or(false);
        drop(guard);
        self.mark_input().await;
        let mut result = structured_result(
            &peer,
            format!(
                "typing {} into {:?}: {}{}{}",
                delivery.status,
                snapshot.title,
                params.text,
                delivery
                    .warning
                    .as_ref()
                    .map(|warning| format!(". {warning}"))
                    .unwrap_or_default(),
                if desktop_shown {
                    format!(". {SHOWING_DESKTOP_WARNING}")
                } else {
                    String::new()
                }
            ),
            serde_json::json!({
                "action": "type", "text": params.text,
                "submitted_characters": keys.len(), "submission_complete": true,
                "retry_safe": retry_safe, "delivery": delivery,
                "verification_context": verification_context,
                "warning": desktop_shown.then_some(SHOWING_DESKTOP_WARNING),
                "window": {
                    "id": snapshot.id, "title": snapshot.title,
                    "resourceClass": snapshot.resource_class, "pid": snapshot.pid,
                    "hidden": snapshot.hidden || desktop_shown,
                },
                "timings_ms": {
                    "typing": typing_elapsed_ms,
                    "compositor_acknowledgment": acknowledgment_elapsed_ms,
                    "field_verification": verification_elapsed_ms,
                },
                "elapsed_ms": started.elapsed().as_millis(),
            }),
        )
        .await;
        result.is_error = Some(matches!(
            delivery.status,
            "pending_or_mismatched" | "not_in_field"
        ));
        Ok(result)
    }

    #[rmcp::tool(
        name = "keyboard_paste",
        description = "Paste UTF-8 text into the focused application using the session clipboard and one Shift+Insert shortcut. Use this for fast long-text or terminal input; keyboard_type remains paced ASCII key input. Click or Tab into the target first. Leaves this text in the session clipboard and PRIMARY selection; the host clipboard is separate. Pasted newlines are clipboard content, not separate Return key presses, but the application may interpret them. Use keyboard_key Return when you intend to submit a terminal command. Reports clipboard length, shortcut submission, and compositor acknowledgement separately. Application text is always unverified by this tool, and form submission is not observed. Inspect the application or its output before retrying, because another paste can duplicate text or commands."
    )]
    async fn keyboard_paste(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardPasteParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.text.is_empty() {
            return Err(McpError::invalid_params("text must not be empty", None));
        }
        let pace = InputPace::new(None, None)?;
        let (mods, main) = parse_combo(KEYBOARD_PASTE_SHORTCUT)?;
        self.touch_activity().await;
        let started = std::time::Instant::now();
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let socket = sess.host_xdg_dir.join("wayland-0");
        let store = self.clipboards.clone();
        let text = params.text;
        let length = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
                .write(socket, "session", text, false, true)
        })
        .await
        .map_err(|error| KwinError::Msg(format!("clipboard task failed: {error}")))?
        .map_err(KwinError::from)?;
        let mut shortcut_submitted = false;
        let mut submission_complete = false;
        let sent: anyhow::Result<()> = async {
            let mut sent = SentInput::new(&sess.eis);
            for modifier in &mods {
                sent.key(*modifier, true, None)?;
            }
            tokio::time::sleep(pace.hold()?).await;
            sent.key(main, true, None)?;
            shortcut_submitted = true;
            tokio::time::sleep(pace.key_hold()?).await;
            sent.key(main, false, None)?;
            tokio::time::sleep(pace.hold()?).await;
            for modifier in mods.iter().rev() {
                sent.key(*modifier, false, None)?;
            }
            drop(sent);
            submission_complete = true;
            sess.eis.roundtrip().await?;
            Ok(())
        }
        .await;
        if let Err(error) = sent {
            return Err(McpError::internal_error(
                format!(
                    "Session clipboard contains {length} characters, but the paste shortcut did not complete with compositor acknowledgement: {error}. Some input may already have arrived; inspect the application before retrying."
                ),
                Some(serde_json::json!({
                    "action": "paste", "clipboard_characters": length,
                    "shortcut": KEYBOARD_PASTE_SHORTCUT,
                    "shortcut_submitted": shortcut_submitted,
                    "submission_complete": submission_complete,
                    "compositor_acknowledged": false,
                    "application_text": "unverified", "retry_safe": false,
                })),
            ));
        }
        let delivery = input_bridge::TextDelivery::unavailable(
            "the paste shortcut does not independently read back the application's value; a compositor acknowledgement does not prove that the application consumed the clipboard or submitted a form"
                .to_owned(),
        );
        drop(guard);
        self.mark_input().await;
        let result = structured_result(
            &peer,
            format!(
                "paste {}: offered {length} clipboard characters and KWin acknowledged {KEYBOARD_PASTE_SHORTCUT}{}",
                delivery.status,
                delivery.warning.as_ref()
                    .map(|warning| format!(". {warning}"))
                    .unwrap_or_default()
            ),
            serde_json::json!({
                "action": "paste", "clipboard_characters": length,
                "shortcut": KEYBOARD_PASTE_SHORTCUT,
                "shortcut_submitted": true, "submission_complete": true,
                "compositor_acknowledged": true, "retry_safe": false,
                "application_text": "unverified", "form_submission": "not_observed",
                "delivery": delivery, "elapsed_ms": started.elapsed().as_millis(),
            }),
        )
        .await;
        Ok(result)
    }

    #[rmcp::tool(
        name = "keyboard_key",
        description = "Press a key or combo in the focused window. Combos use modifier+key syntax (ctrl+shift+t, alt+F4, ctrl+minus); modifiers are ctrl, shift, alt, super. Keys are a single character or a name: Return, Escape, Tab, Space, Backspace, Delete, Home, End, PageUp, PageDown, arrows (Up/Down/Left/Right), F1-F12, NumLock, and punctuation names minus, plus, equal, comma, period, slash, backslash, semicolon, apostrophe, grave, bracketleft, bracketright. An invalid combo sends nothing. The response distinguishes KWin acknowledgement from the unverified application effect. Browser DevTools shortcuts carry an explicit inspection warning, even when browser chrome (not the page) has focus. Ctrl+V and Shift+Insert also report compositor acknowledgment and focused editable-value verification when session clipboard text is observable. Protected clipboard values are not read back. A verified value does not prove later form submission; inspect unverified or mismatched results before retrying. Use keyboard_type for text."
    )]
    async fn keyboard_key(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, None)?;
        self.touch_activity().await;
        let (mods, main) = parse_combo(&params.key)?;
        let is_paste = is_native_paste_combo(&mods, main);
        let (conn, kwin_unique, xdg) = self
            .with_session(|s| {
                Ok((
                    s.kwin_conn.clone(),
                    s.kwin_unique_name.clone(),
                    s.host_xdg_dir.clone(),
                ))
            })
            .await?;
        let active = active_window_info(&conn, &kwin_unique, &xdg)
            .await
            .map(|(_, _, geometry)| geometry);
        let active_title = active
            .as_ref()
            .map(|geometry| geometry.title.clone())
            .unwrap_or_default();
        let page_unfocused = browser_page_unfocused(&conn, &active_title).await;
        let warning = if is_chromium_devtools_shortcut(&active_title, &mods, &params.key) {
            Some(
                "KWin can acknowledge the shortcut without Chrome opening or closing DevTools. \
                 Browser-level shortcuts can work even when the page lacks focus. \
                 Inspect a screenshot to confirm the actual browser state before retrying.",
            )
        } else if page_unfocused {
            Some(
                "AT-SPI did not find page keyboard focus (it may be stale across matching browser windows). \
                 Browser chrome may have focus, but this does not prove the shortcut failed. \
                 Inspect the UI before retrying.",
            )
        } else {
            None
        };
        let paste_preparation = if is_paste {
            match &active {
                Ok(geometry) => {
                    let (address, socket) = self
                        .with_session(|session| {
                            Ok((
                                session.atspi_bus_address.clone(),
                                session.host_xdg_dir.join("wayland-0"),
                            ))
                        })
                        .await?;
                    let store = self.clipboards.clone();
                    let snapshot = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        store
                            .lock()
                            .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
                            .entry(&socket, "session")?
                            .clipboard
                            .read_text()
                    })
                    .await;
                    Some(match snapshot {
                        Ok(Ok(snapshot)) => {
                            prepare_paste_text(
                                snapshot,
                                &address,
                                &geometry.title,
                                [geometry.x, geometry.y, geometry.w, geometry.h],
                            )
                            .await
                        }
                        _ => PreparedPaste::Unavailable {
                            method: "paste_text_unavailable",
                            warning: "The session clipboard text could not be observed. The native paste shortcut was still submitted.",
                        },
                    })
                }
                Err(_) => Some(PreparedPaste::Unavailable {
                    method: "paste_target_unobservable",
                    warning: "The active window could not be observed for paste verification. The native shortcut was still submitted.",
                }),
            }
        } else {
            None
        };
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let mut sent = SentInput::new(&sess.eis);
        for m in &mods {
            sent.key(*m, true, None).map_err(KwinError::from)?;
        }
        if !mods.is_empty() {
            tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
        }
        let k = main;
        sent.key(k, true, None).map_err(KwinError::from)?;
        tokio::time::sleep(pace.key_hold().map_err(KwinError::from)?).await;
        sent.key(k, false, None).map_err(KwinError::from)?;
        if !mods.is_empty() {
            tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
        }
        for m in mods.iter().rev() {
            sent.key(*m, false, None).map_err(KwinError::from)?;
        }
        drop(sent);
        let acknowledgment = sess.eis.roundtrip().await;
        drop(guard);
        self.mark_input().await;
        if let Err(error) = acknowledgment {
            if !is_paste {
                return Err(McpError::internal_error(
                    format!(
                        "Key events were submitted, but KWin acknowledgement failed: {error}. \
                         The shortcut may still have taken effect; inspect the UI before retrying."
                    ),
                    Some(serde_json::json!({
                        "compositor_acknowledged": false,
                        "application_effect": "unverified",
                        "retry_safe": false,
                    })),
                ));
            }
            return Err(McpError::internal_error(
                format!(
                    "The paste shortcut was submitted, but compositor acknowledgment failed: {error}"
                ),
                Some(serde_json::json!({
                    "action":"key", "key":params.key, "shortcut_submitted":true,
                    "submission_complete":true, "compositor_acknowledged":false,
                    "application_text":"unverified", "form_submission":"not_observed", "retry_safe":false,
                })),
            ));
        }
        let paste_delivery = match paste_preparation {
            Some(preparation) => Some(finish_prepared_paste(preparation).await),
            None => None,
        };
        let paste_is_error = paste_delivery.as_ref().is_some_and(|delivery| {
            matches!(delivery.status, "pending_or_mismatched" | "not_in_field")
        });
        let text = match warning {
            Some(warning) => format!("key: {}. warning: {warning}", params.key),
            None => format!("key: {}", params.key),
        };
        let text = if let Some(delivery) = &paste_delivery {
            format!(
                "{text}; paste value {}. {} Form submission was not observed.",
                delivery.status,
                delivery.warning.as_deref().unwrap_or("")
            )
        } else {
            text
        };
        let mut data = serde_json::json!({
            "action": "key", "key": params.key, "warning": warning,
            "compositor_acknowledged": true,
            "application_effect": "unverified",
        });
        if let Some(delivery) = paste_delivery {
            data["paste"] = serde_json::json!({
                "clipboard_scope":"session", "shortcut_acknowledged":true,
                "editable_value":delivery, "form_submission":"not_observed", "retry_safe":false,
            });
        }
        let mut result = structured_result(&peer, text, data).await;
        if is_paste {
            result.is_error = Some(paste_is_error);
        }
        Ok(result)
    }

    #[rmcp::tool(
        name = "keyboard_press",
        description = "Press a key or combo WITHOUT releasing — useful when you need to hold the chord across a screenshot to verify a transient UI (e.g. a menu that opens on combo). The keys stay down until keyboard_release with the same combo; keyboard_type refuses while a modifier is held. To send a shortcut (Ctrl+L, Ctrl+A, Alt+F4), use keyboard_key."
    )]
    async fn keyboard_press(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, None)?;
        self.touch_activity().await;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        for m in &mods {
            sess.eis.key(*m, true).map_err(KwinError::from)?;
        }
        if !mods.is_empty() {
            tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
        }
        let k = main;
        sess.eis.key(k, true).map_err(KwinError::from)?;
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!(
                "press: {}. Still held until keyboard_release {}.",
                params.key, params.key
            ),
            serde_json::json!({
                "action": "press", "key": params.key, "held": true,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "keyboard_release",
        description = "Release a key or combo previously pressed via keyboard_press. Pass the same string."
    )]
    async fn keyboard_release(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<KeyboardKeyParams>,
    ) -> Result<CallToolResult, McpError> {
        let pace = InputPace::new(params.speed, None)?;
        self.touch_activity().await;
        let guard = self.session.read().await;
        let sess = guard.as_ref().ok_or_else(|| {
            McpError::internal_error("no session — call session_start first", None)
        })?;
        let (mods, main) = parse_combo(&params.key)?;
        let k = main;
        sess.eis.key(k, false).map_err(KwinError::from)?;
        if !mods.is_empty() {
            tokio::time::sleep(pace.hold().map_err(KwinError::from)?).await;
        }
        for m in mods.iter().rev() {
            sess.eis.key(*m, false).map_err(KwinError::from)?;
        }
        drop(guard);
        self.mark_input().await;
        Ok(structured_result(
            &peer,
            format!("release: {}", params.key),
            serde_json::json!({
                "action": "release", "key": params.key,
            }),
        )
        .await)
    }

    #[rmcp::tool(
        name = "import_file",
        description = "Refresh a file inside an already-running session from a regular host file, without restarting the desktop. HOME is an overlay: session copies, deletions, and application caches can hide later host additions or edits; host updates are not a reliable live sync. host_path is an absolute host file path; session_path defaults to that same path and must name a file in an existing writable session directory. Existing session files (including private edits) are preserved unless overwrite=true. The copy is verified byte for byte before atomic replacement through the session filesystem; it never writes the overlay upper layer directly. Returns imported paths and bytes. Reload/reopen the note in its application afterward; application caches and unsaved buffers are not refreshed by this tool. Read-only mounts cannot be imported into. Outside explicitly shared host_write_paths, HOME imports remain private; importing into a shared directory writes to live host state."
    )]
    async fn import_file(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ImportFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let sandbox_pid = self
            .with_session(|sess| Ok(sess.sandbox_child.id()))
            .await?;
        let source = PathBuf::from(&params.host_path);
        let destination =
            PathBuf::from(params.session_path.as_deref().unwrap_or(&params.host_path));
        if !source.is_absolute() || !destination.is_absolute() || destination.file_name().is_none()
        {
            return Err(McpError::invalid_params(
                "host_path and session_path must be absolute file paths",
                None,
            ));
        }
        let overwrite = params.overwrite;
        let outcome = tokio::task::spawn_blocking(move || {
            let root = sandbox_root(sandbox_pid).ok_or_else(|| {
                "cannot reach the session filesystem (sandbox not running)".to_owned()
            })?;
            file_import::import_host_file(&root, &source, &destination, overwrite)
                .map(|bytes| (destination, bytes))
        })
        .await
        .map_err(|error| McpError::internal_error(format!("import task: {error}"), None))?;
        match outcome {
            Ok((destination, bytes)) => Ok(structured_result(
                &peer,
                format!("imported {} -> {} ({bytes} bytes, verified); reload/reopen the file in its application", params.host_path, destination.display()),
                serde_json::json!({
                    "status": "imported",
                    "host_path": params.host_path,
                    "session_path": destination,
                    "bytes": bytes,
                    "verified": true,
                    "application_reload_required": true,
                }),
            ).await),
            Err(reason) => Err(McpError::internal_error(
                format!("import_file did not replace the session file: {reason}"),
                Some(serde_json::json!({"status": "not_imported", "reason": reason})),
            )),
        }
    }

    #[rmcp::tool(
        name = "wait_for_file",
        description = "Wait for an absolute session file path to exist as a regular file and remain unchanged for a bounded interval. Readiness compares its inode, byte size, and modification time; this does not prove the producer has exited or that the file is nonempty. The observation is inside the isolated session, not the host view of the same path. Before uploading a host-created file, compare size_bytes with the expected host size; a chooser accepting the path does not verify its contents. HOME overlays and private /tmp can expose different bytes. If the session copy is missing or differs, transfer the intended file into the session, or start a separate session with the existing host file in writable_paths for a private copy. A timeout reports missing or changing files. Call export_file after a session output is ready."
    )]
    async fn wait_for_file(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<WaitForFileParams>,
    ) -> Result<CallToolResult, McpError> {
        let timeout_seconds = params.timeout_seconds.unwrap_or(30);
        if !(1..=300).contains(&timeout_seconds) {
            return Err(McpError::invalid_params(
                "timeout_seconds must be between 1 and 300",
                None,
            ));
        }
        let stable_for_ms = params.stable_for_ms.unwrap_or(500);
        if !(100..=5000).contains(&stable_for_ms) {
            return Err(McpError::invalid_params(
                "stable_for_ms must be between 100 and 5000",
                None,
            ));
        }
        let session_path = PathBuf::from(&params.session_path);
        if !session_path.is_absolute() {
            return Err(McpError::invalid_params(
                format!("session_path must be absolute: {}", params.session_path),
                None,
            ));
        }
        if session_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(McpError::invalid_params(
                "session_path must not contain a parent-directory component",
                None,
            ));
        }

        self.touch_activity().await;
        let sandbox_pid = self
            .with_session(|sess| Ok(sess.sandbox_child.id()))
            .await?;
        let timeout = Duration::from_secs(timeout_seconds);
        let stable_for = Duration::from_millis(stable_for_ms);
        let root = tokio::task::spawn_blocking(move || sandbox_root(sandbox_pid))
            .await
            .map_err(|error| {
                McpError::internal_error(format!("wait_for_file session lookup: {error}"), None)
            })?
            .ok_or_else(|| {
                McpError::internal_error(
                    "cannot reach the session filesystem (sandbox not running)",
                    None,
                )
            })?;
        let relative = session_path.strip_prefix("/").map_err(|error| {
            McpError::invalid_params(format!("invalid session path: {error}"), None)
        })?;
        let source = root.join(relative);
        let observation = wait_for_file_stability(&source, timeout, stable_for).await;

        let observation = match observation {
            Ok(observation) => observation,
            Err(FileReadinessError::NotRegularFile) => {
                return Err(McpError::invalid_params(
                    format!(
                        "{} is not a regular file in the session",
                        params.session_path
                    ),
                    None,
                ));
            }
            Err(FileReadinessError::Inspect(reason)) => {
                return Err(McpError::internal_error(reason, None));
            }
        };

        if observation.ready {
            let size_bytes = observation.size_bytes.ok_or_else(|| {
                McpError::internal_error("ready session file has no observed size", None)
            })?;
            Ok(structured_result(
                &peer,
                format!(
                    "ready: {} ({size_bytes} bytes stable for {stable_for_ms} ms)",
                    params.session_path
                ),
                serde_json::json!({
                    "status": "ready",
                    "session_path": params.session_path,
                    "size_bytes": size_bytes,
                    "stable_for_ms": stable_for_ms,
                    "timeout_seconds": timeout_seconds,
                }),
            )
            .await)
        } else {
            let reason = if observation.exists {
                "changing"
            } else {
                "missing"
            };
            Err(McpError::internal_error(
                format!(
                    "wait_for_file timed out after {timeout_seconds}s: {} is {reason} in the session",
                    params.session_path
                ),
                Some(serde_json::json!({
                    "status": "not_ready",
                    "reason": reason,
                    "session_path": params.session_path,
                    "exists": observation.exists,
                    "size_bytes": observation.size_bytes,
                    "stable_for_ms": stable_for_ms,
                    "timeout_seconds": timeout_seconds,
                })),
            ))
        }
    }

    #[rmcp::tool(
        name = "export_file",
        description = "Copy a file out of the isolated session onto the real host filesystem and verify it. Except in explicitly shared host_write_paths, session writes (downloads, exports, saved files) stay in the session overlay and never reach the host on their own; call this when the user wants the file in a real directory. session_path is the absolute path the session sees (e.g. a finished Chrome download in ~/Downloads). For an asynchronously produced file, call wait_for_file first to observe it becoming stable. host_path is the host file or existing directory to write; omit it to use the same path on the host. An existing host file is not replaced unless overwrite=true; create_parents=true creates missing host directories. Returns the host path and byte count after a byte-for-byte comparison, or an error saying why nothing was written (including a download still in progress)."
    )]
    async fn export_file(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<ExportFileParams>,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        let sandbox_pid = self
            .with_session(|sess| Ok(sess.sandbox_child.id()))
            .await?;
        let session_path = PathBuf::from(&params.session_path);
        if !session_path.is_absolute() {
            return Err(McpError::invalid_params(
                format!("session_path must be absolute: {}", params.session_path),
                None,
            ));
        }
        let mut destination =
            PathBuf::from(params.host_path.as_deref().unwrap_or(&params.session_path));
        if !destination.is_absolute() {
            return Err(McpError::invalid_params(
                format!("host_path must be absolute: {}", destination.display()),
                None,
            ));
        }
        if destination.is_dir() {
            let name = session_path
                .file_name()
                .ok_or_else(|| McpError::invalid_params("session_path has no file name", None))?;
            destination = destination.join(name);
        }
        if params.create_parents
            && let Some(parent) = destination.parent()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                McpError::invalid_params(
                    format!(
                        "export_file wrote nothing: could not create host directory {}: {error}",
                        parent.display()
                    ),
                    Some(serde_json::json!({"status": "not_exported", "reason": "host_directory_not_created"})),
                )
            })?;
        }
        let overwrite = params.overwrite;
        let outcome = tokio::task::spawn_blocking(move || {
            let root = sandbox_root(sandbox_pid).ok_or_else(|| "cannot reach the session filesystem (sandbox not running)".to_owned())?;
            let relative = session_path.strip_prefix("/").map_err(|error| error.to_string())?;
            let source = root.join(relative);
            if !source.exists() {
                for partial in ["crdownload", "part", "download"] {
                    let name = format!("{}.{partial}", session_path.file_name().and_then(|name| name.to_str()).unwrap_or_default());
                    if source.with_file_name(&name).exists() {
                        return Err(format!("{} is still downloading ({name} exists); wait for the download to finish", session_path.display()));
                    }
                }
                return Err(format!("{} does not exist in the session", session_path.display()));
            }
            let metadata = std::fs::metadata(&source)
                .map_err(|error| format!("cannot inspect session path {}: {error}", session_path.display()))?;
            if metadata.is_dir() {
                return Err(format!("{} is a directory; export_file accepts one regular file. Create an archive of this directory inside the session, then export the archive, or list the directory and export each file individually", session_path.display()));
            }
            if !metadata.is_file() {
                return Err(format!("{} is not a regular file in the session", session_path.display()));
            }
            export_session_file(&source, &destination, overwrite).map(|bytes| (destination, bytes))
        })
        .await
        .map_err(|error| McpError::internal_error(format!("export task: {error}"), None))?;
        match outcome {
            Ok((destination, bytes)) => {
                let text = format!(
                    "exported {} -> {} ({bytes} bytes, verified)",
                    params.session_path,
                    destination.display()
                );
                Ok(structured_result(
                    &peer,
                    text,
                    serde_json::json!({
                        "status": "exported",
                        "session_path": params.session_path,
                        "host_path": destination.display().to_string(),
                        "bytes": bytes,
                        "verified": true,
                    }),
                )
                .await)
            }
            Err(reason) => Err(McpError::internal_error(
                format!("export_file wrote nothing: {reason}"),
                Some(
                    serde_json::json!({"status": "not_exported", "session_path": params.session_path, "reason": reason}),
                ),
            )),
        }
    }

    #[rmcp::tool(
        name = "launch_app",
        description = "Run a shell command in the isolated desktop. Waits 15s by default (window_timeout_seconds 1-120) for a new or attributable existing browser window. A command still running with no window by then is an error (window_not_observed). expect_window=false (background or non-GUI work) returns the PID, plus exit_status if the command ends within one second. command_submitted and retry_safe describe the submission, so inspect state before repeating it. Stdout is discarded, except with expect_window=false: then it goes to stdout_path in host_shared_dir, quoted as stdout_tail if the command already exited. Other /tmp paths are private. Absolute host path arguments must be visible inside the session. Exit failures quote the last 20 stderr lines and a matching sandbox.log line. command_resolution reports the leading command word's PATH executable, builtin, or absence. KWin reads are bounded; timeouts keep the session and report the stalled stage. Browser names resolved through PATH get missing Wayland, KWallet, accessibility and no-first-run switches; Chromium also gets a server-assigned forwarded CDP port, replacing --remote-debugging-port. Chrome and Edge need a non-default --user-data-dir for CDP. Successful CDP returns cdp_port. Branded Chrome ignores --load-extension; use chrome://extensions > Load unpacked. A window mapped after the launch baseline counts, even from a helper process; on timeout a late window is reported or a fresh window_snapshot attached. HOME writes stay in an overlay unless shared (export_file copies out); home_overlay names such arguments and stale copies (import_file refreshes). Use saved-credential autofill without reading or typing credentials; otherwise open viewer_open for the user."
    )]
    async fn launch_app(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        use tower::ServiceExt;
        let window_timeout_seconds = params
            .window_timeout_seconds
            .unwrap_or(LAUNCH_WINDOW_TIMEOUT.as_secs());
        if !(1..=LAUNCH_WINDOW_TIMEOUT_MAX_SECS).contains(&window_timeout_seconds) {
            return Err(McpError::invalid_params(
                format!(
                    "window_timeout_seconds must be between 1 and {LAUNCH_WINDOW_TIMEOUT_MAX_SECS}"
                ),
                None,
            ));
        }
        let window_timeout = Duration::from_secs(window_timeout_seconds);
        let call_timeout = window_timeout + LAUNCH_CALL_OVERHEAD_TIMEOUT;
        let progress = Arc::new(std::sync::Mutex::new(LaunchProgress {
            stage: "waiting for the session",
            command_submitted: false,
            window_baseline_errors: Vec::new(),
        }));
        let observed = progress.clone();
        let home_report = self.launch_home_overlay_report(&params.command).await;
        let service = tower::service_fn(|(peer, params)| {
            self.launch_app_inner(peer, params, progress.clone(), window_timeout)
        });
        let result = match tower::timeout::Timeout::new(service, call_timeout)
            .oneshot((peer, params))
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                if error.is::<tower::timeout::error::Elapsed>() {
                    let progress = observed.lock().map_err(|_| {
                        McpError::internal_error("launch progress lock poisoned", None)
                    })?;
                    Err(launch_failure(
                        launch_timeout(&progress, call_timeout),
                        &progress,
                    ))
                } else {
                    let error = match error.downcast::<McpError>() {
                        Ok(error) => *error,
                        Err(error) => McpError::internal_error(error.to_string(), None),
                    };
                    let progress = observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    Err(launch_failure(error, &progress))
                }
            }
        };
        attach_home_overlay_report(result, &home_report)
    }

    /// The HOME overlay report for a launch, taken before the command runs so
    /// it describes the files the command starts with.
    async fn launch_home_overlay_report(&self, command: &str) -> HomeOverlayReport {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return HomeOverlayReport::default();
        };
        let Ok((sandbox_pid, host_write_paths)) = self
            .with_session(|sess| Ok((sess.sandbox_child.id(), sess.host_write_paths.clone())))
            .await
        else {
            return HomeOverlayReport::default();
        };
        // The session starts in the server's working directory.
        let cwd = std::env::current_dir().ok();
        let command = command.to_owned();
        tokio::task::spawn_blocking(move || {
            let checks = launch_file_words(&command, cwd.as_deref())
                .iter()
                .any(|path| path.starts_with(&home));
            let root = checks.then(|| sandbox_root(sandbox_pid)).flatten();
            home_overlay_report(
                &command,
                &home,
                cwd.as_deref(),
                &host_write_paths,
                root.as_deref(),
            )
        })
        .await
        .unwrap_or_default()
    }

    async fn launch_app_inner(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        params: LaunchAppParams,
        progress: Arc<std::sync::Mutex<LaunchProgress>>,
        window_timeout: Duration,
    ) -> Result<CallToolResult, McpError> {
        self.touch_activity().await;
        use futures::StreamExt;
        use std::os::fd::AsFd;
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncWriteExt;
        use tower::ServiceExt;

        let (
            conn,
            kwin_unique,
            xdg,
            service_bus_address,
            atspi_bus_address,
            cdp_port,
            fuse_enabled,
            wallet_note,
        ) = {
            let guard = self.session.read().await;
            let sess = guard.as_ref().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            (
                sess.kwin_conn.clone(),
                sess.kwin_unique_name.clone(),
                sess.host_xdg_dir.clone(),
                sess.service_bus_address.clone(),
                sess.atspi_bus_address.clone(),
                sess.cdp_forward_port,
                sess.fuse_enabled,
                sess.wallet_note.clone(),
            )
        };
        if let Ok(mut progress) = progress.lock() {
            progress.stage = "reading windows before launch";
        }
        let mut baseline = WindowBaseline::PreLaunch;
        let mut baseline_errors = Vec::new();
        let (mut prev_window_id, mut previous_windows) = if params.expect_window == Some(false) {
            (None, None)
        } else {
            match window_baseline(&conn, &kwin_unique, &xdg).await {
                Ok((active, windows)) => (active, Some(windows)),
                Err(reason) => {
                    baseline = WindowBaseline::AfterSubmit;
                    baseline_errors
                        .push(serde_json::json!({"stage": "pre_launch", "reason": reason}));
                    (None, None)
                }
            }
        };
        if let Ok(mut progress) = progress.lock() {
            progress.window_baseline_errors.clone_from(&baseline_errors);
        }

        let launch_id = NEXT_BROWSER_LAUNCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let browser_marker = xdg.join(format!("browser-launch-{launch_id}"));
        let path_error_file = xdg.join(format!("launch-{launch_id}.path-error"));
        let command_paths = launch_path_words(&params.command);
        let pid_file = xdg.join(format!("launch-{launch_id}.pid"));
        let mut checked_command = String::new();
        let resolution_file = xdg.join(format!("launch-{launch_id}.resolution"));
        let resolution_guard = launch_program_name(&params.command).and_then(|program| {
            // Diagnostics must not prevent the requested command from running
            // when its small sidecar cannot be created, e.g. at a /tmp quota.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&resolution_file)
                .ok()?;
            drop(file);
            let destination = shell_quote(&resolution_file.display().to_string());
            checked_command.push_str(&format!(
                "_kwin_mcp_launch_name={}; _kwin_mcp_launch_kind=$(type -t -- \"$_kwin_mcp_launch_name\"); case \"$_kwin_mcp_launch_kind\" in file) printf '%s -> %s' \"$_kwin_mcp_launch_name\" \"$(type -P -- \"$_kwin_mcp_launch_name\")\" > {destination} ;; builtin|keyword|function|alias) printf '%s -> shell %s' \"$_kwin_mcp_launch_name\" \"$_kwin_mcp_launch_kind\" > {destination} ;; *) printf '%s -> not found in session PATH' \"$_kwin_mcp_launch_name\" > {destination} ;; esac\n",
                shell_quote(&program),
            ));
            Some(LaunchResolutionFile(resolution_file.clone()))
        });
        for (path, probe) in launch_path_probes(&params.command, |path| {
            std::fs::symlink_metadata(path).is_ok()
        }) {
            checked_command.push_str(&format!(
                "if [ ! -e {} ]; then printf '%s' {} > {}; exit 125; fi\n",
                shell_quote(&probe.display().to_string()),
                shell_quote(&path.display().to_string()),
                shell_quote(&path_error_file.display().to_string()),
            ));
        }
        // Record the command shell only after all path preflight checks pass.
        checked_command.push_str(&format!(
            "echo $$ > {}\n",
            shell_quote(&pid_file.display().to_string())
        ));
        checked_command.push_str(&params.command);
        // The command's exit status, written when it exits: a launch that ends
        // before a window appears is reported instead of waited out.
        let exit_file = xdg.join(format!("launch-{launch_id}.status"));
        // Without a window to observe, stdout is the command's result. Keep it
        // in host_shared_dir, which host tools read at the same path.
        let stdout_file = (params.expect_window == Some(false))
            .then(|| xdg.join(format!("launch-{launch_id}.stdout")));
        let stdout_redirect = stdout_file
            .as_ref()
            .map(|file| format!(" > {}", shell_quote(&file.display().to_string())))
            .unwrap_or_default();
        let launch_cmd = format!(
            "for x_socket in /tmp/.X11-unix/X*; do if [ -S \"$x_socket\" ]; then export DISPLAY=\":${{x_socket##*X}}\"; break; fi; done; env {}PATH={}:\"$PATH\" DBUS_SESSION_BUS_ADDRESS={} AT_SPI_BUS_ADDRESS={} KWIN_MCP_CDP_PORT={cdp_port} KWIN_MCP_BROWSER_MARKER={} bash -c {}{stdout_redirect}; echo $? > {}",
            if fuse_enabled {
                ""
            } else {
                "APPIMAGE_EXTRACT_AND_RUN=1 "
            },
            shell_quote(&xdg.join("browser-bin").display().to_string()),
            shell_quote(&service_bus_address),
            shell_quote(&atspi_bus_address),
            shell_quote(&browser_marker.display().to_string()),
            shell_quote(&checked_command),
            shell_quote(&exit_file.display().to_string()),
        );
        // sandbox.log holds the session's stderr. This launch's output starts at
        // the log's current end, so an exit can report what the command wrote.
        let sandbox_log = session_disk_path(&xdg).join("sandbox.log");
        let sandbox_log_offset = std::fs::metadata(&sandbox_log).map(|meta| meta.len());
        if let Ok(mut progress) = progress.lock() {
            progress.stage = "writing the launch script";
        }
        let command_path = xdg.join(format!("launch-{launch_id}.sh"));
        let mut script = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&command_path)
            .map_err(|error| launch_script_write_error(&command_path, &error))?;
        let mut command_file = PendingLaunchFile {
            path: command_path,
            submitted: false,
        };
        let quoted_path = shell_quote(&command_file.path.display().to_string());
        let script_body = format!("unlink -- {quoted_path}\n{launch_cmd}");
        script
            .write_all(script_body.as_bytes())
            .map_err(|error| launch_script_write_error(&command_file.path, &error))?;
        drop(script);
        let dispatch = format!("bash {quoted_path}\n");
        // A short pipe write is atomic, so cancellation cannot leave half a
        // shell command that the next launch would complete accidentally.
        if dispatch.len() > nix::libc::PIPE_BUF {
            return Err(McpError::internal_error(
                "launch command path exceeds the atomic pipe-write limit",
                None,
            ));
        }
        {
            if let Ok(mut progress) = progress.lock() {
                progress.stage = "submitting the command to the session shell";
            }
            let guard = self.session.read().await;
            let sess = guard.as_ref().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            let fd = sess
                .sandbox_stdin
                .as_fd()
                .try_clone_to_owned()
                .map_err(KwinError::from)?;
            let mut pipe =
                tokio::net::unix::pipe::Sender::from_owned_fd(fd).map_err(KwinError::from)?;
            pipe.write_all(dispatch.as_bytes())
                .await
                .map_err(KwinError::from)?;
            if let Ok(mut progress) = progress.lock() {
                progress.command_submitted = true;
            }
            command_file.submitted = true;
        }

        if params.expect_window == Some(false) {
            if let Ok(mut progress) = progress.lock() {
                progress.stage = "reading the command's PID";
            }
            let pid = poll_launch_pid(&pid_file, &path_error_file, &exit_file).await;
            let _ = std::fs::remove_file(&pid_file);
            if let Some(error) = invisible_launch_path(&path_error_file) {
                return Err(error);
            }
            let exit_status = if pid.is_some() {
                poll_launch_exit_status(&exit_file, LAUNCH_EXIT_STATUS_GRACE).await
            } else {
                std::fs::read_to_string(&exit_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
            };
            let warning = launch_wallet_warning(wallet_note, &params.command);
            let suffix = warning
                .as_deref()
                .map(|text| format!(". {text}"))
                .unwrap_or_default();
            let pid_text = pid.map_or_else(|| "pid unknown".to_owned(), |pid| format!("pid {pid}"));
            let text = match exit_status {
                Some(status) => format!(
                    "submitted: {} (no window expected; {pid_text}; exited with status {status}){suffix}",
                    params.command
                ),
                None => format!(
                    "submitted: {} (no window expected; {pid_text}){suffix}",
                    params.command
                ),
            };
            // Output so far; the file keeps growing while the command runs.
            let stdout_tail = exit_status
                .and(stdout_file.as_ref())
                .and_then(|file| std::fs::read(file).ok())
                .map(|bytes| {
                    let start = bytes.len().saturating_sub(LAUNCH_STDOUT_TAIL_BYTES);
                    String::from_utf8_lossy(&bytes[start..]).into_owned()
                });
            let mut result = structured_result(
                &peer,
                text,
                serde_json::json!({
                    "action": "launch", "command": params.command,
                    "window": "not_expected", "window_outcome": "not_expected",
                    "command_submitted": true, "retry_safe": false,
                    "pid": pid, "exit_status": exit_status,
                    "stdout_path": stdout_file, "stdout_tail": stdout_tail,
                    "cdp": false, "warning": warning,
                }),
            )
            .await;
            result.is_error = Some(exit_status.is_some_and(|status| status != 0));
            return Ok(result);
        }

        if baseline == WindowBaseline::AfterSubmit {
            match window_baseline(&conn, &kwin_unique, &xdg).await {
                Ok((active, windows)) => {
                    prev_window_id = active;
                    previous_windows = Some(windows);
                }
                Err(reason) => {
                    baseline = WindowBaseline::Unavailable;
                    baseline_errors
                        .push(serde_json::json!({"stage": "after_submit", "reason": reason}));
                }
            }
        }

        if let Ok(mut progress) = progress.lock() {
            progress.stage = "waiting for a window";
            progress.window_baseline_errors.clone_from(&baseline_errors);
        }
        let chromium_launch = launches_chromium(&params.command);
        let waiting = tower::service_fn(|()| {
            poll_launch_window(
                conn.clone(),
                kwin_unique.clone(),
                xdg.clone(),
                prev_window_id.clone(),
                previous_windows.clone(),
                chromium_launch,
                exit_file.clone(),
            )
        });
        let mut window_timed_out = false;
        let (mut win_geo, mut window_reused) =
            match tower::timeout::Timeout::new(waiting, window_timeout)
                .oneshot(())
                .await
            {
                Ok(Some(window)) => (Some(window.geometry), window.reused),
                Ok(None) => (None, false),
                Err(error) if error.is::<tower::timeout::error::Elapsed>() => {
                    window_timed_out = true;
                    (None, false)
                }
                Err(error) => {
                    return Err(match error.downcast::<McpError>() {
                        Ok(error) => *error,
                        Err(error) => McpError::internal_error(error.to_string(), None),
                    });
                }
            };
        let mut window_observed_after_timeout = false;
        let mut timeout_window_snapshot = None;
        let mut timeout_window_snapshot_error = None;
        let mut exit_status = std::fs::read_to_string(&exit_file)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        if win_geo.is_none() && window_timed_out && exit_status.is_none_or(|status| status == 0) {
            tokio::time::sleep(LAUNCH_LATE_WINDOW_GRACE).await;
            match tokio::time::timeout(
                LAUNCH_TIMEOUT_SNAPSHOT_TIMEOUT,
                window_snapshots(&conn, &kwin_unique, &xdg),
            )
            .await
            {
                Ok(Ok(windows)) => {
                    if let Some(window) = previous_windows
                        .as_deref()
                        .and_then(|before| newly_mapped_window(before, &windows))
                    {
                        win_geo = Some(launch_window_geometry(window));
                        window_observed_after_timeout = true;
                    } else {
                        timeout_window_snapshot = Some(launch_window_snapshot(&windows));
                    }
                }
                Ok(Err(error)) => timeout_window_snapshot_error = Some(error.to_string()),
                Err(_) => {
                    timeout_window_snapshot_error = Some(format!(
                        "KWin did not answer the post-timeout window inventory within {} seconds",
                        LAUNCH_TIMEOUT_SNAPSHOT_TIMEOUT.as_secs()
                    ));
                }
            }
            exit_status = std::fs::read_to_string(&exit_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok());
        }
        let _ = std::fs::remove_file(&exit_file);
        let _ = std::fs::remove_file(&pid_file);
        let mut handoff_reason = None;
        let mut handoff_candidates = Vec::new();
        if win_geo.is_none() && exit_status == Some(0) && chromium_launch {
            match previous_windows.as_deref() {
                Some(previous) => match window_snapshots(&conn, &kwin_unique, &xdg).await {
                    Ok(current) => {
                        let changed = changed_existing_browser_windows(previous, &current);
                        let titled = changed_existing_browser_window_titles(previous, &current);
                        handoff_candidates = changed
                            .iter()
                            .map(|window| {
                                serde_json::json!({
                                    "window": window.id,
                                    "title": window.title,
                                })
                            })
                            .collect();
                        match titled.as_slice() {
                            [window] => {
                                win_geo = Some(launch_window_geometry(window));
                                window_reused = true;
                            }
                            _ if changed.is_empty() => {
                                handoff_reason = Some(
                                    "no existing Chromium window changed its title, activation, or minimized state"
                                        .to_owned(),
                                );
                            }
                            [] if changed.len() == 1 => {
                                handoff_reason = Some(
                                    "one existing Chromium window changed activation or minimized state, but its title did not change, so KWin cannot confirm which tab received the URL"
                                        .to_owned(),
                                );
                            }
                            _ => {
                                handoff_reason = Some(format!(
                                    "{} existing Chromium windows changed; the target cannot be distinguished",
                                    changed.len()
                                ));
                            }
                        }
                    }
                    Err(_) => {
                        handoff_reason =
                            Some("the post-launch KWin window list could not be read".to_owned());
                    }
                },
                None => {
                    handoff_reason =
                        Some("the pre-launch KWin window list could not be read".to_owned());
                }
            }
        }
        let command_resolution = resolution_guard
            .as_ref()
            .and_then(|file| std::fs::read_to_string(&file.0).ok())
            .filter(|resolution| !resolution.is_empty());
        if let Some(error) = invisible_launch_path(&path_error_file) {
            return Err(error);
        }

        // The browser wrapper records CDP intent only when it actually ran.
        let cdp_requested = std::fs::read(&browser_marker).is_ok_and(|value| value == b"cdp");
        let _ = std::fs::remove_file(&browser_marker);
        let mut cdp_connected = false;
        if cdp_requested && win_geo.is_some() {
            if let Ok(mut progress) = progress.lock() {
                progress.stage = "connecting to browser accessibility";
            }
            let cdp_url = format!("http://127.0.0.1:{cdp_port}");
            for _ in 0..CDP_CONNECT_POLLS {
                match chromiumoxide::Browser::connect(&cdp_url).await {
                    Ok((browser, mut handler)) => {
                        tokio::spawn(async move { while handler.next().await.is_some() {} });
                        let mut guard = self.session.write().await;
                        if let Some(sess) = guard.as_mut() {
                            sess.cdp_browser = Some(Arc::new(browser));
                        }
                        cdp_connected = true;
                        break;
                    }
                    Err(_) => {
                        tokio::time::sleep(LAUNCH_POLL_INTERVAL).await;
                    }
                }
            }
        }

        // A Chromium-family browser without the wallet cannot decrypt the copied
        // profile's cookies and saved logins, so its sites show signed out.
        let warning = launch_wallet_warning(wallet_note, &params.command);
        let extension_warning = requests_blocked_google_chrome_extension_load(&params.command)
            .then(|| {
                "warning: Google Chrome ignores --load-extension in its branded build; --disable-features=DisableLoadExtensionCommandLineSwitch does not restore command-line loading. Open chrome://extensions and use Load unpacked, or use unbranded Chromium.".to_owned()
            });
        let mut suffix = String::new();
        for text in [warning.as_deref(), extension_warning.as_deref()]
            .into_iter()
            .flatten()
        {
            suffix.push_str(". ");
            suffix.push_str(text);
        }
        let baseline_note = match baseline {
            WindowBaseline::PreLaunch => "",
            WindowBaseline::AfterSubmit => {
                ". warning: the pre-launch inventory timed out, so the baseline was read after submission; any window already in that baseline may predate the command"
            }
            WindowBaseline::Unavailable => {
                ". warning: KWin did not answer before or after submission, so a window reported here may predate the command"
            }
        };
        let suffix = format!("{suffix}{baseline_note}");
        match (win_geo, exit_status) {
            (Some(geo), _) => {
                let (outcome, description) = if window_reused {
                    (
                        "reused_existing",
                        format!("reused window: {} title: {:?}", geo.id, geo.title),
                    )
                } else if window_observed_after_timeout {
                    (
                        "new",
                        format!("new window: {} (observed during timeout recovery)", geo.id),
                    )
                } else {
                    ("new", format!("window: {}", geo.id))
                };
                Ok(structured_result(&peer, format!("launched: {} {description}{suffix}", params.command), serde_json::json!({
                    "action": "launch", "command": params.command, "window": geo.id, "command_submitted": true,
                    "window_outcome": outcome, "window_reused": window_reused,
                    "window_observed_after_timeout": window_observed_after_timeout,
                    "window_title": geo.title,
                    "cdp": cdp_connected, "cdp_port": cdp_connected.then_some(cdp_port), "warning": warning,
                    "extension_warning": extension_warning,
                    "window_timeout_seconds": window_timeout.as_secs(),
                    "window_baseline": baseline.as_str(), "window_baseline_errors": baseline_errors,
                })).await)
            }
            (None, Some(status)) => {
                let browser_unattributed = status == 0 && chromium_launch;
                let sandbox_output = match &sandbox_log_offset {
                    Ok(offset) => sandbox_log_since(&sandbox_log, *offset),
                    Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
                };
                let diagnostics = exit_diagnostics(sandbox_output, &command_paths);
                let text = match status {
                    0 if browser_unattributed => format!(
                        "launched: {} (browser launch returned status 0, but no matching existing window could be attributed: {} after {}s){suffix}",
                        params.command,
                        handoff_reason
                            .as_deref()
                            .unwrap_or("KWin reported no attributable window change"),
                        window_timeout.as_secs(),
                    ),
                    0 => format!(
                        "launched: {} (exited with status 0; no window after {}s){suffix}",
                        params.command,
                        window_timeout.as_secs()
                    ),
                    127 => format!(
                        "{} failed: command not found (exit status 127)",
                        params.command
                    ),
                    _ => format!(
                        "{} exited with status {status} without opening a window",
                        params.command
                    ),
                };
                let snapshot_note = if timeout_window_snapshot.is_some() {
                    ". A fresh post-timeout window inventory is attached as window_snapshot"
                        .to_owned()
                } else if let Some(reason) = timeout_window_snapshot_error.as_deref() {
                    format!(". The post-timeout window inventory was unavailable: {reason}")
                } else {
                    String::new()
                };
                let mut text = format!(
                    "{text}{snapshot_note}{}",
                    exit_diagnostics_text(&diagnostics)
                );
                if let Some(resolution) = command_resolution.as_deref() {
                    text.push_str(&format!("\nfirst command token resolution: {resolution}"));
                }
                let window_outcome = if browser_unattributed {
                    "unattributed"
                } else {
                    "exited"
                };
                let mut payload = serde_json::json!({
                    "action": "launch", "command": params.command, "command_submitted": true,
                    "window": if browser_unattributed { "unattributed" } else { "exited" },
                    "window_outcome": window_outcome,
                    "exit_status": status, "cdp": false, "cdp_port": null, "warning": warning,
                    "extension_warning": extension_warning,
                    "window_timeout_seconds": window_timeout.as_secs(),
                    "window_baseline": baseline.as_str(), "window_baseline_errors": baseline_errors,
                    "window_snapshot": timeout_window_snapshot,
                    "window_snapshot_error": timeout_window_snapshot_error,
                    "stderr_tail": diagnostics.stderr,
                    "sandbox_log_line": diagnostics.sandbox_line,
                    "stderr_error": diagnostics.unavailable,
                    "command_resolution": command_resolution,
                });
                if browser_unattributed {
                    payload["handoff"] = serde_json::json!({
                        "state": "unattributed",
                        "reason": handoff_reason,
                        "candidate_windows": handoff_candidates,
                    });
                }
                let mut result = structured_result(&peer, text, payload).await;
                result.is_error = Some(status != 0);
                Ok(result)
            }
            // The command is still running and no window appeared: a GUI that
            // never mapped, or a background command that should have passed
            // expect_window=false. Neither is a successful window launch.
            (None, None) => {
                let snapshot_note = if timeout_window_snapshot.is_some() {
                    ". A fresh post-timeout window inventory is attached as window_snapshot"
                        .to_owned()
                } else if let Some(reason) = timeout_window_snapshot_error.as_deref() {
                    format!(". The post-timeout window inventory was unavailable: {reason}")
                } else {
                    String::new()
                };
                let mut result = structured_result(&peer, format!(
                    "{} opened no window within {}s and is still running. It was submitted once; inspect window_list and screenshot before any retry, or wait longer with window_timeout_seconds. Pass expect_window: false for background or non-GUI commands.{suffix}{snapshot_note}",
                    params.command,
                    window_timeout.as_secs()
                ), serde_json::json!({
                    "action": "launch", "command": params.command, "window": "timeout",
                    "window_outcome": "timeout", "reason": "window_not_observed",
                    "cdp": false, "cdp_port": null, "warning": warning, "extension_warning": extension_warning,
                    "stage": "waiting for a window", "command_submitted": true, "retry_safe": false,
                    "window_timeout_seconds": window_timeout.as_secs(),
                    "window_baseline": baseline.as_str(), "window_baseline_errors": baseline_errors,
                    "window_snapshot": timeout_window_snapshot,
                    "window_snapshot_error": timeout_window_snapshot_error,
                })).await;
                result.is_error = Some(true);
                Ok(result)
            }
        }
    }
}

const CLI_USAGE: &str = "Usage: kwin-mcp [OPTIONS]\n       kwin-mcp --describe [OPTIONS]\n       kwin-mcp --stats [SECONDS]";

fn print_cli_help() {
    println!(
        "kwin-mcp — isolated KDE desktop automation over MCP stdio\n\n\
{CLI_USAGE}\n\n\
Options:\n\
  -h, --help             Print this help and exit\n\
  -V, --version          Print package version, build number, and Git commit\n\
  --describe             Print MCP initialization and tool schemas as JSON, then exit\n\
  --width N              Default session width in pixels (default: {VIRTUAL_SCREEN_WIDTH})\n\
  --height N             Default session height in pixels (default: {VIRTUAL_SCREEN_HEIGHT})\n\
  --no-override          Prevent session_start from overriding display dimensions\n\
  --no-viewer            Compatibility option; the viewer already starts on demand\n\
  --autoclean            Clean up orphaned session workdirs\n\
  --ttl MINUTES          Expire the session after MINUTES; implies --autoclean\n\
  --keep-sessions MINUTES  After the MCP client closes, keep running sessions for other\n\
                         kwin-mcp processes to forward to until MINUTES idle (default: stop them)\n\
  --memory-high GIB      Session memory pressure threshold (default: {}; 0 disables)\n\
  --memory-max GIB       Session memory limit (default: {}; must be positive)\n\
  --memory-swap-max GIB  Session swap limit (default: {}; 0 disables)\n\
  --stats [SECONDS]      Print recent session_start statistics (default: {DEFAULT_STATS_SECONDS}s)\n\n\
With no mode flag, serve MCP requests on stdin/stdout. Display dimensions must\n\
be {MIN_SCREEN_DIM}..={MAX_SCREEN_DIM}; --memory-high must not exceed --memory-max.",
        DEFAULT_MEMORY_HIGH / GIB_BYTES,
        DEFAULT_MEMORY_MAX / GIB_BYTES,
        DEFAULT_MEMORY_SWAP_MAX / GIB_BYTES,
    );
}

fn parse_cli_args() -> Result<DisplayConfig, String> {
    parse_cli_args_from(std::env::args().skip(1))
}

const GIB_BYTES: u64 = 1 << 30;
const DEFAULT_MEMORY_HIGH: u64 = 3 * GIB_BYTES;
const DEFAULT_MEMORY_MAX: u64 = 4 * GIB_BYTES;
const DEFAULT_MEMORY_SWAP_MAX: u64 = GIB_BYTES;

fn parse_cli_args_from(mut args: impl Iterator<Item = String>) -> Result<DisplayConfig, String> {
    let mut cfg = DisplayConfig {
        width: VIRTUAL_SCREEN_WIDTH,
        height: VIRTUAL_SCREEN_HEIGHT,
        locked: false,
        autoclean: false,
        ttl: None,
        keep_sessions: None,
        memory_high: Some(DEFAULT_MEMORY_HIGH),
        memory_max: DEFAULT_MEMORY_MAX,
        memory_swap_max: DEFAULT_MEMORY_SWAP_MAX,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--width" => cfg.width = parse_dim_arg(&mut args, "--width")?,
            "--height" => cfg.height = parse_dim_arg(&mut args, "--height")?,
            "--no-override" => cfg.locked = true,
            // Kept for callers that used it before viewer startup became opt-in.
            "--no-viewer" => {}
            "--autoclean" => cfg.autoclean = true,
            "--ttl" => cfg.ttl = Some(parse_ttl_arg(&mut args)?),
            "--keep-sessions" => {
                cfg.keep_sessions = Some(parse_minutes_arg(&mut args, "--keep-sessions")?)
            }
            "--memory-high" => {
                cfg.memory_high = match parse_memory_arg(&mut args, "--memory-high")? {
                    0 => None,
                    bytes => Some(bytes),
                }
            }
            "--memory-max" => cfg.memory_max = parse_memory_arg(&mut args, "--memory-max")?,
            "--memory-swap-max" => {
                cfg.memory_swap_max = parse_memory_arg(&mut args, "--memory-swap-max")?
            }
            other => {
                return Err(format!(
                    "unknown argument '{other}'\n{CLI_USAGE}\nTry 'kwin-mcp --help' for options."
                ));
            }
        }
    }
    if cfg.memory_max == 0 {
        return Err("--memory-max must be positive".to_owned());
    }
    if cfg.memory_high.is_some_and(|high| high > cfg.memory_max) {
        return Err(
            "--memory-high must not exceed --memory-max; set both for a larger session".to_owned(),
        );
    }
    // Expiry owns the same workdir terminal transition as --autoclean.
    cfg.autoclean |= cfg.ttl.is_some();
    Ok(cfg)
}

fn parse_memory_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let value = args.next().ok_or_else(|| format!("{flag} requires GiB"))?;
    let gib: u64 = value
        .parse()
        .map_err(|error| format!("{flag} '{value}': {error}"))?;
    gib.checked_mul(GIB_BYTES)
        .ok_or_else(|| format!("{flag} is too large"))
}

/// IPv4 address and prefix on the host interface with the preferred default route.
/// Leave pasta's address selection unchanged when the host has no IPv4 default.
fn host_default_ipv4() -> anyhow::Result<Option<(std::net::Ipv4Addr, u32)>> {
    let routes = std::fs::read_to_string("/proc/net/route")?;
    let mut default: Option<(&str, u32)> = None;
    for line in routes.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 8 || fields[1] != "00000000" || fields[7] != "00000000" {
            continue;
        }
        let (Ok(flags), Ok(metric)) =
            (u32::from_str_radix(fields[3], 16), fields[6].parse::<u32>())
        else {
            continue;
        };
        if flags & IPV4_ROUTE_UP != 0 && default.is_none_or(|(_, best)| metric < best) {
            default = Some((fields[0], metric));
        }
    }
    let Some((interface, _)) = default else {
        return Ok(None);
    };
    let (address, mask) = nix::ifaddrs::getifaddrs()?
        .find_map(|entry| {
            if entry.interface_name != interface {
                return None;
            }
            Some((
                entry.address.as_ref()?.as_sockaddr_in()?.ip(),
                entry.netmask.as_ref()?.as_sockaddr_in()?.ip(),
            ))
        })
        .ok_or_else(|| {
            anyhow::anyhow!("default-route interface {interface} has no IPv4 address and netmask")
        })?;
    let mask_bits = u32::from(mask);
    let prefix = mask_bits.count_ones();
    anyhow::ensure!(
        mask_bits.leading_ones() == prefix,
        "non-contiguous IPv4 netmask {mask} on {interface}"
    );
    Ok(Some((address, prefix)))
}

/// Descriptor limit (soft:hard) for every process in a session. A scope cannot
/// carry rlimits, so prlimit applies it to pasta, which the whole tree inherits.
const SESSION_NOFILE: &str = "--nofile=1024:65536";

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Start the pasta and bwrap tree in a bounded systemd user scope.
/// Refuse startup when the scope is unavailable instead of running uncapped.
fn sandbox_launcher(
    config: DisplayConfig,
    runtime: &Path,
    session_bus: &str,
    scope_unit: Option<&str>,
) -> Result<std::process::Command, String> {
    let usable = std::process::Command::new("systemd-run")
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus)
        .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !usable {
        return Err(
            "systemd user scope unavailable; refusing to start an unbounded session".to_owned(),
        );
    }
    let high = config
        .memory_high
        .map_or_else(|| "infinity".to_owned(), |bytes| bytes.to_string());
    eprintln!(
        "session_start: MemoryHigh={high} MemoryMax={} MemorySwapMax={} bytes",
        config.memory_max, config.memory_swap_max
    );
    let mut command = std::process::Command::new("systemd-run");
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", session_bus)
        .args(["--user", "--scope", "--quiet"]);
    if let Some(unit) = scope_unit {
        command.arg("--unit").arg(unit);
    } else {
        command.arg("--collect");
    }
    command
        .arg("-p")
        .arg(format!("MemoryHigh={high}"))
        .arg("-p")
        .arg(format!("MemoryMax={}", config.memory_max))
        .arg("-p")
        .arg(format!("MemorySwapMax={}", config.memory_swap_max))
        .arg("--");
    // Per process: a runaway session process hits EMFILE at 65536 descriptors
    // instead of the host's 524288; the soft limit keeps the user default.
    if on_path("prlimit") {
        command.args(["prlimit", SESSION_NOFILE]);
    } else {
        eprintln!(
            "session_start: prlimit not found; the session keeps the default descriptor limit"
        );
    }
    command.arg("pasta");
    Ok(command)
}

fn parse_ttl_arg(args: &mut impl Iterator<Item = String>) -> Result<Duration, String> {
    parse_minutes_arg(args, "--ttl")
}

fn parse_minutes_arg(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<Duration, String> {
    let value = args.next().ok_or_else(|| format!("{flag} requires minutes"))?;
    let minutes: u64 = value
        .parse()
        .map_err(|error| format!("{flag} '{value}': {error}"))?;
    if minutes == 0 {
        return Err(format!("{flag} must be positive"));
    }
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| format!("{flag} is too large"))?;
    Ok(Duration::from_secs(seconds))
}

fn parse_dim_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u32, String> {
    let v = args
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?;
    let n: u32 = v.parse().map_err(|e| format!("{flag} '{v}': {e}"))?;
    if !(MIN_SCREEN_DIM..=MAX_SCREEN_DIM).contains(&n) {
        return Err(format!(
            "{flag} {n} out of range {MIN_SCREEN_DIM}..={MAX_SCREEN_DIM}"
        ));
    }
    Ok(n)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Inside the sandbox this binary also serves FUSE (see fuse_bridge).
    let mut argv = std::env::args();
    if let Some(program) = argv.next().as_deref().and_then(fuse_bridge::shim_program) {
        std::process::exit(fuse_bridge::run_client(program));
    }
    match argv.next().as_deref() {
        Some("-h" | "--help") => {
            print_cli_help();
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!(
                "kwin-mcp v{}.{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("BUILD_NUMBER"),
                env!("GIT_HASH")
            );
            return Ok(());
        }
        Some("--describe") => {
            let display = parse_cli_args_from(argv)?;
            let server = SessionRouter::new(display);
            let description = serde_json::json!({
                "initialize": rmcp::ServerHandler::get_info(&server),
                "tools": server.tools(),
            });
            serde_json::to_writer(std::io::stdout().lock(), &description)?;
            println!();
            return Ok(());
        }
        Some("--stats") => {
            let seconds = match argv.next() {
                Some(value) => value
                    .parse::<u64>()
                    .map_err(|error| format!("--stats '{value}': {error}"))?,
                None => DEFAULT_STATS_SECONDS,
            };
            if let Some(extra) = argv.next() {
                return Err(format!("--stats accepts only [SECONDS], got '{extra}'").into());
            }
            print_session_start_stats(seconds)?;
            return Ok(());
        }
        Some("--fuse-helper") => {
            let socket = argv
                .next()
                .unwrap_or_else(|| fuse_bridge::HELPER_SOCKET.to_owned());
            fuse_bridge::run_helper(Path::new(&socket))?;
            return Ok(());
        }
        // One orphan sweep, then exit: kwin-mcp-shim runs this on a timer so
        // leaked workdirs are reaped even while no new server starts.
        Some("--sweep-workdirs") => {
            let owners: std::collections::HashSet<u32> = argv
                .map(|value| {
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|pid| *pid > 1)
                        .ok_or_else(|| {
                            format!(
                                "--sweep-workdirs requires owner PIDs greater than 1, got '{value}'"
                            )
                        })
                })
                .collect::<Result<_, _>>()?;
            let owners = (!owners.is_empty()).then_some(&owners);
            sweep_orphaned_workdirs(owners)?;
            sweep_dead_owner_workdirs(owners)?;
            if let Some(owners) = owners {
                for owner in owners {
                    for root in session_runtime_roots()
                        .into_iter()
                        .chain([session_disk_root()])
                    {
                        let dir = root.join(format!("kwin-mcp-{owner}"));
                        match std::fs::symlink_metadata(&dir) {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                            Ok(_) => {
                                return Err(
                                    format!("workdir sweep retained {}", dir.display()).into()
                                );
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let restored = restore_state()?;
    // Consume the handoff marker before runtime threads or applications inherit it.
    unsafe {
        std::env::remove_var(RESUME_DESCRIPTOR);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run_server(restored));
    // Tokio's stdio reader uses a blocking thread. A signal leaves stdin open,
    // so waiting indefinitely for that thread would keep the server alive
    // after its session and workdir have already been cleaned up.
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

fn configured_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<KwinMcp> {
    // Inject the host's installed browsers into the launch_app description so the
    // agent knows what it can actually run without guessing (issue #28).
    let mut tool_router = KwinMcp::tool_router();
    let browsers = detect_browsers();
    eprintln!(
        "kwin-mcp: detected browsers: {}",
        if browsers.is_empty() {
            "(none)".to_owned()
        } else {
            browsers.join(", ")
        }
    );
    if let Some(route) = tool_router.map.get_mut("launch_app") {
        let hint = if browsers.is_empty() {
            "\n\nNo known browser was found on this host's PATH.".to_owned()
        } else {
            format!(
                "\n\nInstalled browser commands: {}. Firefox supports launch, screenshots and input.",
                browsers.join(", ")
            )
        };
        let base = route
            .attr
            .description
            .take()
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_default();
        route.attr.description = Some(std::borrow::Cow::Owned(base + &hint));
    }
    tool_router
}

// Each isolated desktop is a Tower service. The map owns its lifecycle;
// the shared layer limits execution without serializing reads or other desktops.
type ToolRoutes = rmcp::handler::server::router::tool::ToolRouter<KwinMcp>;
type ToolContext = rmcp::service::RequestContext<rmcp::RoleServer>;
struct SessionCall {
    request: rmcp::model::CallToolRequestParams,
    context: ToolContext,
    routes: Arc<ToolRoutes>,
}

impl tower::Service<SessionCall> for KwinMcp {
    type Response = CallToolResult;
    type Error = McpError;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, call: SessionCall) -> Self::Future {
        let session = self.clone();
        Box::pin(async move {
            let operation = call.request.name.to_string();
            let typing = matches!(operation.as_str(), "keyboard_type" | "keyboard_paste");
            let screenshot = operation == "screenshot";
            let report_memory = screenshot || input_tool(&operation);
            let audit = if input_tool(&operation) {
                session
                    .input_log
                    .as_ref()
                    .map(|log| log.begin(operation.clone(), serde_json::json!(call.context.id)))
                    .transpose()
                    .map_err(KwinError::from)?
            } else {
                None
            };
            let memory_before = if report_memory {
                session.session_memory_snapshot().await
            } else {
                None
            };
            let mut result = call
                .routes
                .call(rmcp::handler::server::tool::ToolCallContext::new(
                    &session,
                    call.request,
                    call.context,
                ))
                .await;
            if let Some(audit) = audit {
                let outcome = if result
                    .as_ref()
                    .is_ok_and(|result| result.is_error != Some(true))
                {
                    "ok"
                } else {
                    "error"
                };
                if let Err(error) = audit.finish(outcome) {
                    if typing {
                        result = Err(typing_log_failure(result, error));
                    } else {
                        result = Err(KwinError::from(error).into());
                    }
                }
            }
            if report_memory {
                result = session
                    .attach_memory_warning(result, memory_before, screenshot)
                    .await;
                let failed = result
                    .as_ref()
                    .map_or(true, |result| result.is_error == Some(true));
                if failed && let Some(snapshot) = session.session_memory_snapshot().await {
                    result = attach_memory_snapshot_to_failure(result, snapshot);
                }
            }
            result
        })
    }
}

const PASTE_VERIFICATION_MAX_BYTES: usize = 1024 * 1024;
const PASTE_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(12);

enum PreparedPaste {
    Ready(input_bridge::TextReceipt),
    Unavailable {
        method: &'static str,
        warning: &'static str,
    },
}

fn is_native_paste_combo(modifiers: &[u32], key: u32) -> bool {
    ["ctrl+v", "shift+insert"].into_iter().any(|combo| {
        parse_combo(combo).is_ok_and(|(expected, expected_key)| {
            key == expected_key
                && modifiers.len() == expected.len()
                && modifiers.iter().all(|modifier| expected.contains(modifier))
        })
    })
}

async fn prepare_paste_text(
    snapshot: kwin_mcp::clipboard::ClipboardText,
    address: &str,
    title: &str,
    bounds: [f64; 4],
) -> PreparedPaste {
    use kwin_mcp::clipboard::ClipboardText;
    let bytes = match snapshot {
        ClipboardText::Secret(_) => {
            return PreparedPaste::Unavailable {
                method: "paste_protected_clipboard",
                warning: "Editable-value verification was skipped for a protected clipboard selection.",
            };
        }
        ClipboardText::Empty => {
            return PreparedPaste::Unavailable {
                method: "paste_text_unavailable",
                warning: "No clipboard text snapshot was available for editable-value verification.",
            };
        }
        ClipboardText::Text(bytes) => bytes,
    };
    if bytes.len() > PASTE_VERIFICATION_MAX_BYTES {
        return PreparedPaste::Unavailable {
            method: "paste_text_too_large",
            warning: "Clipboard text exceeds the one MiB verification limit. The native paste shortcut was still submitted.",
        };
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return PreparedPaste::Unavailable {
            method: "paste_text_unavailable",
            warning: "The clipboard did not provide supported UTF-8 text for editable-value verification.",
        };
    };
    match input_bridge::TextReceipt::capture_paste(address, title, bounds, None, &text).await {
        Ok(receipt) => PreparedPaste::Ready(receipt),
        Err(_) => PreparedPaste::Unavailable {
            method: "paste_target_unobservable",
            warning: "The native paste shortcut was submitted, but the focused editable value could not be observed.",
        },
    }
}

fn unverified_paste_delivery(
    method: &'static str,
    warning: &'static str,
) -> input_bridge::TextDelivery {
    input_bridge::TextDelivery {
        status: "unverified",
        method,
        compositor_acknowledged: true,
        expected_field_characters: None,
        observed_field_characters: None,
        warning: Some(warning.to_owned()),
    }
}

async fn finish_prepared_paste(preparation: PreparedPaste) -> input_bridge::TextDelivery {
    match preparation {
        PreparedPaste::Ready(receipt) => {
            match tokio::time::timeout(PASTE_VERIFICATION_TIMEOUT, receipt.confirm()).await {
                Ok(delivery) => delivery,
                Err(_) => unverified_paste_delivery(
                    "paste_verification_timeout",
                    "The paste shortcut was acknowledged, but editable-value verification timed out. Inspect the field before retrying; do not paste again blindly.",
                ),
            }
        }
        PreparedPaste::Unavailable { method, warning } => {
            unverified_paste_delivery(method, warning)
        }
    }
}

fn input_tool(name: &str) -> bool {
    name.starts_with("mouse_") || name.starts_with("keyboard_")
}

fn ordered_tool(name: &str) -> bool {
    input_tool(name) || matches!(name, "clipboard_write" | "clipboard_read")
}

const SESSION_OWNERSHIP_NOTICE: &str = "This shared server may contain other agents' sessions. Use IDs from your own session_start calls, or explicitly attach to a specific existing session authorized by the user with session_attach. Attachment transfers no lifecycle ownership.";
const SESSION_RESUME_GUIDANCE: &str = "Call session_list to inspect sessions owned by this kwin-mcp process. Use session_stop with an ID listed as dead, stopped, expired, or start_blocked to clear it. If the original process is no longer running and the saved ID is absent, call session_start without session_id to create a new desktop, relaunch the applications, and restore your task. The old desktop cannot be resumed here.";
const SESSION_CONTROL_SOCKET_PREFIX: &str = "kwin-mcp-control-";
const SESSION_CONTROL_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_CONTROL_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

fn session_owner_pid(id: &str) -> Option<u32> {
    id.strip_prefix('s')
        .and_then(|id| id.split_once('-'))
        .and_then(|(pid, _)| pid.parse::<u32>().ok())
}

fn session_control_socket_path(server_pid: u32) -> anyhow::Result<PathBuf> {
    Ok(host_environment::runtime_dir()?
        .join(format!("{SESSION_CONTROL_SOCKET_PREFIX}{server_pid}.sock")))
}

/// Whether `path` names a kwin-mcp server's control socket.
fn is_session_control_socket(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(SESSION_CONTROL_SOCKET_PREFIX))
        .and_then(|rest| rest.strip_suffix(".sock"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Whether process `pid` (as this server sees it) shares this server's PID
/// namespace. A sandboxed session process does not.
fn same_pid_namespace(pid: i32) -> bool {
    match (
        std::fs::read_link("/proc/self/ns/pid"),
        std::fs::read_link(format!("/proc/{pid}/ns/pid")),
    ) {
        (Ok(own), Ok(peer)) => own == peer,
        _ => false,
    }
}

/// Remove control sockets that no server listens on. A server killed without
/// a chance to unlink its socket (SIGKILL, a crash, a host that stops it hard)
/// leaves the file behind, and each one makes a lost session look routable.
/// A listening socket is never connected to: the connection would start a
/// handshake that the probe then abandons, which its server logs as a failure.
/// The kernel's socket table says which paths still have a socket behind them.
fn sweep_stale_control_sockets() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(directory) = host_environment::runtime_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    // Without the table nothing proves a socket stale.
    let Ok(table) = std::fs::read_to_string("/proc/net/unix") else {
        return;
    };
    let bound: std::collections::HashSet<&str> = table
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(7))
        .collect();
    let uid = unsafe { nix::libc::geteuid() };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_session_control_socket(&path) {
            continue;
        }
        let owner_running = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(SESSION_CONTROL_SOCKET_PREFIX))
            .and_then(|rest| rest.strip_suffix(".sock"))
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(kwin_mcp_server_running);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if owner_running
            || !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || path.to_str().is_some_and(|path| bound.contains(path))
        {
            continue;
        }
        // The table is per network namespace, so a server in another one would
        // be missing from it: only a refused connection proves nothing listens.
        match std::os::unix::net::UnixStream::connect(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                match std::fs::remove_file(&path) {
                    Ok(()) => eprintln!(
                        "session control: removed stale socket {}",
                        path.display()
                    ),
                    Err(error) => eprintln!(
                        "session control: could not remove stale socket {}: {error}",
                        path.display()
                    ),
                }
            }
            _ => {}
        }
    }
}

struct SessionControlSocket {
    listener: tokio::net::UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SessionControlSocket {
    fn bind(server_pid: u32) -> std::io::Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

        let path = session_control_socket_path(server_pid).map_err(std::io::Error::other)?;
        let uid = unsafe { nix::libc::geteuid() };
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != uid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "session control path is not an owned socket",
                    ));
                }
                match std::os::unix::net::UnixStream::connect(&path) {
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            "session control socket is already active",
                        ));
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) =>
                    {
                        std::fs::remove_file(&path)?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let listener = tokio::net::UnixListener::bind(&path)?;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket() || metadata.uid() != uid {
            let _ = std::fs::remove_file(&path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session control socket ownership validation failed",
            ));
        }
        Ok(Self {
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    async fn serve(self, router: SessionRouter) {
        let uid = unsafe { nix::libc::geteuid() };
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let credentials = stream.peer_cred().ok();
                        let authorized = credentials
                            .is_some_and(|credentials| credentials.uid() == uid);
                        if !authorized {
                            eprintln!(
                                "session control: rejected peer with a different effective UID"
                            );
                            continue;
                        }
                        // Every session process runs in a sandbox PID namespace
                        // with the host UID; only peers in this server's own PID
                        // namespace may drive sessions through this socket.
                        let same_namespace = credentials
                            .and_then(|credentials| credentials.pid())
                            .is_some_and(same_pid_namespace);
                        if !same_namespace {
                            eprintln!(
                                "session control: rejected peer outside this server's PID namespace"
                            );
                            continue;
                        }
                        let router = router.clone();
                        connections.spawn(async move {
                            match router.serve(stream).await {
                                Ok(service) => {
                                    if let Err(error) = service.waiting().await {
                                        eprintln!("session control: MCP connection failed: {error}");
                                    }
                                }
                                Err(error) => {
                                    eprintln!("session control: MCP handshake failed: {error}")
                                }
                            }
                        });
                    }
                    Err(error) => {
                        eprintln!("session control: accept failed: {error}");
                        break;
                    }
                },
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result {
                        eprintln!("session control: connection task failed: {error}");
                    }
                }
            }
        }
    }
}

impl Drop for SessionControlSocket {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};

        let should_remove = std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { nix::libc::geteuid() }
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        });
        if should_remove {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionAttachArgs {
    session_id: String,
    owner_pid: u32,
    user_authorized: bool,
    application: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionOwnerIdentity {
    pid: u32,
    uid: u32,
    start_ticks: u64,
}

#[derive(Clone, Debug)]
struct AttachedSession {
    session_id: String,
    owner: SessionOwnerIdentity,
    sandbox_pid: u32,
    workdir: PathBuf,
    scope_unit: Option<String>,
}

fn session_attach_error(reason: &'static str, message: impl Into<String>) -> McpError {
    McpError::invalid_params(
        message.into(),
        Some(serde_json::json!({
            "reason":reason, "submitted":false, "retry_safe":false,
        })),
    )
}

fn session_owner_identity(pid: u32) -> Result<SessionOwnerIdentity, String> {
    use std::os::unix::fs::MetadataExt;
    let signed = i32::try_from(pid).map_err(|_| "session owner PID is out of range".to_owned())?;
    if signed <= 0 {
        return Err("session owner PID must be positive".to_owned());
    }
    let process = procfs::process::Process::new(signed)
        .map_err(|error| format!("session owner {pid} is unavailable: {error}"))?;
    let stat = process
        .stat()
        .map_err(|error| format!("cannot verify owner {pid}: {error}"))?;
    if matches!(stat.state, 'Z' | 'X' | 'x') {
        return Err(format!("owner {pid} has exited"));
    }
    let metadata = std::fs::metadata(format!("/proc/{pid}"))
        .map_err(|error| format!("cannot verify owner UID: {error}"))?;
    Ok(SessionOwnerIdentity {
        pid,
        uid: metadata.uid(),
        start_ticks: stat.starttime,
    })
}

fn verify_attached_owner(pid: u32, expected: SessionOwnerIdentity) -> Result<(), McpError> {
    let observed = session_owner_identity(pid)
        .map_err(|error| session_attach_error("attached_session_owner_changed", error))?;
    if observed != expected {
        return Err(session_attach_error(
            "attached_session_owner_changed",
            "The attached owner has exited or changed. The operation was not forwarded.",
        ));
    }
    Ok(())
}

fn attached_session_from_owner_list(
    args: &SessionAttachArgs,
    owner: SessionOwnerIdentity,
    list: &serde_json::Value,
) -> Result<AttachedSession, McpError> {
    if list["server_pid"].as_u64() != Some(u64::from(owner.pid))
        || list["scope"].as_str() != Some("current_process")
    {
        return Err(session_attach_error(
            "session_owner_mismatch",
            "The responding server did not identify the requested owner.",
        ));
    }
    let sessions = list["sessions"].as_array().ok_or_else(|| {
        session_attach_error(
            "invalid_session_list",
            "The owner returned no structured session list.",
        )
    })?;
    let session = sessions
        .iter()
        .find(|session| session["session_id"].as_str() == Some(&args.session_id))
        .ok_or_else(|| {
            session_attach_error(
                "session_not_found",
                "The selected session is not registered with its owner.",
            )
        })?;
    if session["status"].as_str() != Some("running") {
        return Err(session_attach_error(
            "session_not_running",
            "Only an already-running session can be attached.",
        ));
    }
    if session["pid"].as_u64() != Some(u64::from(owner.pid)) {
        return Err(session_attach_error(
            "session_owner_mismatch",
            "The selected session record belongs to a different owner.",
        ));
    }
    let sandbox_pid = session["sandbox_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            session_attach_error(
                "invalid_session_identity",
                "The session has no valid sandbox PID.",
            )
        })?;
    let workdir = session["workdir"]
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            session_attach_error(
                "invalid_session_identity",
                "The session has no absolute work directory.",
            )
        })?;
    Ok(AttachedSession {
        session_id: args.session_id.clone(),
        owner,
        sandbox_pid,
        workdir,
        scope_unit: session["scope_unit"].as_str().map(str::to_owned),
    })
}

async fn forward_session_call(
    server_pid: u32,
    request: rmcp::model::CallToolRequestParams,
) -> Result<Option<CallToolResult>, McpError> {
    forward_session_call_checked(server_pid, request, None).await
}

async fn forward_session_call_checked(
    server_pid: u32,
    request: rmcp::model::CallToolRequestParams,
    expected_owner: Option<SessionOwnerIdentity>,
) -> Result<Option<CallToolResult>, McpError> {
    if let Some(expected) = expected_owner {
        verify_attached_owner(server_pid, expected)?;
    }
    let connected = tokio::time::timeout(SESSION_CONTROL_CONNECT_TIMEOUT, async {
        let path = match session_control_socket_path(server_pid) {
            Ok(path) => path,
            Err(_) => return None,
        };
        let stream = match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => stream,
            Err(_) => return None,
        };
        let credentials = match stream.peer_cred() {
            Ok(credentials) => credentials,
            Err(_) => return None,
        };
        let effective_uid = unsafe { nix::libc::geteuid() };
        if credentials.uid() != effective_uid
            || credentials.pid().and_then(|pid| u32::try_from(pid).ok()) != Some(server_pid)
        {
            return None;
        }

        rmcp::service::serve_client((), stream).await.ok()
    })
    .await;
    let Ok(Some(mut service)) = connected else {
        return if expected_owner.is_some() {
            Err(session_attach_error(
                "attached_session_owner_unavailable",
                "The attached owner could not be reached. The operation was not forwarded.",
            ))
        } else {
            Ok(None)
        };
    };
    if let Some(expected) = expected_owner {
        verify_attached_owner(server_pid, expected)?;
    }
    let result = match service.peer().call_tool(request).await {
        Ok(result) => Ok(Some(result)),
        Err(rmcp::service::ServiceError::McpError(error)) => Err(error),
        Err(error) => Err(McpError::internal_error(
            format!(
                "session owner process {server_pid} lost the control connection while handling the request; inspect the session before retrying"
            ),
            Some(serde_json::json!({
                "reason": "session_owner_connection_failed",
                "retry_safe": false,
                "session_server_pid": server_pid,
                "transport_error": error.to_string(),
            })),
        )),
    };
    let _ = tokio::time::timeout(SESSION_CONTROL_CLOSE_TIMEOUT, service.close()).await;
    result
}

fn unavailable_session_error() -> McpError {
    let server_pid = std::process::id();
    let message = format!(
        "no live session in kwin-mcp process {server_pid}. The previous server's cleanup status is not confirmed from this process. {SESSION_RESUME_GUIDANCE}"
    );
    McpError::invalid_params(
        message,
        Some(serde_json::json!({
            "reason": "session_handle_unavailable",
            "session_status": "none_running",
            "session_available_here": false,
            "session_id": null,
            "server_pid": server_pid,
            "session_list_scope": "current_process",
            "previous_server_cleanup": "not_confirmed_from_current_process",
            "previous_server_cleanup_record": null,
            "resume_guidance": SESSION_RESUME_GUIDANCE,
        })),
    )
}

fn unknown_session_error_with_lifecycle(id: &str) -> Result<McpError, McpError> {
    let mut error = unknown_session_error(id);
    let record = match previous_session_lifecycle(id) {
        Ok(record) => record,
        Err(source) => {
            let reason = if source.kind() == std::io::ErrorKind::InvalidData {
                "malformed_session_lifecycle_record"
            } else {
                "session_lifecycle_log_unreadable"
            };
            return Err(McpError::internal_error(
                format!(
                    "{} Previous session lifecycle record could not be validated: {source}",
                    error.message
                ),
                Some(serde_json::json!({
                    "reason": reason,
                    "session_id": id,
                    "unknown_session_error": error.data,
                })),
            ));
        }
    };
    match record {
        Some(record) => {
            let status = record.status();
            let event_description = match record.state.as_str() {
                SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED => "shutdown cleanup finished",
                SESSION_LIFECYCLE_STATE_SESSION_STOPPED => "session stop",
                SESSION_LIFECYCLE_STATE_SESSION_EXPIRED => "idle TTL expiration",
                SESSION_LIFECYCLE_STATE_SESSION_DIED => "sandbox exit",
                SESSION_LIFECYCLE_STATE_START_FAILED => "failed session_start",
                SESSION_LIFECYCLE_STATE_START_ABANDONED => "blocked session_start cleanup",
                _ => "session lifecycle transition",
            };
            error.message = format!(
                "{} Session status is {status}. The previous server (PID {}) recorded {event_description} at Unix time {} (reason: {}, workdir cleanup: {}).",
                error.message,
                record.server_pid,
                record.finished_at_unix_seconds,
                record.reason,
                record.workdir_cleanup,
            )
            .into();
            let mut data = match error.data.take() {
                Some(serde_json::Value::Object(data)) => data,
                _ => serde_json::Map::new(),
            };
            data.insert("session_status".to_owned(), serde_json::json!(status));
            data.insert(
                "session_ended_at_unix_seconds".to_owned(),
                serde_json::json!(record.finished_at_unix_seconds),
            );
            data.insert(
                "session_end_reason".to_owned(),
                serde_json::json!(record.reason.as_str()),
            );
            if status == "expired" {
                data.insert(
                    "session_expired_at_unix_seconds".to_owned(),
                    serde_json::json!(record.finished_at_unix_seconds),
                );
            } else if status == "stopped" {
                data.insert(
                    "session_stopped_at_unix_seconds".to_owned(),
                    serde_json::json!(record.finished_at_unix_seconds),
                );
            }
            data.insert(
                "previous_session_lifecycle_record".to_owned(),
                serde_json::json!(&record),
            );
            if record.state == SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED {
                data.insert(
                    "previous_server_cleanup".to_owned(),
                    serde_json::json!(SESSION_LIFECYCLE_STATE_SHUTDOWN_FINISHED),
                );
                data.insert(
                    "previous_server_cleanup_record".to_owned(),
                    serde_json::json!(record),
                );
            } else {
                data.insert(
                    "previous_server_cleanup".to_owned(),
                    serde_json::json!("not_applicable"),
                );
                data.insert(
                    "previous_server_cleanup_record".to_owned(),
                    serde_json::Value::Null,
                );
            }
            error.data = Some(serde_json::Value::Object(data));
        }
        None => {
            error.message = format!(
                "{} The previous server's cleanup status is not confirmed from this process.",
                error.message
            )
            .into();
            let mut data = match error.data.take() {
                Some(serde_json::Value::Object(data)) => data,
                _ => serde_json::Map::new(),
            };
            data.insert(
                "previous_server_cleanup".to_owned(),
                serde_json::json!("not_confirmed_from_current_process"),
            );
            data.insert(
                "previous_server_cleanup_record".to_owned(),
                serde_json::Value::Null,
            );
            error.data = Some(serde_json::Value::Object(data));
        }
    }
    error.message = format!("{} {SESSION_RESUME_GUIDANCE}", error.message).into();
    if let Some(serde_json::Value::Object(data)) = error.data.as_mut() {
        data.insert(
            "resume_guidance".to_owned(),
            serde_json::json!(SESSION_RESUME_GUIDANCE),
        );
    }
    Ok(error)
}

/// Whether `pid` is a running kwin-mcp server on this host.
fn kwin_mcp_server_running(pid: u32) -> bool {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|exe| {
            exe.file_name()
                .map(|file| file.to_string_lossy().starts_with("kwin-mcp"))
        })
        .unwrap_or(false)
}

fn unknown_session_error(id: &str) -> McpError {
    let server_pid = std::process::id();
    let session_server_pid = session_owner_pid(id);
    let other_server_pid = session_server_pid.filter(|pid| *pid != server_pid);
    let owner_running = other_server_pid.map(kwin_mcp_server_running);
    let (reason, session_status, message, recovery) = if let Some(other_server_pid) =
        other_server_pid
    {
        let recovery = if owner_running == Some(false) {
            "Call session_start without session_id and relaunch your applications; a session ends when its server process exits.".to_owned()
        } else {
            format!(
            "This process could not reach an authenticated session-control socket for process {other_server_pid}. If it is still running an older kwin-mcp build, update it to enable routing; otherwise call session_start without session_id and relaunch your applications."
            )
        };
        let message = if owner_running == Some(false) {
            format!(
                "session_id '{id}' names kwin-mcp process {other_server_pid}, which is no longer running, so its sessions ended with it. This request reached process {server_pid}, which is what a server restart looks like: a host that respawns the MCP server, for example when a client session is resumed, closes the old server's connection, and a server shuts down its desktops when that happens. {recovery}"
            )
        } else {
            format!(
                "session_id '{id}' names kwin-mcp process {other_server_pid}, but this request reached process {server_pid}. Session IDs are scoped to the process that created them; a live same-user server can handle forwarded requests through its private control socket. This can happen after a server restart or when a request reaches a different MCP server. {recovery}"
            )
        };
        (
            "different_server_process",
            "owned_by_another_process",
            message,
            recovery,
        )
    } else {
        let recovery = "Call session_list to check this server. If the desktop is absent, call session_start without session_id and relaunch your applications.".to_owned();
        (
            "session_not_found",
            "not_found",
            format!(
                "session_id '{id}' is not known to kwin-mcp process {server_pid}; it may have been stopped, expired, or created by another server. {recovery}"
            ),
            recovery,
        )
    };
    McpError::invalid_params(
        message,
        Some(serde_json::json!({
            "reason": reason,
            "session_status": session_status,
            "session_available_here": false,
            "session_id": id,
            "server_pid": server_pid,
            "session_server_pid": session_server_pid,
            "session_server_running": owner_running,
            "recovery": recovery,
        })),
    )
}

#[derive(Clone)]
struct SessionRouter {
    attached: Arc<tokio::sync::RwLock<std::collections::BTreeMap<String, AttachedSession>>>,
    remote_input_orders:
        Arc<std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Weak<InputOrder>>>>,
    clipboards: Arc<std::sync::Mutex<ClipboardStore>>,
    sessions: Arc<std::sync::Mutex<std::collections::BTreeMap<String, KwinMcp>>>,
    next: Arc<std::sync::atomic::AtomicU64>,
    display: DisplayConfig,
    routes: Arc<ToolRoutes>,
    limit: tower::limit::GlobalConcurrencyLimitLayer,
}

impl SessionRouter {
    fn new(display: DisplayConfig) -> Self {
        Self {
            attached: Arc::new(tokio::sync::RwLock::new(std::collections::BTreeMap::new())),
            remote_input_orders: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            clipboards: Arc::new(std::sync::Mutex::new(ClipboardStore::default())),
            sessions: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            next: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            display,
            routes: Arc::new(configured_tool_router()),
            limit: tower::limit::GlobalConcurrencyLimitLayer::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            ),
        }
    }
    fn entries(&self) -> Result<std::collections::BTreeMap<String, KwinMcp>, McpError> {
        self.sessions
            .lock()
            .map(|sessions| sessions.clone())
            .map_err(|_| McpError::internal_error("session map lock poisoned", None))
    }
    fn resolve(
        &self,
        request: &mut rmcp::model::CallToolRequestParams,
    ) -> Result<(String, KwinMcp), McpError> {
        if request.name == "session_start"
            && request
                .arguments
                .as_ref()
                .is_some_and(|args| args.contains_key("session_id"))
        {
            return Err(McpError::invalid_params(
                "session_start does not accept session_id; omit it to create your own new session",
                None,
            ));
        }
        let id = request
            .arguments
            .as_mut()
            .and_then(|args| args.remove("session_id"));
        let id = match id {
            Some(serde_json::Value::String(id)) => Some(id),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "session_id must be a string",
                    None,
                ));
            }
            None => None,
        };
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| McpError::internal_error("session map lock poisoned", None))?;
        if request.name == "session_start" {
            let number = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = format!("s{}-{number}", std::process::id());
            let mut session = KwinMcp::new(self.display);
            session.session_id = id.clone();
            session.clipboards = self.clipboards.clone();
            session.input_log =
                Some(InputLog::open(&id, InputHistory::default(), None).map_err(KwinError::from)?);
            let name = format!("kwin-mcp-{}-{number}", std::process::id());
            session.path = session_runtime_root(&name).join(name);
            sessions.insert(id.clone(), session.clone());
            return Ok((id, session));
        }
        let live: Vec<_> = sessions
            .iter()
            .filter(|(_, session)| session.death_record().is_none())
            .map(|(id, _)| id.clone())
            .collect();
        let id = match id {
            Some(id) => id,
            None if live.len() == 1 => live
                .first()
                .cloned()
                .ok_or_else(|| McpError::internal_error("no session", None))?,
            None if live.is_empty() => {
                return Err(unavailable_session_error());
            }
            None => {
                return Err(McpError::invalid_params(
                    "several sessions exist; pass the session_id returned by session_start",
                    None,
                ));
            }
        };
        let session = sessions.get(&id).cloned();
        let Some(session) = session else {
            return Err(unknown_session_error_with_lifecycle(&id)?);
        };
        Ok((id, session))
    }
    /// session_stop for a handle this process does not hold. A saved ID answers
    /// from its lifecycle record, which is a confirmed no-op. With no ID and no
    /// handles, nothing is running. Anything else is left to resolve.
    async fn stop_unregistered(
        &self,
        requested: Option<&str>,
        peer: &rmcp::Peer<rmcp::RoleServer>,
    ) -> Result<Option<CallToolResult>, McpError> {
        let entries = self.entries()?;
        match requested {
            Some(id) if !entries.contains_key(id) => {
                let Ok(Some(record)) = previous_session_lifecycle(id) else {
                    return Ok(None);
                };
                let status = record.status();
                Ok(Some(
                    structured_result(
                        peer,
                        format!("session {id} is already {status}; nothing to stop"),
                        serde_json::json!({
                            "status": "already_cleared",
                            "session_status": status,
                            "session_lifecycle_record": record,
                            "cookie_errors": [],
                        }),
                    )
                    .await,
                ))
            }
            None if entries.is_empty() => Ok(Some(
                structured_result(
                    peer,
                    "no session running",
                    serde_json::json!({"status": "none", "cookie_errors": []}),
                )
                .await,
            )),
            Some(_) | None => Ok(None),
        }
    }
    async fn attach(
        &self,
        peer: &rmcp::Peer<rmcp::RoleServer>,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, McpError> {
        let args: SessionAttachArgs =
            serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
                .map_err(|error| {
                    session_attach_error(
                        "invalid_attach_arguments",
                        format!("Invalid session_attach arguments: {error}"),
                    )
                })?;
        if !args.user_authorized {
            return Err(session_attach_error(
                "explicit_authorization_required",
                "session_attach requires explicit user authorization for the selected existing session.",
            ));
        }
        let application = args.application.trim();
        if application.is_empty() || application.chars().count() > 512 {
            return Err(session_attach_error(
                "invalid_authorization_context",
                "application must describe the authorized application or desktop in 1–512 characters.",
            ));
        }
        if session_owner_pid(&args.session_id) != Some(args.owner_pid) {
            return Err(session_attach_error(
                "session_owner_mismatch",
                "owner_pid must match the owner encoded in session_id.",
            ));
        }
        let before = session_owner_identity(args.owner_pid)
            .map_err(|error| session_attach_error("session_owner_unavailable", error))?;
        if before.uid != unsafe { nix::libc::geteuid() } {
            return Err(session_attach_error(
                "session_owner_uid_mismatch",
                "The existing session must belong to the current Unix user.",
            ));
        }
        let listed = if args.owner_pid == std::process::id() {
            self.list(peer).await?
        } else {
            let request =
                serde_json::from_value(serde_json::json!({"name":"session_list", "arguments":{}}))
                    .map_err(|error| {
                        McpError::internal_error(
                            format!("Construct session identity query: {error}"),
                            None,
                        )
                    })?;
            forward_session_call_checked(args.owner_pid, request, Some(before))
                .await?
                .ok_or_else(|| {
                    session_attach_error(
                        "session_owner_unavailable",
                        "The existing owner could not be reached.",
                    )
                })?
        };
        if listed.is_error == Some(true) {
            return Err(session_attach_error(
                "session_identity_query_failed",
                "The existing owner could not report live sessions.",
            ));
        }
        let payload = listed.structured_content.as_ref().ok_or_else(|| {
            session_attach_error(
                "invalid_session_list",
                "The owner returned no structured identity.",
            )
        })?;
        let attached = attached_session_from_owner_list(&args, before, payload)?;
        verify_attached_owner(args.owner_pid, before)?;
        let response = serde_json::json!({
            "status":"attached", "session_id":attached.session_id,
            "owner_pid":attached.owner.pid, "owner_uid":attached.owner.uid,
            "owner_start_ticks":attached.owner.start_ticks,
            "sandbox_pid":attached.sandbox_pid, "workdir":attached.workdir,
            "scope_unit":attached.scope_unit, "requested_application":application,
            "attaching_router_pid":std::process::id(),
            "attachment_owns_lifecycle":false, "new_desktop_created":false,
            "ownership_notice":"The original owner retains lifecycle ownership. Dropping this attachment does not stop the existing session.",
        });
        self.attached
            .write()
            .await
            .insert(args.session_id, attached);
        Ok(structured_result(
            peer,
            "Attached to the authorized existing session; no new desktop was created.",
            response,
        )
        .await)
    }

    async fn list(&self, peer: &rmcp::Peer<rmcp::RoleServer>) -> Result<CallToolResult, McpError> {
        let mut records = Vec::new();
        for (id, service) in self.entries()? {
            // A desktop that exited since its last call is recorded as dead first,
            // so the phase below reports it.
            let _ = service.check_liveness().await;
            match service.registry_phase().await {
                // A start still running is not handed out yet. A failed start is
                // reached by the ID its error gave, and session_stop clears it.
                session_registry::Phase::Starting => {}
                session_registry::Phase::Terminal(status)
                    if status == session_registry::STATUS_START_FAILED => {}
                session_registry::Phase::Terminal(status) => {
                    records.push(serde_json::json!({"session_id":id, "pid":std::process::id(), "status":status, "diagnostic":service.death_record().unwrap_or_default()}));
                }
                session_registry::Phase::Running => {
                    let guard = service.session.read().await;
                    if let Some(session) = guard.as_ref() {
                        records.push(serde_json::json!({
                            "session_id": id, "pid": std::process::id(), "sandbox_pid": session.sandbox_child.id(), "scope_unit":session.scope.unit, "status":"running", "memory_limits":session.scope.report(), "input_log":service.input_log.as_ref().map(|log| &log.path),
                            "workdir": session.host_xdg_dir, "host_shared_dir": session.host_xdg_dir, "width": session.screen_width,
                            "host_write_paths": session.host_write_paths,
                            "height": session.screen_height, "idle_seconds": session.last_activity.elapsed().as_secs(),
                            "memory": session.scope.memory_snapshot(session.sandbox_child.id()).map(SessionMemorySnapshot::as_json),
                        }));
                    }
                }
            }
        }
        Ok(structured_result(
            peer,
            format!(
                "{SESSION_OWNERSHIP_NOTICE}\nkwin-mcp process PID {}. session_list covers only this process; a live session on another same-user process is not listed here, but requests with its ID are forwarded to its owner. Terminal sessions are listed as dead, stopped, expired, or start_blocked; a call with a saved ID reports its lifecycle record when available.\n{}",
                std::process::id(),
                serde_json::to_string(&records).map_err(KwinError::from)?
            ),
            serde_json::json!({
                "sessions": records,
                "server_pid": std::process::id(),
                "scope": "current_process",
                "ownership_notice": SESSION_OWNERSHIP_NOTICE,
            }),
        )
        .await)
    }
    /// Idle time of the least idle running session, or None when no session is
    /// running. A session that died or expired does not count.
    async fn least_idle_running_session(&self) -> Option<Duration> {
        let mut least: Option<Duration> = None;
        for service in self.entries().ok()?.into_values() {
            let _ = service.check_liveness().await;
            if !matches!(
                service.registry_phase().await,
                session_registry::Phase::Running
            ) {
                continue;
            }
            let idle = service
                .session
                .read()
                .await
                .as_ref()
                .map(|session| session.last_activity.elapsed());
            if let Some(idle) = idle {
                least = Some(least.map_or(idle, |least| least.min(idle)));
            }
        }
        least
    }
    async fn shutdown(&self) {
        if let Ok(entries) = self.entries() {
            futures::future::join_all(
                entries
                    .into_values()
                    .map(|session| async move { session.shutdown_cleanup().await }),
            )
            .await;
        }
    }
    async fn reap(self) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Ok(entries) = self.entries() else {
                continue;
            };
            for (_, service) in entries {
                service.expire_if_idle().await;
            }
        }
    }
    fn tools(&self) -> Vec<rmcp::model::Tool> {
        let mut tools = self.routes.list_all();
        for tool in &mut tools {
            if tool.name == "session_start" {
                Arc::make_mut(&mut tool.input_schema)
                    .insert("additionalProperties".to_owned(), serde_json::json!(false));
                continue;
            }
            let schema = Arc::make_mut(&mut tool.input_schema);
            let properties = schema
                .entry("properties")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(properties) = properties.as_object_mut() {
                properties.insert("session_id".to_owned(), serde_json::json!({"type": "string", "description": "Session returned by session_start. Required when several sessions exist. Requests for a live session owned by another same-user kwin-mcp process are forwarded to its owner."}));
            }
            let description = tool.description.take().unwrap_or_default();
            tool.description = Some(
                format!("{description} Pass a session_id from your own session_start or an explicitly authorized session_attach; a live session you created through another local same-user process is forwarded to its owner.")
                    .into(),
            );
        }
        tools.push(rmcp::model::Tool::new(
            "session_list",
            format!("List live isolated sessions in this server process. Retained terminal handles are also included. TTL-expired sessions are listed as expired and can be cleared with session_stop. Session IDs do not transfer to a new process after client migration or restart. If a saved ID is missing, a call with that ID reports structured session_status, the previous lifecycle record when available, and recovery guidance. {SESSION_OWNERSHIP_NOTICE}"),
            Arc::new(
                serde_json::from_value(serde_json::json!({"type": "object", "properties": {}}))
                    .unwrap_or_default(),
            ),
        ));
        tools.push(rmcp::model::Tool::new(
            "session_attach",
            "Attach to a specific already-running kwin-mcp desktop explicitly authorized by the user. Reuses its exact session_id without creating a desktop or transferring lifecycle ownership. owner_pid must match the handle. Set user_authorized only after the user authorizes this existing session; discovery is not authorization. application describes that authorization. Do not stop the attached session unless the user requests it.",
            Arc::new(serde_json::from_value(serde_json::json!({
                "type":"object",
                "properties":{
                    "session_id":{"type":"string","minLength":1},
                    "owner_pid":{"type":"integer","minimum":1,"maximum":4294967295_u64},
                    "user_authorized":{"type":"boolean","const":true},
                    "application":{"type":"string","minLength":1,"maxLength":512}
                },
                "required":["session_id","owner_pid","user_authorized","application"],
                "additionalProperties":false
            })).unwrap_or_default()),
        ));
        tools
    }
}

impl rmcp::ServerHandler for SessionRouter {
    fn get_info(&self) -> ServerInfo {
        let mut info = KwinMcp::new(self.display).get_info();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .enable_logging()
            .build();
        info.instructions = Some(format!(
            "{} Each session_start creates a new desktop; keep its ID. Use session_attach only for an existing session the user explicitly authorized; it creates no desktop and transfers no lifecycle ownership. Same-user local owners receive forwarded calls after PID/UID checks; session_list is process-local. Dead handles cannot resume: errors report lifecycle/recovery details. Clear dead/stopped/expired/start_blocked handles with session_stop, then start anew and relaunch apps.",
            info.instructions.unwrap_or_default()
        ));
        info
    }
    async fn list_tools(
        &self,
        _: Option<rmcp::model::PaginatedRequestParams>,
        _: ToolContext,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        Ok(rmcp::model::ListToolsResult {
            tools: self.tools(),
            next_cursor: None,
            ..Default::default()
        })
    }
    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.tools().into_iter().find(|tool| tool.name == name)
    }
    async fn call_tool(
        &self,
        mut request: rmcp::model::CallToolRequestParams,
        context: ToolContext,
    ) -> Result<CallToolResult, McpError> {
        use tower::{Layer, ServiceExt};
        if request.name == "session_list" {
            return self
                .limit
                .layer(tower::service_fn(
                    |peer| async move { self.list(&peer).await },
                ))
                .oneshot(context.peer)
                .await;
        }
        if request.name == "session_attach" {
            return self.attach(&context.peer, request.arguments.clone()).await;
        }
        let requested_id = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("session_id"))
            .and_then(serde_json::Value::as_str);
        if request.name != "session_start"
            && let Some(id) = requested_id
            && let Some(owner_pid) = session_owner_pid(id)
            && owner_pid != std::process::id()
        {
            // Reserve at stdio receipt, then retain the turn through the reply:
            // independent owner handshakes must not reorder queued input.
            let turn = context.extensions.get::<Arc<InputTurn>>().cloned();
            if let Some(turn) = &turn {
                turn.wait().await;
            }
            let expected = self
                .attached
                .read()
                .await
                .get(id)
                .map(|attached| attached.owner);
            let forwarded = match expected {
                Some(expected) => {
                    forward_session_call_checked(owner_pid, request.clone(), Some(expected)).await?
                }
                None => forward_session_call(owner_pid, request.clone()).await?,
            };
            if let Some(result) = forwarded {
                return Ok(result);
            }
        }
        if request.name == "session_stop"
            && let Some(result) = self.stop_unregistered(requested_id, &context.peer).await?
        {
            return Ok(result);
        }
        let (id, session) = self.resolve(&mut request)?;
        let start = request.name == "session_start";
        let stop = request.name == "session_stop";
        if !start
            && !stop
            && let Some(death) = session.check_liveness().await
        {
            let status = death
                .get("session_status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("dead");
            return Err(McpError::internal_error(
                format!(
                    "Session {id} is {status}: {} (RAM limit {}). Call session_stop, then session_start without session_id; relaunch apps to resume your task.",
                    death["reason"],
                    death["memory_max_bytes"]
                        .as_u64()
                        .map_or_else(|| "unknown".to_owned(), |bytes| format!("{bytes} bytes"))
                ),
                Some(death),
            ));
        }
        if !start && !stop && session.registry_phase().await == session_registry::Phase::Starting {
            return Err(McpError::internal_error(
                format!(
                    "Session {id} is still starting; call the tool again after its session_start returns."
                ),
                Some(
                    serde_json::json!({"reason": "session_starting", "session_id": id, "retry_safe": true}),
                ),
            ));
        }
        let agent_input = input_tool(&request.name);
        // Queued input waits outside the global limit so it cannot starve reads.
        let turn = context.extensions.get::<Arc<InputTurn>>().cloned();
        if let Some(turn) = &turn {
            turn.wait().await;
        }
        let _input = if ordered_tool(&request.name) {
            Some(session.input_gate.clone().lock_owned().await)
        } else {
            None
        };
        if agent_input {
            let recent_viewer_input =
                session
                    .viewer_is_active_with_recent_input()
                    .await
                    .map_err(|error| {
                        McpError::internal_error(
                            format!("Could not check live viewer input activity: {error}"),
                            Some(serde_json::json!({
                                "reason":"viewer_activity_check_failed",
                                "retry_safe":true
                            })),
                        )
                    })?;
            if recent_viewer_input {
                return Err(McpError::internal_error(
                    "Agent input is paused for one second after viewer input to avoid interfering with the user's recent interaction. Retry when the viewer has been idle for one second; the viewer can stay open.",
                    Some(serde_json::json!({
                        "reason":"recent_viewer_input",
                        "retry_safe":true,
                        "quiet_period_ms":VIEWER_INPUT_QUIET_PERIOD.as_millis()
                    })),
                ));
            }
        }
        let execution = session.clone();
        let operation = request.name.to_string();
        let result = {
            let call = self.limit.layer(execution).oneshot(SessionCall {
                request,
                context,
                routes: self.routes.clone(),
            });
            if agent_input {
                // Human input can begin after admission, during a paced type
                // or drag. Cancel the remaining agent events as well as
                // rejecting new calls. Dropping the call releases its held
                // keys/buttons through SentInput before liveness inspection.
                let viewer_activity = async {
                    loop {
                        tokio::time::sleep(VIEWER_INPUT_CHECK_INTERVAL).await;
                        match session.viewer_input_is_recent().await {
                            Ok(false) => {}
                            Ok(true) => {
                                break McpError::internal_error(
                                    "Viewer input interrupted the agent operation. Some input may already have arrived; inspect the target before retrying.",
                                    Some(
                                        serde_json::json!({"reason":"viewer_input_interrupted", "retry_safe":false, "submission_complete":false}),
                                    ),
                                );
                            }
                            Err(error) => {
                                break McpError::internal_error(
                                    format!(
                                        "Stopped agent input because viewer activity could not be checked: {error}. Some input may already have arrived; inspect before retrying."
                                    ),
                                    Some(
                                        serde_json::json!({"reason":"viewer_activity_check_failed", "retry_safe":false, "submission_complete":false}),
                                    ),
                                );
                            }
                        }
                    }
                };
                tokio::select! {
                    result = call => result,
                    error = viewer_activity => Err(error),
                }
            } else {
                call.await
            }
        };
        let result = if !start && !stop && result.is_err() {
            match session.check_liveness().await {
                Some(mut death) => result.map_err(|error| {
                    if matches!(operation.as_str(), "keyboard_type" | "keyboard_paste")
                        && let Some(data) = death.as_object_mut()
                    {
                            data.insert("typing_error".to_owned(), serde_json::json!(error.message));
                            data.insert("typing_error_data".to_owned(), error.data.clone().unwrap_or_default());
                            if let Some(original) = error.data.as_ref().and_then(serde_json::Value::as_object) {
                                for (key, value) in original {
                                    data.entry(key.clone()).or_insert_with(|| value.clone());
                                }
                            }
                            data.insert("retry_safe".to_owned(), serde_json::json!(false));
                    }
                    let original = if matches!(operation.as_str(), "keyboard_type" | "keyboard_paste") {
                        format!(". Typing report: {}. Some input may have arrived; do not retype blindly", error.message)
                    } else { String::new() };
                    McpError::internal_error(
                        format!("Session {id} died: {}{original}; call session_stop then session_start without session_id to resume in a new desktop", death["reason"]),
                        Some(death),
                    )
                }),
                None => result.map_err(|error| describe_channel_failure(error, &id, &operation)),
            }
        } else {
            result
        };
        // A failed start registers its handle, which stays until session_stop
        // clears it, or is forgotten when nothing was created. A stop removes its
        // handle once the stop finished, unless a blocked startup still owns cleanup.
        let failed_start = if start {
            result
                .as_ref()
                .err()
                .map(|error| session.register_failed_start(error))
        } else {
            None
        };
        let stop_finished = stop
            && result.as_ref().is_ok_and(|done| {
                done.structured_content
                    .as_ref()
                    .and_then(|data| data["status"].as_str())
                    != Some("cleanup_pending")
            });
        if (stop_finished || failed_start == Some(session_registry::FailedStart::Forget))
            && let Ok(mut sessions) = self.sessions.lock()
        {
            sessions.remove(&id);
        }
        let kept_failed_start = failed_start
            .as_ref()
            .is_some_and(|outcome| *outcome != session_registry::FailedStart::Forget);
        let mut result = match result {
            Ok(result) => result,
            Err(mut error) => {
                if kept_failed_start {
                    error.message =
                        format!("{}; retry session_stop with session_id={id}", error.message)
                            .into();
                    let mut data = error
                        .data
                        .take()
                        .and_then(|data| data.as_object().cloned())
                        .unwrap_or_default();
                    data.insert("session_id".to_owned(), serde_json::json!(id));
                    data.insert(
                        "input_log".to_owned(),
                        serde_json::json!(session.input_log.as_ref().map(|log| &log.path)),
                    );
                    error.data = Some(serde_json::Value::Object(data));
                }
                return Err(error);
            }
        };
        if start {
            if let Some(data) = result
                .structured_content
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                data.insert("session_id".to_owned(), serde_json::json!(id));
                data.insert(
                    "input_log".to_owned(),
                    serde_json::json!(session.input_log.as_ref().map(|log| &log.path)),
                );
            }
            result.content.push(Content::text(format!(
                "session_id: {id}\ninput_log: {}",
                session
                    .input_log
                    .as_ref()
                    .map(|log| log.path.display().to_string())
                    .unwrap_or_default()
            )));
        }
        Ok(result)
    }
}

#[derive(Default)]
struct InputOrder {
    counters: std::sync::Mutex<(u64, u64, std::collections::BTreeSet<u64>)>,
    changed: tokio::sync::Notify,
}
struct InputTurn {
    order: Arc<InputOrder>,
    number: u64,
}
impl InputOrder {
    fn enqueue(self: &Arc<Self>) -> Arc<InputTurn> {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let number = counters.0;
        counters.0 += 1;
        Arc::new(InputTurn {
            order: self.clone(),
            number,
        })
    }
}
impl InputTurn {
    async fn wait(&self) {
        loop {
            let changed = self.order.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .order
                .counters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .1
                == self.number
            {
                return;
            }
            changed.await;
        }
    }
}
impl Drop for InputTurn {
    fn drop(&mut self) {
        let mut counters = self
            .order
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counters.2.insert(self.number);
        loop {
            let current = counters.1;
            if !counters.2.remove(&current) {
                break;
            }
            counters.1 += 1;
        }
        drop(counters);
        self.order.changed.notify_waiters();
    }
}

#[derive(Default)]
struct WireState {
    input: Vec<u8>,
    incoming: std::collections::HashSet<rmcp::model::RequestId>,
    outgoing: std::collections::HashSet<rmcp::model::RequestId>,
    paused: bool,
    reading: bool,
    parsing: bool,
}
#[derive(Clone)]
struct SessionStdio {
    client: Arc<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    reader: Arc<tokio::io::unix::AsyncFd<std::fs::File>>,
    writer: Arc<tokio::sync::Mutex<tokio::io::unix::AsyncFd<std::fs::File>>>,
    state: Arc<std::sync::Mutex<WireState>>,
    changed: Arc<tokio::sync::Notify>,
    router: SessionRouter,
}
impl SessionStdio {
    fn new(
        router: SessionRouter,
        input: Vec<u8>,
        client: std::os::fd::OwnedFd,
    ) -> std::io::Result<Self> {
        use std::os::fd::{AsFd, BorrowedFd};
        let descriptor = |number| -> std::io::Result<tokio::io::unix::AsyncFd<std::fs::File>> {
            let descriptor = unsafe { BorrowedFd::borrow_raw(number) }.try_clone_to_owned()?;
            let flags = nix::fcntl::fcntl(descriptor.as_fd(), nix::fcntl::FcntlArg::F_GETFL)
                .map_err(std::io::Error::other)?;
            let flags =
                nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
            nix::fcntl::fcntl(descriptor.as_fd(), nix::fcntl::FcntlArg::F_SETFL(flags))
                .map_err(std::io::Error::other)?;
            let name = match number {
                0 => "stdin",
                1 => "stdout",
                _ => "stdio",
            };
            tokio::io::unix::AsyncFd::new(std::fs::File::from(descriptor)).map_err(|error| {
                let detail = if error.raw_os_error() == Some(nix::libc::EPERM) {
                    format!(
                        "MCP {name} (fd {number}) is not pollable by epoll; connect stdio through a pipe or terminal: {error}"
                    )
                } else {
                    format!("cannot monitor MCP {name} (fd {number}) with epoll: {error}")
                };
                std::io::Error::new(error.kind(), detail)
            })
        };
        Ok(Self {
            client: Arc::new(tokio::io::unix::AsyncFd::new(client)?),
            reader: Arc::new(descriptor(0)?),
            writer: Arc::new(tokio::sync::Mutex::new(descriptor(1)?)),
            state: Arc::new(std::sync::Mutex::new(WireState {
                input,
                ..Default::default()
            })),
            changed: Arc::new(tokio::sync::Notify::new()),
            router,
        })
    }
    async fn freeze(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paused = true;
        self.changed.notify_waiters();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let empty = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.incoming.is_empty()
                    && state.outgoing.is_empty()
                    && !state.reading
                    && !state.parsing
            };
            if empty {
                return;
            }
            changed.await;
        }
    }
    fn thaw(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paused = false;
        self.changed.notify_waiters();
    }
}
impl rmcp::transport::Transport<rmcp::RoleServer> for SessionStdio {
    type Error = std::io::Error;
    fn send(
        &mut self,
        item: rmcp::model::ServerJsonRpcMessage,
    ) -> impl std::future::Future<Output = std::io::Result<()>> + Send + 'static {
        let wire = self.clone();
        async move {
            let mut bytes = serde_json::to_vec(&item)?;
            bytes.push(b'\n');
            let writer = wire.writer.lock().await;
            let mut offset = 0;
            while offset < bytes.len() {
                let written = writer
                    .async_io(tokio::io::Interest::WRITABLE, |mut file| {
                        std::io::Write::write(&mut file, &bytes[offset..])
                    })
                    .await?;
                if written == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "MCP stdout closed",
                    ));
                }
                offset += written;
            }
            let mut state = wire
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match item {
                rmcp::model::JsonRpcMessage::Response(response) => {
                    state.incoming.remove(&response.id);
                }
                rmcp::model::JsonRpcMessage::Error(error) => {
                    state.incoming.remove(&error.id);
                }
                rmcp::model::JsonRpcMessage::Request(request) => {
                    state.outgoing.insert(request.id);
                }
                rmcp::model::JsonRpcMessage::Notification(_) => {}
            }
            drop(state);
            wire.changed.notify_waiters();
            Ok(())
        }
    }
    async fn receive(&mut self) -> Option<rmcp::model::ClientJsonRpcMessage> {
        use rmcp::model::GetExtensions;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (paused, line) = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let paused = state.paused;
                let line = if paused {
                    None
                } else {
                    state
                        .input
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map(|end| state.input.drain(..=end).collect::<Vec<_>>())
                };
                state.parsing = line.is_some();
                (paused, line)
            };
            if paused {
                changed.await;
                continue;
            }
            if let Some(line) = line {
                let mut message =
                    match serde_json::from_slice::<rmcp::model::ClientJsonRpcMessage>(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            self.state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .parsing = false;
                            self.changed.notify_waiters();
                            eprintln!("MCP invalid message: {error}");
                            continue;
                        }
                    };
                {
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.parsing = false;
                    self.changed.notify_waiters();
                    // A freeze that raced line parsing must retain the whole request.
                    if state.paused {
                        let mut input = line;
                        input.append(&mut state.input);
                        state.input = input;
                        continue;
                    }
                    match &mut message {
                        rmcp::model::JsonRpcMessage::Request(request) => {
                            state.incoming.insert(request.id.clone());
                            if let rmcp::model::ClientRequest::CallToolRequest(call) =
                                &mut request.request
                                && ordered_tool(&call.params.name)
                            {
                                let mut params = call.params.clone();
                                if let Ok((_, session)) = self.router.resolve(&mut params) {
                                    call.extensions_mut().insert(session.input_order.enqueue());
                                } else if let Some(id) = call
                                    .params
                                    .arguments
                                    .as_ref()
                                    .and_then(|args| args.get("session_id"))
                                    .and_then(serde_json::Value::as_str)
                                    && session_owner_pid(id)
                                        .is_some_and(|pid| pid != std::process::id())
                                {
                                    let mut orders = self
                                        .router
                                        .remote_input_orders
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                                    orders.retain(|_, order| order.strong_count() > 0);
                                    let order = orders
                                        .get(id)
                                        .and_then(std::sync::Weak::upgrade)
                                        .unwrap_or_else(|| Arc::new(InputOrder::default()));
                                    orders.insert(id.to_owned(), Arc::downgrade(&order));
                                    call.extensions_mut().insert(order.enqueue());
                                }
                            }
                        }
                        rmcp::model::JsonRpcMessage::Response(response) => {
                            state.outgoing.remove(&response.id);
                        }
                        rmcp::model::JsonRpcMessage::Error(error) => {
                            state.outgoing.remove(&error.id);
                        }
                        rmcp::model::JsonRpcMessage::Notification(_) => {}
                    }
                }
                self.changed.notify_waiters();
                return Some(message);
            }
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.paused {
                    continue;
                }
                state.reading = true;
            }
            let mut bytes = [0u8; 8192];
            let read = tokio::select! {
                read = self.reader.async_io(tokio::io::Interest::READABLE, |mut file| std::io::Read::read(&mut file, &mut bytes)) => Some(read),
                _ = &mut changed => None,
            };
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.reading = false;
                if let Some(Ok(count)) = &read {
                    state.input.extend_from_slice(&bytes[..*count]);
                }
            }
            self.changed.notify_waiters();
            match read {
                Some(Ok(0)) => return None,
                Some(Ok(_)) | None => {}
                Some(Err(error)) => {
                    eprintln!("MCP stdin: {error}");
                    return None;
                }
            }
        }
    }
    async fn close(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const RESUME_DESCRIPTOR: &str = "KWIN_MCP_RESUME_FD";

#[derive(Serialize, Deserialize)]
struct SessionRestore {
    #[serde(default)]
    host_write_paths: Vec<PathBuf>,
    #[serde(default)]
    scope: SessionScope,
    #[serde(default)]
    scope_unit: String,
    id: String,
    #[serde(default)]
    input_history: InputHistory,
    #[serde(default)]
    input_log_path: Option<PathBuf>,
    path: PathBuf,
    sandbox_pid: u32,
    stdin_fd: i32,
    mouse_fd: i32,
    keyboard_fd: i32,
    proxy_fd: i32,
    proxy_guid: String,
    proxy_name: String,
    mouse_sysname: String,
    kbd_sysname: String,
    proxies: Vec<u32>,
    viewer: Option<u32>,
    kwin_unique: String,
    service_bus: String,
    atspi_bus: String,
    width: u32,
    height: u32,
    cdp_port: u16,
    overlay_work_paths: Vec<PathBuf>,
    #[serde(default)]
    cookie_roots: Vec<kwin_mcp::browser_cookies::CookieRoot>,
    #[serde(default)]
    cookie_errors: Vec<String>,
    oversized_files: Vec<PathBuf>,
    socket_links: Vec<(PathBuf, PathBuf)>,
    idle_millis: u64,
    input_millis: Option<u64>,
    wallet: Option<wallet_mediator::State>,
    wallet_note: Option<String>,
    fuse_enabled: bool,
    lease_fd: Option<i32>,
}
#[derive(Serialize, Deserialize)]
struct ServerRestore {
    #[serde(default)]
    deaths: Vec<(String, serde_json::Value)>,
    #[serde(default)]
    client_fd: Option<i32>,
    #[serde(default)]
    clipboards: Vec<ClipboardRestore>,
    pid: u32,
    client: Option<rmcp::model::ClientInfo>,
    next: u64,
    next_launch: u64,
    sessions: Vec<SessionRestore>,
    cleanup: Vec<(String, PathBuf, Option<i32>)>,
    input: Vec<u8>,
}

fn retain_descriptor(
    fd: impl std::os::fd::AsFd,
    retained: &mut Vec<std::os::fd::OwnedFd>,
) -> std::io::Result<i32> {
    use std::os::fd::AsRawFd;
    let copy = fd.as_fd().try_clone_to_owned()?;
    nix::fcntl::fcntl(
        &copy,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
    )
    .map_err(std::io::Error::other)?;
    let number = copy.as_raw_fd();
    retained.push(copy);
    Ok(number)
}
fn adopt_descriptor(number: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{BorrowedFd, FromRawFd};
    if number <= 2 {
        return Err(std::io::Error::other(
            "invalid inherited session descriptor",
        ));
    }
    let descriptor = unsafe { BorrowedFd::borrow_raw(number) };
    nix::fcntl::fcntl(
        descriptor,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map_err(std::io::Error::other)?;
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(number) })
}
fn restore_state() -> anyhow::Result<Option<ServerRestore>> {
    let Some(number) = std::env::var_os(RESUME_DESCRIPTOR) else {
        return Ok(None);
    };
    let number: i32 = number.to_string_lossy().parse()?;
    let mut file = std::fs::File::from(adopt_descriptor(number)?);
    use std::io::Seek;
    file.rewind()?;
    let state: ServerRestore = serde_json::from_reader(file)?;
    anyhow::ensure!(
        state.pid == std::process::id(),
        "reload state belongs to another server PID"
    );
    Ok(Some(state))
}
impl SessionRouter {
    async fn reattach(&self, state: ServerRestore) -> anyhow::Result<Vec<u8>> {
        self.next
            .store(state.next, std::sync::atomic::Ordering::Relaxed);
        NEXT_BROWSER_LAUNCH.store(state.next_launch, std::sync::atomic::Ordering::Relaxed);
        for clipboard in state.clipboards {
            self.clipboards
                .lock()
                .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
                .write(
                    clipboard.socket,
                    "restored",
                    clipboard.text,
                    clipboard.secret,
                    clipboard.primary,
                )?;
        }
        for saved in state.sessions {
            let mut service = KwinMcp::new(self.display);
            service.session_id = saved.id.clone();
            service.clipboards = self.clipboards.clone();
            service.input_log = Some(InputLog::open(
                &saved.id,
                saved.input_history,
                saved.input_log_path,
            )?);
            service.path = saved.path.clone();
            // Preserve the input-device bus identity so KWin's queued name does
            // not replace the session's existing proxy during re-execution.
            let proxy_socket = adopt_descriptor(saved.proxy_fd)?;
            let socket = std::os::unix::net::UnixStream::from(proxy_socket.try_clone()?);
            socket.set_nonblocking(true)?;
            let socket = tokio::net::UnixStream::from_std(socket)?;
            let proxy_conn =
                zbus::connection::Builder::authenticated_socket(socket, saved.proxy_guid.as_str())?
                    .p2p()
                    .unique_name(saved.proxy_name.as_str())?
                    .build()
                    .await?;
            input_bridge::register_devices(
                &proxy_conn,
                vec![
                    input_bridge::InputDevice::new_pointer(saved.mouse_sysname.clone()),
                    input_bridge::InputDevice::new_keyboard(saved.kbd_sysname.clone()),
                ],
            )
            .await?;
            let address = format!("unix:path={}", saved.path.join("bus").display());
            let kwin_conn = zbus::connection::Builder::address(address.as_str())?
                .build()
                .await?;
            let eis_proxy = KWinEisProxy::builder(&kwin_conn)
                .destination(saved.kwin_unique.as_str())?
                .build()
                .await?;
            let (fd, _) = eis_proxy.connect_to_eis(EIS_CAPS_KBD_POINTER).await?;
            let mut eis = tokio::task::spawn_blocking(move || Eis::from_fd(fd.into())).await??;
            eis.audit = service.input_log.clone();
            let wallet = match saved.wallet {
                Some(state) => {
                    let address = format!("unix:path={}", saved.path.join("kwallet_bus").display());
                    Some(wallet_mediator::WalletMediator::start_state(&address, state).await?)
                }
                None => None,
            };
            if saved.lease_fd.is_some() {
                service.workdir.inherit_lease(&saved.path, saved.lease_fd)?;
            }
            *service.session.write().await = Some(Session {
                host_write_paths: saved.host_write_paths,
                scope: {
                    let mut scope = saved.scope;
                    if scope.unit.is_empty() {
                        scope.unit = saved.scope_unit;
                    }
                    scope.recover(saved.sandbox_pid)
                },
                kwin_conn,
                _proxy_conn: proxy_conn,
                _proxy_socket: proxy_socket,
                kwin_unique_name: saved.kwin_unique,
                service_bus_address: saved.service_bus,
                atspi_bus_address: saved.atspi_bus,
                eis,
                sandbox_child: SessionProcess {
                    pid: saved.sandbox_pid,
                    status: None,
                },
                sandbox_stdin: adopt_descriptor(saved.stdin_fd)?,
                host_xdg_dir: saved.path,
                _uinput_mouse: adopt_descriptor(saved.mouse_fd)?,
                _uinput_keyboard: adopt_descriptor(saved.keyboard_fd)?,
                mouse_sysname: saved.mouse_sysname,
                kbd_sysname: saved.kbd_sysname,
                cdp_browser: None,
                cdp_forward_port: saved.cdp_port,
                service_proxy_children: saved
                    .proxies
                    .into_iter()
                    .map(|pid| SessionProcess { pid, status: None })
                    .collect(),
                viewer_child: saved.viewer.map(|pid| SessionProcess { pid, status: None }),
                viewer_unavailable: None,
                overlay_work_paths: saved.overlay_work_paths,
                cookie_roots: saved.cookie_roots,
                cookie_errors: saved.cookie_errors,
                oversized_read_only_files: saved.oversized_files,
                _socket_links: SocketLinks(saved.socket_links),
                screen_width: saved.width,
                screen_height: saved.height,
                last_activity: std::time::Instant::now()
                    .checked_sub(Duration::from_millis(saved.idle_millis))
                    .unwrap_or_else(std::time::Instant::now),
                last_input: saved.input_millis.and_then(|millis| {
                    std::time::Instant::now().checked_sub(Duration::from_millis(millis))
                }),
                wallet,
                wallet_note: saved.wallet_note,
                fuse_enabled: saved.fuse_enabled,
            });
            self.sessions
                .lock()
                .map_err(|_| anyhow::anyhow!("session map lock poisoned"))?
                .insert(saved.id, service);
        }
        for (id, path, lease) in state.cleanup {
            let mut service = KwinMcp::new(self.display);
            service.path = path;
            service.workdir.inherit_lease(&service.path, lease)?;
            self.sessions
                .lock()
                .map_err(|_| anyhow::anyhow!("session map lock poisoned"))?
                .insert(id, service);
        }
        for (id, death) in state.deaths {
            let service = {
                let mut entries = self
                    .sessions
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session map lock poisoned"))?;
                entries
                    .entry(id.clone())
                    .or_insert_with(|| {
                        let mut service = KwinMcp::new(self.display);
                        service.session_id = id;
                        service
                    })
                    .clone()
            };
            service.set_death(death);
        }
        eprintln!(
            "reload: reattached {} sessions in PID {}",
            self.entries()?.len(),
            std::process::id()
        );
        Ok(state.input)
    }
    async fn reexecute(
        &self,
        wire: &SessionStdio,
        client: Option<rmcp::model::ClientInfo>,
        executable: &Path,
    ) -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;
        wire.freeze().await;
        let mut retained = Vec::new();
        let mut saved = Vec::new();
        let mut cleanup = Vec::new();
        let mut deaths = Vec::new();
        let mut gates = Vec::new();
        for (id, service) in self.entries()? {
            gates.push(service.start_gate.clone().lock_owned().await);
            if let Some(death) = service.death_record() {
                deaths.push((id.clone(), death));
            }
            let guard = service.session.read().await;
            let Some(session) = guard.as_ref() else {
                // Holding the start gate means no start is in flight, so a handle
                // with neither a desktop nor a status failed without recording it.
                if service.death_record().is_none() {
                    deaths.push((
                        id.clone(),
                        terminal_record(
                            session_registry::STATUS_START_FAILED,
                            SESSION_LIFECYCLE_REASON_START_FAILED,
                        ),
                    ));
                }
                if let Some(path) = service.workdir.owned() {
                    cleanup.push((id, path, service.workdir.saved_lease(&mut retained)?));
                }
                continue;
            };
            saved.push(SessionRestore {
                host_write_paths: session.host_write_paths.clone(),
                scope_unit: session.scope.unit.clone(),
                scope: session.scope.clone(),
                input_log_path: service.input_log.as_ref().map(|log| log.path.clone()),
                input_history: service
                    .input_log
                    .as_ref()
                    .map(|log| log.history())
                    .unwrap_or_default(),
                id,
                path: service.path.clone(),
                sandbox_pid: session.sandbox_child.id(),
                stdin_fd: retain_descriptor(&session.sandbox_stdin, &mut retained)?,
                mouse_fd: retain_descriptor(&session._uinput_mouse, &mut retained)?,
                keyboard_fd: retain_descriptor(&session._uinput_keyboard, &mut retained)?,
                proxy_fd: retain_descriptor(&session._proxy_socket, &mut retained)?,
                proxy_guid: session._proxy_conn.server_guid().to_string(),
                proxy_name: session
                    ._proxy_conn
                    .unique_name()
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
                mouse_sysname: session.mouse_sysname.clone(),
                kbd_sysname: session.kbd_sysname.clone(),
                proxies: session
                    .service_proxy_children
                    .iter()
                    .map(SessionProcess::id)
                    .collect(),
                viewer: session.viewer_child.as_ref().map(SessionProcess::id),
                kwin_unique: session.kwin_unique_name.clone(),
                service_bus: session.service_bus_address.clone(),
                atspi_bus: session.atspi_bus_address.clone(),
                width: session.screen_width,
                height: session.screen_height,
                cdp_port: session.cdp_forward_port,
                overlay_work_paths: session.overlay_work_paths.clone(),
                cookie_roots: session.cookie_roots.clone(),
                cookie_errors: session.cookie_errors.clone(),
                oversized_files: session.oversized_read_only_files.clone(),
                socket_links: session._socket_links.0.clone(),
                idle_millis: u64::try_from(session.last_activity.elapsed().as_millis())?,
                input_millis: session
                    .last_input
                    .map(|input| u64::try_from(input.elapsed().as_millis()))
                    .transpose()?,
                wallet: session
                    .wallet
                    .as_ref()
                    .map(wallet_mediator::WalletMediator::saved_state)
                    .transpose()?,
                wallet_note: session.wallet_note.clone(),
                fuse_enabled: session.fuse_enabled,
                lease_fd: service.workdir.saved_lease(&mut retained)?,
            });
        }
        let input = wire
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .input
            .clone();
        let clipboard_state = self
            .clipboards
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard store lock poisoned"))?
            .saved()?;
        let state = ServerRestore {
            deaths,
            client_fd: Some(retain_descriptor(wire.client.get_ref(), &mut retained)?),
            clipboards: clipboard_state,
            pid: std::process::id(),
            client,
            next: self.next.load(std::sync::atomic::Ordering::Relaxed),
            next_launch: NEXT_BROWSER_LAUNCH.load(std::sync::atomic::Ordering::Relaxed),
            sessions: saved,
            cleanup,
            input,
        };
        // Wallet and clipboard data stay in an anonymous descriptor, never in a workdir,
        // environment variable, log, or command line.
        let descriptor =
            nix::sys::memfd::memfd_create(c"kwin-mcp-reload", nix::sys::memfd::MFdFlags::empty())?;
        let mut file = std::fs::File::from(descriptor);
        serde_json::to_writer(&mut file, &state)?;
        file.flush()?;
        eprintln!(
            "reload: re-executing PID {} with {} live sessions",
            std::process::id(),
            state.sessions.len()
        );
        let mut command = std::process::Command::new(executable);
        command
            .args(std::env::args_os().skip(1))
            .env(RESUME_DESCRIPTOR, file.as_raw_fd().to_string());
        let error = exec_on_main(command).await?;
        wire.thaw();
        Err(error.into())
    }
}

fn source_stamp(root: &Path) -> Option<std::time::SystemTime> {
    fn latest(path: &Path) -> Option<std::time::SystemTime> {
        if path.is_file() {
            return std::fs::metadata(path).ok()?.modified().ok();
        }
        std::fs::read_dir(path)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| latest(&entry.path()))
            .max()
    }
    ["src", "data", "Cargo.toml", "Cargo.lock", "build.rs"]
        .into_iter()
        .filter_map(|path| latest(&root.join(path)))
        .max()
}
fn binary_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}
async fn watch_reload(
    router: SessionRouter,
    wire: SessionStdio,
    peer: rmcp::Peer<rmcp::RoleServer>,
) {
    let executable = std::env::current_exe().unwrap_or_default();
    let executable = std::env::var_os("KWIN_MCP_BINARY")
        .map(PathBuf::from)
        .unwrap_or(executable);
    let inferred = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|root| root.join("Cargo.toml").is_file())
        .map(Path::to_owned);
    let root = std::env::var_os("KWIN_MCP_REPO")
        .map(PathBuf::from)
        .or(inferred);
    let mut source = root.as_deref().and_then(source_stamp);
    let mut binary = binary_stamp(&executable);
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(root) = &root {
            let next = source_stamp(root);
            if next != source {
                tokio::time::sleep(Duration::from_secs(2)).await;
                source = source_stamp(root);
                let log_path = session_disk_root().join("build.log");
                let result = async {
                    std::fs::create_dir_all(session_disk_root())?;
                    let log = std::fs::File::create(&log_path)?;
                    let mut command = tokio::process::Command::new(
                        std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()),
                    );
                    command
                        .current_dir(root)
                        .args(["build", "--bin", "kwin-mcp"]);
                    if executable
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == "release")
                    {
                        command.arg("--release");
                    }
                    command
                        .stdout(log.try_clone()?)
                        .stderr(log)
                        .kill_on_drop(true);
                    command.status().await
                }
                .await;
                match result {
                    Ok(status) if status.success() => eprintln!("reload: build succeeded"),
                    Ok(status) => {
                        eprintln!(
                            "reload: build failed ({status}); see {}",
                            log_path.display()
                        );
                        continue;
                    }
                    Err(error) => {
                        eprintln!("reload: build failed: {error}");
                        continue;
                    }
                }
            }
        }
        let next = binary_stamp(&executable);
        if next.is_some() && next != binary {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if binary_stamp(&executable) != next {
                continue;
            }
            if let Err(error) = router
                .reexecute(&wire, peer.peer_info().cloned(), &executable)
                .await
            {
                wire.thaw();
                eprintln!("reload: re-execution failed: {error}");
            }
            binary = next;
        }
    }
}

/// A pidfd watches the process, avoids PID reuse, and can be retained across
/// hot reload even when another descendant keeps the client's stdin open.
fn client_pidfd() -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let parent = nix::unistd::getppid();
    if parent.as_raw() <= 1 {
        return Err(std::io::Error::other(
            "no live launching client for this stdio server",
        ));
    }
    // SAFETY: pidfd_open takes a PID and flags, with no pointer arguments.
    let descriptor = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, parent.as_raw(), 0) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let descriptor = i32::try_from(descriptor).map_err(std::io::Error::other)?;
    // SAFETY: pidfd_open returned a new descriptor, owned by this process.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) };
    if nix::unistd::getppid() != parent {
        return Err(std::io::Error::other(
            "launching client exited during watcher setup",
        ));
    }
    Ok(owned)
}

async fn run_server(restored: Option<ServerRestore>) -> Result<(), Box<dyn std::error::Error>> {
    let client = match restored.as_ref().and_then(|state| state.client_fd) {
        Some(descriptor) => adopt_descriptor(descriptor)?,
        None => client_pidfd()?,
    };
    unsafe {
        nix::libc::signal(nix::libc::SIGPIPE, nix::libc::SIG_IGN);
    }
    let (process_requests, mut process_commands) = tokio::sync::mpsc::unbounded_channel();
    PROCESS_REQUESTS
        .set(process_requests)
        .map_err(|_| "session process owner already exists")?;
    let display = parse_cli_args()?;
    let resumed = restored.is_some();
    let resumed_client = restored.as_ref().and_then(|state| state.client.clone());
    if display.autoclean
        && let Err(error) = sweep_orphaned_workdirs(None)
    {
        eprintln!("autoclean: orphan sweep skipped: {error}");
    }
    // A disk twin whose server died without a lease, or whose /tmp workdir is
    // gone, is not reached by the leased sweep above. Scanning every process
    // command line is slow on a loaded host, so it stays off the startup path.
    if display.autoclean {
        std::thread::spawn(|| {
            if let Err(error) = sweep_dead_owner_workdirs(None) {
                eprintln!("autoclean: leaked workdir sweep skipped: {error}");
            }
        });
    }
    eprintln!(
        "kwin-mcp: display default {}x{}{}; viewer on demand",
        display.width,
        display.height,
        if display.locked {
            " (locked, --no-override)"
        } else {
            ""
        },
    );
    let keep_sessions = display.keep_sessions;
    let router = SessionRouter::new(display);
    let shutdown = router.clone();
    let input = match restored {
        Some(state) => router.reattach(state).await?,
        None => Vec::new(),
    };
    sweep_stale_control_sockets();
    let session_control = match SessionControlSocket::bind(std::process::id()) {
        Ok(socket) => Some(tokio::spawn(socket.serve(router.clone()))),
        Err(error) => {
            eprintln!("session control: cross-process routing unavailable: {error}");
            None
        }
    };
    let transport = SessionStdio::new(router.clone(), input, client)?;
    let wire = transport.clone();
    let service = match resumed_client {
        Some(client) => rmcp::service::serve_directly(router, transport, Some(client)),
        None => tokio::select! {
            result = router.serve(transport) => result?,
            _ = wire.client.readable() => {
                shutdown.shutdown().await;
                return Ok(());
            },
        },
    };
    if resumed {
        service.peer().notify_tool_list_changed().await?;
    }
    let reload = tokio::spawn(watch_reload(
        shutdown.clone(),
        wire.clone(),
        service.peer().clone(),
    ));
    let cancellation = service.cancellation_token();
    let reaper = tokio::spawn(shutdown.clone().reap());
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let waited = {
        let wait_future = service.waiting();
        tokio::pin!(wait_future);
        loop {
            tokio::select! {
                result = &mut wait_future => break Some(result),
                _ = sigterm.recv() => { eprintln!("shutdown: SIGTERM"); break None },
                _ = sigint.recv() => { eprintln!("shutdown: SIGINT"); break None },
                _ = sighup.recv() => { eprintln!("shutdown: SIGHUP"); break None },
                result = wire.client.readable() => {
                    if let Err(error) = result { eprintln!("client lifetime watcher: {error}"); }
                    eprintln!("shutdown: launching client exited; stopping owned session scopes");
                    break None;
                },
                request = process_commands.recv() => match request {
                    Some(request) => request.execute(),
                    None => {
                        eprintln!("shutdown: the process command channel closed");
                        break None;
                    },
                },
            }
        }
    };
    // Say why the server is going away: a host that respawns it looks the same
    // as a crash from the client side, and every owned session goes with it.
    match &waited {
        Some(Ok(reason)) => eprintln!("shutdown: {}", quit_cause(reason)),
        Some(Err(error)) => eprintln!("shutdown: the MCP service task failed: {error}"),
        None => {}
    }
    // The client closing is how a host respawns this server. With --keep-sessions
    // the running sessions outlive it, served through the control socket to
    // whichever kwin-mcp the host starts next (session IDs name this process, and
    // calls with them are forwarded here). A signal, or the launching client
    // exiting, still ends everything at once.
    if let Some(grace) = keep_sessions
        && matches!(&waited, Some(Ok(rmcp::service::QuitReason::Closed)))
        && session_control.is_some()
        && let Some(idle) = shutdown.least_idle_running_session().await
    {
        reload.abort();
        eprintln!(
            "shutdown: keeping running sessions for forwarded requests until they have been idle {} minute(s) (idle now: {} s)",
            grace.as_secs() / 60,
            idle.as_secs()
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    match shutdown.least_idle_running_session().await {
                        None => {
                            eprintln!("shutdown: no running session remains");
                            break;
                        }
                        Some(idle) if idle >= grace => {
                            eprintln!("shutdown: sessions idle for {} s, past --keep-sessions", idle.as_secs());
                            break;
                        }
                        Some(_) => {}
                    }
                },
                _ = sigterm.recv() => { eprintln!("shutdown: SIGTERM"); break },
                _ = sigint.recv() => { eprintln!("shutdown: SIGINT"); break },
                _ = sighup.recv() => { eprintln!("shutdown: SIGHUP"); break },
                result = wire.client.readable() => {
                    if let Err(error) = result { eprintln!("client lifetime watcher: {error}"); }
                    eprintln!("shutdown: launching client exited; stopping owned session scopes");
                    break;
                },
                request = process_commands.recv() => match request {
                    Some(request) => request.execute(),
                    None => break,
                },
            }
        }
    }
    if let Some(session_control) = session_control {
        session_control.abort();
        let _ = session_control.await;
    }
    drop(process_commands);
    if waited.is_none() {
        cancellation.cancel();
    }
    reload.abort();
    let _ = reload.await;
    reaper.abort();
    let _ = reaper.await;
    shutdown.shutdown().await;
    if let Some(result) = waited {
        result?;
    }
    Ok(())
}

fn quit_cause(reason: &rmcp::service::QuitReason) -> String {
    match reason {
        rmcp::service::QuitReason::Closed => {
            "the MCP client closed the connection (stdin reached end of file)".to_owned()
        }
        rmcp::service::QuitReason::Cancelled => "the MCP service was cancelled".to_owned(),
        rmcp::service::QuitReason::JoinError(error) => {
            format!("the MCP service task failed: {error}")
        }
        other => format!("the MCP service ended: {other:?}"),
    }
}

#[cfg(test)]
mod file_content_tests {
    use super::same_reader_contents;
    use std::io::Cursor;

    #[test]
    fn source_eof_does_not_accept_a_longer_copy() -> std::io::Result<()> {
        for (source, copy) in [
            (b"".as_slice(), b"stale copied contents".as_slice()),
            (
                b"same prefix".as_slice(),
                b"same prefix plus stale tail".as_slice(),
            ),
        ] {
            assert!(!same_reader_contents(
                &mut Cursor::new(source),
                &mut Cursor::new(copy),
            )?);
        }
        Ok(())
    }

    #[test]
    fn equal_readers_are_verified() -> std::io::Result<()> {
        for contents in [b"".as_slice(), b"matching file contents".as_slice()] {
            assert!(same_reader_contents(
                &mut Cursor::new(contents),
                &mut Cursor::new(contents),
            )?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod host_wayland_scan_tests {
    use super::live_wayland_socket;
    use std::os::unix::net::UnixListener;

    #[test]
    fn the_live_numbered_socket_wins_over_stale_and_unrelated_names() -> std::io::Result<()> {
        let runtime = std::env::temp_dir().join(format!("kwin-mcp-wayland-scan-{}", std::process::id()));
        std::fs::create_dir_all(&runtime)?;
        // A bound-then-dropped listener leaves a socket file nothing answers on.
        drop(UnixListener::bind(runtime.join("wayland-0"))?);
        let _live = UnixListener::bind(runtime.join("wayland-1"))?;
        std::fs::write(runtime.join("wayland-0.lock"), [])?;
        std::fs::write(runtime.join("wayland-x"), [])?;
        let found = live_wayland_socket(&runtime);
        std::fs::remove_dir_all(&runtime)?;
        assert_eq!(found.as_deref(), Some(std::ffi::OsStr::new("wayland-1")));
        assert_eq!(live_wayland_socket(&runtime), None);
        Ok(())
    }
}

#[cfg(test)]
mod keep_sessions_flag_tests {
    use super::parse_cli_args_from;
    use std::time::Duration;

    fn parse(args: &[&str]) -> Result<Option<Duration>, String> {
        parse_cli_args_from(args.iter().map(|arg| (*arg).to_owned())).map(|cfg| cfg.keep_sessions)
    }

    #[test]
    fn sessions_stop_with_the_client_unless_asked_to_stay() -> Result<(), String> {
        assert_eq!(parse(&[])?, None);
        assert_eq!(
            parse(&["--keep-sessions", "30"])?,
            Some(Duration::from_secs(30 * 60))
        );
        assert!(parse(&["--keep-sessions", "0"]).is_err());
        assert!(parse(&["--keep-sessions"]).is_err());
        assert!(parse(&["--keep-sessions", "soon"]).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod memory_limit_tests {
    use super::{GIB_BYTES, parse_cli_args_from, sandbox_launcher};

    #[test]
    fn default_session_retains_hard_limits_when_throttling_is_disabled() -> Result<(), String> {
        let default = parse_cli_args_from(std::iter::empty())?;
        assert_eq!(default.memory_high, Some(3 * GIB_BYTES));
        assert_eq!(default.memory_max, 4 * GIB_BYTES);
        assert_eq!(default.memory_swap_max, GIB_BYTES);
        let disabled = parse_cli_args_from(
            ["--memory-high", "0", "--memory-swap-max", "0"]
                .map(str::to_owned)
                .into_iter(),
        )?;
        assert_eq!(disabled.memory_high, None);
        assert_eq!(disabled.memory_max, 4 * GIB_BYTES);
        assert_eq!(disabled.memory_swap_max, 0);
        Ok(())
    }

    #[test]
    fn larger_gateway_requires_an_explicit_hard_limit() -> Result<(), String> {
        assert!(
            parse_cli_args_from(["--memory-high", "8"].map(str::to_owned).into_iter()).is_err()
        );
        let gateway = parse_cli_args_from(
            [
                "--memory-high",
                "8",
                "--memory-max",
                "10",
                "--memory-swap-max",
                "6",
            ]
            .map(str::to_owned)
            .into_iter(),
        )?;
        assert_eq!(gateway.memory_high, Some(8 * GIB_BYTES));
        assert_eq!(gateway.memory_max, 10 * GIB_BYTES);
        assert_eq!(gateway.memory_swap_max, 6 * GIB_BYTES);
        Ok(())
    }

    #[test]
    fn memory_limits_reject_zero_hard_limit_missing_values_and_overflow() {
        for values in [
            vec!["--memory-max", "0"],
            vec!["--memory-max"],
            vec!["--memory-swap-max", "18446744073709551615"],
            vec!["--memory-high", "-1"],
        ] {
            assert!(parse_cli_args_from(values.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    #[ignore = "requires a systemd user manager; starts a headless scope, no GUI"]
    fn real_scope_reports_the_configured_ram_and_swap_limits() -> Result<(), String> {
        for values in [
            vec![],
            vec![
                "--memory-high",
                "8",
                "--memory-max",
                "10",
                "--memory-swap-max",
                "6",
            ],
        ] {
            let config = parse_cli_args_from(values.into_iter().map(str::to_owned))?;
            let runtime =
                super::host_environment::runtime_dir().map_err(|error| error.to_string())?;
            let session_bus = super::host_environment::session_bus_address(&runtime)
                .map_err(|error| error.to_string())?;
            let launcher = sandbox_launcher(config, &runtime, &session_bus, None)?;
            let mut args: Vec<_> = launcher.get_args().map(std::ffi::OsStr::to_owned).collect();
            assert_eq!(args.pop().as_deref(), Some(std::ffi::OsStr::new("pasta")));
            // Exercise the same scope properties with a readback process
            // instead of starting a compositor or application.
            let output = std::process::Command::new(launcher.get_program())
                .envs(launcher.get_envs().filter_map(|(key, value)| value.map(|value| (key, value))))
                .args(args)
                .args(["sh", "-c", "cg=$(cut -d: -f3 /proc/self/cgroup); for name in memory.high memory.max memory.swap.max; do cat \"/sys/fs/cgroup$cg/$name\"; done"])
                .output().map_err(|error| error.to_string())?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let actual = String::from_utf8_lossy(&output.stdout);
            let expected = format!(
                "{}\n{}\n{}\n",
                config.memory_high.unwrap_or_default(),
                config.memory_max,
                config.memory_swap_max
            );
            eprintln!("scope limits readback: {actual}");
            assert_eq!(actual, expected);
        }
        Ok(())
    }
}

#[cfg(test)]
mod level_tree_tests {
    use super::{A11Y_TREE_MAX_CHARS, LevelTree, tree_field};

    #[test]
    fn budget_excludes_an_entire_wide_level_and_counts_its_children() {
        let mut tree = LevelTree::default();
        let root = tree.push("root".to_owned(), "root-id".to_owned(), None);
        tree.push("a".repeat(16_000), "a".to_owned(), Some(root));
        tree.push("b".repeat(16_000), "b".to_owned(), Some(root));
        let (text, info) = tree.render(usize::MAX);
        assert_eq!(info["levels"], 1);
        assert_eq!(info["nodes"], 1);
        assert_eq!(text, "root\t+2 children\troot=root-id\n");
        assert!(text.chars().count() <= A11Y_TREE_MAX_CHARS);
    }

    #[test]
    fn budget_counts_unicode_characters_and_indents_with_one_tab() {
        let mut tree = LevelTree::default();
        let root = tree.push("é".repeat(15_000), "root-id".to_owned(), None);
        tree.push("界".repeat(14_000), "child-id".to_owned(), Some(root));
        let (text, info) = tree.render(usize::MAX);
        assert_eq!(info["levels"], 2);
        assert_eq!(info["chars"], 29_003);
        assert!(text.len() > A11Y_TREE_MAX_CHARS);
        assert!(text.contains("\n\t界"));
    }

    #[test]
    fn structured_info_carries_the_same_tree_text() {
        let mut tree = LevelTree::default();
        let root = tree.push("root".to_owned(), "root-id".to_owned(), None);
        tree.push("child".to_owned(), "child-id".to_owned(), Some(root));
        let (text, info) = tree.render(usize::MAX);
        assert_eq!(text, "root\n\tchild\n");
        assert_eq!(info["tree"], text);
        let (text, info) = tree.render_partial(usize::MAX, true);
        assert_eq!(info["tree"], text);
    }

    #[test]
    fn oversized_root_level_is_not_cut_in_the_middle() {
        let mut tree = LevelTree::default();
        tree.push("x".repeat(A11Y_TREE_MAX_CHARS), "root-id".to_owned(), None);
        let (text, info) = tree.render(usize::MAX);
        assert!(text.is_empty());
        assert_eq!(info["levels"], 0);
        assert_eq!(info["more_below"], true);
    }

    #[test]
    fn labels_cannot_create_extra_lines_or_indentation() {
        assert_eq!(tree_field("a\nb\rc\td"), "a\\nb\\rc\\td");
    }
}

#[cfg(test)]
mod instructions_tests {
    use super::{DisplayConfig, SessionRouter};
    use rmcp::ServerHandler;

    /// Claude Code cuts server instructions at 2048 characters; anything past
    /// it (the coordinate rules were last) never reaches the agent.
    #[test]
    fn server_instructions_fit_the_client_limit() {
        for locked in [false, true] {
            let server = SessionRouter::new(DisplayConfig {
                width: 3840,
                height: 2160,
                locked,
                autoclean: false,
                ttl: None,
                keep_sessions: None,
                memory_high: None,
                memory_max: 0,
                memory_swap_max: 0,
            });
            let text = server.get_info().instructions.unwrap_or_default();
            assert!(!text.is_empty());
            assert!(
                text.chars().count() <= 2048,
                "{} characters",
                text.chars().count()
            );
            for tool in server.tools() {
                let description = tool.description.as_deref().unwrap_or_default();
                assert!(!description.is_empty(), "{} has no description", tool.name);
                assert!(
                    description.chars().count() <= 2048,
                    "{}: {} characters",
                    tool.name,
                    description.chars().count()
                );
            }
        }
    }
}

#[cfg(test)]
mod capture_file_tests {
    use super::read_frame_file;
    use std::io::Write;
    use std::time::Duration;

    fn frame_file() -> Result<std::fs::File, Box<dyn std::error::Error>> {
        let fd =
            nix::sys::memfd::memfd_create("test-frame", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)?;
        Ok(std::fs::File::from(fd))
    }

    #[tokio::test]
    async fn complete_frame_is_read_from_the_start() -> Result<(), Box<dyn std::error::Error>> {
        let frame = frame_file()?;
        frame.try_clone()?.write_all(b"frame")?;
        assert_eq!(read_frame_file(&frame, 5).await?, b"frame");
        Ok(())
    }

    #[tokio::test]
    async fn frame_is_returned_once_the_writer_fills_it() -> Result<(), Box<dyn std::error::Error>>
    {
        let frame = frame_file()?;
        let mut writer = frame.try_clone()?;
        writer.write_all(b"fra")?;
        let finish = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.write_all(b"me")
        });
        let read = tokio::time::timeout(Duration::from_millis(500), read_frame_file(&frame, 5));
        assert_eq!(read.await??, b"frame");
        finish.await??;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unfilled_frame_allows_other_tasks_and_cancels_with_a_deadline()
    -> Result<(), Box<dyn std::error::Error>> {
        let frame = frame_file()?;
        let other_task = tokio::spawn(async {
            tokio::task::yield_now().await;
            "responsive"
        });
        let read = tokio::time::timeout(Duration::from_millis(30), read_frame_file(&frame, 5));
        assert!(read.await.is_err());
        assert_eq!(other_task.await?, "responsive");
        Ok(())
    }
}

#[cfg(test)]
mod window_resolution_tests {
    use super::{WindowSnapshot, activation_target, find_window, window_closed};

    const PARENT: &str = "{3f1c2a6e-8d4b-4c7e-9a1f-0b2c3d4e5f60}";
    const CHILD: &str = "{7a9e0c1d-2b3c-4d5e-8f60-1a2b3c4d5e6f}";
    const GRANDCHILD: &str = "{c0ffee00-1234-4abc-8def-000000000001}";
    const OTHER: &str = "{00000000-0000-4000-8000-000000000002}";

    fn window(id: &str) -> WindowSnapshot {
        WindowSnapshot {
            id: id.to_owned(),
            title: String::new(),
            resource_class: String::new(),
            resource_name: String::new(),
            pid: 1,
            active: false,
            minimized: false,
            hidden: false,
            modal: false,
            transient: false,
            dialog: false,
            popup: false,
            transient_for: None,
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
            stacking_order: 0,
        }
    }

    fn child_of(id: &str, parent: &str) -> WindowSnapshot {
        WindowSnapshot {
            transient: true,
            transient_for: Some(parent.to_owned()),
            ..window(id)
        }
    }

    #[test]
    fn listed_id_resolves_in_any_spelling() {
        let windows = [window(PARENT), window(OTHER)];
        for spelling in [
            PARENT,
            "3f1c2a6e-8d4b-4c7e-9a1f-0b2c3d4e5f60",
            "{3F1C2A6E-8D4B-4C7E-9A1F-0B2C3D4E5F60}",
            " {3f1c2a6e-8d4b-4c7e-9a1f-0b2c3d4e5f60} ",
        ] {
            assert_eq!(
                find_window(&windows, spelling).map(|window| window.id.as_str()),
                Some(PARENT),
                "{spelling:?} should name the listed window"
            );
        }
    }

    #[test]
    fn unlisted_or_partial_ids_do_not_resolve() {
        let windows = [window(PARENT)];
        for spelling in [OTHER, "3f1c2a6e", "", "{}"] {
            assert!(
                find_window(&windows, spelling).is_none(),
                "{spelling:?} should not name the listed window"
            );
        }
    }

    #[test]
    fn activation_focuses_the_visible_transient_child() {
        let windows = [child_of(CHILD, PARENT), window(PARENT)];
        assert_eq!(activation_target(&windows, &windows[1]).id, CHILD);
    }

    #[test]
    fn activation_ignores_minimized_and_popup_children() {
        let mut minimized = child_of(CHILD, PARENT);
        minimized.minimized = true;
        let mut popup = child_of(GRANDCHILD, PARENT);
        popup.popup = true;
        let windows = [minimized, popup, window(PARENT)];
        assert_eq!(activation_target(&windows, &windows[2]).id, PARENT);
    }

    #[test]
    fn activation_takes_the_topmost_descendant_of_a_chain() {
        let windows = [
            child_of(GRANDCHILD, CHILD),
            child_of(CHILD, PARENT),
            window(PARENT),
        ];
        assert_eq!(activation_target(&windows, &windows[2]).id, GRANDCHILD);
    }

    #[test]
    fn activation_terminates_on_a_transient_cycle() {
        let windows = [
            child_of(CHILD, PARENT),
            child_of(PARENT, CHILD),
            window(OTHER),
        ];
        assert_eq!(activation_target(&windows, &windows[2]).id, OTHER);
    }

    #[test]
    fn closed_window_error_names_the_window() {
        let message = window_closed(PARENT).to_string();
        assert!(message.contains(PARENT), "{message}");
        assert!(message.contains("closed"), "{message}");
    }
}

#[cfg(test)]
mod user_namespace_guidance_tests {
    use super::{UserNamespaceFacts, user_namespace_guidance};

    fn confined() -> UserNamespaceFacts {
        UserNamespaceFacts {
            uid_map: "0 1000 1".to_owned(),
            max_user_namespaces: "0".to_owned(),
            no_new_privs: "1".to_owned(),
            seccomp: "2".to_owned(),
            apparmor_restriction: "1".to_owned(),
        }
    }

    #[test]
    fn a_refused_uid_map_is_explained_with_the_kernel_settings() {
        let guidance = user_namespace_guidance(
            "bwrap: setting up uid map: Operation not permitted\n",
            &confined(),
        )
        .expect("a refused uid map gets guidance");
        for fact in [
            "uid_map: 0 1000 1",
            "user.max_user_namespaces: 0",
            "NoNewPrivs: 1",
            "Seccomp: 2",
            "kernel.apparmor_restrict_unprivileged_userns: 1",
            "Start kwin-mcp outside that sandbox",
        ] {
            assert!(guidance.contains(fact), "missing {fact:?}: {guidance}");
        }
    }

    #[test]
    fn other_sandbox_failures_get_no_namespace_guidance() {
        for stderr in [
            "",
            "bwrap: Can't find source path /nonexistent: No such file or directory",
            "bwrap: execvp kwin_wayland: No such file or directory",
        ] {
            assert_eq!(user_namespace_guidance(stderr, &confined()), None, "{stderr}");
        }
        assert!(
            user_namespace_guidance("bwrap: No permissions to create new namespace", &confined())
                .is_some()
        );
    }
}

#[cfg(test)]
mod launch_warning_tests {
    use super::{
        launch_wallet_warning, launches_chromium, requests_blocked_google_chrome_extension_load,
    };

    #[test]
    fn a_walletless_chromium_launch_says_how_to_get_the_saved_logins() {
        let reason = "org.kde.ksecretd is not running on the host; kwin-mcp does not start it";
        let warning = launch_wallet_warning(Some(reason.to_owned()), "google-chrome-stable about:blank")
            .expect("a Chromium launch without a wallet warns");
        assert!(warning.contains(reason), "{warning}");
        assert!(
            warning.contains("start and unlock the host's KWallet before session_start"),
            "the warning must name the remedy: {warning}"
        );
        assert!(warning.contains("sign in inside the session"), "{warning}");
        assert_eq!(launch_wallet_warning(Some(reason.to_owned()), "kate notes.txt"), None);
        assert_eq!(launch_wallet_warning(None, "google-chrome-stable"), None);
    }

    #[test]
    fn chromium_family_commands_are_recognized() {
        assert!(launches_chromium(
            "google-chrome-stable --test-type https://claude.ai"
        ));
        assert!(launches_chromium("FOO=1 /usr/bin/chromium --app=file:///x"));
        assert!(!launches_chromium("code ."));
        assert!(!launches_chromium("kate notes.txt"));
        assert!(!launches_chromium(""));
    }

    #[test]
    fn branded_chrome_extension_switch_is_reported_but_chromium_is_not() {
        assert!(requests_blocked_google_chrome_extension_load(
            "google-chrome-stable --disable-features=DisableLoadExtensionCommandLineSwitch --load-extension=/tmp/extension"
        ));
        assert!(requests_blocked_google_chrome_extension_load(
            "/usr/bin/google-chrome --load-extension /tmp/extension"
        ));
        assert!(!requests_blocked_google_chrome_extension_load(
            "chromium --load-extension=/tmp/extension"
        ));
        assert!(!requests_blocked_google_chrome_extension_load(
            "google-chrome-stable --new-window chrome://extensions"
        ));
        assert!(!requests_blocked_google_chrome_extension_load(
            "code --load-extension=/tmp/extension"
        ));
    }
}

#[cfg(test)]
mod startup_report_tests {
    use super::{
        DisplayConfig, KwinMcp, descriptor_targets, hard_timeout_error, lifecycle_busy_error,
    };
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn server() -> KwinMcp {
        KwinMcp::new(DisplayConfig {
            width: 1920,
            height: 1080,
            locked: false,
            autoclean: false,
            ttl: None,
            keep_sessions: None,
            memory_high: None,
            memory_max: 0,
            memory_swap_max: 0,
        })
    }

    fn set_deadline(service: &KwinMcp, deadline: Option<Instant>) {
        *service
            .startup_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
    }

    #[test]
    fn blocked_timeout_names_the_call_its_age_and_the_cleanup_it_holds() {
        let stage = "preparing browser profiles: snapshot host cookies /home/a/.config/google-chrome/Default/Cookies";
        let error = hard_timeout_error(
            Duration::from_secs(20),
            stage,
            Duration::from_secs(23),
            true,
            Path::new("/run/user/1000/kwin-mcp-42"),
        );
        assert!(error.message.contains(&format!(
            "exceeded 20s hard limit: {stage} has not returned after 23s"
        )));
        assert!(
            error
                .message
                .contains(&format!("{stage} has not returned after 23s"))
        );
        assert!(error.message.contains(
            "cleanup of /run/user/1000/kwin-mcp-42 and its disk overlay runs when it does"
        ));
        let data = error.data.unwrap_or_default();
        assert_eq!(data["reason"], "hard_timeout");
        assert_eq!(data["host_call_blocked"], true);
        assert_eq!(data["blocked_seconds"], 23);
        assert_eq!(data["stage"], stage);
        assert!(
            data["pending_cleanup"]
                .as_str()
                .is_some_and(|text| text.contains("has not returned after 23s"))
        );
    }

    #[test]
    fn unblocked_timeout_reports_no_blocked_call() {
        let error = hard_timeout_error(
            Duration::from_secs(20),
            "reading the host mount table (/proc/self/mountinfo)",
            Duration::from_secs(1),
            false,
            Path::new("/run/user/1000/kwin-mcp-42"),
        );
        assert_eq!(
            error.message,
            "session_start exceeded 20s hard limit while reading the host mount table (/proc/self/mountinfo)"
        );
        let data = error.data.unwrap_or_default();
        assert_eq!(data["host_call_blocked"], false);
        assert!(data.get("pending_cleanup").is_none());
    }

    #[test]
    fn lifecycle_refusal_carries_the_detail_it_was_given() {
        let error = lifecycle_busy_error(
            "session_stop",
            "reading /proc/42",
            "a previous start is still blocked: reading /proc/42 has not returned after 30s",
        );
        assert_eq!(
            error.message,
            "session_stop could not start: a previous start is still blocked: reading /proc/42 has not returned after 30s. Retry shortly."
        );
        let data = error.data.unwrap_or_default();
        assert_eq!(data["reason"], "lifecycle_busy");
        assert_eq!(data["stage"], "reading /proc/42");
    }

    #[test]
    fn host_step_refuses_to_run_once_the_budget_is_spent() -> Result<(), String> {
        let service = server();
        let deadline = Instant::now() + Duration::from_secs(60);
        set_deadline(&service, Some(deadline));
        service
            .host_step("reading desktop settings")
            .map_err(|error| error.to_string())?;
        assert_eq!(service.start_stage_report().0, "reading desktop settings");
        set_deadline(&service, Some(Instant::now()));
        let refused = service.host_step("mounting live SQLite databases read-only");
        let message = refused
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert_eq!(
            message,
            "startup deadline elapsed before mounting live SQLite databases read-only"
        );
        assert_eq!(service.start_stage_report().0, "reading desktop settings");
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_call_reports_the_pending_cleanup_when_its_budget_runs_out()
    -> Result<(), String> {
        let service = server();
        // A timed-out start's deferred cleanup holds the gate while its host
        // call is blocked, and the record names the workdir it will remove.
        let blocked = service.start_gate.clone().lock_owned().await;
        service.set_pending_cleanup(Some(PathBuf::from("/run/user/1000/kwin-mcp-7")));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        let refused = service.lifecycle_gate(deadline, "session_start").await;
        let error = refused
            .err()
            .ok_or("the gate was granted while cleanup was pending")?;
        assert!(
            error
                .message
                .starts_with("session_start could not start: a previous start is still blocked")
        );
        assert!(error.message.contains(
            "cleanup of /run/user/1000/kwin-mcp-7 and its disk overlay runs when it does"
        ));
        drop(blocked);
        service.set_pending_cleanup(None);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(
            service
                .lifecycle_gate(deadline, "session_start")
                .await
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn descriptor_targets_lists_an_open_file_by_its_path() -> Result<(), String> {
        let path = std::env::temp_dir().join(format!("kwin-mcp-fd-test-{}", std::process::id()));
        let file = std::fs::File::create(&path).map_err(|error| error.to_string())?;
        let targets = descriptor_targets(
            i32::try_from(std::process::id()).map_err(|error| error.to_string())?,
        );
        drop(file);
        std::fs::remove_file(&path).map_err(|error| error.to_string())?;
        assert!(targets.iter().any(
            |target| matches!(target, procfs::process::FDTarget::Path(open) if *open == path)
        ));
        Ok(())
    }
}

#[cfg(test)]
mod typing_pace_tests {
    use super::{
        InputPace, TYPED_KEY_HOLD_MAX, TYPED_KEY_INTERVAL_FLOOR, TYPING_BUDGET, budgeted_interval,
        capped_key_hold,
    };
    use std::time::Duration;

    #[test]
    fn sampled_holds_stay_below_the_key_repeat_delay() -> Result<(), String> {
        let pace = InputPace::new(None, None).map_err(|error| format!("{error:?}"))?;
        let mut longest_sample = Duration::ZERO;
        for _ in 0..20_000 {
            let (hold, _) = pace
                .key_timing('-', Some('o'))
                .map_err(|error| format!("{error:?}"))?;
            longest_sample = longest_sample.max(hold);
            assert!(capped_key_hold(hold) <= TYPED_KEY_HOLD_MAX);
        }
        // The unclipped tail exceeds the cap, so the clip is exercised.
        assert!(longest_sample > TYPED_KEY_HOLD_MAX, "{longest_sample:?}");
        Ok(())
    }

    #[test]
    fn typical_holds_pass_through_the_cap_unchanged() {
        let typical = Duration::from_millis(93);
        assert_eq!(capped_key_hold(typical), typical);
        assert_eq!(
            capped_key_hold(Duration::from_millis(700)),
            TYPED_KEY_HOLD_MAX
        );
    }

    #[test]
    fn short_inputs_keep_their_sampled_interval() {
        let sampled = Duration::from_millis(111);
        assert_eq!(budgeted_interval(sampled, TYPING_BUDGET, 120), sampled);
    }

    #[test]
    fn long_inputs_shrink_to_their_share_of_the_remaining_budget() {
        assert_eq!(
            budgeted_interval(Duration::from_millis(100), Duration::from_millis(500), 10),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn exhausted_budget_falls_to_the_interval_floor_without_lengthening() {
        assert_eq!(
            budgeted_interval(Duration::from_millis(100), Duration::ZERO, 10),
            TYPED_KEY_INTERVAL_FLOOR
        );
        let short = Duration::from_millis(3);
        assert_eq!(budgeted_interval(short, Duration::ZERO, 10), short);
    }

    #[test]
    fn final_character_uses_the_remaining_budget_without_dividing_by_zero() {
        assert_eq!(
            budgeted_interval(Duration::from_millis(100), Duration::from_millis(40), 0),
            Duration::from_millis(40)
        );
    }

    #[test]
    fn long_compressed_schedule_fits_the_budget() {
        // 1,300 characters at the profile's 111 ms mean would take about 144 s.
        let sampled = Duration::from_millis(111);
        let characters = 1_300;
        let mut elapsed = Duration::ZERO;
        for index in 0..characters {
            let remaining = TYPING_BUDGET.saturating_sub(elapsed);
            elapsed += budgeted_interval(sampled, remaining, characters - index);
        }
        assert!(elapsed <= TYPING_BUDGET, "{elapsed:?}");
        assert!(
            elapsed + Duration::from_millis(1) >= TYPING_BUDGET,
            "{elapsed:?}"
        );
    }
}

#[cfg(test)]
mod launch_diagnostics_tests {
    use super::{
        ExitDiagnostics, IfDqblk, LAUNCH_STDERR_READ_BYTES, exit_diagnostics,
        exit_diagnostics_text, filesystem_name, launch_path_probes, launch_path_words,
        launch_script_write_error, quota_summary, relevant_sandbox_line, sandbox_log_since,
    };
    use std::path::{Path, PathBuf};

    fn exit_text(output: &str, command_paths: &[&str]) -> String {
        let paths: Vec<PathBuf> = command_paths.iter().map(PathBuf::from).collect();
        exit_diagnostics_text(&exit_diagnostics(Ok(output.to_owned()), &paths))
    }

    #[test]
    fn exit_text_carries_the_last_twenty_stderr_lines() {
        let output: String = (1..=25).map(|n| format!("line {n}\n")).collect();
        let text = exit_text(&output, &[]);
        assert!(
            text.starts_with("\nstderr (last 20 lines):\nline 6\n"),
            "{text}"
        );
        assert!(text.ends_with("\nline 25"), "{text}");
        assert!(!text.contains("line 5\n"), "{text}");
    }

    #[test]
    fn blank_stderr_lines_do_not_count_toward_the_tail() {
        assert_eq!(
            exit_text("boom\n\n\nbang\n\n", &[]),
            "\nstderr (last 2 lines):\nboom\nbang"
        );
        assert_eq!(exit_text("boom\n", &[]), "\nstderr (last 1 line):\nboom");
    }

    #[test]
    fn exit_without_stderr_says_so() {
        assert_eq!(exit_text("", &[]), "\nno stderr was written");
    }

    #[test]
    fn sandbox_log_line_names_the_path_the_session_cannot_see() {
        let output = "sh: line 1: /tmp/claude-1000/obs57/probe.txt: No such file or directory\n";
        let diagnostics = exit_diagnostics(Ok(output.to_owned()), &[]);
        assert_eq!(
            diagnostics,
            ExitDiagnostics {
                stderr: vec![
                    "sh: line 1: /tmp/claude-1000/obs57/probe.txt: No such file or directory"
                        .to_owned()
                ],
                sandbox_line: Some(
                    "sh: line 1: /tmp/claude-1000/obs57/probe.txt: No such file or directory"
                        .to_owned()
                ),
                unavailable: None,
            }
        );
        // Already in the tail, so it is not repeated as a separate sandbox.log line.
        assert_eq!(
            exit_text(output, &[]),
            format!("\nstderr (last 1 line):\n{}", output.trim_end())
        );
    }

    #[test]
    fn a_relevant_line_outside_the_tail_is_named_separately() {
        let mut output = String::from(
            "sh: line 1: /tmp/claude-1000/obs57/probe.txt: No such file or directory\n",
        );
        for n in 1..=20 {
            output.push_str(&format!("later {n}\n"));
        }
        let text = exit_text(&output, &[]);
        assert!(
            text.starts_with("\nstderr (last 20 lines):\nlater 1\n"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "\nsandbox.log: sh: line 1: /tmp/claude-1000/obs57/probe.txt: No such file or directory"
            ),
            "{text}"
        );
    }

    #[test]
    fn relevant_line_is_the_newest_one_naming_a_command_path() {
        let output = "kwin: warning about a window\nold: /usr/bin/drawd failed\nnew: /usr/bin/drawd exited\nunrelated line\n";
        assert_eq!(
            relevant_sandbox_line(output, &[PathBuf::from("/usr/bin/drawd")]).as_deref(),
            Some("new: /usr/bin/drawd exited")
        );
        // The root directory names every absolute path, so it cannot pick a line.
        assert_eq!(
            relevant_sandbox_line("plasma: /usr/share/x\n", &[PathBuf::from("/")]),
            None
        );
    }

    #[test]
    fn unreadable_sandbox_log_is_reported_not_hidden() {
        let diagnostics =
            exit_diagnostics(Err(std::io::Error::from(std::io::ErrorKind::NotFound)), &[]);
        assert!(diagnostics.stderr.is_empty());
        assert!(
            exit_diagnostics_text(&diagnostics).starts_with("\nstderr unavailable: "),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn launch_paths_include_option_values_once_and_skip_relative_words() {
        let words = launch_path_words(
            "drawd --sock=/tmp/a/sock SOCKET=/run/x 1280 relative/path /tmp/a/sock",
        );
        assert_eq!(
            words,
            vec![PathBuf::from("/tmp/a/sock"), PathBuf::from("/run/x")]
        );
    }

    #[test]
    fn a_path_to_create_is_probed_through_its_host_parent() {
        let host =
            |path: &Path| matches!(path.to_str(), Some("/tmp/claude/old" | "/usr/bin/drawd"));
        let probes = launch_path_probes(
            "/usr/bin/drawd /tmp/claude/old/sock /tmp/missing/dir/sock",
            host,
        );
        assert_eq!(
            probes,
            vec![
                (
                    PathBuf::from("/usr/bin/drawd"),
                    PathBuf::from("/usr/bin/drawd")
                ),
                (
                    PathBuf::from("/tmp/claude/old/sock"),
                    PathBuf::from("/tmp/claude/old")
                ),
            ]
        );
    }

    #[test]
    fn quota_summary_names_the_limit_that_is_reached() {
        const GIB_KIB: u64 = 1024 * 1024;
        const GIB_BYTES: u64 = 1024 * 1024 * 1024;
        let blocks = IfDqblk {
            dqb_bhardlimit: 2 * GIB_KIB,
            dqb_curspace: GIB_BYTES,
            ..IfDqblk::default()
        };
        assert_eq!(
            quota_summary(&blocks).as_deref(),
            Some("1.00 GiB used of a 2.00 GiB block limit")
        );
        let full_blocks = IfDqblk {
            dqb_bhardlimit: GIB_KIB,
            dqb_curspace: GIB_BYTES,
            ..IfDqblk::default()
        };
        assert_eq!(
            quota_summary(&full_blocks).as_deref(),
            Some("1.00 GiB used of a 1.00 GiB block limit (at its limit)")
        );
        let inodes = IfDqblk {
            dqb_ihardlimit: 100,
            dqb_curinodes: 100,
            ..IfDqblk::default()
        };
        assert_eq!(
            quota_summary(&inodes).as_deref(),
            Some("100 of 100 inodes used (at its limit)")
        );
        assert_eq!(quota_summary(&IfDqblk::default()), None);
    }

    #[test]
    fn write_failures_name_tmpfs_and_overlay_filesystems() {
        assert_eq!(filesystem_name(nix::sys::statfs::TMPFS_MAGIC), "tmpfs");
        assert_eq!(
            filesystem_name(nix::sys::statfs::OVERLAYFS_SUPER_MAGIC),
            "overlayfs"
        );
    }

    #[test]
    fn script_write_failure_says_nothing_was_submitted() {
        let error = std::io::Error::from_raw_os_error(nix::libc::EDQUOT);
        let failure =
            launch_script_write_error(Path::new("/nonexistent-kwin-mcp/launch-1.sh"), &error);
        assert!(
            failure.message.contains("Disk quota exceeded")
                && failure.message.contains("Nothing was submitted"),
            "{failure:?}"
        );
        let data = failure.data.unwrap_or_default();
        assert_eq!(data["reason"], "write_failed");
        assert_eq!(data["command_submitted"], false);
    }

    #[test]
    fn sandbox_log_read_starts_at_the_offset_and_drops_a_partial_line() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "kwin-mcp-launch-diagnostics-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)?;
        let log = dir.join("sandbox.log");
        std::fs::write(&log, "earlier output\nboom\n")?;
        assert_eq!(sandbox_log_since(&log, 15)?, "boom\n");

        // A read that starts inside a long output begins at the next whole line.
        let mut long = String::from("old\n");
        long.push_str(&"x".repeat(usize::try_from(LAUNCH_STDERR_READ_BYTES).unwrap_or(0) + 10));
        long.push_str("\nlast\n");
        std::fs::write(&log, &long)?;
        assert_eq!(sandbox_log_since(&log, 4)?, "last\n");
        std::fs::remove_dir_all(&dir)
    }
}

#[cfg(test)]
mod window_mode_tests {
    use super::{WindowMode, kwinrulesrc_contents};

    #[test]
    fn maximized_mode_writes_the_initial_maximize_rule() {
        assert_eq!(
            kwinrulesrc_contents(WindowMode::Maximized),
            "[1]\nDescription=No decorations, initially maximized\nnoborder=true\nnoborderrule=2\n\
             maximizehoriz=true\nmaximizehorizrule=3\nmaximizevert=true\nmaximizevertrule=3\n\
             wmclassmatch=0\n\n[General]\ncount=1\nrules=1\n"
        );
    }

    #[test]
    fn native_mode_keeps_decorations_off_and_drops_maximize() {
        let rules = kwinrulesrc_contents(WindowMode::Native);
        assert_eq!(
            rules,
            "[1]\nDescription=No decorations\nnoborder=true\nnoborderrule=2\n\
             wmclassmatch=0\n\n[General]\ncount=1\nrules=1\n"
        );
        assert!(!rules.contains("maximize"));
    }

    #[test]
    fn wire_names_select_the_mode_and_default_is_maximized() -> Result<(), serde_json::Error> {
        assert!(matches!(WindowMode::default(), WindowMode::Maximized));
        assert!(matches!(
            serde_json::from_str::<WindowMode>("\"native\"")?,
            WindowMode::Native
        ));
        assert!(matches!(
            serde_json::from_str::<WindowMode>("\"maximized\"")?,
            WindowMode::Maximized
        ));
        assert!(serde_json::from_str::<WindowMode>("\"Native\"").is_err());
        Ok(())
    }
}

#[cfg(test)]
mod session_memory_warning_tests {
    use super::{
        BYTES_PER_GIB, CallToolResult, Content, McpError, SessionMemorySnapshot,
        SessionMemoryWarning, attach_memory_snapshot_to_failure, attach_memory_warning_to_result,
        parse_memory_event, parse_pressure_avg10,
    };

    #[test]
    fn failures_carry_the_memory_snapshot_unless_a_warning_already_does() -> Result<(), String> {
        let calm = snapshot(BYTES_PER_GIB, Some(3 * BYTES_PER_GIB), Some(0.5), Some(0));
        let timeout = McpError::internal_error(
            "screenshot: frame unavailable",
            Some(serde_json::json!({"reason": "frame_unavailable"})),
        );
        let failed = match attach_memory_snapshot_to_failure(Err(timeout), calm) {
            Err(error) => error.data.unwrap_or_default(),
            Ok(_) => return Err("a failure became a success".to_owned()),
        };
        assert_eq!(failed["reason"], "frame_unavailable");
        assert_eq!(failed["memory"]["current_bytes"], BYTES_PER_GIB);
        assert_eq!(failed["memory"]["high_bytes"], 3 * BYTES_PER_GIB);
        assert_eq!(failed["memory"]["pressure_full_avg10_percent"], 0.5);

        let warned = McpError::internal_error(
            "screenshot: frame unavailable",
            Some(serde_json::json!({"memory_warning": {"reason": "memory_high"}})),
        );
        let warned = match attach_memory_snapshot_to_failure(Err(warned), calm) {
            Err(error) => error.data.unwrap_or_default(),
            Ok(_) => return Err("a failure became a success".to_owned()),
        };
        assert!(warned.get("memory").is_none(), "{warned}");

        let mut error_result = CallToolResult::success(vec![Content::text("timed out")]);
        error_result.is_error = Some(true);
        error_result.structured_content = Some(serde_json::json!({"window_outcome": "timeout"}));
        let reported = attach_memory_snapshot_to_failure(Ok(error_result), calm)
            .map_err(|error| error.message.to_string())?;
        assert_eq!(
            reported.structured_content.unwrap_or_default()["memory"]["current_bytes"],
            BYTES_PER_GIB
        );

        let mut success = CallToolResult::success(vec![Content::text("ok")]);
        success.structured_content = Some(serde_json::json!({"status": "ok"}));
        let unchanged = attach_memory_snapshot_to_failure(Ok(success), calm)
            .map_err(|error| error.message.to_string())?;
        assert!(
            unchanged
                .structured_content
                .unwrap_or_default()
                .get("memory")
                .is_none()
        );
        Ok(())
    }

    fn snapshot(
        current: u64,
        high: Option<u64>,
        pressure_full_avg10: Option<f64>,
        high_events: Option<u64>,
    ) -> SessionMemorySnapshot {
        SessionMemorySnapshot {
            current,
            high,
            swap_current: Some(BYTES_PER_GIB),
            swap_max: Some(BYTES_PER_GIB),
            pressure_full_avg10,
            high_events,
        }
    }

    fn memory_warning() -> Result<SessionMemoryWarning, String> {
        SessionMemoryWarning::from_snapshots(
            None,
            Some(snapshot(
                4 * BYTES_PER_GIB,
                Some(3 * BYTES_PER_GIB),
                Some(67.18),
                Some(12),
            )),
        )
        .ok_or_else(|| "expected a memory warning".to_owned())
    }

    #[test]
    fn reaching_memory_high_reports_ram_swap_and_psi() -> Result<(), String> {
        let mut pressured = snapshot(
            4 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(67.18),
            Some(12),
        );
        pressured.swap_current = Some(BYTES_PER_GIB - 24_576);
        let warning = SessionMemoryWarning::from_snapshots(None, Some(pressured))
            .ok_or_else(|| "expected a memory warning".to_owned())?;
        assert_eq!(warning.reason, "memory_high");
        assert!(warning.message.contains("RAM 4.00/3.00 GiB"));
        assert!(warning.message.contains("swap 1.00/1.00 GiB nearly full"));
        assert!(warning.message.contains("memory PSI full avg10=67.18%"));
        assert!(
            warning
                .message
                .contains("session_start with a larger memory_high, memory_max"),
            "the warning must say how to raise the limits: {}",
            warning.message
        );
        assert_eq!(warning.as_json()["memory_current_bytes"], 4 * BYTES_PER_GIB);
        Ok(())
    }

    #[test]
    fn full_psi_pressure_warns_below_memory_high() -> Result<(), String> {
        let warning = SessionMemoryWarning::from_snapshots(
            None,
            Some(snapshot(
                2 * BYTES_PER_GIB,
                Some(3 * BYTES_PER_GIB),
                Some(10.0),
                Some(0),
            )),
        )
        .ok_or_else(|| "expected a PSI warning".to_owned())?;
        assert_eq!(warning.reason, "memory_pressure_full");
        assert!(warning.message.contains("session memory pressure is high"));
        Ok(())
    }

    #[test]
    fn pressure_at_call_start_is_reported_after_recovery() -> Result<(), String> {
        let before = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(20.0),
            Some(4),
        );
        let after = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(0.0),
            Some(4),
        );
        let warning = SessionMemoryWarning::from_snapshots(Some(before), Some(after))
            .ok_or_else(|| "expected a warning for pressure at call start".to_owned())?;
        assert_eq!(warning.reason, "memory_pressure_full");
        assert!(warning.message.contains("memory PSI full avg10=20.00%"));
        assert!(
            warning
                .message
                .contains("pressure was high when this call started")
        );
        Ok(())
    }

    #[test]
    fn a_new_high_event_warns_after_memory_recovers() -> Result<(), String> {
        let before = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(0.0),
            Some(4),
        );
        let after = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(0.0),
            Some(7),
        );
        let warning = SessionMemoryWarning::from_snapshots(Some(before), Some(after))
            .ok_or_else(|| "expected a warning for new memory.high events".to_owned())?;
        assert_eq!(warning.memory_high_events_delta, Some(3));
        assert!(warning.message.contains("memory.high events +3"));
        Ok(())
    }

    #[test]
    fn low_memory_and_psi_do_not_warn() {
        let before = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(9.99),
            Some(4),
        );
        let after = snapshot(
            2 * BYTES_PER_GIB,
            Some(3 * BYTES_PER_GIB),
            Some(9.99),
            Some(4),
        );
        assert!(SessionMemoryWarning::from_snapshots(Some(before), Some(after)).is_none());
    }

    #[test]
    fn cgroup_pressure_and_event_files_are_parsed() {
        let pressure = "some avg10=99.00 avg60=1.00 avg300=0.00 total=5\nfull avg10=67.18 avg60=56.57 avg300=12.40 total=9\n";
        assert_eq!(parse_pressure_avg10(pressure, "full"), Some(67.18));
        assert_eq!(parse_pressure_avg10(pressure, "some"), Some(99.0));
        assert_eq!(
            parse_memory_event("low 0\nhigh 540602\nmax 1\n", "high"),
            Some(540602)
        );
        assert_eq!(parse_memory_event("low 0\nhigh 0\n", "oom_kill"), None);
    }

    #[test]
    fn structured_results_include_warning_data_and_text() -> Result<(), String> {
        let mut result = CallToolResult::success(vec![Content::text("clicked")]);
        result.structured_content = Some(serde_json::json!({"action": "click"}));
        let reported = attach_memory_warning_to_result(Ok(result), memory_warning()?, false)
            .map_err(|error| error.message.to_string())?;
        let data = reported
            .structured_content
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| "structured result was lost".to_owned())?;
        assert_eq!(data["memory_warning"]["reason"], "memory_high");
        assert_eq!(reported.content.len(), 2);
        Ok(())
    }

    #[test]
    fn inline_screenshot_keeps_image_and_adds_warning_text() -> Result<(), String> {
        let result = CallToolResult::success(vec![Content::image("encoded", "image/png")]);
        let reported = attach_memory_warning_to_result(Ok(result), memory_warning()?, true)
            .map_err(|error| error.message.to_string())?;
        assert!(reported.structured_content.is_none());
        let serialized = serde_json::to_value(reported).map_err(|error| error.to_string())?;
        assert!(
            serialized["content"][1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("warning: session memory throttled"))
        );
        Ok(())
    }

    #[test]
    fn screenshot_timeouts_name_pressure_but_other_errors_do_not() -> Result<(), String> {
        let timeout = McpError::internal_error(
            "screenshot: compositor did not finish a frame within 10 seconds",
            None,
        );
        let timeout = match attach_memory_warning_to_result(Err(timeout), memory_warning()?, true) {
            Err(error) => error,
            Ok(_) => return Err("screenshot timeout unexpectedly succeeded".to_owned()),
        };
        assert!(
            timeout
                .message
                .contains("likely cause: session memory throttled")
        );

        let invalid = McpError::invalid_params("region and cursor are mutually exclusive", None);
        let invalid = match attach_memory_warning_to_result(Err(invalid), memory_warning()?, true) {
            Err(error) => error,
            Ok(_) => return Err("invalid screenshot parameters unexpectedly succeeded".to_owned()),
        };
        assert!(!invalid.message.contains("likely cause"));
        assert!(
            invalid
                .message
                .contains("warning: session memory throttled")
        );
        let warning = invalid
            .data
            .as_ref()
            .and_then(|data| data.get("memory_warning"))
            .and_then(|warning| warning.get("reason"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(warning, Some("memory_high"));
        Ok(())
    }
}

#[cfg(test)]
mod launch_failure_context_tests {
    use super::{LaunchProgress, McpError, launch_failure, launch_script_write_error};
    use std::path::Path;

    #[test]
    fn issue500_quota_failure_retains_errno_path_and_pre_submit_recovery() {
        let io = std::io::Error::from_raw_os_error(nix::libc::EDQUOT);
        let failure = launch_failure(
            launch_script_write_error(Path::new("/nonexistent-issue500/launch.sh"), &io),
            &LaunchProgress {
                stage: "writing the launch script",
                command_submitted: false,
                window_baseline_errors: Vec::new(),
            },
        );
        let data = failure.data.unwrap_or_default();
        assert_eq!(data["os_error"], nix::libc::EDQUOT);
        assert_eq!(data["path"], "/nonexistent-issue500/launch.sh");
        assert_eq!(data["stage"], "writing the launch script");
        assert_eq!(data["operation"], "launch_app");
        assert_eq!(data["retry_safe"], true);
        assert_eq!(data["command_submitted"], false);
        assert!(failure.message.contains("Disk quota exceeded"));
        assert!(failure.message.contains("free blocks alone"));
    }

    #[test]
    fn issue500_generic_post_submit_failure_never_authorizes_replay() {
        let failure = launch_failure(
            McpError::internal_error("connection failed", None),
            &LaunchProgress {
                stage: "connecting to browser accessibility",
                command_submitted: true,
                window_baseline_errors: Vec::new(),
            },
        );
        let data = failure.data.unwrap_or_default();
        assert_eq!(data["stage"], "connecting to browser accessibility");
        assert_eq!(data["command_submitted"], true);
        assert_eq!(data["retry_safe"], false);
        assert!(failure.message.contains("do not launch a duplicate"));
    }

    #[test]
    fn issue500_submission_evidence_cannot_be_downgraded_by_error_data() {
        let failure = launch_failure(
            McpError::internal_error(
                "late error",
                Some(serde_json::json!({"command_submitted":false})),
            ),
            &LaunchProgress {
                stage: "waiting for a window",
                command_submitted: true,
                window_baseline_errors: Vec::new(),
            },
        );
        let data = failure.data.unwrap_or_default();
        assert_eq!(data["command_submitted"], true);
        assert_eq!(data["retry_safe"], false);
    }

    #[test]
    fn issue500_specific_error_fields_survive_generic_annotation() {
        let failure = launch_failure(
            McpError::invalid_params(
                "invisible path",
                Some(
                    serde_json::json!({"reason":"path_not_visible", "path":"/tmp/hidden",
                "stage":"path preflight", "command_submitted":true}),
                ),
            ),
            &LaunchProgress {
                stage: "fallback",
                command_submitted: false,
                window_baseline_errors: Vec::new(),
            },
        );
        let data = failure.data.unwrap_or_default();
        assert_eq!(data["reason"], "path_not_visible");
        assert_eq!(data["path"], "/tmp/hidden");
        assert_eq!(data["stage"], "path preflight");
        assert_eq!(data["retry_safe"], false);
    }
}

#[cfg(test)]
mod session_control_socket_tests {
    use super::{is_session_control_socket, same_pid_namespace};
    use std::path::Path;

    #[test]
    fn control_sockets_are_recognized_by_name() {
        assert!(is_session_control_socket(Path::new(
            "/run/user/1000/kwin-mcp-control-387755.sock"
        )));
        assert!(!is_session_control_socket(Path::new(
            "/run/user/1000/kwin-mcp-control-.sock"
        )));
        assert!(!is_session_control_socket(Path::new(
            "/run/user/1000/kwin-mcp-control-12x.sock"
        )));
        assert!(!is_session_control_socket(Path::new("/run/user/1000/bus")));
    }

    #[test]
    fn this_process_shares_its_own_pid_namespace() {
        let own = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        assert!(same_pid_namespace(own));
        assert!(!same_pid_namespace(i32::MAX));
    }
}

#[cfg(test)]
mod launch_pid_tests {
    use super::parse_launch_pid;

    #[test]
    fn only_a_complete_nonzero_number_is_a_pid() {
        assert_eq!(parse_launch_pid("4242\n"), Some(4242));
        assert_eq!(parse_launch_pid(""), None);
        assert_eq!(parse_launch_pid("0\n"), None);
        assert_eq!(parse_launch_pid("42x"), None);
        assert_eq!(parse_launch_pid("-7"), None);
    }
}

#[cfg(test)]
mod ssh_config_tests {
    use super::ssh_config_needs_copy;

    #[test]
    fn only_safe_root_owned_drop_ins_need_a_session_copy() {
        // A root-owned drop-in is unmapped inside the sandbox.
        assert!(ssh_config_needs_copy(0, 0o100644, 1000));
        assert!(!ssh_config_needs_copy(1000, 0o100664, 1000));
        assert!(!ssh_config_needs_copy(1000, 0o100646, 1000));
        assert!(!ssh_config_needs_copy(0, 0o100664, 1000));
        assert!(!ssh_config_needs_copy(2000, 0o100644, 1000));
        // A file the session user owns, without group or other write, is used as-is.
        assert!(!ssh_config_needs_copy(1000, 0o100600, 1000));
        assert!(!ssh_config_needs_copy(1000, 0o100644, 1000));
    }
}

#[cfg(test)]
mod ui_match_context_tests {
    use super::{
        AtspiNode, WindowSnapshot, find_ui_frame_window, find_ui_native_match,
        find_ui_role_matches, find_ui_search_text,
    };

    fn window() -> WindowSnapshot {
        WindowSnapshot {
            id: "owned-window".to_owned(),
            title: "Dialog".to_owned(),
            resource_class: "kdialog".to_owned(),
            resource_name: "kdialog".to_owned(),
            pid: 1,
            active: true,
            minimized: false,
            hidden: false,
            modal: false,
            transient: false,
            dialog: true,
            popup: false,
            transient_for: None,
            x: 344.0,
            y: 299.0,
            w: 312.0,
            h: 101.0,
            stacking_order: 0,
        }
    }
    fn node(bounds: (i32, i32, i32, i32)) -> AtspiNode {
        AtspiNode {
            name: "Stop".to_owned(),
            role: "push button".to_owned(),
            states: vec!["visible".to_owned(), "enabled".to_owned()],
            bounds,
        }
    }

    #[test]
    fn issue418_button_filter_never_matches_explanatory_static_text() {
        assert!(find_ui_role_matches(Some(" Button "), "push button"));
        assert!(find_ui_role_matches(Some("button"), "toggle button"));
        assert!(!find_ui_role_matches(Some("button"), "static text"));
        assert!(!find_ui_role_matches(Some("button"), "label"));
        assert!(find_ui_role_matches(None, "static text"));
    }

    #[test]
    fn issue633_name_query_matches_nonbreaking_space() {
        let name = find_ui_search_text("Sign\u{00a0}in");
        let query = find_ui_search_text("Sign in");
        assert!(name.contains(&query));
    }

    #[test]
    fn issue533_native_local_origin_is_normalized_against_its_own_frame() {
        let win = window();
        let result = find_ui_native_match(
            &node((135, 65, 80, 25)),
            Some(&win),
            &[win.clone()],
            (0, 0, 312, 101),
        );
        assert_eq!(result["mouse_coordinates_usable"], true);
        assert_eq!(result["bounds"]["x"], 135.0);
        assert_eq!(result["bounds"]["y"], 65.0);
        assert_eq!(result["mapped_screen_bounds"]["x"], 479.0);
        assert_eq!(result["mapped_screen_bounds"]["y"], 364.0);
        assert_eq!(result["visible"], true);
        assert!(
            result["clickable"].is_null(),
            "native geometry cannot prove absence of an in-app overlay"
        );
    }

    #[test]
    fn issue533_true_screen_origin_produces_the_same_window_coordinates() {
        let win = window();
        let result = find_ui_native_match(
            &node((479, 364, 80, 25)),
            Some(&win),
            &[win.clone()],
            (344, 299, 312, 101),
        );
        assert_eq!(result["bounds"]["x"], 135.0);
        assert_eq!(result["bounds"]["y"], 65.0);
        assert_eq!(result["mouse_coordinates_usable"], true);
    }

    #[test]
    fn issue533_unverified_scale_and_ambiguous_windows_never_offer_mouse_coordinates() {
        let win = window();
        let result = find_ui_native_match(
            &node((50, 20, 80, 25)),
            Some(&win),
            &[win.clone()],
            (0, 0, 156, 50),
        );
        assert_eq!(result["mouse_coordinates_usable"], false);
        assert_eq!(result["clickable"], false);
        let mut second = win.clone();
        second.id = "other-window".to_owned();
        second.x = 0.0;
        second.y = 0.0;
        assert!(find_ui_frame_window(&[win, second], "Dialog", (0, 0, 312, 101)).is_none());
    }

    #[test]
    fn issue418_covering_window_is_not_reported_as_a_clickable_native_button() {
        let win = window();
        let mut cover = win.clone();
        cover.id = "cover".to_owned();
        cover.stacking_order = 1;
        let result = find_ui_native_match(
            &node((135, 65, 80, 25)),
            Some(&win),
            &[win.clone(), cover],
            (0, 0, 312, 101),
        );
        assert_eq!(result["occluded"], true);
        assert_eq!(result["clickable"], false);
    }
}

#[cfg(test)]
mod home_overlay_report_tests {
    use super::{home_overlay_report, launch_file_words};
    use std::path::{Path, PathBuf};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> std::io::Result<Self> {
            let dir = std::env::temp_dir().join(format!(
                "kwin-611-{label}-{}-{:x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn write(&self, relative: &str, text: &str) -> std::io::Result<PathBuf> {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
            Ok(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A session root holding `path` at the same absolute path.
    fn copy_into_root(root: &Path, path: &Path, text: &str) -> std::io::Result<()> {
        let inside = root.join(path.strip_prefix("/").unwrap_or(path));
        if let Some(parent) = inside.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(inside, text)
    }

    #[test]
    fn relative_words_count_only_when_they_name_a_file_in_cwd() -> std::io::Result<()> {
        let scratch = Scratch::new("words")?;
        let script = scratch.write("work/run85.sh", "echo new\n")?;
        let cwd = scratch.0.join("work");
        let words = launch_file_words("bash run85.sh BIN 1 --flag=x /abs/out", Some(&cwd));
        assert_eq!(words, vec![PathBuf::from("/abs/out"), script]);
        Ok(())
    }

    #[test]
    fn host_write_paths_and_paths_outside_home_are_not_overlay_paths() -> std::io::Result<()> {
        let scratch = Scratch::new("paths")?;
        let home = scratch.0.join("home");
        let shared = home.join("shared");
        let command = format!(
            "run {} {} /usr/bin/true {}",
            home.join("work/out").display(),
            shared.join("out").display(),
            home.display()
        );
        let report = home_overlay_report(&command, &home, Some(&home), &[shared], None);
        assert_eq!(report.overlay_paths, vec![home.join("work/out")]);
        assert_eq!(
            report.working_directory, None,
            "HOME itself is not reported"
        );
        assert!(report.different_copies.is_empty());
        Ok(())
    }

    #[test]
    fn an_old_or_missing_session_copy_is_reported_and_a_same_view_is_not() -> std::io::Result<()> {
        let scratch = Scratch::new("copies")?;
        let home = scratch.0.join("home");
        let edited = scratch.write("home/work/run85.sh", "echo queued-visual-rect-empty\n")?;
        let missing = scratch.write("home/work/fonts.txt", "fonts\n")?;
        let command = format!("bash run85.sh {}", missing.display());
        let cwd = home.join("work");
        // The host root is a session that sees exactly the host's files.
        let same = home_overlay_report(&command, &home, Some(&cwd), &[], Some(Path::new("/")));
        assert!(
            same.different_copies.is_empty(),
            "{:?}",
            same.different_copies
        );
        let root = scratch.0.join("session-root");
        copy_into_root(&root, &edited, "echo queued-rect=0,0,0,0 colors=2\n")?;
        let stale = home_overlay_report(&command, &home, Some(&cwd), &[], Some(&root));
        let paths: Vec<&str> = stale
            .different_copies
            .iter()
            .filter_map(|copy| copy["path"].as_str())
            .collect();
        assert_eq!(
            paths,
            vec![missing.display().to_string(), edited.display().to_string()]
        );
        assert!(stale.different_copies[0]["session_bytes"].is_null());
        assert_eq!(stale.different_copies[1]["session_bytes"], 34);
        assert!(
            stale.notes()[0].contains("import_file with overwrite=true"),
            "{:?}",
            stale.notes()
        );
        Ok(())
    }

    #[test]
    fn an_imported_copy_with_the_host_bytes_and_a_new_mtime_is_not_reported() -> std::io::Result<()>
    {
        let scratch = Scratch::new("imported")?;
        let home = scratch.0.join("home");
        let script = scratch.write("home/work/run85.sh", "echo queued-visual-rect-empty\n")?;
        let root = scratch.0.join("session-root");
        std::thread::sleep(std::time::Duration::from_millis(20));
        copy_into_root(&root, &script, "echo queued-visual-rect-empty\n")?;
        let command = format!("bash {}", script.display());
        let report = home_overlay_report(&command, &home, None, &[], Some(&root));
        assert!(
            report.different_copies.is_empty(),
            "{:?}",
            report.different_copies
        );
        assert_eq!(report.overlay_paths, vec![script]);
        Ok(())
    }
}

#[cfg(test)]
mod overlay_floor_tests {
    use super::overlay_floor_message;
    use std::path::Path;

    #[test]
    fn the_refusal_names_the_retry_a_caller_can_make() {
        let message = overlay_floor_message(
            Path::new("/home/u/.cache/kwin-mcp/kwin-mcp-1"),
            2 << 30,
            607 << 20,
        );
        assert!(message.contains("needs 2.00 GiB free"), "{message}");
        assert!(message.contains("0.59 GiB free"), "{message}");
        assert!(message.contains("disk_min_free_mib=303"), "{message}");
        assert!(overlay_floor_message(Path::new("/x"), 2 << 30, 0).contains("disk_min_free_mib=1"));
    }
}

#[cfg(test)]
mod browser_alias_tests {
    use super::create_browser_wrappers;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_browser_installed_under_its_other_name_still_launches() -> std::io::Result<()> {
        let root = std::env::temp_dir().join(format!("kwin-mcp-alias-{}", std::process::id()));
        let stubs = root.join("stubs");
        std::fs::create_dir_all(&stubs)?;
        let stub = stubs.join("google-chrome-stable");
        std::fs::write(&stub, "#!/bin/sh\necho stub-ran\n")?;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))?;
        let bin = create_browser_wrappers(&root)?;
        // Only google-chrome-stable exists; the caller typed google-chrome.
        let run = |name: &str| {
            std::process::Command::new(bin.join(name))
                .env("PATH", format!("{}:{}:/usr/bin:/bin", bin.display(), stubs.display()))
                .env_remove("KWIN_MCP_CDP_PORT")
                .output()
        };
        let aliased = run("google-chrome")?;
        let missing = run("brave")?;
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(String::from_utf8_lossy(&aliased.stdout).trim(), "stub-ran");
        assert_eq!(missing.status.code(), Some(127));
        Ok(())
    }
}
