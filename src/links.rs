//! The links a remote viewer's pictures cross: how the streams that leave one
//! interface share its budget, and which of the caller's interfaces a new
//! stream arrives on.
//!
//! # Sharing an interface ([`Shares`])
//!
//! Every frame sender is a process of its own (`kwin-viewer --stream-serve`),
//! and each paces its socket in the kernel. The listener that started them
//! keeps one [`Shares`] per interface and a socket to every sender. A sender
//! says when a picture goes onto the wire and when its last byte has left
//! ([`Report::Wire`]); the listener answers every change with a [`Grant`] to
//! each sender of that interface:
//!
//! * `pace`: what the sender's socket is paced to now. The budget is divided
//!   among the streams that have a picture on the wire at this moment, so the
//!   paces of the sending streams add up to the budget, a stream alone has
//!   all of it, and the share of a stream that stops is taken over at once.
//!   A stream that is not sending is told the pace it would start with.
//! * `share`: what the stream has while the streams around it keep sending
//!   pictures in a row. A stream counts as streaming from its second picture
//!   within [`SHARE_LINGER`] of the one before until [`SHARE_LINGER`] after
//!   its last, so one picture after a still screen (a click, a blinking
//!   cursor) changes nobody's share, and the gaps between the pictures of a
//!   scrolling screen do not either. `ModeControl` decides between exact
//!   pixels and DSC with this number, which therefore stays as steady as the
//!   set of streaming viewers.
//!
//! A stream whose viewer sits behind a slower link reports that link's speed
//! ([`Report::Cap`]); it gets no more than that, and the rest goes to the
//! others (max-min fairness).
//!
//! # Paths ([`paths_to`], [`choose`])
//!
//! A path is one of the caller's interfaces that reaches the session's
//! machine: it is up, has a carrier and holds an address in the subnet of the
//! peer (or is the one the routing table uses). A stream stays on the path it
//! started on. A new stream takes the path that gives it the largest fair
//! share, where the fair share of a path is its usable speed divided by the
//! streams it would then carry, and no more than the sending machine's
//! interface gives a further stream. A wired path wins ties and is always
//! taken while it carries no stream; a wireless path is used only when its
//! fair share is larger than the wired one's. The streams of every kwin-mcp
//! server of this user are counted ([`Registry`]).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long after the last byte of a picture its stream still counts as
/// streaming, and how close to the picture before it the next must begin for
/// the stream to start counting.
pub const SHARE_LINGER: Duration = Duration::from_millis(250);
/// Share of a wired link's speed a stream may fill: `viewer_stream`'s.
pub const WIRED_USABLE_PERCENT: u64 = crate::viewer_stream::LINK_USABLE_PERCENT;
/// Share of the rate a wireless interface receives at that TCP delivers: the
/// rest is the medium's own overhead (acknowledgements, contention,
/// retransmissions).
pub const WIRELESS_USABLE_PERCENT: u64 = 50;
/// File beside a stream viewer's log that names its path and current share.
pub const PATH_FILE: &str = "viewer-stream-path.json";
/// Socket beside a stream viewer's log on which the kwin-mcp server that
/// started the viewer hands out the ticket of a new frame socket, for a
/// viewer whose path died.
pub const RECONNECT_SOCKET: &str = "reconnect.sock";
/// What a viewer writes on [`RECONNECT_SOCKET`] to get a ticket (one line).
pub const RECONNECT_REQUEST: &str = "ticket";
/// Names a JSON file for `kwin-viewer --stream` that lists the paths to use
/// instead of this machine's interfaces ([`PathTable`]); for tests.
pub const PATHS_ENV: &str = "KWIN_VIEWER_STREAM_PATHS";
const BITS_PER_MEGABIT: u64 = 1_000_000;

