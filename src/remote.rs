//! WebSocket transport between kwin-mcp servers on different machines: the
//! shared secret, the listener handshake that checks it, and one JSON-RPC
//! message per text frame for rmcp.

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

/// One JSON-RPC message per text frame, in the pair form rmcp takes as a transport.
fn json_frames<S, Tx, Rx>(
    socket: S,
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
    let sink = sink.with(|message: Tx| {
        futures::future::ready(
            serde_json::to_string(&message)
                .map(tungstenite::Message::text)
                .map_err(WireError::from),
        )
    });
    let stream = stream
        .take_while(|frame| futures::future::ready(frame.is_ok()))
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

/// Listener side: completes the WebSocket handshake only for a peer that
/// presents the shared secret.
// The handshake callback's refusal type is the library's.
#[allow(clippy::result_large_err)]
pub async fn accept<Tx, Rx>(
    stream: tokio::net::TcpStream,
    secret: &Secret,
) -> Result<
    (
        impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
        impl Stream<Item = Rx> + Send + Unpin + 'static,
    ),
    String,
>
where
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    detect_dead_peer(&stream).map_err(|error| format!("socket options: {error}"))?;
    let expected = format!("{AUTHORIZATION_SCHEME}{}", secret.0);
    let authorize = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let presented = request
            .headers()
            .get(http::header::AUTHORIZATION)
            .map_or(&[][..], |value| value.as_bytes());
        if constant_time_eq(presented, expected.as_bytes()) {
            Ok(response)
        } else {
            let mut refusal = ErrorResponse::new(Some("shared secret required".to_owned()));
            *refusal.status_mut() = http::StatusCode::UNAUTHORIZED;
            Err(refusal)
        }
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
    Ok(json_frames(socket))
}

/// Client side: connects to a listener and presents the shared secret.
pub async fn connect<Tx, Rx>(
    authority: &str,
    secret: &Secret,
) -> Result<
    (
        impl Sink<Tx, Error = WireError> + Send + Unpin + 'static,
        impl Stream<Item = Rx> + Send + Unpin + 'static,
    ),
    String,
>
where
    Tx: serde::Serialize + Send + 'static,
    Rx: serde::de::DeserializeOwned + Send + 'static,
{
    use tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{authority}/")
        .into_client_request()
        .map_err(|error| format!("invalid listener address: {error}"))?;
    let mut credential =
        http::HeaderValue::from_str(&format!("{AUTHORIZATION_SCHEME}{}", secret.0))
            .map_err(|_| "the shared secret is not a valid header value".to_owned())?;
    credential.set_sensitive(true);
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, credential);
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let stream = tokio::net::TcpStream::connect(authority)
            .await
            .map_err(|error| format!("could not connect: {error}"))?;
        detect_dead_peer(&stream).map_err(|error| format!("socket options: {error}"))?;
        let (socket, _) = async_tungstenite::tokio::client_async_with_config(
            request,
            stream,
            Some(websocket_config()),
        )
        .await
        .map_err(|error| {
            if refused_secret(&error) {
                "the listener refused the shared secret".to_owned()
            } else {
                format!("the WebSocket handshake failed: {error}")
            }
        })?;
        Ok(json_frames(socket))
    })
    .await
    .map_err(|_| "connecting timed out".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PORT, authority, constant_time_eq, parse_listen};

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
