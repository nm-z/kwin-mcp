//! WebSocket transport between kwin-mcp servers on different machines: the
//! shared secret, the listener handshake that checks it, one JSON-RPC message
//! per text frame for rmcp on the control socket, and the separate frame
//! socket that carries a remote viewer's pictures and input.

use async_tungstenite::tungstenite;
use futures::{Sink, SinkExt, Stream, StreamExt};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http;

/// Port a listener binds, and a `host` argument reaches, when none is given.
pub const DEFAULT_PORT: u16 = 47913;
/// File under the kwin-mcp configuration directory that holds the shared secret.
const SECRET_FILE: &str = "remote-secret";
const AUTHORIZATION_SCHEME: &str = "Bearer ";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A silent peer is probed after this long, then every `KEEPALIVE_INTERVAL`.
const KEEPALIVE_IDLE_SECONDS: u32 = 10;
const KEEPALIVE_INTERVAL_SECONDS: u32 = 5;
const KEEPALIVE_PROBES: u32 = 3;
/// An inline screenshot is one message; the library default of 16 MiB per
/// frame would cut a large one.
const MAX_MESSAGE_BYTES: usize = 256 << 20;
/// Request path of a viewer's frame socket; any other path is a control socket.
const FRAMES_PATH: &str = "/frames";
/// Handshake header naming the `viewer_open` call a frame socket belongs to.
const TICKET_HEADER: &str = "x-kwin-mcp-frame-ticket";
/// Environment variable that hands a viewer the ticket of its frame socket.
pub const TICKET_ENV: &str = "KWIN_VIEWER_FRAME_TICKET";
/// A ticket is this many random bytes, written as hexadecimal.
const TICKET_BYTES: usize = 16;
/// Kernel send and receive buffer of a frame socket. A frame the peer has not
/// taken yet must hold the sender back instead of queueing behind older
/// frames, so the buffers stay near one compressed frame.
const FRAME_SOCKET_BUFFER_BYTES: usize = 1 << 20;
/// How long [`Handover::hold`] waits for a control socket to stand between
/// two messages, and how often it looks.
const HANDOVER_WAIT: Duration = Duration::from_secs(2);
const HANDOVER_POLL: Duration = Duration::from_millis(5);

/// A viewer's frame socket after the handshake: binary and text messages in
/// both directions.
pub type FrameSocket = async_tungstenite::WebSocketStream<
    async_tungstenite::tokio::TokioAdapter<tokio::net::TcpStream>,
>;

/// The shared secret. It has no `Debug` or `Display`, so it cannot be logged.
pub struct Secret(String);

pub fn secret_path() -> Result<PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| "neither XDG_CONFIG_HOME nor HOME is set".to_owned())?;
    Ok(config.join("kwin-mcp").join(SECRET_FILE))
}

/// Reads the secret from its file, which must be a regular file owned by this
/// user with no group or other permission bits.
pub fn read_secret() -> Result<Secret, String> {
    use std::os::unix::fs::MetadataExt;
    let path = secret_path()?;
    let shown = path.display();
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        format!("shared secret file {shown} is unavailable ({error}); create it with mode 0600")
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!("shared secret file {shown} is not a regular file"));
    }
    if metadata.uid() != unsafe { nix::libc::geteuid() } {
        return Err(format!(
            "shared secret file {shown} belongs to another user"
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "shared secret file {shown} is readable by group or others; run chmod 600 on it"
        ));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("shared secret file {shown} could not be read: {error}"))?;
    let secret = text.trim();
    if secret.is_empty() || !secret.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "shared secret file {shown} must hold one line of printable ASCII without spaces"
        ));
    }
    Ok(Secret(secret.to_owned()))
}

/// Compares without stopping at the first difference, so timing does not show
/// how much of a guess was right.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

/// `--listen` value: an IP address with an optional port.
pub fn parse_listen(value: &str) -> Result<SocketAddr, String> {
    value
        .parse::<SocketAddr>()
        .or_else(|_| {
            value
                .parse::<IpAddr>()
                .map(|address| SocketAddr::new(address, DEFAULT_PORT))
        })
        .map_err(|_| format!("--listen '{value}' is not an IP address with an optional :PORT"))
}