/// The first of `items` that `wanted` accepts.
pub fn first<T>(
    items: impl IntoIterator<Item = T>,
    mut wanted: impl FnMut(&T) -> bool,
) -> Option<T> {
    let mut items = items.into_iter();
    loop {
        let item = items.next()?;
        if wanted(&item) {
            return Some(item);
        }
    }
}

/// What the listener grants one sender, in bits per second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Grant {
    /// What the socket is paced to now.
    pub pace: u64,
    /// What the stream has while the streaming streams keep streaming.
    pub share: u64,
    /// The streams `share` divides the budget among, this one included.
    pub streams: u32,
}

/// Length of an encoded [`Grant`]: two u64 and a u32, little-endian.
pub const GRANT_BYTES: usize = 20;

impl Grant {
    pub fn encode(&self) -> [u8; GRANT_BYTES] {
        let mut bytes = [0u8; GRANT_BYTES];
        bytes[..8].copy_from_slice(&self.pace.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.share.to_le_bytes());
        bytes[16..].copy_from_slice(&self.streams.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; GRANT_BYTES]) -> Self {
        let word = |at: usize| {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(word)
        };
        let mut streams = [0u8; 4];
        streams.copy_from_slice(&bytes[16..]);
        Self {
            pace: word(0),
            share: word(8),
            streams: u32::from_le_bytes(streams),
        }
    }
}

/// What a sender tells the listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    /// A picture went onto the wire (true), or its last byte left (false).
    Wire(bool),
    /// The viewer's own link carries this many bits per second; None: no
    /// limit besides this interface.
    Cap(Option<u64>),
}

/// Length of an encoded [`Report`]: a tag and a little-endian u64.
pub const REPORT_BYTES: usize = 9;
const REPORT_OFF_WIRE: u8 = 0;
const REPORT_ON_WIRE: u8 = 1;
const REPORT_CAP: u8 = 2;

