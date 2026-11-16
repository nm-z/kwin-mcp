//! Host serial devices in a session (#981).
//!
//! A session's /dev is bwrap's own (`--dev`): null, zero, the pty multiplexer,
//! and the render and input nodes kwin-mcp binds. No serial device is virtual,
//! so a session shows every one the host has, at its own name: each tty whose
//! device number belongs to a driver that the kernel's table of tty drivers
//! (/proc/tty/drivers) types `serial` (ttyS, ttyUSB, ttyACM, ttyAMA, ttyGS,
//! rfcomm and the rest; consoles, virtual terminals and ptys have drivers of
//! other types), and /dev/serial, where udev's by-id and by-path links resolve
//! to those names. sysfs is no guide: a gadget's ttyGS port has no `device`
//! link there. A shown node is the host's inode, so its owner, mode and ACL are
//! the host's, and session processes keep the user's supplementary groups
//! (uucp, dialout).
//!
//! bwrap binds the devices present when the session starts. A device plugged
//! in later, or one that comes back as a new node, reaches the session through
//! `--serial-devices`, a host process per session like the loopback forwarder:
//! it watches host /dev with inotify, compares each serial name there with the
//! session's through /proc/PID/root, where a shown node has the host's inode,
//! and hands each difference to `--serial-bind`.
//!
//! `--serial-bind` attaches host nodes in the session's mount namespace, where
//! the host's /dev cannot be reached and a bind needs its source in the
//! namespace it is made in. The session's mount namespace belongs to the user
//! namespace bwrap made, which descends from one this user made (pasta's, or
//! the namespace holder's), so a process of this user on the host may enter it
//! with every capability there. The binder enters it, makes a copy of the
//! host's mount namespace owned by it, clones each node from that copy with
//! `open_tree(OPEN_TREE_CLONE)` into a detached mount its descriptor keeps,
//! enters the session's mount namespace and attaches each one with
//! `move_mount`. A node the host removes is unmounted lazily, so a program that
//! has it open keeps its descriptor, and its mount point is deleted. Entering a
//! user namespace takes a single-threaded process, so the binder is a process of
//! its own, started before any runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Server flag: keep a session's serial devices in step with the host's.
pub const WATCH_FLAG: &str = "--serial-devices";
/// The watcher's helper: show or remove devices in the session.
pub const BIND_FLAG: &str = "--serial-bind";

/// udev's directory of links to serial devices (by-id, by-path) in /dev.
const LINKS: &str = "serial";
const HOST_DEV: &str = "/dev";
const TTY_CLASS: &str = "/sys/class/tty";
/// The kernel's tty drivers, each with its device numbers and type.
const TTY_DRIVERS: &str = "/proc/tty/drivers";
/// The longest device name accepted; tty names are short.
const NAME_MAX: usize = 64;

/// How long the watcher lets a burst of /dev changes settle: an adapter makes
/// its nodes one interface at a time.
const SETTLE: Duration = Duration::from_millis(150);
/// How often the watcher checks that its session is still there.
const POLL: Duration = Duration::from_secs(1);
/// Delays before a failed comparison or bind is tried again, doubling.
const RETRY_FIRST: Duration = Duration::from_millis(500);
const RETRY_LAST: Duration = Duration::from_secs(30);
/// Mounts stacked on one name that `clear` takes off before it gives up.
const STACKED_MOUNTS: usize = 8;

/// linux/nsfs.h: the user namespace that owns a namespace, as a new descriptor.
const NS_GET_USERNS: libc::Ioctl = 0xb701;
/// linux/mount.h
const OPEN_TREE_CLONE: libc::c_uint = 1;
const OPEN_TREE_CLOEXEC: libc::c_uint = libc::O_CLOEXEC.unsigned_abs();
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;
const MOUNT_ATTR_RDONLY: u64 = 0x1;

/// linux/mount.h `struct mount_attr`, for mount_setattr.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// A name's file: a bind shows the host's inode, so the same pair in the
/// session means the session shows this node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Node {
    dev: u64,
    ino: u64,
}

impl Node {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

/// A tty's name as /dev and sysfs have it: no path, nothing a shell expands.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name != LINKS
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

/// The device numbers a serial driver owns: major, first and last minor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Numbers {
    major: u64,
    first: u64,
    last: u64,
}

