# Installing Vance

Vance ships as a native installer for each platform. Download the build for
your OS and run it.

| Platform | Installer | Notes |
| --- | --- | --- |
| macOS | `.dmg` (universal: Intel + Apple Silicon) | Drag Vance to Applications. |
| Windows | `.exe` (NSIS) | Per-user install; includes the auto-updater. |
| Linux | `.AppImage` (primary), plus `.deb` / `.rpm` / Flatpak | Mark the AppImage executable and run it. |

## Optional AI tools

Captions (speech-to-text), voice isolation, subject masking, and generative
images are **optional** add-ons. Vance installs and manages them for you into
its own per-user folder — nothing else on your system changes.

- The Windows installer **asks** whether to add them during setup.
- On macOS/Linux, the **first-run wizard** asks.
- Either way you can skip it and add or remove them **any time** from the
  **AI tools** panel inside the editor.

Three of the four install via `pip` (they need Python 3 on your system);
generative images additionally downloads a model. The editor works fully
without any of them.

## FFmpeg

Vance uses **FFmpeg** for encoding on export. Most builds bundle it; if your
build does not, install it and make sure it is on your `PATH`:

- **macOS:** `brew install ffmpeg`
- **Windows:** `winget install Gyan.FFmpeg`
- **Linux:** use your package manager (e.g. `apt install ffmpeg`).

The first-run wizard tells you whether FFmpeg was found. See
[First Run & Setup](./first-run.md).

## Updates

On Windows and macOS, Vance can update itself from the **stable** or
**nightly** channel. Bundled codec packs (where present) update as a separate
artifact, so a codec update never forces a full app reinstall.

## Verifying the download

Release artifacts are code-signed where the platform supports it (a notarized
DMG on macOS, a signed MSI on Windows). If your OS warns that the app is
unsigned, you likely have an unofficial build — prefer the official release.