impl Report {
    pub fn encode(&self) -> [u8; REPORT_BYTES] {
        let (tag, value) = match self {
            Self::Wire(false) => (REPORT_OFF_WIRE, 0),
            Self::Wire(true) => (REPORT_ON_WIRE, 0),
            Self::Cap(cap) => (REPORT_CAP, cap.unwrap_or(0)),
        };
        let mut bytes = [0u8; REPORT_BYTES];
        bytes[0] = tag;
        bytes[1..].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    /// None for a record of a kind this build does not know.
    pub fn decode(bytes: &[u8; REPORT_BYTES]) -> Option<Self> {
        let mut value = [0u8; 8];
        value.copy_from_slice(&bytes[1..]);
        let value = u64::from_le_bytes(value);
        match bytes[0] {
            REPORT_OFF_WIRE => Some(Self::Wire(false)),
            REPORT_ON_WIRE => Some(Self::Wire(true)),
            REPORT_CAP => Some(Self::Cap((value > 0).then_some(value))),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Member {
    on_wire: bool,
    /// When the last byte of its last picture left.
    ended: Option<Instant>,
    /// It sends pictures in a row.
    streaming: bool,
    cap: Option<u64>,
}

/// The streams of one interface and what each may send. See the module text.
#[derive(Clone, Debug)]
pub struct Shares {
    budget: u64,
    members: BTreeMap<u64, Member>,
}

impl Shares {
    /// `budget`: bits per second the interface's streams may fill together.
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            members: BTreeMap::new(),
        }
    }

    pub fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
    }

    pub fn join(&mut self, id: u64) {
        self.members.insert(id, Member::default());
    }

    pub fn leave(&mut self, id: u64) {
        self.members.remove(&id);
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Stream `id` reported at `now`.
    pub fn report(&mut self, id: u64, report: Report, now: Instant) {
        let Some(member) = self.members.get_mut(&id) else {
            return;
        };
        match report {
            Report::Wire(true) => {
                member.streaming = member
                    .ended
                    .is_some_and(|ended| now.saturating_duration_since(ended) < SHARE_LINGER);
                member.on_wire = true;
            }
            Report::Wire(false) => {
                member.on_wire = false;
                member.ended = Some(now);
            }
            Report::Cap(cap) => member.cap = cap,
        }
    }

    /// Stops counting the streams whose last picture left [`SHARE_LINGER`]
    /// before `now`, and returns when the next one is due.
    pub fn expire(&mut self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for member in self.members.values_mut() {
            if !member.streaming || member.on_wire {
                continue;
            }
            match member.ended.map(|ended| ended + SHARE_LINGER) {
                Some(due) if due > now => next = Some(next.map_or(due, |next| next.min(due))),
                Some(_) | None => member.streaming = false,
            }
        }
        next
    }

    /// What `id` gets of the budget beside the other streams that `counts`
    /// accepts, and how many streams that is: every stream gets an equal
    /// part, except that one with a cap below its part gets the cap and
    /// leaves the rest to the others.
    fn allocation(&self, id: u64, counts: impl Fn(&Member) -> bool) -> (u64, u32) {
        let mut caps: Vec<(u64, bool)> = self
            .members
            .iter()
            .filter(|(other, member)| **other == id || counts(member))
            .map(|(other, member)| (member.cap.unwrap_or(u64::MAX), *other == id))
            .collect();
        caps.sort_unstable();
        let streams = u32::try_from(caps.len()).unwrap_or(u32::MAX);
        let mut left = self.budget;
        let mut sharing = u64::from(streams);
        for (cap, own) in caps {
            let part = cap.min(left / sharing.max(1));
            if own {
                return (part.max(1), streams);
            }
            left -= part;
            sharing -= 1;
        }
        (self.budget.max(1), 1)
    }

    /// The grant of every stream.
    pub fn grants(&self) -> Vec<(u64, Grant)> {
        self.members
            .keys()
            .map(|id| {
                let (pace, _) = self.allocation(*id, |member| member.on_wire);
                let (share, streams) = self.allocation(*id, |member| member.streaming);
                (
                    *id,
                    Grant {
                        pace,
                        share,
                        streams,
                    },
                )
            })
            .collect()
    }
}

/// One network interface of this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub loopback: bool,
    /// Up, with a carrier.
    pub running: bool,
    pub wireless: bool,
    /// Mb/s the kernel reports: the negotiated speed of a wired link, the
    /// rate a wireless one receives at. None when it reports none.
    pub mbps: Option<u64>,
    /// Addresses with their prefix lengths.
    pub addresses: Vec<(IpAddr, u8)>,
}

impl Interface {
    pub fn holds(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        self.addresses.iter().any(|(own, _)| *own == address)
    }
}

fn prefix_length(mask: IpAddr) -> u8 {
    let ones = match mask {
        IpAddr::V4(mask) => u32::from(mask).leading_ones(),
        IpAddr::V6(mask) => u128::from(mask).leading_ones(),
    };
    u8::try_from(ones).unwrap_or(u8::MAX)
}

fn socket_ip(address: Option<&nix::sys::socket::SockaddrStorage>) -> Option<IpAddr> {
    let v4 = address
        .and_then(|address| address.as_sockaddr_in())
        .map(|address| IpAddr::V4(address.ip()));
    let v6 = address
        .and_then(|address| address.as_sockaddr_in6())
        .map(|address| IpAddr::V6(address.ip()).to_canonical());
    v4.or(v6)
}

/// This machine's interfaces. An interface without an address is listed too.
pub fn interfaces() -> Vec<Interface> {
    use nix::net::if_::InterfaceFlags;
    let mut table: Vec<Interface> = Vec::new();
    let Ok(entries) = nix::ifaddrs::getifaddrs() else {
        return table;
    };
    for entry in entries {
        let known = table
            .iter()
            .position(|known| known.name == entry.interface_name);
        let at = match known {
            Some(at) => at,
            None => {
                let name = entry.interface_name.clone();
                let system = PathBuf::from("/sys/class/net").join(&name);
                let wireless = system.join("wireless").exists() || system.join("phy80211").exists();
                let mbps = if wireless {
                    nl80211::receive_mbps(&name)
                } else {
                    std::fs::read_to_string(system.join("speed"))
                        .ok()
                        .and_then(|text| text.trim().parse::<u64>().ok())
                        .filter(|mbps| *mbps > 0)
                };
                table.push(Interface {
                    loopback: entry.flags.contains(InterfaceFlags::IFF_LOOPBACK),
                    running: entry
                        .flags
                        .contains(InterfaceFlags::IFF_UP | InterfaceFlags::IFF_RUNNING),
                    wireless,
                    mbps,
                    addresses: Vec::new(),
                    name,
                });
                table.len() - 1
            }
        };
        let Some(address) = socket_ip(entry.address.as_ref()) else {
            continue;
        };
        let prefix = socket_ip(entry.netmask.as_ref()).map_or(0, prefix_length);
        if let Some(interface) = table.get_mut(at) {
            interface.addresses.push((address, prefix));
        }
    }
    table
}

/// The source address the routing table gives a connection to `peer`. No
/// packet is sent.
pub fn routed_source(peer: std::net::SocketAddr) -> Option<IpAddr> {
    let unspecified: std::net::SocketAddr = match peer {
        std::net::SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        std::net::SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = std::net::UdpSocket::bind(unspecified).ok()?;
    socket.connect(peer).ok()?;
    Some(socket.local_addr().ok()?.ip().to_canonical())
}

/// One way a stream's pictures reach this machine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Path {
    pub interface: String,
    /// The interface's address the frame socket is bound to.
    pub source: IpAddr,
    #[serde(default)]
    pub wireless: bool,
    /// Mb/s the interface reports; None when it reports none.
    #[serde(default)]
    pub mbps: Option<u64>,
    /// The routing table's own choice: a socket on it needs no binding.
    #[serde(default)]
    pub routed: bool,
}

impl Path {
    /// Bits per second a stream alone on this path may fill. A wired
    /// interface that reports no speed is taken for
    /// [`crate::viewer_stream::UNKNOWN_LINK_MBPS`]; a wireless one that
    /// reports none carries a stream only when there is no other path.
    pub fn usable_bits_per_second(&self) -> u64 {
        let (mbps, percent) = if self.wireless {
            (self.mbps.unwrap_or(0), WIRELESS_USABLE_PERCENT)
        } else {
            (
                self.mbps.unwrap_or(crate::viewer_stream::UNKNOWN_LINK_MBPS),
                WIRED_USABLE_PERCENT,
            )
        };
        mbps.saturating_mul(BITS_PER_MEGABIT) / 100 * percent
    }