/// The serial drivers' numbers in /proc/tty/drivers, whose lines end in a
/// major, a minor or range of them, and a type: "acm /dev/ttyACM 166 0-255
/// serial".
fn serial_drivers(table: &str) -> Vec<Numbers> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().rev();
            let (kind, minors, major) = (fields.next()?, fields.next()?, fields.next()?);
            if kind != "serial" {
                return None;
            }
            let (first, last) = minors.split_once('-').unwrap_or((minors, minors));
            Some(Numbers {
                major: major.parse().ok()?,
                first: first.parse().ok()?,
                last: last.parse().ok()?,
            })
        })
        .collect()
}

/// The host's serial devices by name, with /dev/serial under `LINKS`.
pub fn host_devices() -> BTreeMap<String, Node> {
    let table = std::fs::read_to_string(TTY_DRIVERS).unwrap_or_default();
    scan(
        Path::new(TTY_CLASS),
        Path::new(HOST_DEV),
        &serial_drivers(&table),
    )
}

fn scan(class: &Path, dev: &Path, drivers: &[Numbers]) -> BTreeMap<String, Node> {
    let mut devices: BTreeMap<String, Node> = serial_names(class, drivers)
        .into_iter()
        .filter_map(|(name, number)| {
            let node = device_node(dev, &name, number)?;
            Some((name, node))
        })
        .collect();
    if let Some(node) = link_directory(dev) {
        devices.insert(LINKS.to_owned(), node);
    }
    devices
}

/// The ttys whose device numbers belong to a serial driver, with them.
fn serial_names(class: &Path, drivers: &[Numbers]) -> Vec<(String, (u64, u64))> {
    let Ok(entries) = std::fs::read_dir(class) else {
        return Vec::new();
    };
    let mut names: Vec<(String, (u64, u64))> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| valid_name(name))
        .filter_map(|name| {
            let number = std::fs::read_to_string(class.join(&name).join("dev")).ok()?;
            let (major, minor) = number.trim().split_once(':')?;
            let (major, minor) = (major.parse::<u64>().ok()?, minor.parse::<u64>().ok()?);
            drivers
                .iter()
                .any(|driver| {
                    driver.major == major && (driver.first..=driver.last).contains(&minor)
                })
                .then_some((name, (major, minor)))
        })
        .collect();
    names.sort();
    names
}

/// `dev/name`, when it is the character device with `number`.
fn device_node(dev: &Path, name: &str, (major, minor): (u64, u64)) -> Option<Node> {
    let metadata = std::fs::symlink_metadata(dev.join(name)).ok()?;
    let rdev = metadata.rdev();
    (metadata.file_type().is_char_device()
        && nix::sys::stat::major(rdev) == major
        && nix::sys::stat::minor(rdev) == minor)
        .then(|| Node::of(&metadata))
}

/// udev's /dev/serial, when it is a directory of the /dev file system.
fn link_directory(dev: &Path) -> Option<Node> {
    let root = std::fs::symlink_metadata(dev).ok()?;
    let metadata = std::fs::symlink_metadata(dev.join(LINKS)).ok()?;
    (metadata.is_dir() && metadata.dev() == root.dev()).then(|| Node::of(&metadata))
}

/// bwrap arguments that show `devices` in the session; they follow `--dev`.
/// The `-try` forms skip a device unplugged before bwrap runs.
pub fn bwrap_args(devices: &BTreeMap<String, Node>) -> Vec<String> {
    devices
        .keys()
        .flat_map(|name| {
            let path = format!("{HOST_DEV}/{name}");
            let kind = if name == LINKS {
                "--ro-bind-try"
            } else {
                "--dev-bind-try"
            };
            [kind.to_owned(), path.clone(), path]
        })
        .collect()
}

/// session_start's list: each port with hardware behind it, and the udev
/// links that name it. A serial-core port of type 0 (PORT_UNKNOWN: the legacy
/// ttyS ports with no UART) is shown in the session but not listed.
pub fn report(devices: &BTreeMap<String, Node>) -> Vec<serde_json::Value> {
    report_under(Path::new(TTY_CLASS), Path::new(HOST_DEV), devices)
}

