# Contributing to Vance

Read [`PLAN.md`](../PLAN.md) first. It is the source of truth.

## Setup

### All platforms

1. **Rust** — install [rustup](https://rustup.rs). The toolchain version is pinned in `rust-toolchain.toml`; rustup will fetch it automatically.
2. **Node + pnpm** — Node version is pinned in `.nvmrc`. With nvm: `nvm use`. Then `corepack enable && corepack prepare pnpm@latest --activate`.

### macOS

```sh
xcode-select --install                   # Xcode Command Line Tools
brew install pkg-config
brew install ffmpeg@7                    # NB: ffmpeg@7, not ffmpeg (8.x is too new for our bindings)
brew install opencolorio                 # used by future phases
brew install lilv                        # ONLY for `--features lv2` (LV2 plugin hosting); pulls serd
```

`lilv` (and its `serd` dependency) are only needed if you build the plugin
host with `--features lv2`. The default build doesn't link them, so you can
skip `brew install lilv` unless you're working on LV2 hosting.

Why `ffmpeg@7`: `ffmpeg-the-third` 3.x (our binding crate) supports FFmpeg up to 7.1. Brew's default `ffmpeg` formula now ships 8.x, which removed headers our bindings need. `.cargo/config.toml` points the build at the side-by-side `ffmpeg@7` install via `FFMPEG_DIR`, so the build "just works" once `ffmpeg@7` is present — no shell setup required.

### Windows

- Install [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) (C++ workload).
- Install [WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) (usually pre-installed on Windows 11).
- FFmpeg + OpenColorIO via [vcpkg](https://vcpkg.io). Setup details land in Phase 1 when we actually consume them.

### Linux

```sh
# Ubuntu / Debian
sudo apt install build-essential pkg-config libssl-dev curl wget file \
  libgtk-3-dev libwebkit2gtk-4.1-dev librsvg2-dev \
  libayatana-appindicator3-dev libxdo-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev \
  libopencolorio-dev

# ONLY for `--features lv2` (LV2 plugin hosting); skip for the default build:
sudo apt install liblilv-dev libserd-dev
```

For Fedora / Arch the package names differ; consult your distro's documentation. PRs welcome to expand this section.

## Development workflow

```sh
cargo xtask build      # build all Rust crates
cargo xtask test       # run all Rust tests
cargo xtask fmt        # format all Rust code
cargo xtask clippy     # lint all Rust code
cargo xtask ui-dev     # run the Tauri dev shell with hot reload
cargo xtask ui-build   # production build of the frontend
cargo xtask ui-fmt     # format frontend code
cargo xtask ui-lint    # lint frontend code
cargo xtask app-build  # build the frontend AND rebuild vance-app
cargo xtask app-run    # app-build, then launch the app

# IMPORTANT: Tauri 2 embeds the frontend INTO the binary at compile time.
# After any change under ui/, you MUST rebuild vance-app for the change
# to appear. `cargo xtask app-build` does both in one step.
```

CI runs `fmt`, `clippy`, `test`, `ui-lint`, `ui-build` on Windows / macOS / Linux. PRs must pass CI.

### Building and testing without filling your disk

The dependency tree is very large: Tauri, wgpu, wasmtime, FFmpeg and sqlx. A careless build setup grows `target/` past 100 GB. These rules keep it small:

- **Everyday loop: check, then test only what you touched.**
  ```sh
  cargo xtask check                    # type-check everything; no codegen, small on disk
  cargo xtask test -p vance-timeline   # tests for the crate you changed
  cargo test -p vance-app --test it export_video::   # one test module
  ```
  Run the full `cargo xtask test` once per milestone or before a PR, not after every edit.
- **Debug info is minimal on purpose.** Our crates get line tables only, so panics and backtraces still show `file:line`. Dependencies get none. If you need full debug info for a debugger session, turn it on for one build: `CARGO_PROFILE_DEV_DEBUG=true cargo build -p <crate>`.
- **One integration-test binary per heavy crate.** Every `tests/*.rs` file is otherwise its own executable that links the crate's whole dependency tree. Crates that already have a `tests/it/main.rs` take new tests there as modules. Don't add new top-level `tests/*.rs` files to those crates.
- **Avoid release builds unless you need them.** `--release` (LTO) is a second full copy of everything. Use it only for performance checks and installers. The same goes for flipping feature flags (`--features clap`, `lv2`, `ocio`) back and forth: each feature set compiles its own copy of the affected crates.
- **Disk guard.** `cargo xtask check|build|test|clippy|bench` refuse to start, or stop the build, when free space drops below 5 GB. Change the threshold with `VANCE_MIN_FREE_GB`. When `target/` gets too big, `cargo clean` is always safe.

## Architectural decisions

Land an ADR in `docs/adr/` (template at `docs/adr/0000-template.md`) for any cross-crate decision. ADRs are short, monotonically numbered, and never deleted (mark superseded ones explicitly).

## License & sign-off

By contributing, you agree your contribution is dual-licensed under Apache-2.0 OR MIT (see [`LICENSE-APACHE`](LICENSE-APACHE) and [`LICENSE-MIT`](LICENSE-MIT)).

Sign your commits with the [Developer Certificate of Origin](https://developercertificate.org/) (`git commit -s`).

## A note on the viewport

The video viewport is rendered by a native `wgpu` surface that sits *visually above* the Tauri webview chrome (see [ADR 0005](../docs/adr/0005-ui-viewport-parenting.md)). Two consequences:

- **Don't put HTML chrome over the viewport area.** It will be hidden by the native surface.
- **Floating UI that needs to appear over the viewport** (menus, tooltips) must be drawn by a native overlay, not HTML. Patterns for this land in Phase 4.