    /// `enp10s0 (192.168.1.211, 1000 Mb/s)`.
    pub fn describe(&self) -> String {
        let speed = self.mbps.map_or_else(
            || "no speed reported".to_owned(),
            |mbps| format!("{mbps} Mb/s"),
        );
        let kind = if self.wireless { ", wireless" } else { "" };
        format!("{} ({}, {speed}{kind})", self.interface, self.source)
    }
}

fn same_subnet(own: IpAddr, prefix: u8, peer: IpAddr) -> bool {
    let shift = |bits: u32| bits.saturating_sub(u32::from(prefix));
    match (own, peer) {
        (IpAddr::V4(own), IpAddr::V4(peer)) => {
            let shift = shift(u32::BITS);
            prefix > 0
                && u32::from(own).checked_shr(shift).unwrap_or(0)
                    == u32::from(peer).checked_shr(shift).unwrap_or(0)
        }
        (IpAddr::V6(own), IpAddr::V6(peer)) => {
            let shift = shift(u128::BITS);
            // A link-local address reaches the peer only with a scope.
            prefix > 0
                && !own.is_unicast_link_local()
                && u128::from(own).checked_shr(shift).unwrap_or(0)
                    == u128::from(peer).checked_shr(shift).unwrap_or(0)
        }
        (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

/// The paths from the interfaces in `table` to `peer`, the preferred first:
/// wired before wireless, then the faster. `routed` is the source address the
/// routing table uses. None for a peer on this machine itself, which has no
/// choice of path.
pub fn paths_to(table: &[Interface], peer: IpAddr, routed: Option<IpAddr>) -> Vec<Path> {
    let peer = peer.to_canonical();
    if peer.is_loopback() || table.iter().any(|interface| interface.holds(peer)) {
        return Vec::new();
    }
    let mut paths: Vec<Path> = table
        .iter()
        .filter(|interface| interface.running && !interface.loopback)
        .filter_map(|interface| {
            let is_routed = routed.is_some_and(|routed| interface.holds(routed));
            let source = routed.filter(|_| is_routed).or_else(|| {
                first(&interface.addresses, |(own, prefix)| {
                    same_subnet(*own, *prefix, peer)
                })
                .map(|(own, _)| *own)
            })?;
            Some(Path {
                interface: interface.name.clone(),
                source,
                wireless: interface.wireless,
                mbps: interface.mbps,
                routed: is_routed,
            })
        })
        .collect();
    prefer(&mut paths);
    paths
}

/// Sorts `paths` by preference: wired before wireless, then the faster.
pub fn prefer(paths: &mut [Path]) {
    paths.sort_by(|left, right| {
        left.wireless
            .cmp(&right.wireless)
            .then(
                right
                    .usable_bits_per_second()
                    .cmp(&left.usable_bits_per_second()),
            )
            .then(left.interface.cmp(&right.interface))
    });
}

/// A stream that is open on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Load {
    /// The interface its pictures arrive on.
    pub interface: String,
    /// The listener that sends them, as `name:port`.
    pub peer: String,
    /// The budget of the interface they leave that machine on, in bits per
    /// second, once the sender has told.
    pub sender_bits_per_second: Option<u64>,
}

/// What a further stream from `peer` would have on `path`.
pub fn fair_share(path: &Path, loads: &[Load], peer: &str) -> u64 {
    let count = |streams: usize| u64::try_from(streams).unwrap_or(u64::MAX).saturating_add(1);
    let on_path = loads
        .iter()
        .filter(|load| load.interface == path.interface)
        .count();
    let own = path.usable_bits_per_second() / count(on_path);
    // The streams of one machine leave it on one interface, whatever path
    // they arrive on.
    let from_peer: Vec<&Load> = loads.iter().filter(|load| load.peer == peer).collect();
    match from_peer
        .iter()
        .filter_map(|load| load.sender_bits_per_second)
        .max()
    {
        Some(budget) => own.min(budget / count(from_peer.len())),
        None => own,
    }
}

/// The path in `paths` (in the order of [`paths_to`]) for a new stream from
/// `peer`, given the streams already open. See the module text for the rule.
pub fn choose<'a>(paths: &'a [Path], loads: &[Load], peer: &str) -> Option<&'a Path> {
    // The first of the paths with the largest fair share.
    let best = |wireless: bool| {
        paths
            .iter()
            .filter(|path| path.wireless == wireless)
            .map(|path| (path, fair_share(path, loads, peer)))
            .reduce(|best, next| if next.1 > best.1 { next } else { best })
    };
    match (best(false), best(true)) {
        (Some((wired, wired_share)), Some((wireless, wireless_share))) => {
            let wired_carries = loads.iter().any(|load| load.interface == wired.interface);
            if wired_carries && wireless_share > wired_share {
                Some(wireless)
            } else {
                Some(wired)
            }
        }
        (Some((path, _)), None) | (None, Some((path, _))) => Some(path),
        (None, None) => None,
    }
}

/// The paths a test or an operator lists in the file [`PATHS_ENV`] names,
/// instead of this machine's interfaces. A path is alive while it is listed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PathTable {
    pub paths: Vec<Path>,
    /// Where the streams on these paths are counted; the default is this
    /// user's [`Registry`].
    #[serde(default)]
    pub registry: Option<PathBuf>,
}

