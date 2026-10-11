//! Release packaging and explicit deployment. No service activation is performed.
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BINARIES: &[&str] = &[
    "kwin-mcp",
    "kwin-mcp-strict",
    "kwin-viewer",
    "kwin-mcp-deploy",
];
const BUILD_HOSTS: &[&str] = &["sentry", "archy"];
const INSTALL_HOSTS: &[&str] = &["engi", "archy", "benji"];
const USAGE: &str = "kwin-mcp-deploy: build, package and explicitly install a release

Commands:
  build SOURCE_DIR NEW_PACKAGE_DIR  Build committed HEAD on the lowest CPU host
  package ARTIFACT_DIR NEW_PACKAGE_DIR  Package already compiled release binaries
  inspect PACKAGE_DIR  Verify a package without installing anything
  install PACKAGE_DIR HOST NEW_ABSOLUTE_RELEASE_DIR  Install an immutable release

Build hosts: sentry, archy. Install hosts: engi, archy, benji.
Unreachable build hosts are skipped; an unreachable benji install is skipped.
Existing package/release destinations are rejected. Install never changes a live
symlink, restarts a service, or enables units. Select a private path for evidence.
Requirements: git, ssh, scp, sha256sum; builders also need Cargo, pkg-config,
libpipewire-0.3 and rsvg-convert. SSH uses BatchMode and a 5 second connect timeout.
";
const CPU_PROBE: &str = r#"set -eu
command -v cargo >/dev/null
command -v rsvg-convert >/dev/null
pkg-config --exists libpipewire-0.3
read -r label a b c idle wait irq soft steal rest < /proc/stat
first_total=$((a+b+c+idle+wait+irq+soft+steal))
first_idle=$((idle+wait))
sleep 1
read -r label a b c idle wait irq soft steal rest < /proc/stat
total=$((a+b+c+idle+wait+irq+soft+steal-first_total))
idle_delta=$((idle+wait-first_idle))
[ "$total" -gt 0 ]
printf '%s\n' "$((10000*(total-idle_delta)/total))"
"#;

fn checked(mut command: Command) -> Result<Output> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{command:?}: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output)
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn ssh(host: &str, script: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", host]);
    command.arg(format!("bash -c {}", quote(script)));
    command
}

fn copy_remote(host: &str, source: &Path, remote: &str) -> Result<()> {
    let mut command = Command::new("scp");
    command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
    command.arg(source).arg(format!("{host}:{remote}"));
    checked(command)?;
    Ok(())
}

fn nonce() -> Result<String> {
    Ok(format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}

fn manifest(directory: &Path) -> Result<Vec<u8>> {
    let mut command = Command::new("sha256sum");
    command.current_dir(directory);
    command.args(BINARIES.iter().map(|name| format!("bin/{name}")));
    command.args([
        "systemd/kwin-mcp-gateway.service",
        "systemd/kwin-mcp-tunnel.service",
        "DEPLOYMENT.md",
        "release.txt",
    ]);
    Ok(checked(command)?.stdout)
}

fn verify_package(directory: &Path) -> Result<()> {
    // Recompute the fixed file list rather than trusting paths from an input manifest.
    if manifest(directory)? != fs::read(directory.join("SHA256SUMS"))? {
        return Err("package checksum mismatch".into());
    }
    Ok(())
}

fn package(artifacts: &Path, destination: &Path, identity: &str) -> Result<()> {
    for name in BINARIES {
        let metadata = fs::metadata(artifacts.join(name))?;
        use std::os::unix::fs::PermissionsExt;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("{name} is not an executable file").into());
        }
    }
    // A failed package remains visible for diagnosis; it is never installable without its manifest.
    fs::create_dir(destination)?;
    fs::create_dir(destination.join("bin"))?;
    fs::create_dir(destination.join("systemd"))?;
    for name in BINARIES {
        fs::copy(artifacts.join(name), destination.join("bin").join(name))?;
    }
    fs::write(
        destination.join("systemd/kwin-mcp-gateway.service"),
        include_str!("../../packaging/systemd/kwin-mcp-gateway.service"),
    )?;
    fs::write(
        destination.join("systemd/kwin-mcp-tunnel.service"),
        include_str!("../../packaging/systemd/kwin-mcp-tunnel.service"),
    )?;
    fs::write(
        destination.join("DEPLOYMENT.md"),
        include_str!("../../docs/deployment.md"),
    )?;
    fs::write(destination.join("release.txt"), identity)?;
    fs::write(destination.join("SHA256SUMS"), manifest(destination)?)?;
    verify_package(destination)?;
    println!("package={}", destination.display());
    Ok(())
}

