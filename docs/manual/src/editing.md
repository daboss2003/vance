# The Editing Timeline

## Importing media

Use **File ▸ Open** or drag a file into the Browser. Vance probes it with the
decoder and registers it in the project's media library. Drag a library item
onto the timeline to place it.

## Two timeline modes

- **Track mode** (classic): clips live on independent V/A tracks; edits affect
  only the track you touch.
- **Magnetic mode** (FCPX-style): edits ripple across tracks and *connected*
  clips follow their anchor when you move or ripple-delete. Toggle magnetic
  mode per sequence.

## Core edits

| Action | What it does |
| --- | --- |
| Insert / Overwrite | Place a clip, pushing (insert) or replacing (overwrite) what's there. |
| Ripple delete | Remove a clip and close the gap. |
| Trim In / Out | Adjust a clip's start or end. |
| Roll | Move the cut between two clips. |
| Slip | Change what's shown without moving the clip. |
| Slide | Move a clip between its neighbors, adjusting them. |
| Split | Cut a clip at the playhead (connections inherit onto the right half). |

## Clip connections (magnetic)

Connect a clip (a title, a sound effect) to an anchor clip so it travels with
the anchor under magnetic moves and ripples. Deleting or splitting an anchor
keeps connections consistent.

## Markers

Drop markers (cue, chapter, range brackets) with notes to navigate and to
hint chapters on export.

## Version history with branching

Vance records your edit history and lets you **branch** — explore an
alternative cut without losing the original, then switch back or keep either.

## Bins & smart bins

Organize media into bins with Avid-grade custom columns (Scene, Take, Rating,
Camera, …). **Smart bins** collect items that match a query automatically.