/// One open stream viewer in the [`Registry`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Entry {
    pid: u32,
    /// The process's start time, which tells it from a later one with the
    /// same number.
    started: u64,
    interface: String,
    peer: String,
    #[serde(default)]
    sender_bits_per_second: Option<u64>,
}

/// Field 22 of `/proc/<pid>/stat`: when the process started, in clock ticks
/// since boot. None when there is no such process.
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The fields after the command, which may itself hold spaces.
    let (_, rest) = stat.rsplit_once(") ")?;
    rest.split(' ').nth(19)?.parse().ok()
}

/// The stream viewers of this user that are open now, whichever kwin-mcp
/// server started them: one file per viewer process in a directory, read and
/// written under a lock so two viewers that start together see each other.
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// `dir`, or this user's own under the runtime directory.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || {
                        std::env::temp_dir()
                            .join(format!("kwin-mcp-streams-{}", nix::unistd::getuid()))
                    },
                    |runtime| PathBuf::from(runtime).join("kwin-mcp-streams"),
                )
        });
        Self { dir }
    }

    fn own_file(&self) -> PathBuf {
        self.dir.join(format!("{}.json", std::process::id()))
    }

    fn locked(&self) -> std::io::Result<nix::fcntl::Flock<std::fs::File>> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
        nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, error)| std::io::Error::from(error))
    }

    /// The streams of the other viewer processes that still run. Files of
    /// processes that are gone are removed.
    fn others(&self) -> Vec<Load> {
        let mut loads = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return loads;
        };
        for file in entries.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let entry = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Entry>(&text).ok());
            match entry {
                Some(entry) if process_start(entry.pid) == Some(entry.started) => {
                    if entry.pid != std::process::id() {
                        loads.push(Load {
                            interface: entry.interface,
                            peer: entry.peer,
                            sender_bits_per_second: entry.sender_bits_per_second,
                        });
                    }
                }
                Some(_) | None => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        loads
    }

    fn write(&self, interface: &str, peer: &str, sender: Option<u64>) -> std::io::Result<()> {
        let pid = std::process::id();
        let entry = Entry {
            pid,
            started: process_start(pid).unwrap_or(0),
            interface: interface.to_owned(),
            peer: peer.to_owned(),
            sender_bits_per_second: sender,
        };
        let text = serde_json::to_string(&entry).map_err(std::io::Error::other)?;
        let temporary = self.dir.join(format!("{pid}.tmp"));
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, self.own_file())
    }

    /// Chooses the path of this process's stream from `peer` among `paths`
    /// ([`choose`]) and records it, so the next viewer counts it.
    pub fn claim(&self, paths: &[Path], peer: &str) -> std::io::Result<Option<Path>> {
        let _lock = self.locked()?;
        let Some(path) = choose(paths, &self.others(), peer) else {
            return Ok(None);
        };
        self.write(&path.interface, peer, None)?;
        Ok(Some(path.clone()))
    }

    /// Records the budget of the interface this process's stream leaves the
    /// sending machine on.
    pub fn sender_budget(
        &self,
        interface: &str,
        peer: &str,
        bits_per_second: Option<u64>,
    ) -> std::io::Result<()> {
        let _lock = self.locked()?;
        self.write(interface, peer, bits_per_second)
    }

    /// This process's stream ended.
    pub fn release(&self) {
        let _ = std::fs::remove_file(self.own_file());
    }
}

