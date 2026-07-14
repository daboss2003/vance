# First Run & Setup

The first time you open Vance, a short **welcome wizard** appears. It takes
about a minute, and you can **Skip setup** at any point — the editor is fully
usable without completing it.

## Steps

1. **Welcome** — a quick overview of what Vance does.
2. **What Vance found** — a live capability check. Vance reports:
   - **Required:** FFmpeg and the bundled decoder / encoder / audio engines.
     If a required item is missing, install it (the wizard shows the exact
     command) and reopen Vance.
   - **Optional AI tools:** speech-to-text (captions), voice isolation, subject
     masking, and generative images. Each lights up when you install its
     helper; none is required.
3. **Privacy** — two opt-ins, both **off by default**:
   - *Send crash reports* — crash logs are always saved locally; this only
     enables sharing them.
   - *Anonymous usage stats* — aggregate, no personal data.
4. **You're set** — start editing.

## Changing your choices later

Everything in the wizard is available afterward in **Settings**. Your choices
are stored in a small `settings.json` in the app config directory:

- macOS: `~/Library/Application Support/Vance/`
- Windows: `%APPDATA%\Vance\`
- Linux: `~/.config/Vance/`

## AI assistant setup

The optional AI assistant can use a hosted provider (paste an API key) or a
local backend. This is never required to edit; set it up when you want it.

## Where crash logs live

If Vance ever crashes, a report is written to `…/Vance/crashes/last-crash.log`
(and appended to `history.log`). Attach it to a bug report.
