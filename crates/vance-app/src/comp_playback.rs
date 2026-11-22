//! Timeline-driven compositing playback (ADR 0007): one loop over the
//! sequence with per-clip streaming feeds and an audio-master clock. There
//! is no clip chaining: the mixer runs straight through cuts and gaps play
//! as black. Filled in by the integration seam.