fn build(source: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        return Err("package destination already exists".into());
    }
    let jobs = match env::var("CARGO_BUILD_JOBS") {
        Ok(value) => {
            let count: u32 = value.parse()?;
            if count == 0 {
                return Err("CARGO_BUILD_JOBS must be positive".into());
            }
            format!("export CARGO_BUILD_JOBS={count}; ")
        }
        Err(env::VarError::NotPresent) => String::new(),
        Err(error) => return Err(error.into()),
    };
    let source = fs::canonicalize(source)?;
    let mut status = Command::new("git");
    status
        .arg("-C")
        .arg(&source)
        .args(["status", "--porcelain"]);
    if !checked(status)?.stdout.is_empty() {
        return Err("build requires a clean committed source checkout".into());
    }
    let mut git = Command::new("git");
    git.arg("-C").arg(&source).args(["rev-parse", "HEAD"]);
    let commit = String::from_utf8(checked(git)?.stdout)?.trim().to_owned();
    let requested_hosts =
        env::var("KWIN_MCP_DEPLOY_BUILD_HOSTS").unwrap_or_else(|_| BUILD_HOSTS.join(" "));
    let hosts: Vec<_> = requested_hosts.split_whitespace().collect();
    if hosts.is_empty() || hosts.iter().any(|host| !BUILD_HOSTS.contains(host)) {
        return Err("KWIN_MCP_DEPLOY_BUILD_HOSTS accepts only sentry and archy".into());
    }
    let build_root = env::var("KWIN_MCP_DEPLOY_BUILD_ROOT")
        .unwrap_or_else(|_| "builds/kwin-mcp-deploy".to_owned());
    if build_root != "builds/kwin-mcp-deploy" && !valid_absolute_path(&build_root) {
        return Err("KWIN_MCP_DEPLOY_BUILD_ROOT requires a simple absolute path".into());
    }
    let probes: Vec<_> = hosts
        .iter()
        .map(|host| {
            let host = host.to_string();
            (
                host.clone(),
                thread::spawn(move || -> Option<u32> {
                    match checked(ssh(&host, CPU_PROBE)) {
                        Ok(output) => String::from_utf8(output.stdout).ok()?.trim().parse().ok(),
                        Err(error) => {
                            eprintln!("{host}: unavailable: {error}");
                            None
                        }
                    }
                }),
            )
        })
        .collect();
    let mut candidates = Vec::new();
    for (host, probe) in probes {
        if let Ok(Some(cpu)) = probe.join() {
            eprintln!("{host}: cpu={:.2}%", f64::from(cpu) / 100.0);
            candidates.push((cpu, host));
        }
    }
    candidates.sort_unstable();
    let (_, host) = candidates.first().ok_or("no eligible build host")?;
    let run = format!("{build_root}/{}", nonce()?);
    let temporary = env::temp_dir().join(format!("kwin-mcp-deploy-{}", nonce()?));
    fs::create_dir(&temporary)?;
    let result = (|| -> Result<()> {
        let bundle = temporary.join("source.bundle");
        let mut git = Command::new("git");
        git.arg("-C")
            .arg(&source)
            .arg("bundle")
            .arg("create")
            .arg(&bundle)
            .arg("HEAD");
        checked(git)?;
        checked(ssh(
            host,
            &format!(
                "set -eu; umask 077; mkdir -p {}; mkdir {}",
                quote(&build_root),
                quote(&run)
            ),
        ))?;
        copy_remote(host, &bundle, &format!("{run}/source.bundle"))?;
        let script = format!(
            "set -eu; cd {}; git clone -q source.bundle source; cd source; git checkout -q --detach {}; export CARGO_TARGET_DIR=\"$PWD/../target\"; {jobs}cargo build --locked --release --bins > ../build.log 2>&1 || {{ cat ../build.log >&2; exit 1; }}; cat ../build.log",
            quote(&run),
            quote(&commit)
        );
        eprintln!("builder={host} source={run}/source target={run}/target commit={commit}");
        // Keep all compiler output in the private remote build.log, then return it to the caller.
        let output = checked(ssh(host, &script))?;
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
        for name in BINARIES {
            let mut scp = Command::new("scp");
            scp.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
            scp.arg(format!("{host}:{run}/target/release/{name}"))
                .arg(&temporary);
            checked(scp)?;
        }
        package(
            &temporary,
            destination,
            &format!("commit={commit}\nbuilder={host}\nremote_build={run}\n"),
        )
    })();
    fs::remove_dir_all(&temporary)?;
    result
}

