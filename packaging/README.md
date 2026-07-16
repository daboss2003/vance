# Packaging assets

Per-platform installer glue for the "add the optional AI tools?" flow. Every
path below drives the **same** headless primitive that the in-app *AI tools*
panel uses, so there is one behaviour to reason about:

```
vance-app --install-ai-tools all        # install all four
vance-app --install-ai-tools whisper,sd # a subset
vance-app --list-ai-tools               # catalog + method
vance-app --ai-tools-status             # JSON status
```

(The three Python tools install via `pip` into Vance's managed venv — no hosting
needed; `sd` is a manifest download. See [`../docs/INSTALLER.md`](../docs/INSTALLER.md).)

| Platform | Prompt lives in | Runs as | File |
|----------|-----------------|---------|------|
| **Windows** (NSIS `.exe`) | installer postinstall hook | the user (`installMode: currentUser`) | [`../crates/vance-app/windows/installer.nsh`](../crates/vance-app/windows/installer.nsh) |
| **macOS** (`.dmg`, default) | app **first-run onboarding** | the user | *(no postinstall — dmg has none)* |
| **macOS** (`.pkg`, optional) | pkg postinstall | dropped to console user | [`macos/postinstall.sh`](macos/postinstall.sh) |
| **Linux** (`.deb`/`.AppImage`) | app **first-run onboarding** | the user | [`linux/postinst`](linux/postinst) *(guidance only — does not auto-install)* |

## Why not always prompt in the OS installer?

Vance's managed tools dir is **per-user** (`…/Vance/tools`). A macOS `.pkg` or
Linux `.deb` postinstall runs as **root**, so a heavy per-user install there
would land in the wrong home and block a system package operation. Those
platforms therefore prompt at **first run** (onboarding), which runs as the
user. Windows NSIS is configured `installMode: currentUser`, so its postinstall
*can* safely call the CLI directly.

Whatever the entry point, declining is lossless: the always-available in-app
**AI tools** panel adds them later.
