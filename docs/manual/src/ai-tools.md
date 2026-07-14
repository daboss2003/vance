# AI-Assisted Tools

Vance's AI tools are **optional** and driven by models you install yourself —
Vance never bundles model weights. Each tool shows a clear message telling you
what to install if its helper is missing, and the editor is fully usable
without any of them.

## Speech-to-text captions

See [Captions & Transcription](./captions.md). Uses Whisper / whisper.cpp.

## Voice isolation & dialogue leveling

Isolate vocals from a mixed track (music/FX removed) using **demucs**
(`pip install demucs`). Vance imports the isolated stem as new audio. Dialogue
leveling measures loudness (BS.1770) and normalizes the spoken range.

## Magic Mask (subject isolation)

Isolate the subject of a clip using **backgroundremover**
(`pip install -U backgroundremover`). The matted result imports as media you
can composite over lower tracks.

## Smart Reframe

Automatically reframe a clip to a new aspect ratio (e.g. 16:9 → 9:16),
keyframing a transform to keep the subject in frame. Re-running is idempotent.

## Generative images

Synthesize an image from a text prompt using a model **you** configure. Two
ways to wire it:

1. Set `VANCE_IMAGEGEN_CMD` to a command template with placeholders —
   `{prompt}`, `{negative}`, `{width}`, `{height}`, `{steps}`, `{seed}`,
   `{out}`. Vance tokenizes it into arguments (no shell), so a prompt with
   spaces or punctuation is always a single, safe argument.
2. Install **`sd`** (stable-diffusion.cpp) on your `PATH` and point
   `VANCE_SD_MODEL` at the weights; Vance builds the command for you.

The generated image lands in the Browser as a still. img2img / inpaint from a
clip frame is on the roadmap.