fn report_under(
    class: &Path,
    dev: &Path,
    devices: &BTreeMap<String, Node>,
) -> Vec<serde_json::Value> {
    let links = udev_links(dev);
    devices
        .keys()
        .filter(|name| *name != LINKS && has_hardware(class, name))
        .map(|name| {
            let named: Vec<&str> = links
                .iter()
                .filter(|(_, target)| target == name)
                .map(|(link, _)| link.as_str())
                .collect();
            let mut entry = serde_json::json!({ "path": format!("{HOST_DEV}/{name}") });
            if !named.is_empty() {
                entry["links"] = serde_json::json!(named);
            }
            entry
        })
        .collect()
}

fn has_hardware(class: &Path, name: &str) -> bool {
    std::fs::read_to_string(class.join(name).join("type")).map_or(true, |kind| kind.trim() != "0")
}

/// udev's links under /dev/serial, each with the /dev name it resolves to.
fn udev_links(dev: &Path) -> Vec<(String, String)> {
    let mut links = Vec::new();
    let Ok(groups) = std::fs::read_dir(dev.join(LINKS)) else {
        return links;
    };
    for group in groups.flatten() {
        let Ok(entries) = std::fs::read_dir(group.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            links.push((
                format!(
                    "{HOST_DEV}/{LINKS}/{}/{}",
                    group.file_name().to_string_lossy(),
                    entry.file_name().to_string_lossy()
                ),
                name.to_owned(),
            ));
        }
    }
    links.sort();
    links
}

/// The binder's arguments that make the session show `host`: `+NAME` for a
/// name whose session node is not the host's, `-NAME` for one the session
/// shows that the host no longer has. A name gone from both is forgotten.
fn changes(
    host: &BTreeMap<String, Node>,
    in_session: impl Fn(&str) -> Option<Node>,
    shown: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut changes: Vec<String> = host
        .iter()
        .filter(|(name, node)| in_session(name) != Some(**node))
        .map(|(name, _)| format!("+{name}"))
        .collect();
    shown.retain(|name| host.contains_key(name) || in_session(name).is_some());
    changes.extend(
        shown
            .iter()
            .filter(|name| !host.contains_key(*name))
            .map(|name| format!("-{name}")),
    );
    changes
}

