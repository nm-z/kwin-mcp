use std::{
    env,
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt,
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};

const DEFAULT_MCP_SERVER: &str = "kwin-mcp";
const STRICT_MARKER: &str = "KWIN_MCP_STRICT";

// These are the host-session values the MCP server needs for its viewer,
// clipboard bridge, wallet access, and other deliberate host integrations.
const FORWARDED_HOST_VARIABLES: &[&str] = &[
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_STARTER_ADDRESS",
    "DBUS_STARTER_BUS_TYPE",
];

// Keep less common desktop-control channels out of general Codex commands too.
const BLOCKED_HOST_GUI_VARIABLES: &[&str] = &[
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_STARTER_ADDRESS",
    "DBUS_STARTER_BUS_TYPE",
    "AT_SPI_BUS_ADDRESS",
    "SESSION_MANAGER",
    "ICEAUTHORITY",
    "SWAYSOCK",
    "I3SOCK",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "MIR_SOCKET",
    "XDG_ACTIVATION_TOKEN",
    "DESKTOP_STARTUP_ID",
];

const USAGE: &str = "Usage: kwin-mcp-strict [OPTIONS] [-- [CODEX_ARGS]...]

Launch Codex without host GUI/session access while forwarding the original
host-session environment only to a configured kwin-mcp stdio server.

Options:
  --codex <PATH>            Codex executable to launch (default: codex)
  --mcp-server <NAME>       Configured MCP server name (default: kwin-mcp)
  --allow-host-gui          Explicitly opt out and retain the host environment
  -h, --help                Print this help
  -V, --version             Print the package version

Put Codex's own arguments after --. For example:
  kwin-mcp-strict -- --model gpt-5.6-terra
";

struct Options {
    codex: OsString,
    mcp_server: String,
    allow_host_gui: bool,
    codex_args: Vec<OsString>,
}

fn parse_options() -> Result<Option<Options>> {
    let mut args = env::args_os().skip(1);
    let mut codex = OsString::from("codex");
    let mut mcp_server = DEFAULT_MCP_SERVER.to_owned();
    let mut allow_host_gui = false;
    let mut codex_args = Vec::new();

    while let Some(argument) = args.next() {
        if argument == OsStr::new("--") {
            codex_args.extend(args);
            break;
        }

        if argument == OsStr::new("--codex") {
            codex = args.next().context("--codex requires an executable path")?;
        } else if argument == OsStr::new("--mcp-server") {
            mcp_server = args
                .next()
                .context("--mcp-server requires a name")?
                .into_string()
                .map_err(|_| anyhow!("--mcp-server must be valid UTF-8"))?;
        } else if argument == OsStr::new("--allow-host-gui") {
            allow_host_gui = true;
        } else if argument == OsStr::new("-h") || argument == OsStr::new("--help") {
            print!("{USAGE}");
            return Ok(None);
        } else if argument == OsStr::new("-V") || argument == OsStr::new("--version") {
            println!("kwin-mcp-strict {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        } else {
            bail!(
                "unknown launcher option '{}'; put Codex arguments after --",
                argument.to_string_lossy()
            );
        }
    }

    if mcp_server.is_empty()
        || !mcp_server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        bail!("--mcp-server must contain only ASCII letters, digits, '_' or '-'");
    }

    Ok(Some(Options {
        codex,
        mcp_server,
        allow_host_gui,
        codex_args,
    }))
}

fn add_config(command: &mut Command, override_value: String) {
    command.arg("--config").arg(override_value);
}

fn exec_codex(mut command: Command, executable: &OsStr) -> Result<()> {
    let error = command.exec();
    Err(error).with_context(|| {
        format!(
            "failed to execute Codex command '{}'",
            executable.to_string_lossy()
        )
    })
}

fn main() -> Result<()> {
    let Some(options) = parse_options()? else {
        return Ok(());
    };

    let mut command = Command::new(&options.codex);

    if options.allow_host_gui {
        eprintln!(
            "kwin-mcp-strict: WARNING: host GUI access explicitly enabled (--allow-host-gui)"
        );
        command.env(STRICT_MARKER, "0").args(&options.codex_args);
        return exec_codex(command, &options.codex);
    }

    let mut forwarded_environment = Vec::new();
    for variable in FORWARDED_HOST_VARIABLES {
        if let Some(value) = env::var_os(variable) {
            let value = value
                .into_string()
                .map_err(|_| anyhow!("{variable} contains non-UTF-8 data"))?;
            forwarded_environment.push((*variable, value));
        }
    }

    for variable in BLOCKED_HOST_GUI_VARIABLES {
        command.env_remove(variable);
    }
    command.env(STRICT_MARKER, "1");

    // A login/profile environment or a lower-precedence Codex config must not
    // silently restore desktop access for shell tools.
    add_config(
        &mut command,
        "shell_environment_policy.experimental_use_profile=false".to_owned(),
    );
    for variable in BLOCKED_HOST_GUI_VARIABLES {
        add_config(
            &mut command,
            format!("shell_environment_policy.set.{variable}=\"\""),
        );
    }
    add_config(
        &mut command,
        format!("shell_environment_policy.set.{STRICT_MARKER}=\"1\""),
    );

    // Codex documents mcp_servers.<id>.env as a per-stdio-server environment
    // map. JSON string literals are also valid TOML basic strings here.
    for (variable, value) in &forwarded_environment {
        let value = serde_json::to_string(value)
            .with_context(|| format!("failed to encode {variable} for Codex configuration"))?;
        add_config(
            &mut command,
            format!("mcp_servers.{}.env.{variable}={value}", options.mcp_server),
        );
    }

    eprintln!(
        "kwin-mcp-strict: strict host-GUI isolation active; forwarded {} host-session value(s) only to MCP server '{}' (use --allow-host-gui to opt out)",
        forwarded_environment.len(),
        options.mcp_server
    );

    command.args(&options.codex_args);
    exec_codex(command, &options.codex)
}
