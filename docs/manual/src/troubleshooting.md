# Troubleshooting

## "Export failed" or no video is produced

Vance needs **FFmpeg** on your `PATH` to encode. Check the first-run wizard's
capability list, or install it (see [Installing Vance](./install.md)) and
reopen Vance.

## An AI tool says it isn't installed

The AI tools call external programs you install yourself. The error message
names the exact command, for example:

- Captions → Whisper / whisper.cpp
- Voice isolation → `pip install demucs`
- Magic Mask → `pip install -U backgroundremover`
- Generative images → `VANCE_IMAGEGEN_CMD`, or `sd` + `VANCE_SD_MODEL`

Install the tool, make sure it's on `PATH` (restart Vance so it picks up the
new `PATH`), and retry.

## The app crashed

A crash report is saved locally at:

- macOS: `~/Library/Application Support/Vance/crashes/last-crash.log`
- Windows: `%APPDATA%\Vance\crashes\last-crash.log`
- Linux: `~/.config/Vance/crashes/last-crash.log`

`history.log` in the same folder keeps a running trail. Attach `last-crash.log`
to a bug report. Crash reports are never uploaded unless you opt in.

## Playback stutters or is slow

- Turn on **proxies** for high-resolution or long-GOP media.
- Close other GPU-heavy apps.
- Check that hardware decode is available for your footage.

## A plugin destabilizes the app

Audio plugins (CLAP/LV2) run in a **sandboxed sidecar**, so a plugin crash
should not take down the editor — it drops the plugin instead. Remove the
offending plugin from the chain and continue.

## Resetting first-run / settings

Delete `settings.json` in the app config directory (see
[First Run & Setup](./first-run.md)) to see the welcome wizard again. This
does not touch your projects.