/// The rate a wireless interface receives at, from the kernel's nl80211.
mod nl80211 {
    use nix::sys::socket::{
        AddressFamily, MsgFlags, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, recv, send,
        setsockopt, socket, sockopt,
    };
    use std::os::fd::AsRawFd;

    const NLMSG_ERROR: u16 = 2;
    const NLMSG_DONE: u16 = 3;
    const NLM_F_REQUEST: u16 = 0x001;
    const NLM_F_DUMP: u16 = 0x300;
    const NLMSG_HEADER_BYTES: usize = 16;
    const GENL_HEADER_BYTES: usize = 4;
    const ATTRIBUTE_HEADER_BYTES: usize = 4;
    const ALIGNMENT: usize = 4;
    /// Bits of an attribute's type that are flags (nested, byte order).
    const ATTRIBUTE_TYPE_MASK: u16 = 0x3fff;
    const GENL_ID_CTRL: u16 = 0x10;
    const CTRL_CMD_GETFAMILY: u8 = 3;
    const CTRL_ATTR_FAMILY_ID: u16 = 1;
    const CTRL_ATTR_FAMILY_NAME: u16 = 2;
    const FAMILY_NAME: &[u8] = b"nl80211\0";
    const NL80211_CMD_GET_STATION: u8 = 17;
    const NL80211_ATTR_IFINDEX: u16 = 3;
    const NL80211_ATTR_STA_INFO: u16 = 21;
    const NL80211_STA_INFO_RX_BITRATE: u16 = 14;
    /// In units of 100 kb/s, as 16 and as 32 bits.
    const NL80211_RATE_INFO_BITRATE: u16 = 1;
    const NL80211_RATE_INFO_BITRATE32: u16 = 5;
    const RATE_UNITS_PER_MEGABIT: u64 = 10;
    const REPLY_BYTES: usize = 32 << 10;
    const REPLY_WAIT_MICROSECONDS: i64 = 200_000;
    /// Datagrams one request may take: a dump of one station is two.
    const MOST_REPLIES: usize = 16;

