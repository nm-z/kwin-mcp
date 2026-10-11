# Viewer resize and input regression

`viewer_resize_e2e.rb` drives the production `kwin-mcp` stdio entrypoint. It starts an outer isolated desktop for the viewer and an inner isolated session containing a full-screen C GTK test application. Both sessions use empty private homes. Host browser profiles and host windows are outside this test.

## Build

On the currently authorized build host, use an owned source and target directory and the team’s current CPU/memory policy:

```bash
CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_INCREMENTAL=false \
  CARGO_TARGET_DIR=/path/to/private-target cargo build --locked --bins
bash tests/build_viewer_fixtures.sh /path/to/private-fixtures
```

The fixtures require GTK 4, Wayland client development files, `wayland-scanner`, and the Plasma `fake-input.xml` protocol. They use the system XML, falling back to the already-locked `wayland-protocols-plasma` Cargo dependency. Set `KWIN_MCP_FAKE_INPUT_XML` to use another private copy.

Copy `kwin-mcp`, `kwin-viewer`, `viewer-input-probe`, and `viewer-touch` into one private evidence directory on engi. Keep the source script available at a path under `/home`; session `/tmp` is private.

## Run

```bash
XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  KWIN_MCP_E2E_BIN_DIR=/path/to/private-bin \
  ruby tests/viewer_resize_e2e.rb /path/to/new-evidence-directory
```

The runtime also needs `ruby`, `kdotool`, and ImageMagick `magick` on the KDE host. The test performs these real operations:

1. `session_start` for each production stdio server, then `launch_app` and `viewer_open` for the inner session.
2. Unmaximize only the viewer in the outer isolated KWin. Resize its width through that compositor’s `kdotool`, then drag the bottom edge through real Meta+right-button input.
3. Query the resulting geometry through `window_list`, capture through `screenshot`, and verify all four picture corners contain the expected colors, with no bars.
4. Click through the outer session’s `mouse_click` at a known viewer position. Send a real Wayland touch down/up sequence into the same outer compositor. Verify the C application in the inner session received the expected pixels within two pixels.
5. Export the application events and viewer log, then call `session_stop` on both sessions.

Evidence includes JSON-RPC logs, requested and resulting geometry, screenshots, source application click coordinates, viewer resize requests and the sizes returned by `request_inner_size`.

The normal result requires the 800×600 session ratio after both resizes. Under #250’s compositor exception, a differing ratio is reported as a limitation only when the viewer log shows `request_inner_size` returning exactly that measured, uncorrected geometry. Input and corner-color assertions still must pass. A mismatch without that evidence fails the test.