/// bwrap's child in the session, two levels below the top process (pasta, or
/// the namespace holder, whose other child, pasta, has none).
fn session_process(top: u32) -> Option<u32> {
    let top = i32::try_from(top).ok()?;
    let parents: Vec<(i32, i32)> = procfs::process::all_processes()
        .ok()?
        .flatten()
        .filter_map(|process| Some((process.pid, process.stat().ok()?.ppid)))
        .collect();
    let children = |parent: i32| {
        parents
            .iter()
            .filter(move |(_, ppid)| *ppid == parent)
            .map(|(pid, _)| *pid)
    };
    children(top)
        .flat_map(children)
        .min()
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Wait up to `timeout` for a change in host /dev; true when there was one.
fn wait_for_change(
    inotify: &nix::sys::inotify::Inotify,
    timeout: Duration,
) -> std::io::Result<bool> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut descriptors = [PollFd::new(inotify.as_fd(), PollFlags::POLLIN)];
    let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
    match poll(&mut descriptors, timeout) {
        Ok(0) | Err(nix::errno::Errno::EINTR) => Ok(false),
        Ok(_) => {
            drain(inotify);
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

fn drain(inotify: &nix::sys::inotify::Inotify) {
    while inotify.read_events().is_ok_and(|events| !events.is_empty()) {}
}

/// Run `--serial-bind` in the session of `pid`; the changes it reports done.
fn bind(pid: u32, changes: &[String]) -> BTreeSet<String> {
    let output = std::process::Command::new("/proc/self/exe")
        .arg(BIND_FLAG)
        .arg(pid.to_string())
        .args(changes)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
        Err(error) => {
            eprintln!("{WATCH_FLAG}: start {BIND_FLAG}: {error}");
            BTreeSet::new()
        }
    }
}

/// `--serial-devices TOP_PID [NAME...]`: keep the session below `TOP_PID`
/// showing the host's serial devices. The NAMEs are the ones bwrap bound.
pub fn run_watcher(mut arguments: impl Iterator<Item = String>) -> std::io::Result<()> {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    let top: u32 = arguments
        .next()
        .and_then(|pid| pid.parse().ok())
        .ok_or_else(|| {
            invalid(format!(
                "{WATCH_FLAG} needs the PID of the session's top process"
            ))
        })?;
    let mut shown: BTreeSet<String> = arguments
        .filter(|name| name == LINKS || valid_name(name))
        .collect();
    let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)?;
    inotify.add_watch(
        HOST_DEV,
        AddWatchFlags::IN_CREATE
            | AddWatchFlags::IN_DELETE
            | AddWatchFlags::IN_MOVED_FROM
            | AddWatchFlags::IN_MOVED_TO,
    )?;
    let host_dev = std::fs::metadata(HOST_DEV)?.dev();
    let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
    let mut session: Option<u32> = None;
    // Compare at once: the host may have changed since bwrap's arguments.
    let mut due = Some(Instant::now());
    let mut retry = RETRY_FIRST;
    loop {
        if !alive(top) || session.is_some_and(|pid| !alive(pid)) {
            return Ok(());
        }
        if wait_for_change(&inotify, POLL)? {
            std::thread::sleep(SETTLE);
            drain(&inotify);
            due = Some(Instant::now());
            retry = RETRY_FIRST;
        }
        if due.is_none_or(|at| Instant::now() < at) {
            continue;
        }
        let ready = session.or_else(|| session_process(top)).filter(|pid| {
            // Until bwrap has made the session's /dev, this is the host's.
            std::fs::metadata(format!("/proc/{pid}/root{HOST_DEV}"))
                .is_ok_and(|metadata| metadata.dev() != host_dev)
        });
        let Some(pid) = ready else {
            due = Some(Instant::now() + retry);
            retry = (retry * 2).min(RETRY_LAST);
            continue;
        };
        session = Some(pid);
        let session_dev = PathBuf::from(format!("/proc/{pid}/root{HOST_DEV}"));
        let in_session = |name: &str| {
            std::fs::symlink_metadata(session_dev.join(name))
                .ok()
                .map(|metadata| Node::of(&metadata))
        };
        let wanted = changes(&host_devices(), in_session, &mut shown);
        if wanted.is_empty() {
            due = None;
            continue;
        }
        let done = bind(pid, &wanted);
        for change in &done {
            if let Some(name) = change.strip_prefix('+') {
                shown.insert(name.to_owned());
            } else if let Some(name) = change.strip_prefix('-') {
                shown.remove(name);
            }
        }
        if wanted.iter().all(|change| done.contains(change)) {
            due = None;
            retry = RETRY_FIRST;
        } else {
            due = Some(Instant::now() + retry);
            retry = (retry * 2).min(RETRY_LAST);
        }
    }
}

/// Open `path` as a directory handle that only names it (O_PATH).
fn directory_handle(path: &str) -> std::io::Result<OwnedFd> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(path)
        .map(OwnedFd::from)
}

fn node_of_descriptor(descriptor: OwnedFd) -> std::io::Result<(OwnedFd, Node)> {
    let file = std::fs::File::from(descriptor);
    let node = file.metadata().map(|metadata| Node::of(&metadata));
    let descriptor = OwnedFd::from(file);
    Ok((descriptor, node?))
}

/// The user namespace that owns `namespace`.
fn namespace_owner(namespace: &std::fs::File) -> std::io::Result<OwnedFd> {
    // SAFETY: NS_GET_USERNS reads no argument and returns a new descriptor.
    let descriptor = unsafe { libc::ioctl(namespace.as_raw_fd(), NS_GET_USERNS) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the descriptor is new and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// A detached mount of host /dev/`name`, cloned from this process's mount
/// namespace, when it is still `node`.
fn clone_node(name: &str, node: Node) -> std::io::Result<OwnedFd> {
    let path = CString::new(format!("{HOST_DEV}/{name}")).map_err(std::io::Error::other)?;
    // SAFETY: the path is a NUL-terminated string that outlives the call.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::c_long::from(libc::AT_FDCWD),
            path.as_ptr(),
            libc::c_long::from(OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC),
        )
    };
    let descriptor = i32::try_from(descriptor).map_err(std::io::Error::other)?;
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: open_tree returned a new descriptor that nothing else owns.
    let tree = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let (tree, cloned) = node_of_descriptor(tree)?;
    if cloned != node {
        return Err(std::io::Error::other("the host replaced it meanwhile"));
    }
    if name == LINKS {
        read_only(&tree);
    }
    Ok(tree)
}

