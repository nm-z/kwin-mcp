# Codex plugin scaffold

`.codex-plugin/plugin.json` makes this repository a Codex plugin that carries
metadata only. It deliberately declares **no** `mcpServers`, `skills`, `apps`,
or `hooks`. This page records why, what has to exist before an MCP endpoint can
be declared, and the evidence that it does not exist yet.

Manifest shape follows the local Codex `plugin-creator` reference
(`~/.codex/skills/.system/plugin-creator/references/plugin-json-spec.md`):
required real values for `name`, strict-semver `version`, `description`,
`author.name`, and the `interface` block; `hooks` is rejected by validation;
`apps` and `mcpServers` belong in the manifest only when their targets exist.

## Why `mcpServers` is omitted

The manifest schema accepts `mcpServers` either as a path to `./.mcp.json` or
as an object whose values are MCP server configs. Every server config needs a
concrete target: `{"type": "http", "url": "..."}` for a remote server, or a
`command` for a local stdio server. There is no representation for "endpoint
not yet available". A placeholder URL would install cleanly and then fail on
first tool call, so the field is left out until the contract below is met.

A stdio entry pointing at `target/release/kwin-mcp` is representable but was
not added either. It would hard-code a machine-specific absolute path and
duplicate the existing `[mcp_servers.kwin-mcp]` entry in `~/.codex/config.toml`,
which already forwards the host-session environment variables the server needs.
The plugin's purpose is the remote route.

## Observed blocker (2026-09-04)

| Check | Result |
| --- | --- |
| `kwin-mcp` transport | `rmcp::transport::io::stdio()` only (`src/main.rs:3387`); `Cargo.toml` enables rmcp features `server`, `transport-io` and no HTTP transport |
| `~/.cloudflared/config.yml` ingress | `canvas.nm-z.com`, `gmail.nm-z.com`, then catch-all `http_status:404` |
| `~/.cloudflared/hp-mcp.yml` ingress | same two hostnames, same catch-all; no `kwin.nm-z.com` rule in either file |
| `GET https://kwin.nm-z.com/mcp` | HTTP 404, `content-type: text/plain`, body `404 page not found`, `server: cloudflare` |
| `POST https://kwin.nm-z.com/mcp` (MCP `initialize`, streamable HTTP headers) | HTTP 404 |
| `GET https://kwin.nm-z.com/` | HTTP 404 |
| Authentication contract for a remote KWin endpoint | none defined anywhere in this repository or the local Codex/cloudflared configuration |

Reproduce the probe:

```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://kwin.nm-z.com/mcp
curl -sS -o /dev/null -w '%{http_code}\n' -X POST \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  --data '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' \
  https://kwin.nm-z.com/mcp
```

Both print `404` today. The plugin must not declare the endpoint while they do.

## Required route and auth contract

All four items must hold before `mcpServers` is added. None of them exist yet;
this section specifies them, it does not claim them.

### 1. Transport on the server

Codex remote MCP servers use **Streamable HTTP** (official docs:
<https://developers.openai.com/codex/mcp>). `kwin-mcp` therefore needs a
Streamable HTTP listener in addition to stdio. In rmcp that is the
`transport-streamable-http-server` feature; the listener should bind a loopback
address only (for example `127.0.0.1:<port>`) so the tunnel is the sole ingress.

Endpoint behavior at path `/mcp`:

- `POST /mcp` accepts a JSON-RPC message with `Content-Type: application/json`
  and `Accept: application/json, text/event-stream`, and answers with JSON or an
  SSE stream.
- The `initialize` response carries `Mcp-Session-Id`; later requests must send
  it back, and the server rejects unknown session IDs with 404.
- `GET /mcp` with `Accept: text/event-stream` either opens the server-to-client
  stream or returns 405.
- `DELETE /mcp` ends the session or returns 405.

stdio must keep working unchanged for the local `config.toml` entry and for
`kwin-mcp-strict`.

### 2. Route through the tunnel

The hostname already resolves to Cloudflare and the edge answers 404 for every
path, which is consistent with a tunnel catch-all. The tunnel that will carry
the service needs an ingress rule ahead of its catch-all:

```yaml
ingress:
  - hostname: kwin.nm-z.com
    service: http://127.0.0.1:<port>   # the loopback listener from item 1
  # existing rules ...
  - service: http_status:404
```

If `kwin.nm-z.com` is not yet routed to that specific tunnel, the DNS route
must be created for it as well. Success criterion: the POST probe above returns
200, or 401 once item 3 is enabled, never 404.

### 3. Authentication

GUI automation of a real desktop must never be reachable unauthenticated. The
endpoint must reject requests without credentials, and the mechanism must be one
Codex can supply. Codex documents these options for remote servers:

| Mechanism | Server side | Codex side |
| --- | --- | --- |
| Bearer token | validate `Authorization: Bearer <token>`; return 401 otherwise | `bearer_token_env_var = "<ENV_VAR>"` |
| OAuth 2.1 (MCP authorization) | return 401 with `WWW-Authenticate` and serve `/.well-known/oauth-protected-resource`; act as an OAuth resource server | `auth = "oauth"` (default) and `codex mcp login <name>` |
| Gateway headers (for example a Cloudflare Access service token) | gateway enforces the header pair before the origin | `env_http_headers = { "<Header>" = "<ENV_VAR>" }` |

Pick one, record it here with the exact header or well-known URL it uses, and
confirm an unauthenticated POST returns 401 before the route goes live. The
`plugin.json` `mcpServers` object form only carries `type` and `url`; token or
header wiring lives in the user's Codex MCP configuration.

### 4. Manifest change

Only after items 1 to 3 are verified, add to `.codex-plugin/plugin.json`:

```json
"mcpServers": {
  "kwin-mcp": {
    "type": "http",
    "url": "https://kwin.nm-z.com/mcp"
  }
}
```

Bump `version`, re-run the validation below, and re-run the curl probe so the
commit that adds the URL also records a non-404 response.

## Validate the manifest

`jq` is the only tool needed:

```bash
jq empty .codex-plugin/plugin.json
jq -e '
  (.name | test("^[a-z0-9]+(-[a-z0-9]+)*$")) and
  (.version | test("^[0-9]+\\.[0-9]+\\.[0-9]+(-[0-9A-Za-z.-]+)?(\\+[0-9A-Za-z.-]+)?$")) and
  (.description | length > 0) and
  (.author.name | length > 0) and
  (.interface.displayName | length > 0) and
  (has("hooks") | not) and
  (has("mcpServers") | not) and
  (has("apps") | not) and
  ((tostring | test("\\[TODO")) | not)
' .codex-plugin/plugin.json
```

## Install locally

Codex installs plugins from a marketplace, not from a bare path. The plugin
folder name must equal the manifest `name`, so install from a checkout whose
directory is named `kwin-mcp` (the canonical checkout at
`~/Desktop/kwin-mcp` qualifies). Add an entry to the personal marketplace at
`~/.agents/plugins/marketplace.json` whose `source.path` resolves to that
directory, then run `codex plugin add kwin-mcp@<marketplace-name>` and start a
new Codex thread. Until `mcpServers` is added, installing yields metadata only.
