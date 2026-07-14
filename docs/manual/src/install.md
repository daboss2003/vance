# Installing Vance

Vance ships as a native installer for each platform. Download the build for
your OS and run it.

| Platform | Installer | Notes |
| --- | --- | --- |
| macOS | `.dmg` (universal: Intel + Apple Silicon) | Drag Vance to Applications. |
| Windows | `.msi` | Includes the auto-updater. |
| Linux | `.AppImage` (primary), plus `.deb` / `.rpm` / Flatpak | Mark the AppImage executable and run it. |

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