/// `host` argument as `name:port`: a hostname or IP address, with the default
/// port when none is given.
pub fn authority(host: &str) -> Result<String, String> {
    if let Ok(address) = host.parse::<SocketAddr>() {
        return Ok(address.to_string());
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(address, DEFAULT_PORT).to_string());
    }
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (
            name,
            port.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("host '{host}' has an invalid port"))?,
        ),
        None => (host, DEFAULT_PORT),
    };
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(format!(
            "host '{host}' is not a hostname or IP address with an optional :port"
        ));
    }
    Ok(format!("{name}:{port}"))
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("WebSocket: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

fn websocket_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES))
}

/// A peer that vanishes without closing (power loss, a pulled cable) must
/// still end the connection, because sessions end with it.
fn detect_dead_peer(stream: &tokio::net::TcpStream) -> std::io::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};
    stream.set_nodelay(true)?;
    setsockopt(stream, sockopt::KeepAlive, &true)?;
    setsockopt(stream, sockopt::TcpKeepIdle, &KEEPALIVE_IDLE_SECONDS)?;
    setsockopt(
        stream,
        sockopt::TcpKeepInterval,
        &KEEPALIVE_INTERVAL_SECONDS,
    )?;
    setsockopt(stream, sockopt::TcpKeepCount, &KEEPALIVE_PROBES)?;
    let unacknowledged_ms =
        (KEEPALIVE_IDLE_SECONDS + KEEPALIVE_INTERVAL_SECONDS * KEEPALIVE_PROBES) * 1000;
    setsockopt(stream, sockopt::TcpUserTimeout, &unacknowledged_ms)?;
    Ok(())
}

/// Where [`FrameTracker`] stands in a connection's bytes.
enum Stage {
    /// In the HTTP handshake, with this many bytes of its closing blank line seen.
    Handshake(usize),
    /// Before a frame's first byte.
    First,
    /// Before a frame's second byte.
    Second,
    /// In a frame's extended payload length.
    Length,
    /// In a frame's masking key and payload.
    Body,
}

/// Follows one direction of a WebSocket connection's bytes (RFC 6455 section
/// 5.2): whether they end exactly where a message ends, and how many messages
/// they held. The WebSocket library keeps bytes it has read or not yet written
/// in buffers of its own, which cannot be inspected or moved to another
/// process image, so this is how a control socket is known to be safe to hand
/// over.
struct FrameTracker {
    stage: Stage,
    /// Bytes of the current stage still to come.
    left: u64,
    /// Payload length read so far, and the masking key's length.
    length: u64,
    mask: u64,
    /// The current frame ends a message, and is a control frame.
    fin: bool,
    control: bool,
    /// A data message has begun and its last fragment has not passed.
    fragmented: bool,
    /// Messages that passed completely, control frames included.
    messages: u64,
}

impl FrameTracker {
    const HANDSHAKE_END: &[u8] = b"\r\n\r\n";

    /// `handshake`: the bytes start with the HTTP handshake instead of a frame.
    fn new(handshake: bool) -> Self {
        Self {
            stage: if handshake {
                Stage::Handshake(0)
            } else {
                Stage::First
            },
            left: 0,
            length: 0,
            mask: 0,
            fin: false,
            control: false,
            fragmented: false,
            messages: 0,
        }
    }

    fn feed(&mut self, mut bytes: &[u8]) {
        while let Some((&byte, rest)) = bytes.split_first() {
            match self.stage {
                Stage::Handshake(seen) => {
                    bytes = rest;
                    let seen = if Self::HANDSHAKE_END.get(seen) == Some(&byte) {
                        seen + 1
                    } else {
                        usize::from(byte == b'\r')
                    };
                    self.stage = if seen == Self::HANDSHAKE_END.len() {
                        Stage::First
                    } else {
                        Stage::Handshake(seen)
                    };
                }
                Stage::First => {
                    bytes = rest;
                    self.fin = byte & 0x80 != 0;
                    self.control = byte & 0x08 != 0;
                    self.stage = Stage::Second;
                }
                Stage::Second => {
                    bytes = rest;
                    self.mask = if byte & 0x80 == 0 { 0 } else { 4 };
                    self.length = 0;
                    match byte & 0x7f {
                        126 => (self.stage, self.left) = (Stage::Length, 2),
                        127 => (self.stage, self.left) = (Stage::Length, 8),
                        short => self.body(u64::from(short)),
                    }
                }
                Stage::Length => {
                    bytes = rest;
                    self.length = self.length << 8 | u64::from(byte);
                    self.left -= 1;
                    if self.left == 0 {
                        self.body(self.length);
                    }
                }
                Stage::Body => {
                    let taken = usize::try_from(self.left)
                        .unwrap_or(usize::MAX)
                        .min(bytes.len());
                    bytes = bytes.get(taken..).unwrap_or_default();
                    self.left -= u64::try_from(taken).unwrap_or(self.left);
                    if self.left == 0 {
                        self.end_frame();
                    }
                }
            }
        }
    }

