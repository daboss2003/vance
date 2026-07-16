#!/bin/bash
# macOS .pkg postinstall — OPTIONAL AI-tools prompt for a productbuild/pkgbuild
# flavored Vance installer.
#
# NOTE: Vance's default macOS distribution is a .dmg (drag Vance.app to
# Applications), which has NO postinstall hook — on that path the AI-tools
# prompt is the app's FIRST-RUN onboarding step instead, which is cleaner
# because it runs as the user. This script is only for a .pkg build.
#
# A .pkg postinstall runs as ROOT, but Vance's managed tools dir is PER-USER
# (~/Library/Application Support/Vance/tools). So we drop to the logged-in
# console user before invoking the same headless CLI the app and the Windows
# installer use: `vance-app --install-ai-tools all`.
set -euo pipefail

APP="/Applications/Vance.app/Contents/MacOS/vance-app"
[ -x "$APP" ] || exit 0   # nothing to do if the app isn't where we expect

# The user who initiated the install (set by Installer.app for postinstall).
target_user="${USER:-}"
[ -n "$target_user" ] || exit 0
uid="$(id -u "$target_user" 2>/dev/null || true)"
[ -n "$uid" ] || exit 0

# Only the pip tools need Python; if it's absent, skip silently — the user can
# add tools later from the in-app AI tools panel.
if ! sudo -u "$target_user" /usr/bin/env bash -lc 'command -v python3 >/dev/null 2>&1'; then
  exit 0
fi

# Fire-and-forget, running as the console user so files land in the right home.
# (Installer UIs shouldn't block on a multi-minute download; progress shows in
# the app's AI tools panel.)
launchctl asuser "$uid" sudo -u "$target_user" "$APP" --install-ai-tools all >/dev/null 2>&1 &

exit 0
