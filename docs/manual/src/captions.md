# Captions & Transcription

## Importing & exporting

Vance reads and writes **SRT** and **VTT** subtitles. Import a sidecar file to
attach cues to a sequence; export cues back out as `.srt` / `.vtt`, or (on
export) burn them in.

The parsers are tolerant: byte-order marks, unusual timestamp formats, numeric
cue bodies, and VTT voice tags are handled without dropping cues.

## Editing cues

Cues are stored on the sequence in milliseconds and kept sorted by start time.
Edit text and timing in the Captions panel; each change is a tracked edit.

## Automatic transcription (optional)

Vance can generate captions with an external speech-to-text engine — it does
**not** bundle a model. Install one and put it on your `PATH`:

- OpenAI **Whisper** (`pip install -U openai-whisper`), or
- **whisper.cpp** (the `main` / `whisper` binary).

Then run transcription on a clip; Vance runs the engine off the UI thread and
imports the resulting cues. If no engine is found, you get a clear message
telling you what to install.