    fn body(&mut self, payload: u64) {
        self.left = payload.saturating_add(self.mask);
        if self.left == 0 {
            self.end_frame();
        } else {
            self.stage = Stage::Body;
        }
    }

    fn end_frame(&mut self) {
        self.stage = Stage::First;
        if self.control {
            // A control frame is whole, and may stand between a message's fragments.
            self.messages += 1;
        } else if self.fin {
            self.messages += 1;
            self.fragmented = false;
        } else {
            self.fragmented = true;
        }
    }

    /// The bytes so far end where a message ends.
    fn idle(&self) -> bool {
        matches!(self.stage, Stage::First) && !self.fragmented
    }
}

/// What is known about a client's control socket besides the WebSocket
/// library's own state.
struct Wire {
    /// Reads and writes wait: the socket is being handed over.
    held: bool,
    waiting: Vec<std::task::Waker>,
    /// The socket's descriptor while the stream that owns it lives.
    descriptor: Option<std::os::fd::RawFd>,
    /// Bytes taken from and given to the kernel.
    received: FrameTracker,
    sent: FrameTracker,
    /// Messages the WebSocket library handed to the reader, and messages
    /// handed to it for sending.
    delivered: u64,
    queued: u64,
    /// The connection closed or failed.
    ended: bool,
}

impl Wire {
    /// No byte of a message is in a buffer of this process: everything read
    /// from the kernel was delivered as whole messages, and everything queued
    /// was written to the kernel as whole messages.
    fn idle(&self) -> bool {
        !self.ended
            && self.received.idle()
            && self.sent.idle()
            && self.received.messages == self.delivered
            && self.sent.messages == self.queued
    }
}

type SharedWire = std::sync::Arc<std::sync::Mutex<Wire>>;

fn wire(shared: &SharedWire) -> std::sync::MutexGuard<'_, Wire> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A client's control socket under the WebSocket library. It follows the
/// bytes that pass and stops them while the socket is held.
struct Tracked {
    stream: tokio::net::TcpStream,
    wire: SharedWire,
}

impl Tracked {
    fn new(stream: tokio::net::TcpStream, handshake: bool) -> (Self, Handover) {
        use std::os::fd::AsRawFd;
        let wire = std::sync::Arc::new(std::sync::Mutex::new(Wire {
            held: false,
            waiting: Vec::new(),
            descriptor: Some(stream.as_raw_fd()),
            received: FrameTracker::new(handshake),
            sent: FrameTracker::new(handshake),
            delivered: 0,
            queued: 0,
            ended: false,
        }));
        let handover = Handover(wire.clone());
        (Self { stream, wire }, handover)
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        // The descriptor closes with the stream, and its number may be reused.
        wire(&self.wire).descriptor = None;
    }
}

impl tokio::io::AsyncRead for Tracked {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let mut wire = wire(&this.wire);
        if wire.held {
            wire.waiting.push(context.waker().clone());
            return std::task::Poll::Pending;
        }
        let before = buffer.filled().len();
        let polled = std::pin::Pin::new(&mut this.stream).poll_read(context, buffer);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            wire.received
                .feed(buffer.filled().get(before..).unwrap_or_default());
        }
        polled
    }
}

impl tokio::io::AsyncWrite for Tracked {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let mut wire = wire(&this.wire);
        if wire.held {
            wire.waiting.push(context.waker().clone());
            return std::task::Poll::Pending;
        }
        let polled = std::pin::Pin::new(&mut this.stream).poll_write(context, bytes);
        if let std::task::Poll::Ready(Ok(written)) = &polled {
            wire.sent.feed(bytes.get(..*written).unwrap_or_default());
        }
        polled
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_flush(context)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_shutdown(context)
    }
}

/// Lets a client's control socket outlive the process image that opened it.
/// A server that re-executes itself keeps the TCP connection, so the listener
/// sees no disconnect and keeps the sessions: [`Handover::hold`] stops the
/// socket between two messages, [`Handover::socket`] is the descriptor the
/// next image inherits, and [`adopt_control`] continues on it there. Nothing
/// here keeps the connection open when the process dies instead.
#[derive(Clone)]
pub struct Handover(SharedWire);