fn valid_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.ends_with('/')
        && !path.split('/').skip(1).any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
}

fn install(directory: &Path, host: &str, destination: &str) -> Result<()> {
    if !INSTALL_HOSTS.contains(&host) {
        return Err("install host must be engi, archy or benji".into());
    }
    // SFTP/scp and shell paths share this conservative grammar. No ~ or option ambiguity.
    if !valid_absolute_path(destination) {
        return Err(
            "release destination must be an absolute path with simple path components".into(),
        );
    }
    verify_package(directory)?;
    if let Err(error) = checked(ssh(host, "true")) {
        if host == "benji" {
            eprintln!("benji unreachable; skipped: {error}");
            return Ok(());
        }
        return Err(error);
    }
    let stage = format!("{destination}.incoming-{}", nonce()?);
    let remote = format!(
        "set -eu; umask 077; test ! -e {dest}; test ! -L {dest}; mkdir {stage}",
        dest = quote(destination),
        stage = quote(&stage)
    );
    checked(ssh(host, &remote))?;
    let result = (|| -> Result<()> {
        let mut scp = Command::new("scp");
        scp.args(["-r", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5"]);
        scp.arg(directory.join("bin"))
            .arg(directory.join("systemd"));
        for file in ["SHA256SUMS", "DEPLOYMENT.md", "release.txt"] {
            scp.arg(directory.join(file));
        }
        scp.arg(format!("{host}:{stage}/"));
        checked(scp)?;
        let script = format!(
            "set -eu; cd {stage}; sha256sum -c SHA256SUMS; cd /; test ! -e {dest}; test ! -L {dest}; mv -T -n {stage} {dest}; test ! -e {stage}",
            dest = quote(destination),
            stage = quote(&stage)
        );
        let output = checked(ssh(host, &script))?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        println!("installed={host}:{destination}");
        Ok(())
    })();
    if result.is_err() {
        let _cleanup = checked(ssh(host, &format!("rm -rf -- {}", quote(&stage))));
    }
    result
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [] => {
            print!("{USAGE}");
            Ok(())
        }
        [help] if help == "--help" || help == "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        [action, directory] if action == "inspect" => {
            let directory = Path::new(directory);
            verify_package(directory)?;
            print!("{}", fs::read_to_string(directory.join("release.txt"))?);
            println!("verified={}", directory.display());
            Ok(())
        }
        [action, source, destination] if action == "build" => {
            build(Path::new(source), Path::new(destination))
        }
        [action, source, destination] if action == "package" => package(
            Path::new(source),
            Path::new(destination),
            "origin=prebuilt; verify artifact provenance separately\n",
        ),
        [action, package, host, destination] if action == "install" => {
            install(Path::new(package), host, destination)
        }
        _ => Err(format!("invalid arguments\n{USAGE}").into()),
    }
}