/// Make a detached mount read-only, as bwrap's --ro-bind does. Best effort:
/// nobody in the session may write to the host's /dev/serial either way.
fn read_only(tree: &OwnedFd) {
    let attributes = MountAttr {
        attr_set: MOUNT_ATTR_RDONLY,
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: the empty path and the attributes outlive the call, and the
    // size passed is that of the structure.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::c_long::from(tree.as_raw_fd()),
            c"".as_ptr(),
            libc::c_long::from(libc::AT_EMPTY_PATH),
            std::ptr::from_ref(&attributes),
            std::mem::size_of::<MountAttr>(),
        )
    };
}

/// What the current directory has at `name`.
fn current(name: &str) -> Option<Node> {
    std::fs::symlink_metadata(name)
        .ok()
        .map(|metadata| Node::of(&metadata))
}

fn unmount(name: &str) -> std::io::Result<()> {
    let path = CString::new(name).map_err(std::io::Error::other)?;
    // SAFETY: the path is a NUL-terminated string that outlives the call.
    if unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH | libc::UMOUNT_NOFOLLOW) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Remove what the current directory (the session's /dev) has at `name`:
/// unmount it, lazily so a program that has the device open keeps it, and
/// delete the mount point under it.
fn clear(name: &str) -> std::io::Result<()> {
    let own = std::fs::symlink_metadata(".")?.dev();
    for _ in 0..STACKED_MOUNTS {
        match std::fs::symlink_metadata(name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            // Another file system's inode here is a mount.
            Ok(metadata) if metadata.dev() != own => unmount(name)?,
            Ok(metadata) if metadata.is_dir() => return std::fs::remove_dir(name),
            Ok(_) => return std::fs::remove_file(name),
        }
    }
    Err(std::io::Error::other("still mounted after unmounting"))
}

/// Attach `tree`, a clone of `node`, at `name` in the current directory.
fn attach(name: &str, node: Node, tree: &OwnedFd) -> std::io::Result<()> {
    if current(name) == Some(node) {
        return Ok(());
    }
    clear(name)?;
    if name == LINKS {
        std::fs::DirBuilder::new().mode(0o755).create(name)?;
    } else {
        // A device node is mounted on a file.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(name)?;
    }
    let target = CString::new(name).map_err(std::io::Error::other)?;
    // SAFETY: both paths are NUL-terminated strings that outlive the call. The
    // target is not followed if it is a symlink: MOVE_MOUNT_T_SYMLINKS is unset.
    let moved = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            libc::c_long::from(tree.as_raw_fd()),
            c"".as_ptr(),
            libc::c_long::from(libc::AT_FDCWD),
            target.as_ptr(),
            libc::c_long::from(MOVE_MOUNT_F_EMPTY_PATH),
        )
    };
    if moved != 0 {
        let error = std::io::Error::last_os_error();
        let _ = clear(name);
        return Err(error);
    }
    Ok(())
}