impl Handover {
    /// Stops the socket once no byte of a message is in a buffer of this
    /// process, in either direction, and reports whether it stopped. The
    /// caller must already have stopped sending requests and collected every
    /// response. A socket that does not get there in `HANDOVER_WAIT` runs on.
    pub async fn hold(&self) -> bool {
        let deadline = std::time::Instant::now() + HANDOVER_WAIT;
        loop {
            {
                let mut wire = wire(&self.0);
                if wire.idle() {
                    wire.held = true;
                    return true;
                }
                if wire.ended || std::time::Instant::now() >= deadline {
                    return false;
                }
            }
            tokio::time::sleep(HANDOVER_POLL).await;
        }
    }

    /// Lets a held socket run again: the handover did not happen.
    pub fn release(&self) {
        let waiting = {
            let mut wire = wire(&self.0);
            wire.held = false;
            std::mem::take(&mut wire.waiting)
        };
        for waker in waiting {
            waker.wake();
        }
    }

    /// Another descriptor of the socket, closed on exec like the first.
    pub fn socket(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        let wire = wire(&self.0);
        let descriptor = wire
            .descriptor
            .ok_or_else(|| std::io::Error::other("the control socket is closed"))?;
        // SAFETY: the stream that owns the descriptor clears `descriptor`
        // under this lock before it closes it.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(descriptor) }.try_clone_to_owned()
    }
}

/// One JSON-RPC message per text frame, in the pair form rmcp takes as a
/// transport. `counts` is the client's control socket whose messages to count.
fn json_frames<S, Tx, Rx>(
    socket: S,
    counts: Option<SharedWire>,
) -> (
    impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
    impl Stream<Item = Rx> + Send + Unpin + 'static,
)
where
    S: Sink<tungstenite::Message, Error = tungstenite::Error>
        + Stream<Item = Result<tungstenite::Message, tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    let (sink, stream) = StreamExt::split(socket);
    let queued = counts.clone();
    let sink = sink.with(move |message: Tx| {
        let frame = serde_json::to_string(&message)
            .map(tungstenite::Message::text)
            .map_err(WireError::from);
        if let (Ok(_), Some(counts)) = (&frame, &queued) {
            wire(counts).queued += 1;
        }
        futures::future::ready(frame)
    });
    let stream = stream
        .take_while(move |frame| {
            if let Some(counts) = &counts {
                let mut wire = wire(counts);
                match frame {
                    Ok(tungstenite::Message::Close(_)) | Err(_) => wire.ended = true,
                    // The library answers a ping with a pong of its own.
                    Ok(tungstenite::Message::Ping(_)) => {
                        wire.delivered += 1;
                        wire.queued += 1;
                    }
                    Ok(_) => wire.delivered += 1,
                }
            }
            futures::future::ready(frame.is_ok())
        })
        .filter_map(|frame| {
            futures::future::ready(match frame {
                Ok(tungstenite::Message::Text(text)) => serde_json::from_str(text.as_str()).ok(),
                _ => None,
            })
        });
    (sink, stream)
}

/// Whether a handshake ended with the listener's answer to a wrong or missing secret.
fn refused_secret(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Http(response) if response.status() == http::StatusCode::UNAUTHORIZED)
}

/// What a peer asked the listener for.
pub enum Accepted<Tx, Rx> {
    /// MCP control socket: one JSON-RPC message per text frame.
    Control(Tx, Rx),
    /// A viewer's frame socket, handshake complete and nothing read from it
    /// yet: the listener's side speaks first on this socket, so the stream can
    /// be handed to the process that captures the session.
    Frames {
        ticket: String,
        socket: std::net::TcpStream,
    },
}

/// A new frame-socket ticket: 128 random bits as hexadecimal.
pub fn new_ticket() -> String {
    use std::fmt::Write;
    let bytes: [u8; TICKET_BYTES] = rand::random();
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// Whether `ticket` has the form [`new_ticket`] produces.
pub fn valid_ticket(ticket: &str) -> bool {
    ticket.len() == TICKET_BYTES * 2 && ticket.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn bound_frame_buffers(stream: &tokio::net::TcpStream) -> std::io::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};
    setsockopt(stream, sockopt::SndBuf, &FRAME_SOCKET_BUFFER_BYTES)?;
    setsockopt(stream, sockopt::RcvBuf, &FRAME_SOCKET_BUFFER_BYTES)?;
    Ok(())
}