    fn aligned(length: usize) -> usize {
        length.div_ceil(ALIGNMENT) * ALIGNMENT
    }

    pub(super) fn attribute(kind: u16, payload: &[u8]) -> Vec<u8> {
        let length = ATTRIBUTE_HEADER_BYTES + payload.len();
        let mut bytes = Vec::with_capacity(aligned(length));
        bytes.extend_from_slice(&u16::try_from(length).unwrap_or(u16::MAX).to_ne_bytes());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(payload);
        bytes.resize(aligned(length), 0);
        bytes
    }

    /// The payload of the first attribute of type `kind` in `bytes`. The
    /// flag bits of a type are not compared.
    pub(super) fn lookup(mut bytes: &[u8], kind: u16) -> Option<&[u8]> {
        while let Some(header) = bytes.get(..ATTRIBUTE_HEADER_BYTES) {
            let length = usize::from(u16::from_ne_bytes([header[0], header[1]]));
            let payload = bytes.get(ATTRIBUTE_HEADER_BYTES..length)?;
            if u16::from_ne_bytes([header[2], header[3]]) & ATTRIBUTE_TYPE_MASK == kind {
                return Some(payload);
            }
            bytes = bytes.get(aligned(length)..).unwrap_or_default();
        }
        None
    }

    fn request(family: u16, flags: u16, command: u8, attributes: &[u8]) -> Vec<u8> {
        let length = NLMSG_HEADER_BYTES + GENL_HEADER_BYTES + attributes.len();
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&u32::try_from(length).unwrap_or(u32::MAX).to_ne_bytes());
        bytes.extend_from_slice(&family.to_ne_bytes());
        bytes.extend_from_slice(&(NLM_F_REQUEST | flags).to_ne_bytes());
        // Sequence and port: one request at a time on a socket of its own.
        bytes.extend_from_slice(&1u32.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        bytes.extend_from_slice(&[command, 1, 0, 0]);
        bytes.extend_from_slice(attributes);
        bytes
    }