/// `--serial-bind PID [+NAME|-NAME]...`: show (`+`) or remove (`-`) host serial
/// devices in the session whose bwrap child is PID, and print each change done.
/// It must run before the process has a second thread: a user namespace can
/// only be entered by a single-threaded process.
pub fn run_binder(mut arguments: impl Iterator<Item = String>) -> std::io::Result<()> {
    use nix::sched::{CloneFlags, setns, unshare};
    let pid: u32 = arguments
        .next()
        .and_then(|pid| pid.parse().ok())
        .ok_or_else(|| invalid(format!("{BIND_FLAG} needs the PID of bwrap's child")))?;
    let (mut adds, mut removes) = (Vec::new(), Vec::new());
    for argument in arguments {
        let (sign, name) = argument
            .split_at_checked(1)
            .ok_or_else(|| invalid(format!("{BIND_FLAG}: empty change")))?;
        if name != LINKS && !valid_name(name) {
            return Err(invalid(format!("{BIND_FLAG}: not a device name: {name}")));
        }
        match sign {
            "+" => adds.push(name.to_owned()),
            "-" => removes.push(name.to_owned()),
            _ => {
                return Err(invalid(format!(
                    "{BIND_FLAG}: {argument} is not +NAME or -NAME"
                )));
            }
        }
    }
    // While it is in the session, no session process may read this one.
    nix::sys::prctl::set_dumpable(false)?;
    let host_dev = std::fs::metadata(HOST_DEV)?.dev();
    let namespace = std::fs::File::open(format!("/proc/{pid}/ns/mnt"))?;
    // The session's /dev, held before entering: everything below is relative
    // to it, and the namespace's root need not be the session's.
    let session_dev = directory_handle(&format!("/proc/{pid}/root{HOST_DEV}"))?;
    let (session_dev, session_dev_node) = node_of_descriptor(session_dev)?;
    if session_dev_node.dev == host_dev {
        return Err(std::io::Error::other(
            "the session's /dev is not set up yet",
        ));
    }
    let owner = namespace_owner(&namespace)?;
    // Only the host's serial devices, as they are now.
    let host = host_devices();
    let mut wanted = Vec::new();
    for name in adds {
        match host.get(&name) {
            Some(node) => wanted.push((name, *node)),
            None => eprintln!("{BIND_FLAG}: +{name}: the host has no such serial device"),
        }
    }
    setns(&owner, CloneFlags::CLONE_NEWUSER)?;
    // A copy of the host's mount namespace that the session's user namespace
    // owns, so that its nodes may be cloned.
    unshare(CloneFlags::CLONE_NEWNS)?;
    let mut trees = Vec::new();
    for (name, node) in wanted {
        match clone_node(&name, node) {
            Ok(tree) => trees.push((name, node, tree)),
            Err(error) => eprintln!("{BIND_FLAG}: +{name}: {error}"),
        }
    }
    setns(&namespace, CloneFlags::CLONE_NEWNS)?;
    // SAFETY: fchdir reads the descriptor, which stays open for the call.
    if unsafe { libc::fchdir(session_dev.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut done = std::io::stdout().lock();
    for name in removes {
        match clear(&name) {
            Ok(()) => writeln!(done, "-{name}")?,
            Err(error) => eprintln!("{BIND_FLAG}: -{name}: {error}"),
        }
    }
    for (name, node, tree) in trees {
        match attach(&name, node, &tree) {
            Ok(()) => writeln!(done, "+{name}")?,
            Err(error) => eprintln!("{BIND_FLAG}: +{name}: {error}"),
        }
    }
    done.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        Node, Numbers, changes, report_under, scan, serial_drivers, serial_names, valid_name,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    /// /proc/tty/drivers of a host with a USB gadget port loaded.
    const DRIVERS: &str = "\
/dev/tty             /dev/tty        5       0 system:/dev/tty
/dev/console         /dev/console    5       1 system:console
/dev/ptmx            /dev/ptmx       5       2 system
/dev/vc/0            /dev/vc/0       4       0 system:vtmaster
g_serial             /dev/ttyGS    511 0-7 serial
acm                  /dev/ttyACM   166 0-255 serial
ttyprintk            /dev/ttyprintk   5       3 console
serial_8250          /dev/ttyS       4 64-95 serial
pty_slave            /dev/pts      136 0-1048575 pty:slave
unknown              /dev/tty        4 1-63 console
";

    struct Tree(PathBuf);

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(tag: &str) -> std::io::Result<Tree> {
        let root =
            std::env::temp_dir().join(format!("kwin-mcp-serial-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        Ok(Tree(root))
    }

    /// A sysfs tty entry with its device number and, for a serial-core port,
    /// its UART type.
    fn tty(class: &Path, name: &str, number: &str, kind: Option<&str>) -> std::io::Result<()> {
        let entry = class.join(name);
        std::fs::create_dir_all(&entry)?;
        std::fs::write(entry.join("dev"), format!("{number}\n"))?;
        if let Some(kind) = kind {
            std::fs::write(entry.join("type"), format!("{kind}\n"))?;
        }
        Ok(())
    }

    #[test]
    fn the_driver_table_gives_the_serial_numbers() {
        let numbers = |major, first, last| Numbers { major, first, last };
        assert_eq!(
            serial_drivers(DRIVERS),
            [numbers(511, 0, 7), numbers(166, 0, 255), numbers(4, 64, 95)]
        );
    }

    #[test]
    fn names_are_plain_tty_names() {
        for name in [
            "ttyACM1", "ttyUSB0", "ttyS31", "rfcomm0", "ttyGS0", "ttymxc0",
        ] {
            assert!(valid_name(name), "{name}");
        }
        for name in [
            "",
            ".",
            "..",
            "serial",
            "dri/card0",
            "tty name",
            "$(x)",
            &"t".repeat(65),
        ] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn only_ttys_of_serial_drivers_count() -> std::io::Result<()> {
        let root = tree("names")?;
        let class = root.0.join("class");
        tty(&class, "ttyACM1", "166:1", None)?;
        // A gadget port: no device link in sysfs, still a serial driver's.
        tty(&class, "ttyGS0", "511:0", None)?;
        tty(&class, "ttyS0", "4:64", Some("4"))?;
        tty(&class, "tty1", "4:1", None)?;
        tty(&class, "console", "5:1", None)?;
        tty(&class, "ttyprintk", "5:3", None)?;
        let names: Vec<String> = serial_names(&class, &serial_drivers(DRIVERS))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["ttyACM1", "ttyGS0", "ttyS0"]);
        Ok(())
    }

    #[test]
    fn a_name_needs_its_character_device() -> std::io::Result<()> {
        let root = tree("nodes")?;
        let (class, dev) = (root.0.join("class"), root.0.join("dev"));
        tty(&class, "ttyACM1", "166:1", None)?;
        std::fs::create_dir_all(&dev)?;
        let drivers = serial_drivers(DRIVERS);
        // A regular file is no device node: nothing to show.
        std::fs::write(dev.join("ttyACM1"), "")?;
        assert!(scan(&class, &dev, &drivers).is_empty());
        // The link directory is kept on its own.
        std::fs::create_dir_all(dev.join("serial/by-id"))?;
        assert_eq!(
            scan(&class, &dev, &drivers).keys().collect::<Vec<_>>(),
            ["serial"]
        );
        Ok(())
    }

    #[test]
    fn the_report_names_ports_with_hardware_and_their_links() -> std::io::Result<()> {
        let root = tree("report")?;
        let (class, dev) = (root.0.join("class"), root.0.join("dev"));
        tty(&class, "ttyACM1", "166:1", None)?;
        tty(&class, "ttyS0", "4:64", Some("4"))?;
        tty(&class, "ttyS1", "4:65", Some("0"))?;
        let by_id = dev.join("serial/by-id");
        std::fs::create_dir_all(&by_id)?;
        std::os::unix::fs::symlink("../../ttyACM1", by_id.join("usb-KitProg3-if02"))?;
        let node = Node { dev: 1, ino: 1 };
        let devices: BTreeMap<String, Node> = ["ttyACM1", "ttyS0", "ttyS1", "serial"]
            .into_iter()
            .map(|name| (name.to_owned(), node))
            .collect();
        assert_eq!(
            serde_json::Value::from(report_under(&class, &dev, &devices)),
            serde_json::json!([
                {"path": "/dev/ttyACM1", "links": ["/dev/serial/by-id/usb-KitProg3-if02"]},
                {"path": "/dev/ttyS0"},
            ])
        );
        Ok(())
    }

    #[test]
    fn changes_bind_new_nodes_and_remove_vanished_ones() {
        let (old, new) = (Node { dev: 9, ino: 1 }, Node { dev: 9, ino: 2 });
        let session: BTreeMap<&str, Node> =
            [("ttyACM0", old), ("ttyACM1", old), ("ttyUSB0", old)].into();
        let host: BTreeMap<String, Node> = [
            ("ttyACM0".to_owned(), old),
            // Unplugged and plugged in again: a new inode.
            ("ttyACM1".to_owned(), new),
            // Plugged in after the session started.
            ("ttyACM2".to_owned(), new),
        ]
        .into();
        let mut shown: BTreeSet<String> = ["ttyACM0", "ttyACM1", "ttyUSB0", "ttyS4"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let wanted = changes(&host, |name| session.get(name).copied(), &mut shown);
        assert_eq!(wanted, ["+ttyACM1", "+ttyACM2", "-ttyUSB0"]);
        // Gone from the host and from the session: nothing left to remove.
        assert!(!shown.contains("ttyS4"));
    }
}