/// Listener side: completes the WebSocket handshake only for a peer that
/// presents the shared secret, and reports which socket it opened.
// The handshake callback's refusal type is the library's.
#[allow(clippy::result_large_err)]
pub async fn accept<Tx, Rx>(
    stream: tokio::net::TcpStream,
    secret: &Secret,
) -> Result<
    Accepted<
        impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
        impl Stream<Item = Rx> + Send + Unpin + 'static,
    >,
    String,
>
where
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    detect_dead_peer(&stream).map_err(|error| format!("socket options: {error}"))?;
    let expected = format!("{AUTHORIZATION_SCHEME}{}", secret.0);
    // The ticket, once the peer asked for a frame socket.
    let mut frames: Option<String> = None;
    let authorize = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let refuse = |status: http::StatusCode, reason: &str| {
            let mut refusal = ErrorResponse::new(Some(reason.to_owned()));
            *refusal.status_mut() = status;
            refusal
        };
        let presented = request
            .headers()
            .get(http::header::AUTHORIZATION)
            .map_or(&[][..], |value| value.as_bytes());
        if !constant_time_eq(presented, expected.as_bytes()) {
            return Err(refuse(
                http::StatusCode::UNAUTHORIZED,
                "shared secret required",
            ));
        }
        if request.uri().path() == FRAMES_PATH {
            let ticket = request
                .headers()
                .get(TICKET_HEADER)
                .and_then(|value| value.to_str().ok())
                .filter(|ticket| valid_ticket(ticket))
                .ok_or_else(|| refuse(http::StatusCode::BAD_REQUEST, "frame ticket required"))?;
            frames = Some(ticket.to_owned());
        }
        Ok(response)
    };
    let socket = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        async_tungstenite::tokio::accept_hdr_async_with_config(
            stream,
            authorize,
            Some(websocket_config()),
        ),
    )
    .await
    .map_err(|_| "the handshake timed out".to_owned())?
    .map_err(|error| {
        if refused_secret(&error) {
            "the peer did not present the shared secret".to_owned()
        } else {
            format!("the handshake failed: {error}")
        }
    })?;
    let Some(ticket) = frames else {
        let (sink, stream) = json_frames(socket, None);
        return Ok(Accepted::Control(sink, stream));
    };
    let stream = socket.into_inner().into_inner();
    bound_frame_buffers(&stream).map_err(|error| format!("socket options: {error}"))?;
    let socket = stream
        .into_std()
        .map_err(|error| format!("frame socket handover: {error}"))?;
    Ok(Accepted::Frames { ticket, socket })
}

/// The process that captures a session takes over a frame socket [`accept`]
/// handed out: the handshake is done and the viewer waits for the first message.
pub async fn adopt_frames(socket: std::net::TcpStream) -> Result<FrameSocket, String> {
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("frame socket: {error}"))?;
    let stream = tokio::net::TcpStream::from_std(socket)
        .map_err(|error| format!("frame socket: {error}"))?;
    Ok(async_tungstenite::WebSocketStream::from_raw_socket(
        async_tungstenite::tokio::TokioAdapter::new(stream),
        tungstenite::protocol::Role::Server,
        Some(websocket_config()),
    )
    .await)
}

fn client_request(
    authority: &str,
    path: &str,
    secret: &Secret,
) -> Result<tungstenite::handshake::client::Request, String> {
    use tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{authority}{path}")
        .into_client_request()
        .map_err(|error| format!("invalid listener address: {error}"))?;
    let mut credential =
        http::HeaderValue::from_str(&format!("{AUTHORIZATION_SCHEME}{}", secret.0))
            .map_err(|_| "the shared secret is not a valid header value".to_owned())?;
    credential.set_sensitive(true);
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, credential);
    Ok(request)
}

fn handshake_error(error: tungstenite::Error) -> String {
    if refused_secret(&error) {
        "the listener refused the shared secret".to_owned()
    } else {
        format!("the WebSocket handshake failed: {error}")
    }
}

/// Where a frame socket leaves the viewer's machine when the routing table's
/// own choice is not wanted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The local address the socket is bound to before it connects.
    pub address: IpAddr,
    /// The interface that holds the address.
    pub interface: String,
}

/// How a frame socket was tied to its [`Source`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    /// To the address and, with `SO_BINDTODEVICE`, to the interface:
    /// everything the socket sends leaves on that interface.
    Device,
    /// To the address alone, because binding to the interface was refused.
    /// The peer sends to that address; what this side sends (acknowledgements
    /// and input) leaves on the interface the routing table picks.
    Address,
}

