# Vance

A cross-platform, GPU-accelerated, professional-grade non-linear video editor written in Rust, with a first-class AI assistant.

**Status:** pre-alpha. Phase 0 (foundations). Not usable yet.

See [`PLAN.md`](../PLAN.md) for the full roadmap, architecture, and tier ledger (v1 / v2 / v3+).

## Build

Prerequisites:
- Rust (pinned in `rust-toolchain.toml`; install via [rustup](https://rustup.rs))
- Node + pnpm (Node version pinned in `.nvmrc`; use `nvm use`)
- Platform-specific Tauri prerequisites — see `CONTRIBUTING.md`

```sh
cargo xtask build      # build everything
cargo xtask test       # run all tests
cargo xtask ui-dev     # run the Tauri dev shell
```

## License

Dual-licensed under either:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

FFmpeg is bundled separately under LGPL by default; an optional GPL Encoder Pack adds `libx264` / `libx265` software encode under GPL terms. See `LICENSING.md` for details.
