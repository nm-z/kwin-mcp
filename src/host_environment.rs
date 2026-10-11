//! Resolve the host runtime environment inside the MCP server. The strict
//! launcher forwards inherited desktop values but does not invent them.

use anyhow::{Context, Result, ensure};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use zbus::address::transport::{Unix, UnixSocket};
use zbus::address::{Address, Transport};

const USER_RUNTIME_ROOT: &str = "/run/user";
const PRIVATE_RUNTIME_MODE: u32 = 0o700;

pub fn runtime_dir() -> Result<PathBuf> {
    // geteuid has no preconditions and does not change process state.
    let uid = unsafe { nix::libc::geteuid() };
    let inherited = std::env::var_os("XDG_RUNTIME_DIR");
    let directory = inherited
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(USER_RUNTIME_ROOT).join(uid.to_string()));
    ensure!(
        directory.is_absolute(),
        "XDG_RUNTIME_DIR must be an absolute, nonempty path; unset it to use {USER_RUNTIME_ROOT}/{uid}"
    );
    let metadata = std::fs::metadata(&directory).with_context(|| {
        format!(
            "host runtime directory {} is unavailable; set XDG_RUNTIME_DIR to your logged-in user's runtime directory",
            directory.display()
        )
    })?;
    ensure!(
        metadata.is_dir(),
        "XDG_RUNTIME_DIR {} is not a directory",
        directory.display()
    );
    ensure!(
        metadata.uid() == uid,
        "XDG_RUNTIME_DIR {} is owned by UID {}, but the server's effective UID is {uid}",
        directory.display(),
        metadata.uid()
    );
    ensure!(
        metadata.mode() & 0o7777 == PRIVATE_RUNTIME_MODE,
        "XDG_RUNTIME_DIR {} must have mode 0700 (found {:04o}); provide your private login runtime directory",
        directory.display(),
        metadata.mode() & 0o7777
    );
    if inherited.is_none() {
        eprintln!(
            "kwin-mcp: XDG_RUNTIME_DIR is unset; using validated {} for effective UID {uid}",
            directory.display()
        );
    }
    Ok(directory)
}

pub fn session_bus_address(runtime: &Path) -> Result<String> {
    match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        Ok(address) => {
            ensure!(
                !address.is_empty(),
                "DBUS_SESSION_BUS_ADDRESS is empty; unset it to use the validated runtime directory's bus socket"
            );
            let parsed = address.parse::<Address>().context(
                "invalid DBUS_SESSION_BUS_ADDRESS; provide a valid D-Bus address or unset it to use the user runtime bus",
            )?;
            if let Transport::Unix(unix) = parsed.transport()
                && let UnixSocket::File(socket) = unix.path()
            {
                ensure!(
                    socket.is_absolute(),
                    "DBUS_SESSION_BUS_ADDRESS Unix socket path must be absolute"
                );
                let metadata = std::fs::metadata(socket).with_context(|| {
                    format!(
                        "DBUS_SESSION_BUS_ADDRESS socket {} is unavailable; correct the override or unset it to use the user runtime bus",
                        socket.display()
                    )
                })?;
                ensure!(
                    metadata.file_type().is_socket(),
                    "DBUS_SESSION_BUS_ADDRESS path {} is not a Unix socket",
                    socket.display()
                );
                ensure!(
                    metadata.uid() == std::fs::metadata(runtime)?.uid(),
                    "DBUS_SESSION_BUS_ADDRESS socket {} is not owned by the runtime directory's user",
                    socket.display()
                );
            }
            Ok(address)
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("DBUS_SESSION_BUS_ADDRESS must contain valid UTF-8")
        }
        Err(std::env::VarError::NotPresent) => {
            let socket = runtime.join("bus");
            let metadata = std::fs::metadata(&socket).with_context(|| {
                format!(
                    "DBUS_SESSION_BUS_ADDRESS is unset and {} is unavailable; set the address of your logged-in user's session bus",
                    socket.display()
                )
            })?;
            ensure!(
                metadata.file_type().is_socket(),
                "DBUS_SESSION_BUS_ADDRESS is unset and {} is not a Unix socket",
                socket.display()
            );
            // runtime_dir already validates the owner of this private directory.
            let runtime_owner = std::fs::metadata(runtime)?.uid();
            ensure!(
                metadata.uid() == runtime_owner,
                "DBUS_SESSION_BUS_ADDRESS fallback {} is not owned by the runtime directory's user",
                socket.display()
            );
            eprintln!(
                "kwin-mcp: DBUS_SESSION_BUS_ADDRESS is unset; using validated {}",
                socket.display()
            );
            Ok(Address::new(Transport::Unix(Unix::new(UnixSocket::File(socket)))).to_string())
        }
    }
}
