//! Reaching the host's loopback listeners from a session.
//!
//! A session has its own network namespace and with it its own loopback. pasta
//! can forward it (`--tcp-ns auto`): it lists the host's listening ports once a
//! second and binds each of them in the namespace on a dual-stack socket. The
//! list has ports only, not addresses, so a server the host binds on 127.0.0.1
//! alone is also offered on the session's ::1. A client that asks for
//! `localhost` tries ::1 first; pasta accepts, cannot connect to the host's ::1,
//! and resets the connection. Whether the client sees a refused connect (and
//! tries 127.0.0.1) or an established one that is then reset (and gives up)
//! depends on which of the two processes runs first: on an idle machine 156 of
//! 200 `curl http://localhost:PORT` requests failed with "connection reset by
//! peer", and Chrome showed its error page under the title "localhost" for half
//! of its navigations (#862). pasta takes no listening address for forwards in
//! this direction, so the ports cannot be split by family there.
//!
//! The session's pasta therefore forwards no TCP port into the host, and this
//! forwarder does: it lists the host's listeners with their addresses, and
//! offers each port in the session only on the loopback address of a family the
//! host accepts it on. A connect to the other family is refused by the
//! session's kernel, which is what the same connect gets on the host.
//!
//! Two processes do it, both this binary. `--loopback-forwarder` runs on the
//! host beside the session's proxies: it scans, accepts and copies bytes.
//! `--loopback-sockets` is its child: it joins the session's user and network
//! namespaces (the user namespace was made by pasta or by the namespace holder
//! as this user, which is what grants the right to enter its network
//! namespace), and only makes listening
//! sockets there, which it passes back over a socket pair. A socket keeps the
//! namespace it was made in, so the forwarder accepts session connections while
//! it connects to the host's loopback itself.

use std::collections::{BTreeSet, HashMap};
use std::io::{IoSlice, IoSliceMut, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Server flag: forward the host's loopback listeners into a session.
pub const FORWARDER_FLAG: &str = "--loopback-forwarder";
/// The forwarder's child: make listening sockets inside the session.
pub const SOCKETS_FLAG: &str = "--loopback-sockets";

/// How often the host's listeners are listed. A server started on the host is
/// reachable from the session within this time.
const SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// How long the forwarder waits for the session's namespaces to exist.
const NAMESPACE_WAIT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LISTEN_BACKLOG: i32 = 128;
/// The state column of a listening socket in /proc/net/tcp.
const TCP_LISTEN: &str = "0A";
/// Answers of the socket maker: a listener follows, or the errno of the failure.
const MADE: u8 = 0;

/// One address family of a loopback port.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    fn loopback(self) -> IpAddr {
        match self {
            Self::V4 => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Self::V6 => IpAddr::V6(Ipv6Addr::LOCALHOST),
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::V4 => 4,
            Self::V6 => 6,
        }
    }
}

