# Repository Guidelines

## Project Structure & Module Organization

`src/main.rs` contains the `kwin-mcp` stdio server, session lifecycle, KWin integration, and MCP tools. `src/input_bridge.rs` owns input-device bridging. `src/bin/kwin-viewer.rs` is the live host viewer and forwards window input into the isolated session. `build.rs` records build metadata and converts `cursor_v6_fixed.svg` into the embedded cursor image. Cargo configuration and dependency locks live in `Cargo.toml` and `Cargo.lock`.

## Build, Test, and Development Commands

- `cargo build`: build both debug binaries. Requires `rsvg-convert` from librsvg.
- `cargo build --release`: produce optimized binaries in `target/release/`.
- `cargo run --bin kwin-mcp -- --width 1232 --height 924`: run the production MCP server over stdio with an explicit virtual display size.
- `cargo test`: compile all test targets and run the committed test suite.
- `cargo fmt --all -- --check`: verify standard Rust formatting.
- `cargo clippy --all-targets`: enforce the deny-level lints declared in `Cargo.toml`.

Runtime work requires KDE Plasma 6, KWin, bubblewrap, and membership in the `input`, `uinput`, `video`, and `render` groups.

## Coding Style & Naming Conventions

Use Rust 2024 and rustfmt defaults. Name modules and functions with `snake_case`, types and traits with `UpperCamelCase`, and constants with `SCREAMING_SNAKE_CASE`. Keep protocol, timing, display, and kernel values in named authoritative definitions rather than scattered literals. Return structured errors with `Result`; project lints reject `unwrap`, `expect`, `todo`, unchecked casts, wildcard imports, and dead code.

## Testing Guidelines

There is currently no dedicated test directory or coverage threshold. For behavior changes, test end to end through the built `kwin-mcp` stdio entrypoint: call `session_start`, perform the affected MCP operation, and independently verify the resulting GUI or system state. Do not substitute mocks or internal-function tests for this path. Add regression cases under `tests/` with behavior-oriented names when an automated real-system path is available.

## Commit & Pull Request Guidelines

Recent subjects are short and imperative, optionally using a conventional scope, such as `fix(screenshot): bind /dev/nvidia* nodes` or `viewer: collapse nested if-let`. Keep each commit focused. Pull requests should describe the observable change, list real-path verification, identify KDE, GPU, or device assumptions, and include before/after screenshots for viewer or capture changes. Link the relevant issue when one exists.
