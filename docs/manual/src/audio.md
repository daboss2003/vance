# Audio & the Mixer

Vance has a Fairlight-class mixer. Every track flows through a full channel
strip, and you can route to submix buses before the master.

## Signal flow

For each clip and track, audio flows:

```
clip gain / fades / pan (pre-fader)
  → track fader
  → track insert effects (EQ, comp, limiter, plugins)
  → aux sends (pre/post-fader)
  → bus graph (per-bus inserts, fader, pan, sends, routing)
  → master
```

Live playback and offline export **share this exact fold**, so a mix you hear
is the mix you get on export.

## Channel strips & buses

- **Inserts:** per-track and per-bus effect chains.
- **Sends:** tap a track or bus to a bus, pre- or post-fader.
- **Buses:** submix groups (dialogue, music, FX) that route to another bus or
  the master. Routing is cycle-checked.

## Audio plugins

Vance can host third-party **CLAP** and **LV2** audio plugins out of process
(in a sandboxed sidecar) so a plugin crash can't take down the editor. Enable
plugin support at build time; on macOS, LV2 needs `brew install lilv`.

## Waveforms & peaks

Waveforms are drawn from cached peak files, generated in the background on
import.