    /// The attribute bytes of every message in one datagram, and whether the
    /// answer ends with it.
    pub(super) fn messages(mut datagram: &[u8]) -> (Vec<&[u8]>, bool) {
        let mut bodies = Vec::new();
        while let Some(header) = datagram.get(..NLMSG_HEADER_BYTES) {
            let length = usize::try_from(u32::from_ne_bytes([
                header[0], header[1], header[2], header[3],
            ]))
            .unwrap_or(usize::MAX);
            let kind = u16::from_ne_bytes([header[4], header[5]]);
            if kind == NLMSG_ERROR || kind == NLMSG_DONE {
                return (bodies, true);
            }
            let Some(body) = datagram.get(NLMSG_HEADER_BYTES + GENL_HEADER_BYTES..length) else {
                return (bodies, true);
            };
            bodies.push(body);
            datagram = datagram.get(aligned(length)..).unwrap_or_default();
        }
        (bodies, false)
    }

    /// The receive rate in a station's attributes, in Mb/s.
    pub(super) fn receive_rate(station: &[u8]) -> Option<u64> {
        let rate = lookup(
            lookup(station, NL80211_ATTR_STA_INFO)?,
            NL80211_STA_INFO_RX_BITRATE,
        )?;
        let units = match lookup(rate, NL80211_RATE_INFO_BITRATE32) {
            Some(wide) => u64::from(u32::from_ne_bytes(wide.try_into().ok()?)),
            None => u64::from(u16::from_ne_bytes(
                lookup(rate, NL80211_RATE_INFO_BITRATE)?.try_into().ok()?,
            )),
        };
        Some(units / RATE_UNITS_PER_MEGABIT).filter(|mbps| *mbps > 0)
    }

    /// Sends `request` and hands the attribute bytes of each reply message
    /// to `take` until the answer ends or `take` returns a value.
    fn exchange<T>(
        socket: &std::os::fd::OwnedFd,
        request: &[u8],
        dump: bool,
        mut take: impl FnMut(&[u8]) -> Option<T>,
    ) -> Option<T> {
        send(socket.as_raw_fd(), request, MsgFlags::empty()).ok()?;
        let mut reply = vec![0u8; REPLY_BYTES];
        for _ in 0..MOST_REPLIES {
            let length = recv(socket.as_raw_fd(), &mut reply, MsgFlags::empty()).ok()?;
            let (bodies, ended) = messages(reply.get(..length)?);
            for body in bodies {
                if let Some(value) = take(body) {
                    return Some(value);
                }
            }
            if ended || !dump {
                break;
            }
        }
        None
    }

    /// The rate the wireless interface `name` receives at from the station
    /// it is connected to, in Mb/s. None when the kernel does not tell.
    pub(super) fn receive_mbps(name: &str) -> Option<u64> {
        let index = nix::net::if_::if_nametoindex(name).ok()?;
        let socket = socket(
            AddressFamily::Netlink,
            SockType::Raw,
            SockFlag::SOCK_CLOEXEC,
            SockProtocol::NetlinkGeneric,
        )
        .ok()?;
        bind(socket.as_raw_fd(), &NetlinkAddr::new(0, 0)).ok()?;
        let wait = nix::sys::time::TimeVal::new(0, REPLY_WAIT_MICROSECONDS);
        setsockopt(&socket, sockopt::ReceiveTimeout, &wait).ok()?;
        let family = exchange(
            &socket,
            &request(
                GENL_ID_CTRL,
                0,
                CTRL_CMD_GETFAMILY,
                &attribute(CTRL_ATTR_FAMILY_NAME, FAMILY_NAME),
            ),
            false,
            |body| {
                Some(u16::from_ne_bytes(
                    lookup(body, CTRL_ATTR_FAMILY_ID)?.try_into().ok()?,
                ))
            },
        )?;
        exchange(
            &socket,
            &request(
                family,
                NLM_F_DUMP,
                NL80211_CMD_GET_STATION,
                &attribute(NL80211_ATTR_IFINDEX, &index.to_ne_bytes()),
            ),
            true,
            receive_rate,
        )
    }
}

#[cfg(test)]
mod tests;