/// A TCP connection to `authority` from `source`: bound before it connects.
async fn connect_from(
    authority: &str,
    source: &Source,
) -> Result<(tokio::net::TcpStream, Bound), String> {
    let peers = tokio::net::lookup_host(authority)
        .await
        .map_err(|error| format!("could not resolve {authority}: {error}"))?;
    let peer = crate::links::first(peers, |peer| peer.is_ipv4() == source.address.is_ipv4())
        .ok_or_else(|| {
            format!(
                "{authority} has no address of the family of {}",
                source.address
            )
        })?;
    let socket = if peer.is_ipv4() {
        tokio::net::TcpSocket::new_v4()
    } else {
        tokio::net::TcpSocket::new_v6()
    }
    .map_err(|error| format!("frame socket: {error}"))?;
    // Unprivileged since Linux 5.7 for a socket that is bound to no device.
    let bound = match socket.bind_device(Some(source.interface.as_bytes())) {
        Ok(()) => Bound::Device,
        Err(_) => Bound::Address,
    };
    socket
        .bind(SocketAddr::new(source.address, 0))
        .map_err(|error| format!("could not bind to {}: {error}", source.address))?;
    let stream = socket.connect(peer).await.map_err(|error| {
        format!(
            "could not connect from {} on {}: {error}",
            source.address, source.interface
        )
    })?;
    Ok((stream, bound))
}

/// Viewer side: opens the frame socket of the `viewer_open` call that carried
/// `ticket` on the control socket. With `source`, the socket is bound to that
/// address and interface before it connects; the result says how.
pub async fn connect_frames(
    authority: &str,
    secret: &Secret,
    ticket: &str,
    source: Option<&Source>,
) -> Result<(FrameSocket, Option<Bound>), String> {
    let mut request = client_request(authority, FRAMES_PATH, secret)?;
    request.headers_mut().insert(
        TICKET_HEADER,
        http::HeaderValue::from_str(ticket).map_err(|_| "invalid frame ticket".to_owned())?,
    );
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (stream, bound) = match source {
            Some(source) => {
                let (stream, bound) = connect_from(authority, source).await?;
                (stream, Some(bound))
            }
            None => (
                tokio::net::TcpStream::connect(authority)
                    .await
                    .map_err(|error| format!("could not connect: {error}"))?,
                None,
            ),
        };
        detect_dead_peer(&stream).map_err(|error| format!("socket options: {error}"))?;
        bound_frame_buffers(&stream).map_err(|error| format!("socket options: {error}"))?;
        let (socket, _) = async_tungstenite::tokio::client_async_with_config(
            request,
            stream,
            Some(websocket_config()),
        )
        .await
        .map_err(handshake_error)?;
        Ok((socket, bound))
    })
    .await
    .map_err(|_| "connecting timed out".to_owned())?
}

/// Client side: connects to a listener and presents the shared secret.
/// Returns the transport and what hands its socket to the next process image.
pub async fn connect<Tx, Rx>(
    authority: &str,
    secret: &Secret,
) -> Result<
    (
        (
            impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
            impl Stream<Item = Rx> + Send + Unpin + 'static,
        ),
        Handover,
    ),
    String,
>
where
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    let request = client_request(authority, "/", secret)?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|error| format!("could not connect: {error}"))?;
        detect_dead_peer(&stream).map_err(|error| format!("socket options: {error}"))?;
        let (stream, handover) = Tracked::new(stream, true);
        let (socket, _) = async_tungstenite::tokio::client_async_with_config(
            request,
            stream,
            Some(websocket_config()),
        )
        .await
        .map_err(handshake_error)?;
        Ok((json_frames(socket, Some(handover.0.clone())), handover))
    })
    .await
    .map_err(|_| "connecting timed out".to_owned())?
}

/// Client side, in the process image that replaced the one that called
/// [`connect`]: continues on the inherited control socket, which
/// [`Handover::hold`] left between two messages. The WebSocket layer has no
/// state besides its buffers, so there is no new handshake and the listener
/// sees one unbroken connection.
pub async fn adopt_control<Tx, Rx>(
    socket: std::os::fd::OwnedFd,
) -> Result<
    (
        (
            impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
            impl Stream<Item = Rx> + Send + Unpin + 'static,
        ),
        Handover,
    ),
    String,