/// The local address of one /proc/net/tcp or /proc/net/tcp6 row. The kernel
/// prints each 32-bit word of the address as a native-endian number.
fn row_address(field: &str) -> Option<(IpAddr, u16)> {
    let (address, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let mut bytes = Vec::with_capacity(16);
    for word in address.as_bytes().chunks(8) {
        let word = u32::from_str_radix(std::str::from_utf8(word).ok()?, 16).ok()?;
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    let address = match bytes.len() {
        4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => return None,
    };
    Some((address, port))
}

/// The loopback ports a connection from this machine reaches, by family, from
/// the text of /proc/net/tcp and /proc/net/tcp6. A listener on a wildcard
/// address counts, one on another interface's address does not. A listener on
/// `::` is taken to accept IPv4 as well, which is the kernel's default
/// (the tables do not show IPV6_V6ONLY).
pub fn loopback_listeners(tcp: &str, tcp6: &str) -> BTreeSet<(Family, u16)> {
    let mut listeners = BTreeSet::new();
    for row in tcp.lines().chain(tcp6.lines()) {
        let mut fields = row.split_whitespace().skip(1);
        let (Some(local), _, Some(state)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if state != TCP_LISTEN {
            continue;
        }
        let Some((address, port)) = row_address(local) else {
            continue;
        };
        let local = |address: Ipv4Addr| address.is_loopback() || address.is_unspecified();
        match address {
            IpAddr::V4(address) if local(address) => {
                listeners.insert((Family::V4, port));
            }
            IpAddr::V6(address) if address.is_unspecified() => {
                listeners.insert((Family::V4, port));
                listeners.insert((Family::V6, port));
            }
            IpAddr::V6(address) if address.is_loopback() => {
                listeners.insert((Family::V6, port));
            }
            IpAddr::V6(address) if address.to_ipv4_mapped().is_some_and(local) => {
                listeners.insert((Family::V4, port));
            }
            IpAddr::V4(_) | IpAddr::V6(_) => {}
        }
    }
    listeners
}

fn host_listeners() -> std::io::Result<BTreeSet<(Family, u16)>> {
    let tcp = std::fs::read_to_string("/proc/net/tcp")?;
    // A host without IPv6 has no second table.
    let tcp6 = std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default();
    Ok(loopback_listeners(&tcp, &tcp6))
}

fn invalid(text: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, text.into())
}

/// A listening socket on the loopback address of `family`, in the network
/// namespace of the calling thread.
fn listen_on_loopback(family: Family, port: u16) -> std::io::Result<OwnedFd> {
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, SockaddrIn6, bind, listen,
        setsockopt, socket, sockopt,
    };
    let domain = match family {
        Family::V4 => AddressFamily::Inet,
        Family::V6 => AddressFamily::Inet6,
    };
    let fd = socket(domain, SockType::Stream, SockFlag::SOCK_CLOEXEC, None)?;
    setsockopt(&fd, sockopt::ReuseAddr, &true)?;
    match family {
        Family::V4 => bind(
            fd.as_raw_fd(),
            &SockaddrIn::from(std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        )?,
        Family::V6 => {
            setsockopt(&fd, sockopt::Ipv6V6Only, &true)?;
            bind(
                fd.as_raw_fd(),
                &SockaddrIn6::from(std::net::SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 0, 0)),
            )?
        }
    }
    listen(
        &fd,
        Backlog::new(LISTEN_BACKLOG).map_err(std::io::Error::from)?,
    )?;
    Ok(fd)
}

fn enter_namespace(pid: u32, name: &str, kind: nix::libc::c_int) -> std::io::Result<()> {
    let namespace = std::fs::File::open(format!("/proc/{pid}/ns/{name}"))?;
    // setns only reads the descriptor, which stays open for the call.
    if unsafe { nix::libc::setns(namespace.as_raw_fd(), kind) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `--loopback-sockets PID`: join the user and network namespaces of `PID`, then
/// answer each `[family, port]` request on stdin with a listening socket. It
/// must run before the process has a second thread: a user namespace can only
/// be entered by a single-threaded process.
pub fn run_socket_maker(mut arguments: impl Iterator<Item = String>) -> std::io::Result<()> {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    let pid: u32 = arguments
        .next()
        .and_then(|pid| pid.parse().ok())
        .ok_or_else(|| {
            invalid(format!(
                "{SOCKETS_FLAG} needs the PID of a process in the session"
            ))
        })?;
    enter_namespace(pid, "user", nix::libc::CLONE_NEWUSER)?;
    enter_namespace(pid, "net", nix::libc::CLONE_NEWNET)?;
    let mut requests = std::io::stdin().lock();
    let replies = std::io::stdin().as_raw_fd();
    sendmsg::<()>(
        replies,
        &[IoSlice::new(&[MADE])],
        &[],
        MsgFlags::empty(),
        None,
    )?;
    let mut request = [0u8; 3];
    loop {
        match requests.read_exact(&mut request) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let family = if request[0] == Family::V6.code() {
            Family::V6
        } else {
            Family::V4
        };
        let port = u16::from_be_bytes([request[1], request[2]]);
        match listen_on_loopback(family, port) {
            Ok(listener) => {
                let descriptors = [listener.as_raw_fd()];
                sendmsg::<()>(
                    replies,
                    &[IoSlice::new(&[MADE])],
                    &[ControlMessage::ScmRights(&descriptors)],
                    MsgFlags::empty(),
                    None,
                )?;
            }
            Err(error) => {
                // Zero means made, so an error without a number is reported as EIO.
                let errno = error
                    .raw_os_error()
                    .and_then(|errno| u8::try_from(errno).ok());
                let code = errno.filter(|errno| *errno != MADE).unwrap_or(5);
                sendmsg::<()>(
                    replies,
                    &[IoSlice::new(&[code])],
                    &[],
                    MsgFlags::empty(),
                    None,
                )?;
            }
        }
    }
}

/// The forwarder's end of the socket pair to the socket maker.
struct SocketMaker {
    channel: UnixStream,
    _child: std::process::Child,
}

impl SocketMaker {
    fn start(namespace_pid: u32) -> std::io::Result<Self> {
        let (mut channel, theirs) = UnixStream::pair()?;
        let mut child = std::process::Command::new("/proc/self/exe")
            .arg(SOCKETS_FLAG)
            .arg(namespace_pid.to_string())
            .stdin(OwnedFd::from(theirs))
            .stdout(std::process::Stdio::null())
            .spawn()?;
        // It says so once it is inside the namespaces; if it could not enter
        // them it exits, and the read ends without that byte.
        let mut entered = [0u8; 1];
        if let Err(error) = channel.read_exact(&mut entered) {
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            channel,
            _child: child,
        })
    }

    /// A listener in the session. An outer error means the maker is gone, and
    /// with it the session; an inner one that this port could not be bound.
    fn listen(&mut self, family: Family, port: u16) -> std::io::Result<std::io::Result<OwnedFd>> {
        use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
        let [high, low] = port.to_be_bytes();
        self.channel.write_all(&[family.code(), high, low])?;
        let mut status = [0u8; 1];
        let mut space = nix::cmsg_space!([RawFd; 1]);
        let (bytes, descriptor) = {
            let mut buffers = [IoSliceMut::new(&mut status)];
            let message = recvmsg::<()>(
                self.channel.as_raw_fd(),
                &mut buffers,
                Some(&mut space),
                MsgFlags::MSG_CMSG_CLOEXEC,
            )?;
            let mut descriptor = None;
            for control in message.cmsgs()? {
                if let ControlMessageOwned::ScmRights(descriptors) = control {
                    for raw in descriptors {
                        // The kernel made this descriptor for this process just
                        // now; nothing else owns it.
                        descriptor = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                }
            }
            (message.bytes, descriptor)
        };
        if bytes == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(match (status[0], descriptor) {
            (MADE, Some(listener)) => Ok(listener),
            (MADE, None) => Err(invalid("the socket maker sent no listener")),
            (errno, _) => Err(std::io::Error::from_raw_os_error(i32::from(errno))),
        })
    }
}

/// A child of the session's top process that is in another network namespace
/// than this one: the command pasta runs in its new namespaces, or, when the
/// top process is the namespace holder (a server under no_new_privs), its
/// bwrap; the holder's other child, the attached pasta, stays in the host's.
fn namespace_process(top: u32) -> Option<u32> {
    let own = std::fs::read_link("/proc/self/ns/net").ok()?;
    let top = i32::try_from(top).ok()?;
    procfs::process::all_processes()
        .ok()?
        .flatten()
        .filter(|process| process.stat().is_ok_and(|stat| stat.ppid == top))
        .filter(|process| {
            std::fs::read_link(format!("/proc/{}/ns/net", process.pid))
                .is_ok_and(|namespace| namespace != own)
        })
        .filter_map(|process| u32::try_from(process.pid).ok())
        .min()
}

/// Close `stream` with a reset: the host refused the connection, and a client
/// must not take that for an empty answer.
fn reset(stream: &tokio::net::TcpStream) {
    let linger = nix::libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    let _ = nix::sys::socket::setsockopt(stream, nix::sys::socket::sockopt::Linger, &linger);
}

async fn serve(listener: tokio::net::TcpListener, family: Family, port: u16) {
    loop {
        let Ok((mut inbound, _)) = listener.accept().await else {
            tokio::time::sleep(SCAN_INTERVAL).await;
            continue;
        };
        tokio::spawn(async move {
            let host = SocketAddr::new(family.loopback(), port);
            let connected =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(host)).await;
            match connected {
                Ok(Ok(mut outbound)) => {
                    let _ = inbound.set_nodelay(true);
                    let _ = outbound.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
                Ok(Err(_)) | Err(_) => reset(&inbound),
            }
        });
    }
}

/// `--loopback-forwarder TOP_PID EXCLUDED_PORT`: until the session's top process
/// (pasta, or the namespace holder) ends, keep
/// one listener in it for every loopback port and family the host listens on,
/// except `EXCLUDED_PORT` (the session's own debugging port, which pasta
/// forwards the other way), and carry each connection to the host's loopback.
pub fn run_forwarder(mut arguments: impl Iterator<Item = String>) -> std::io::Result<()> {
    let mut number = |what: &str| {
        arguments
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| invalid(format!("{FORWARDER_FLAG} needs {what}")))
    };
    let top = number("the PID of the session's top process")?;
    let excluded = u16::try_from(number("the port to leave out")?)
        .map_err(|_| invalid("port out of range"))?;
    let alive = || std::path::Path::new(&format!("/proc/{top}")).exists();
    let deadline = std::time::Instant::now() + NAMESPACE_WAIT;
    // The maker is started before the runtime: it is this process image, and
    // has to be single-threaded when it enters the session's user namespace.
    // A process seen in the namespaces can be a short-lived helper of pasta
    // that is gone before the maker opens its namespaces, so this is retried.
    let mut maker = loop {
        let started = namespace_process(top).map(SocketMaker::start);
        match started {
            Some(Ok(maker)) => break maker,
            Some(Err(_)) | None if alive() && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Some(Err(error)) => return Err(error),
            None => return Err(invalid(format!("process {top} has no session namespaces"))),
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut serving: HashMap<(Family, u16), tokio::task::JoinHandle<()>> = HashMap::new();
        let mut refused: BTreeSet<(Family, u16)> = BTreeSet::new();
        loop {
            if !alive() {
                return Ok(());
            }
            let mut wanted = host_listeners()?;
            wanted.retain(|(_, port)| *port != excluded);
            serving.retain(|key, task| {
                let keep = wanted.contains(key);
                if !keep {
                    task.abort();
                }
                keep
            });
            refused.retain(|key| wanted.contains(key));
            for key in wanted {
                if serving.contains_key(&key) {
                    continue;
                }
                let (family, port) = key;
                let listener = match maker.listen(family, port) {
                    // The session is gone.
                    Err(_) => return Ok(()),
                    Ok(listener) => listener,
                };
                let listener = listener.and_then(|listener| {
                    let listener = std::net::TcpListener::from(listener);
                    listener.set_nonblocking(true)?;
                    tokio::net::TcpListener::from_std(listener)
                });
                match listener {
                    Ok(listener) => {
                        refused.remove(&key);
                        serving.insert(key, tokio::spawn(serve(listener, family, port)));
                    }
                    // Usually a session process listening on the port itself;
                    // said once, and tried again while the host listens.
                    Err(error) => {
                        if refused.insert(key) {
                            eprintln!("host port {port} ({family:?}) is not forwarded into the session: {error}");
                        }
                    }
                }
            }
            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{Family, loopback_listeners};

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    fn row(local: &str, state: &str) -> String {
        format!(
            "   0: {local} 00000000:0000 {state} 00000000:00000000 00:00000000 00000000  1000        0 1 1 0 100 0 0 10 0\n"
        )
    }

    fn word(bytes: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(bytes))
    }

    #[test]
    fn a_port_is_offered_only_in_the_families_the_host_accepts() {
        let v4 = |bytes: [u8; 4], port: u16| format!("{}:{port:04X}", word(bytes));
        let v6 = |bytes: [u8; 16], port: u16| {
            let words: String = bytes
                .chunks(4)
                .map(|c| word([c[0], c[1], c[2], c[3]]))
                .collect();
            format!("{words}:{port:04X}")
        };
        let mut loopback6 = [0u8; 16];
        loopback6[15] = 1;
        let mut mapped = [0u8; 16];
        mapped[10..].copy_from_slice(&[0xff, 0xff, 127, 0, 0, 1]);
        let tcp = [
            HEADER.to_owned(),
            row(&v4([127, 0, 0, 1], 38113), "0A"),
            row(&v4([0, 0, 0, 0], 8080), "0A"),
            // Another interface's address is not reachable through loopback.
            row(&v4([192, 168, 1, 6], 9000), "0A"),
            // An established connection is not a listener.
            row(&v4([127, 0, 0, 1], 5555), "01"),
        ]
        .concat();
        let tcp6 = [
            HEADER.to_owned(),
            row(&v6(loopback6, 5173), "0A"),
            row(&v6([0u8; 16], 3000), "0A"),
            row(&v6(mapped, 7000), "0A"),
        ]
        .concat();
        let listeners: Vec<_> = loopback_listeners(&tcp, &tcp6).into_iter().collect();
        assert_eq!(
            listeners,
            vec![
                (Family::V4, 3000),
                (Family::V4, 7000),
                (Family::V4, 8080),
                // The server of #862: bound on 127.0.0.1, so not offered on ::1.
                (Family::V4, 38113),
                (Family::V6, 3000),
                (Family::V6, 5173),
            ]
        );
    }

    #[test]
    fn rows_that_are_not_sockets_are_skipped() {
        assert!(loopback_listeners(HEADER, "garbage\n\n   1: nonsense 0A\n").is_empty());
    }
}
