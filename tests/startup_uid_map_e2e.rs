#![allow(clippy::expect_used)]

//! A `bwrap` that cannot set up its uid map must produce actionable guidance,
//! not just its stderr. A stand-in `bwrap` first on PATH prints the refusal the
//! real one gives inside a confined sandbox, so no KDE session is needed, but
//! the server still starts its real pasta and systemd scope.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires passt (pasta) and a systemd user session"]
fn a_refused_uid_map_is_reported_with_guidance() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "kwin-mcp-uid-map-{}",
        std::process::id()
    )));
    let home = fixture.0.join("home");
    let bin = fixture.0.join("bin");
    std::fs::create_dir_all(home.join(".cache")).expect("create private HOME");
    std::fs::create_dir_all(&bin).expect("create stand-in directory");
    let bwrap = bin.join("bwrap");
    std::fs::write(
        &bwrap,
        "#!/bin/sh\necho 'bwrap: setting up uid map: Operation not permitted' >&2\nexit 1\n",
    )
    .expect("write stand-in bwrap");
    std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755))
        .expect("make stand-in executable");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH is set")
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_kwin-mcp"))
        .args(["--autoclean", "--no-viewer"])
        .env("PATH", path)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_STATE_HOME", home.join(".state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kwin-mcp");
    let mut stdin = child.stdin.take().expect("server stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("server stdout"));
    let mut send = |message: Value| {
        writeln!(stdin, "{message}").expect("send request");
        stdin.flush().expect("flush request");
    };
    let mut reply = |id: u64| -> Value {
        loop {
            let mut line = String::new();
            let read = stdout.read_line(&mut line).expect("read reply");
            assert!(read > 0, "server closed stdout before reply {id}");
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && message["id"] == id
            {
                return message;
            }
        }
    };
    send(json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{
            "protocolVersion":"2025-06-18", "capabilities":{},
            "clientInfo":{"name":"startup-uid-map-e2e","version":"1"}
        }
    }));
    assert!(reply(1)["result"].is_object());
    send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
    send(json!({
        "jsonrpc":"2.0", "id":2, "method":"tools/call",
        "params":{"name":"session_start",
            "arguments":{"browser_profiles":"fresh", "startup_timeout_seconds":30}}
    }));
    let response = reply(2);
    let _ = child.kill();
    let _ = child.wait();
    let message = response["error"]["message"]
        .as_str()
        .expect("session_start must fail");
    assert!(
        message.contains("bwrap: setting up uid map: Operation not permitted"),
        "{message}"
    );
    assert!(
        message.contains("could not create the session's user namespace"),
        "no guidance in: {message}"
    );
    assert!(
        response["error"]["data"]["guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("uid_map:")),
        "{response}"
    );
}
