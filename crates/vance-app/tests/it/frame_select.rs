//! Frame selection: Blocking == Realtime == `decode_at` PTS for 48 targets
//! (first frame with pts >= target; hold the last at EOF) — ADR 0007.
//! Filled in by the sources seam.
