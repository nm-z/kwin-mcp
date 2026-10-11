# Release packaging and deployment

`kwin-mcp-deploy` is a shipped release-management CLI. Run it separately from the
stdio MCP server. It never activates services or changes the binaries used by
running clients. A reviewer chooses when to activate an installed release.

## Build and stage

```bash
kwin-mcp-deploy build /absolute/clean/checkout /absolute/new/package
```

The source must have a clean, committed HEAD. The command probes **sentry** and
**archy** concurrently, samples aggregate CPU use for one second, and selects the
lowest use eligible host (ties choose archy). Unreachable hosts or hosts lacking
Cargo, librsvg or libpipewire are skipped. **engi never compiles** and **benji is
not a builder**. There is no build slot limit or dependency on `dsh build`.

A Git bundle transfers the exact commit without requiring it to be pushed.
Each run owns `~/builds/kwin-mcp-deploy/<unique-id>/source`, its separate `target`,
and `build.log`. Builds use `cargo build --locked --release --bins` and the
builder's normal Cargo parallelism. Source, target and logs remain on the builder
for review; remove only that run directory when its evidence is no longer needed.
The command returns the log and packages all four binaries with the units, this
guide, source identity and SHA256SUMS. It does not install anything.

For a separately compiled release, use:

```bash
kwin-mcp-deploy package /absolute/artifacts /absolute/new/package
```

This checks executable files and records that artifact provenance needs separate
verification. Packaging preserves the input binaries. Missing files and existing
package destinations fail; an incomplete package is retained for diagnosis and
cannot be installed until its manifest is complete.

## Explicit installation

Create the destination's parent directory first. Install into a **new** release
directory on one explicitly named host:

```bash
kwin-mcp-deploy install /absolute/package engi /home/nate/releases/kwin-mcp-review-248
kwin-mcp-deploy install /absolute/package archy /home/nate/releases/kwin-mcp-review-248
kwin-mcp-deploy install /absolute/package benji /home/nate/releases/kwin-mcp-review-248
```

Allowed destinations are engi, archy and benji. An unreachable benji is reported
and skipped successfully; other connectivity failures fail the operation. There
is no implicit fleet install. SSH uses BatchMode and a five second connection
timeout; it never asks for credentials. Release paths must be absolute and use
letters, digits, `_`, `-` and `.` in each component, without `.` or `..` components.

The CLI checks the local fixed file manifest, transfers into a private sibling
staging directory, checks the remote checksums, and renames the whole directory
without replacing an existing release. Failed transfers remove their own staging
directory. Existing releases, live symlinks and services are never changed. This
allows review and removal of a private release without affecting production.
Inspect `release.txt` and `SHA256SUMS` before selecting a release in your client's
stdio configuration. Hosts must have compatible architecture, glibc and runtime
libraries; checksums do not establish ABI compatibility. Run a real stdio session
on each installation host before activation. Stop every verification session.

## Optional OAuth gateway and tunnel units

Packages ship templates under `systemd/`. They are **not enabled or started** by
the CLI. Copy reviewed templates to `~/.config/systemd/user/` when intentionally
configuring a gateway, then run `systemctl --user daemon-reload`. Both units follow
`graphical-session.target`; the tunnel binds to `kwin-mcp-gateway.service` and
stops when that gateway stops. Their restart delay is five seconds.

The gateway implementation is an **external prerequisite**, not a binary in this
repository: provide `~/.local/bin/kwin-mcp-gateway` and its private
`~/.config/kwin-mcp-remote/env` according to that gateway's OAuth documentation.
Do not put OAuth secrets into the package or repository. The unit uses `UMask=0077`
and requires the executable and environment file to exist. It deliberately does
not guess OAuth variable names or generate credentials. A missing gateway keeps
the service from starting; it does not provide OAuth protection by itself.

Install `/usr/bin/cloudflared` separately and configure
`~/.cloudflared/kwin-mcp.yml` with your tunnel identity and ingress for your OAuth
gateway. Keep credentials private. The shipped tunnel template does not hardcode
a domain or tunnel token. Review both configurations and verify authentication
before exposing the gateway. Service enablement, tunnel creation, live activation
and rollback of a client's selected release are operator decisions.

`kwin-mcp-shim` from the scratch script is not built by this repository and is not
part of a release package. Deploy any external shim through its owning project.