>
where
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    let failed = |error: std::io::Error| format!("inherited control socket: {error}");
    let socket = std::net::TcpStream::from(socket);
    // Fails for a descriptor that is not a connected socket.
    socket.peer_addr().map_err(failed)?;
    socket.set_nonblocking(true).map_err(failed)?;
    let stream = tokio::net::TcpStream::from_std(socket).map_err(failed)?;
    let (stream, handover) = Tracked::new(stream, false);
    let socket = async_tungstenite::WebSocketStream::from_raw_socket(
        async_tungstenite::tokio::TokioAdapter::new(stream),
        tungstenite::protocol::Role::Client,
        Some(websocket_config()),
    )
    .await;
    Ok((json_frames(socket, Some(handover.0.clone())), handover))
}

/// Text that every kwin-mcp binary with [`adopt_control`] contains.
pub const ADOPTS_CONTROL_MARK: &str = "kwin-mcp continues on inherited control sockets (1)";

/// Whether the kwin-mcp binary at `path` continues on an inherited control
/// socket. One that does not would hold the descriptor open without knowing
/// it and pass it to every process it starts, and the listener would keep the
/// sessions for as long as any of them lived. A server asks this of the binary
/// it is about to re-execute into, which after an install of an older build
/// is not its own.
pub fn adopts_control(path: &std::path::Path) -> bool {
    // Opaque to the optimizer, so the text stays whole in the binary.
    let mark = std::hint::black_box(ADOPTS_CONTROL_MARK).as_bytes();
    let Some((first, rest)) = mark.split_first() else {
        return false;
    };
    std::fs::read(path).is_ok_and(|image| {
        image
            .iter()
            .enumerate()
            .any(|(at, byte)| byte == first && image.get(at + 1..at + mark.len()) == Some(rest))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Accepted, DEFAULT_PORT, FrameTracker, Secret, accept, adopt_control, adopts_control,
        authority, connect, constant_time_eq, new_ticket, parse_listen, valid_ticket,
    };
    use futures::{SinkExt, StreamExt};

    #[test]
    fn only_a_binary_with_this_code_is_taken_to_adopt_control_sockets() -> Result<(), String> {
        let own = std::env::current_exe().map_err(|error| error.to_string())?;
        assert!(adopts_control(&own), "{}", own.display());
        assert!(!adopts_control(std::path::Path::new("/proc/self/cmdline")));
        assert!(!adopts_control(&own.with_extension("missing")));
        Ok(())
    }

    #[test]
    fn a_tracker_is_idle_only_where_a_message_ends() {
        let mut tracker = FrameTracker::new(true);
        tracker.feed(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r");
        assert!(!tracker.idle(), "inside the handshake");
        tracker.feed(b"\n");
        assert!(tracker.idle(), "after the handshake");

        // A short unmasked text frame, one byte at a time.
        let short = [0x81, 0x02, b'h', b'i'];
        for (index, byte) in short.iter().enumerate() {
            tracker.feed(&[*byte]);
            assert_eq!(tracker.idle(), index + 1 == short.len(), "byte {index}");
        }
        assert_eq!(tracker.messages, 1);

        // Masked frames with a 16-bit and a 64-bit length, cut before the last byte.
        for (header, payload) in [
            (vec![0x82, 0x80 | 126, 0x01, 0x2c], 300),
            (
                vec![0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0x01, 0x11, 0x70],
                70_000,
            ),
        ] {
            let mut frame = header;
            frame.extend([1, 2, 3, 4]);
            frame.extend(vec![0x81; payload]);
            let (most, last) = frame.split_at(frame.len() - 1);
            tracker.feed(most);
            assert!(!tracker.idle(), "one byte short of {payload}");
            tracker.feed(last);
            assert!(tracker.idle(), "after {payload}");
        }
        assert_eq!(tracker.messages, 3);

        // An empty masked frame is its header and key.
        tracker.feed(&[0x81, 0x80, 9, 9, 9]);
        assert!(!tracker.idle());
        tracker.feed(&[9]);
        assert!(tracker.idle());
        assert_eq!(tracker.messages, 4);

        // A message in two fragments with a ping between them ends with the second.
        tracker.feed(&[0x01, 0x01, b'a']);
        assert!(!tracker.idle(), "between fragments");
        tracker.feed(&[0x89, 0x00]);
        assert!(!tracker.idle(), "a ping between fragments");
        assert_eq!(tracker.messages, 5);
        tracker.feed(&[0x80, 0x01, b'b', 0x81]);
        assert!(!tracker.idle(), "the next frame began");
        assert_eq!(tracker.messages, 6);
    }

    /// What a re-executing server does to its control socket, without the
    /// exec: hold it, keep a descriptor, lose the WebSocket stream with all its
    /// state, and continue on the descriptor. The listener's side must see one
    /// connection.
    #[tokio::test]
    async fn a_held_control_socket_continues_under_a_new_websocket_stream() -> Result<(), String> {
        let text = |error: &dyn std::fmt::Display| error.to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| text(&error))?;
        let address = listener
            .local_addr()
            .map_err(|error| text(&error))?
            .to_string();
        // Answers each message with how many this connection has carried.
        let served = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.map_err(|error| text(&error))?;
            let secret = Secret("test-secret".to_owned());
            let Accepted::Control(mut sink, mut stream) =
                accept::<String, String>(stream, &secret).await?
            else {
                return Err("a frame socket".to_owned());
            };
            let mut carried = 0;
            while let Some(message) = stream.next().await {
                carried += 1;
                sink.send(format!("{carried}:{message}"))
                    .await
                    .map_err(|error| text(&error))?;
            }
            Ok(carried)
        });

        let secret = Secret("test-secret".to_owned());
        let ((mut sink, mut stream), handover) =
            connect::<String, String>(&address, &secret).await?;
        // A large message spans many reads and writes.
        let large = "x".repeat(300_000);
        for message in ["one", large.as_str()] {
            sink.send(message.to_owned())
                .await
                .map_err(|error| text(&error))?;
            let answer = stream.next().await.ok_or("no answer")?;
            assert!(answer.ends_with(message), "{}", answer.len());
        }
        assert!(handover.hold().await, "an idle socket was not held");
        // A held socket moves nothing: this message stays in this process.
        let mut stuck = Box::pin(sink.send("lost".to_owned()));
        assert!(futures::poll!(&mut stuck).is_pending());
        drop(stuck);
        handover.release();
        assert!(!handover.hold().await, "held with a message half sent");
        handover.release();
        sink.flush().await.map_err(|error| text(&error))?;
        assert_eq!(stream.next().await.as_deref(), Some("3:lost"));

        assert!(handover.hold().await);
        let socket = handover.socket().map_err(|error| text(&error))?;
        drop((sink, stream));
        assert!(handover.socket().is_err(), "the old stream still owns it");
        let ((mut sink, mut stream), adopted) = adopt_control::<String, String>(socket).await?;
        sink.send("two".to_owned())
            .await
            .map_err(|error| text(&error))?;
        assert_eq!(stream.next().await.as_deref(), Some("4:two"));
        assert!(adopted.hold().await, "the adopted socket was not held");
        drop((sink, stream));
        assert_eq!(served.await.map_err(|error| text(&error))?, Ok(4));
        Ok(())
    }

    #[test]
    fn a_new_ticket_is_valid_and_differs_from_the_last() {
        let (first, second) = (new_ticket(), new_ticket());
        assert!(valid_ticket(&first) && valid_ticket(&second));
        assert_ne!(first, second);
        let long = format!("{first}0");
        for bad in [
            "",
            "abc",
            &first[1..],
            long.as_str(),
            "g".repeat(32).as_str(),
        ] {
            assert!(!valid_ticket(bad), "{bad}");
        }
    }

    #[test]
    fn only_an_identical_secret_matches() {
        assert!(constant_time_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!constant_time_eq(b"Bearer abc", b"Bearer ab"));
        assert!(!constant_time_eq(b"", b"Bearer abc"));
        assert!(!constant_time_eq(b"Bearer abc\0", b"Bearer abc"));
    }

    #[test]
    fn a_host_gets_the_default_port_unless_it_names_one() -> Result<(), String> {
        assert_eq!(authority("sentry")?, format!("sentry:{DEFAULT_PORT}"));
        assert_eq!(authority("sentry.lan:9000")?, "sentry.lan:9000");
        assert_eq!(authority("10.0.0.7")?, format!("10.0.0.7:{DEFAULT_PORT}"));
        assert_eq!(authority("[::1]:9000")?, "[::1]:9000");
        assert_eq!(authority("::1")?, format!("[::1]:{DEFAULT_PORT}"));
        for bad in [
            "",
            ":9000",
            "sentry:0",
            "sentry:port",
            "a b",
            "user@sentry",
            "a/b",
        ] {
            assert!(authority(bad).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn listen_takes_an_address_with_an_optional_port() -> Result<(), String> {
        assert_eq!(parse_listen("127.0.0.1:9000")?.port(), 9000);
        assert_eq!(parse_listen("0.0.0.0")?.port(), DEFAULT_PORT);
        assert!(parse_listen("sentry").is_err());
        assert!(parse_listen("9000").is_err());
        Ok(())
    }
}
