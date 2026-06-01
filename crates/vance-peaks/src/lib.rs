//! Audio peak (waveform) cache — byte-exact `.peaks` format + streaming
//! builder + reader. Phase 7.1 M1.
//!
//! # On-disk layout (`<media_content_hash_hex>.peaks`, little-endian)
//!
//! ```text
//!   bytes
//!     0..4    magic               b"VPK1"
//!     4..6    format_version      u16 = 1
//!     6..7    lane_count          u8  = 3   (min / max / rms)
//!     7..8    level_count         u8  in [1, LEVEL_COUNT_MAX]
//!     8..12   sample_rate         u32       source sample rate (Hz)
//!    12..20   total_samples       u64       per-channel sample count (mono frames)
//!    20..32   content_hash[12]    [u8;12]   first 12 B of media's BLAKE3
//!    -------------------------- 32 B header --------------------------
//!    32..(32+17*level_count)      level directory, one 17 B entry per level:
//!                                          { samples_per_bucket u32,
//!                                            bucket_count u32,
//!                                            offset u64,
//!                                            level_idx u8 }
//!    pad..    alignment pad to 4-byte boundary
//!    pad..    level payloads, each one packed lane-major:
//!                 [ min_i16 × bucket_count ]    -- 2*N B
//!                 [ max_i16 × bucket_count ]    -- 2*N B
//!                 [ rms_u16 × bucket_count ]    -- 2*N B
//!             then padded to 4-byte alignment so the next level's
//!             min-lane starts at a 4-byte boundary (also 2-byte
//!             aligned for zero-copy Int16Array views in JS).
//! ```
//!
//! ## Stereo collapse
//!
//! Per source frame, BOTH channels feed the bucket min/max envelope
//! independently — `cur_min = min(cur_min, L, R)` and
//! `cur_max = max(cur_max, L, R)`. This matches Audacity / ffmpeg
//! `showwavespic` and renders anti-phase content (L = -R) correctly,
//! where the previous single-louder-channel picker dropped one side of
//! the excursion. RMS uses the standard stereo formula
//! `ms_i = (L_i^2 + R_i^2) / 2`, then a bucket's
//! `rms = sqrt(mean(ms_i))` over the bucket's frames. Mono inputs are
//! treated as `R = L`.
//!
//! ## Pyramid
//!
//! Level 0 holds the source-resolution buckets. L1, L2, … derive in
//! the same streaming pass via 4× decimation so the writer never needs
//! a second pass over the file. The decimator is frame-weighted so the
//! trailing partial-window bucket carries exact RMS even when its
//! child buckets cover unequal frame counts (e.g. a tail L0 holding
//! <256 frames decimated with three full siblings):
//!
//! ```text
//!   Ln.min = min(Ln-1[k].min for k in window)
//!   Ln.max = max(Ln-1[k].max for k in window)
//!   Ln.rms = sqrt( sum(frames_k * rms_k^2) / sum(frames_k) )
//! ```
//!
//! The on-disk format stores no per-bucket frame count; readers MUST
//! derive the last bucket's effective frame span at any level from
//! `header.total_samples` and `level.samples_per_bucket`. See
//! [`LevelDir::frames_in_bucket`].
//!
//! ## Levels (current build)
//!
//! At 48 kHz, samples-per-bucket / coverage:
//!
//! | level | samples/bucket | ms / bucket |
//! |-------|----------------|-------------|
//! | L0    |            256 |        5.33 |
//! | L1    |          1,024 |       21.33 |
//! | L2    |          4,096 |       85.33 |
//! | L3    |         16,384 |      341.33 |
//!
//! `LEVEL_COUNT_MAX = 8` is reserved in the format; the writer emits
//! only as many levels as are useful for the given `total_samples` (at
//! least 1, never more than fits with ≥ 2 buckets at the coarsest).
//! Readers must consult `header.level_count`, not the const, when
//! iterating.
//!
//! ## Quantization
//!
//! Source samples are clamped to `[-1.0, 1.0]` and scaled by 32767 for
//! `min/max` (i16) and 65535 for `rms` (u16). NaN/inf inputs are
//! treated as 0.0.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

/// File magic: bytes literally `V`, `P`, `K`, `1`.
pub const PEAK_MAGIC: [u8; 4] = *b"VPK1";

/// On-disk format version. Bump on breaking changes.
pub const FORMAT_VERSION: u16 = 1;

/// Number of lanes per bucket. v1 = 3 (min, max, rms). A future
/// stereo-split format would bump `lane_count`; readers MUST refuse
/// any `lane_count != 3` until the matching format_version supports it.
pub const LANE_COUNT: u8 = 3;

/// Maximum number of pyramid levels the format supports. At base
/// `L0 = 256` samples/bucket and decimation 4, `L7` covers
/// `256 * 4^7 = 4_194_304` samples (≈ 87 s @ 48 kHz) — enough that a
/// 24-hour audio file still has ≥ 1000 coarsest-level buckets.
pub const LEVEL_COUNT_MAX: u8 = 8;

/// 6 bytes per bucket per level — `i16 + i16 + u16`. The lanes are
/// stored lane-major within each level so JS can build typed-array
/// views without copying.
pub const BYTES_PER_BUCKET: usize = 6;

/// Fixed-size file header in bytes.
pub const HEADER_SIZE: usize = 32;

/// One level-directory entry, fixed size.
pub const LEVEL_DIR_ENTRY_SIZE: usize = 17;

/// L0 samples per bucket (every level is L0 × 4^level).
pub const L0_SAMPLES_PER_BUCKET: u32 = 256;

/// Decimation factor between adjacent pyramid levels.
pub const LEVEL_RATIO: u32 = 4;

const DECIMATION: usize = LEVEL_RATIO as usize;

/// Samples-per-bucket for `level`, computed as `256 * 4^level`. Returns
/// `None` for any level whose result would not fit in `u32`. With
/// `L0_SAMPLES_PER_BUCKET = 256 = 2^8`, the result is `2^(8 + 2*level)`,
/// so level 11 produces `2^30 = 1_073_741_824` (still fits) and level
/// 12 would produce `2^32` (wraps to 0). Well above `LEVEL_COUNT_MAX`.
pub const fn samples_per_bucket(level: u8) -> Option<u32> {
    if level >= 12 {
        return None;
    }
    Some(L0_SAMPLES_PER_BUCKET << (level as u32 * 2))
}

/// First 12 bytes of the media's content hash — enough to make
/// accidental collisions astronomically unlikely while keeping the
/// header compact.
pub type HashPrefix = [u8; 12];

/// File-level errors. `Io` covers read/write failures; the others
/// indicate the file is not a `.peaks` we can use.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum PeakError {
    /// Wraps `std::io::Error`.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// File too short to even hold the header.
    #[error("file truncated: expected at least {expected} bytes, got {actual}")]
    Truncated {
        /// Bytes we needed.
        expected: usize,
        /// Bytes we got.
        actual: usize,
    },

    /// First four bytes were not `VPK1`.
    #[error("bad magic: expected {expected:?}, got {actual:?}")]
    BadMagic {
        /// Expected magic bytes.
        expected: [u8; 4],
        /// What we read.
        actual: [u8; 4],
    },

    /// Format version this build can't parse.
    #[error("unsupported format_version: file says {found}, this build supports {supported}")]
    UnsupportedVersion {
        /// Version in the file.
        found: u16,
        /// Version this build groks.
        supported: u16,
    },

    /// `lane_count` is wrong, or `level_count` is out of `[1, LEVEL_COUNT_MAX]`.
    #[error("unsupported layout: lane_count={lane_count}, level_count={level_count}")]
    UnsupportedLayout {
        /// What the file claims.
        lane_count: u8,
        /// What the file claims.
        level_count: u8,
    },

    /// Level directory entry contradicts the v1 spec or another entry.
    #[error("inconsistent directory at level {level}: {kind}")]
    InconsistentDirectory {
        /// Index of the offending entry.
        level: u8,
        /// Specific kind of inconsistency.
        kind: DirectoryProblem,
    },

    /// Level directory points outside the file (post overflow-check).
    #[error("level {level} payload out of range: offset {offset} + len {len} > file_len {file_len}")]
    LevelOutOfRange {
        /// Which level.
        level: u8,
        /// Recorded payload offset.
        offset: u64,
        /// Recorded payload length in bytes.
        len: u64,
        /// File length in bytes.
        file_len: u64,
    },

    /// Caller asked for a bucket range that doesn't fit the level.
    #[error("range out of bounds: level {level} has {available} buckets, asked for [{start}..{end})")]
    RangeOutOfBounds {
        /// Which level.
        level: u8,
        /// How many buckets the level actually has.
        available: u32,
        /// Requested start.
        start: u32,
        /// Requested end (exclusive).
        end: u32,
    },

    /// Caller asked for a level the file doesn't have.
    #[error("invalid level: {level} >= {level_count}")]
    InvalidLevel {
        /// Requested level.
        level: u8,
        /// Number of levels in the file.
        level_count: u8,
    },

    /// File would exceed `usize::MAX` bytes on this target.
    #[error("file size {bytes} exceeds usize::MAX on this target")]
    FileTooLarge {
        /// File size in bytes.
        bytes: u64,
    },
}

/// Specific kind of directory inconsistency. Surfaces what the v1
/// spec/invariant said vs what the file said.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DirectoryProblem {
    /// `level_idx` field doesn't match the entry's slot index.
    LevelIdxMismatch {
        /// What the file claims.
        found: u8,
        /// What was expected (= slot index).
        expected: u8,
    },
    /// `samples_per_bucket` doesn't match `256 * 4^level`.
    SamplesPerBucketMismatch {
        /// What the file claims.
        found: u32,
        /// What was expected.
        expected: u32,
    },
    /// `bucket_count` is wrong for the header's `total_samples`.
    BucketCountMismatch {
        /// What the file claims.
        found: u32,
        /// What was expected: `ceil(total_samples / samples_per_bucket)`.
        expected: u32,
    },
    /// Two adjacent level payloads overlap, or this level starts before
    /// the post-directory region.
    OverlappingOrInvalidOffset {
        /// What the file claims.
        found: u64,
        /// Minimum legal start offset (post previous-level end / post
        /// directory).
        minimum: u64,
    },
    /// Arithmetic on offsets/lengths overflowed u64.
    OffsetArithmeticOverflow,
}

impl std::fmt::Display for DirectoryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LevelIdxMismatch { found, expected } => {
                write!(f, "level_idx mismatch (found {found}, expected {expected})")
            }
            Self::SamplesPerBucketMismatch { found, expected } => write!(
                f,
                "samples_per_bucket mismatch (found {found}, expected {expected})"
            ),
            Self::BucketCountMismatch { found, expected } => write!(
                f,
                "bucket_count mismatch (found {found}, expected {expected})"
            ),
            Self::OverlappingOrInvalidOffset { found, minimum } => write!(
                f,
                "overlapping or invalid offset (found {found}, minimum {minimum})"
            ),
            Self::OffsetArithmeticOverflow => write!(f, "offset arithmetic overflow"),
        }
    }
}

/// Parsed `.peaks` header. Cheap to copy.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PeakHeader {
    /// File format version (currently `FORMAT_VERSION`).
    pub format_version: u16,
    /// Lanes per bucket (currently 3).
    pub lane_count: u8,
    /// Pyramid levels in the file (`1..=LEVEL_COUNT_MAX`).
    pub level_count: u8,
    /// Source sample rate in Hz.
    pub sample_rate: u32,
    /// Per-channel sample count (mono frames) folded into the cache.
    pub total_samples: u64,
    /// First 12 bytes of the source media's BLAKE3 content hash.
    pub content_hash_prefix: HashPrefix,
}

impl PeakHeader {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&PEAK_MAGIC);
        out.extend_from_slice(&self.format_version.to_le_bytes());
        out.push(self.lane_count);
        out.push(self.level_count);
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.total_samples.to_le_bytes());
        out.extend_from_slice(&self.content_hash_prefix);
        debug_assert_eq!(out.len(), HEADER_SIZE);
    }

    fn parse(bytes: &[u8]) -> Result<Self, PeakError> {
        if bytes.len() < HEADER_SIZE {
            return Err(PeakError::Truncated {
                expected: HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != PEAK_MAGIC {
            return Err(PeakError::BadMagic {
                expected: PEAK_MAGIC,
                actual: magic,
            });
        }
        let format_version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if format_version != FORMAT_VERSION {
            return Err(PeakError::UnsupportedVersion {
                found: format_version,
                supported: FORMAT_VERSION,
            });
        }
        let lane_count = bytes[6];
        let level_count = bytes[7];
        if lane_count != LANE_COUNT || level_count == 0 || level_count > LEVEL_COUNT_MAX {
            return Err(PeakError::UnsupportedLayout {
                lane_count,
                level_count,
            });
        }
        let sample_rate = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let total_samples = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let mut content_hash_prefix = [0u8; 12];
        content_hash_prefix.copy_from_slice(&bytes[20..32]);
        Ok(Self {
            format_version,
            lane_count,
            level_count,
            sample_rate,
            total_samples,
            content_hash_prefix,
        })
    }
}

/// Per-level directory entry. Tells readers where a level's payload
/// lives and how many buckets it holds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LevelDir {
    /// Source samples per bucket at this level (`256 * 4^level`).
    pub samples_per_bucket: u32,
    /// Number of buckets at this level. `ceil(total_samples / samples_per_bucket)`.
    pub bucket_count: u32,
    /// Byte offset of this level's payload start (the mins lane).
    pub offset: u64,
    /// Level index (0..LEVEL_COUNT_MAX). Stored explicitly so a
    /// misordered directory is detectable.
    pub level_idx: u8,
}

impl LevelDir {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.samples_per_bucket.to_le_bytes());
        out.extend_from_slice(&self.bucket_count.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.push(self.level_idx);
    }

    fn parse(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), LEVEL_DIR_ENTRY_SIZE);
        Self {
            samples_per_bucket: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            bucket_count: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            offset: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            level_idx: bytes[16],
        }
    }

    /// Payload length in bytes for this level (all three lanes).
    pub const fn payload_len(&self) -> u64 {
        self.bucket_count as u64 * BYTES_PER_BUCKET as u64
    }

    /// Byte offset of the `min` lane (same as `offset`).
    pub const fn min_lane_offset(&self) -> u64 {
        self.offset
    }

    /// Byte offset of the `max` lane.
    pub const fn max_lane_offset(&self) -> u64 {
        self.offset + self.bucket_count as u64 * 2
    }

    /// Byte offset of the `rms` lane.
    pub const fn rms_lane_offset(&self) -> u64 {
        self.offset + self.bucket_count as u64 * 4
    }

    /// Effective frame count covered by bucket `bucket_idx` given the
    /// file's `total_samples`. All buckets cover `samples_per_bucket`
    /// frames except possibly the LAST, which covers
    /// `total_samples - bucket_idx * samples_per_bucket` (in
    /// `1..=samples_per_bucket`). Returns `samples_per_bucket` for any
    /// non-final bucket. Use this when mapping source-sample times to
    /// pixels and when interpreting the last bucket's RMS.
    pub const fn frames_in_bucket(&self, bucket_idx: u32, total_samples: u64) -> u32 {
        if self.bucket_count == 0 || total_samples == 0 {
            return 0;
        }
        if bucket_idx >= self.bucket_count {
            return 0;
        }
        if bucket_idx + 1 < self.bucket_count {
            return self.samples_per_bucket;
        }
        // Last bucket: cover `total_samples - prior_full_buckets * spb`,
        // clamped to `[1, samples_per_bucket]`.
        let consumed = bucket_idx as u64 * self.samples_per_bucket as u64;
        if consumed >= total_samples {
            return 0;
        }
        let remaining = total_samples - consumed;
        if remaining > self.samples_per_bucket as u64 {
            self.samples_per_bucket
        } else {
            remaining as u32
        }
    }
}

/// One bucket of (min, max, rms) at any level.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Bucket {
    /// Most-negative envelope sample in the bucket, i16-quantized.
    pub min: i16,
    /// Most-positive envelope sample in the bucket, i16-quantized.
    pub max: i16,
    /// `sqrt(mean((L^2+R^2)/2))` × 65535, clamped to `[0, 65535]`.
    pub rms: u16,
}

impl Bucket {
    /// Identity bucket (silence) — used for the empty-input edge case.
    pub const SILENT: Self = Self {
        min: 0,
        max: 0,
        rms: 0,
    };
}

/// Internal bucket carrying its effective frame count so partial-window
/// decimation can do a frame-weighted RMS. Never serialized.
#[derive(Debug, Clone, Copy)]
struct BuilderBucket {
    bucket: Bucket,
    frames: u32,
}

/// Streaming peak builder. Feed PCM via `push_stereo_chunk` (or
/// `push_mono_chunk`), then `finish()` to get the per-level payloads.
///
/// The builder is allocation-light: it keeps a 4-bucket sliding window
/// for each derived level rather than buffering whole levels in memory,
/// so peak extraction memory is `O(LEVEL_COUNT_MAX)` in source duration.
pub struct PeakBuilder {
    /// Built L0 buckets so far (the only level we store fully — L1..LN
    /// only buffer the 4-bucket sliding window because they're driven
    /// directly into the FinishedPeaks vec at every emission).
    l0: Vec<Bucket>,
    /// All derived levels, one Vec per level (`derived[i]` = L(i+1)).
    derived: Vec<Vec<Bucket>>,
    /// 4-bucket decimation windows: `windows[0]` consumes L0,
    /// `windows[i]` consumes derived[i-1]. Each holds BuilderBuckets so
    /// the partial-window drain at finish() can weight by frames.
    windows: Vec<Vec<BuilderBucket>>,
    /// In-progress L0 accumulator.
    cur_min: f32,
    cur_max: f32,
    cur_sumsq: f64,
    cur_frames: u32,
    /// Total mono frames consumed.
    total_frames: u64,
}

impl Default for PeakBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PeakBuilder {
    /// Create a fresh builder. Maintains windows for all
    /// `LEVEL_COUNT_MAX` derived levels; unused tail levels are dropped
    /// by `finish()` so the on-disk file only carries useful levels.
    pub fn new() -> Self {
        Self {
            l0: Vec::new(),
            derived: (0..LEVEL_COUNT_MAX as usize - 1).map(|_| Vec::new()).collect(),
            windows: (0..LEVEL_COUNT_MAX as usize - 1)
                .map(|_| Vec::with_capacity(DECIMATION))
                .collect(),
            cur_min: f32::INFINITY,
            cur_max: f32::NEG_INFINITY,
            cur_sumsq: 0.0,
            cur_frames: 0,
            total_frames: 0,
        }
    }

    /// Feed one interleaved-stereo PCM chunk: `[L0, R0, L1, R1, ...]`.
    /// Length must be even; an odd-length tail is treated as a partial
    /// frame and silently dropped (matches the decoder's invariant).
    pub fn push_stereo_chunk(&mut self, samples: &[f32]) {
        let frames = samples.len() / 2;
        for f in 0..frames {
            let l = sanitize(samples[2 * f]);
            let r = sanitize(samples[2 * f + 1]);
            self.push_lr(l, r);
        }
    }

    /// Feed mono PCM. Right-channel == left for the RMS formula.
    pub fn push_mono_chunk(&mut self, samples: &[f32]) {
        for &s in samples {
            let s = sanitize(s);
            self.push_lr(s, s);
        }
    }

    fn push_lr(&mut self, l: f32, r: f32) {
        // Both channels feed the envelope independently — anti-phase
        // and asymmetric-stereo content render the full visible swing.
        if l < self.cur_min {
            self.cur_min = l;
        }
        if r < self.cur_min {
            self.cur_min = r;
        }
        if l > self.cur_max {
            self.cur_max = l;
        }
        if r > self.cur_max {
            self.cur_max = r;
        }
        // ms_i = (L^2 + R^2) / 2 — accumulate then mean at flush.
        self.cur_sumsq += ((l as f64) * (l as f64) + (r as f64) * (r as f64)) * 0.5;
        self.cur_frames += 1;
        self.total_frames += 1;
        if self.cur_frames >= L0_SAMPLES_PER_BUCKET {
            self.flush_l0();
        }
    }

    fn flush_l0(&mut self) {
        if self.cur_frames == 0 {
            return;
        }
        let frames = self.cur_frames;
        let ms_mean = self.cur_sumsq / frames as f64;
        let rms = ms_mean.max(0.0).sqrt();
        // `sanitize()` clamps every public input to a finite value
        // before it ever reaches `cur_min`/`cur_max`, so by the time we
        // get here (cur_frames > 0) both extremes must be finite. Hold
        // the invariant in debug so a future direct-write path can't
        // silently break the i16 quantizer.
        debug_assert!(
            self.cur_min.is_finite() && self.cur_max.is_finite(),
            "envelope extremes must be finite after >= 1 sample"
        );
        let bucket = Bucket {
            min: quantize_i16(self.cur_min),
            max: quantize_i16(self.cur_max),
            rms: quantize_u16(rms as f32),
        };
        self.l0.push(bucket);
        self.feed_window(0, BuilderBucket { bucket, frames });
        self.cur_min = f32::INFINITY;
        self.cur_max = f32::NEG_INFINITY;
        self.cur_sumsq = 0.0;
        self.cur_frames = 0;
    }

    fn feed_window(&mut self, level_index: usize, b: BuilderBucket) {
        if level_index >= self.windows.len() {
            return;
        }
        self.windows[level_index].push(b);
        if self.windows[level_index].len() == DECIMATION {
            let window: Vec<BuilderBucket> = self.windows[level_index].drain(..).collect();
            let derived = decimate(&window);
            self.derived[level_index].push(derived.bucket);
            self.feed_window(level_index + 1, derived);
        }
    }

    /// Finalize: flush the partial L0 bucket and drain partial decimation
    /// windows so every L0 bucket is represented at every emitted level.
    /// Trailing levels with zero buckets are dropped.
    pub fn finish(mut self) -> FinishedPeaks {
        self.flush_l0();
        // Drain partial windows from L0→L1, then L1→L2, etc.
        for level_index in 0..self.windows.len() {
            if !self.windows[level_index].is_empty() {
                let window: Vec<BuilderBucket> =
                    self.windows[level_index].drain(..).collect();
                let derived = decimate(&window);
                self.derived[level_index].push(derived.bucket);
                // Propagate this partial-derived bucket into the next
                // window so coarser levels don't truncate at the tail.
                if level_index + 1 < self.windows.len() {
                    self.windows[level_index + 1].push(derived);
                }
            }
        }
        // Assemble levels: L0 first, then as many derived as carry at
        // least one bucket AND can have ≥ 2 buckets at the coarsest
        // (so we don't waste disk on a 1-bucket level that adds zero
        // information past its parent).
        let mut levels: Vec<Vec<Bucket>> = Vec::with_capacity(LEVEL_COUNT_MAX as usize);
        levels.push(std::mem::take(&mut self.l0));
        for v in self.derived.iter_mut() {
            if v.is_empty() {
                break;
            }
            levels.push(std::mem::take(v));
        }
        // Trim trailing levels that carry no decimation gain (same
        // bucket count as their parent — typically a partial-window
        // drain of a single bucket through every coarser level).
        while levels.len() >= 2 {
            let i = levels.len() - 1;
            if levels[i].len() == levels[i - 1].len() {
                levels.pop();
            } else {
                break;
            }
        }
        // Guarantee ≥ 1 level (empty L0 is fine).
        if levels.is_empty() {
            levels.push(Vec::new());
        }
        FinishedPeaks {
            total_frames: self.total_frames,
            levels,
        }
    }

    /// Live progress: total source frames consumed so far. Useful for
    /// the `peaks://progress` event the PeaksService emits.
    pub const fn frames_consumed(&self) -> u64 {
        self.total_frames
    }
}

/// Output of `PeakBuilder::finish`: the level payloads ready to write,
/// L0 first. `levels.len()` is the effective level count (`1..=LEVEL_COUNT_MAX`).
#[derive(Debug, Clone)]
pub struct FinishedPeaks {
    /// Total per-channel frames consumed by the builder.
    pub total_frames: u64,
    /// Buckets per level: `levels[0]` = L0, `levels[1]` = L1, ….
    pub levels: Vec<Vec<Bucket>>,
}

/// Atomically write `header + finished` to `final_path` via
/// `<final_path>.<pid>.<counter>.tmp` + fsync + rename + parent-dir
/// fsync (Unix). The temp file is removed on every error path. The
/// header's `total_samples` and `level_count` are overridden to match
/// the builder so the caller can't desync them.
///
/// Returns the bytes written (header + dir + pad + payloads).
pub fn write_atomic(
    final_path: &Path,
    header: &PeakHeader,
    finished: &FinishedPeaks,
) -> Result<usize, PeakError> {
    let bytes = encode(header, finished)?;
    let tmp_path = unique_tmp_path(final_path);
    let guard = TmpFileGuard::new(tmp_path.clone());
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, final_path)?;
    guard.disarm();
    fsync_parent_dir(final_path);
    Ok(bytes.len())
}

/// RAII guard that removes the tmp path on Drop unless `disarm` was
/// called after a successful rename.
struct TmpFileGuard {
    path: Option<std::path::PathBuf>,
}

impl TmpFileGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }
    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = fs::remove_file(&p);
        }
    }
}

/// Construct a unique tmp path so concurrent writers to the same final
/// path do not race on a deterministic filename. Format:
/// `<final>.<pid>.<counter>.peaks.tmp`.
fn unique_tmp_path(final_path: &Path) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = final_path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    use std::ffi::OsString;
    let mut suffix = OsString::new();
    suffix.push(format!(".{}.{}.peaks.tmp", pid, counter));
    name.push(suffix);
    final_path.with_file_name(name)
}

/// On Unix, durable rename requires fsync on the parent directory.
/// The rename has already returned in the page cache by the time this
/// runs, so the file is visible immediately even on failure — but a
/// crash before the dirent reaches disk can revert it. We log every
/// failure path at WARN so operators can detect degraded durability
/// without surfacing a non-fatal error to the caller's hot path.
fn fsync_parent_dir(final_path: &Path) {
    #[cfg(unix)]
    {
        let parent = match final_path.parent() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    target: "vance_peaks",
                    path = ?final_path,
                    "parent-dir fsync skipped: path has no parent; rename may not be crash-durable"
                );
                return;
            }
        };
        match fs::File::open(parent) {
            Ok(d) => {
                if let Err(e) = d.sync_all() {
                    tracing::warn!(
                        target: "vance_peaks",
                        path = ?final_path,
                        err = ?e,
                        "parent-dir fsync failed; rename may not be crash-durable"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "vance_peaks",
                    parent = ?parent,
                    err = ?e,
                    "parent-dir open failed; cannot fsync; rename may not be crash-durable"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = final_path;
    }
}

/// Pure-Rust encoder — exposed for tests and for callers that need to
/// ship the bytes over IPC without touching disk.
///
/// `header.total_samples` and `header.level_count` are derived from
/// `finished` and overwrite whatever the caller passed in
/// (`lane_count`, `format_version`, `sample_rate`, and
/// `content_hash_prefix` are taken from the caller verbatim). Returns
/// `Err(FileTooLarge)` if the computed file size would exceed
/// `usize::MAX` on the current target.
pub fn encode(header: &PeakHeader, finished: &FinishedPeaks) -> Result<Vec<u8>, PeakError> {
    let level_count = finished.levels.len();
    // `FinishedPeaks` is `pub` with `pub levels`, so external callers
    // can construct one with arbitrary length. Validate explicitly —
    // not just in debug — so encode/open agree on what's writable.
    if level_count == 0 || level_count > LEVEL_COUNT_MAX as usize {
        return Err(PeakError::UnsupportedLayout {
            lane_count: header.lane_count,
            level_count: u8::try_from(level_count).unwrap_or(u8::MAX),
        });
    }
    let mut header = *header;
    header.total_samples = finished.total_frames;
    header.level_count = level_count as u8;

    let dir_entries = build_directory(&finished.levels)?;
    let total = file_byte_len(&dir_entries)?;
    let mut out = Vec::with_capacity(total);

    header.write(&mut out);
    for d in &dir_entries {
        d.write(&mut out);
    }
    pad_to_alignment(&mut out, 4);
    debug_assert_eq!(out.len() as u64, dir_entries[0].offset);

    for (level_idx, level) in finished.levels.iter().enumerate() {
        debug_assert_eq!(out.len() as u64, dir_entries[level_idx].offset);
        write_level_payload(&mut out, level);
        pad_to_alignment(&mut out, 4);
    }
    debug_assert_eq!(out.len(), total);

    Ok(out)
}

fn build_directory(levels: &[Vec<Bucket>]) -> Result<Vec<LevelDir>, PeakError> {
    let dir_size = LEVEL_DIR_ENTRY_SIZE.checked_mul(levels.len()).ok_or(
        PeakError::InconsistentDirectory {
            level: 0,
            kind: DirectoryProblem::OffsetArithmeticOverflow,
        },
    )?;
    let dir_start = HEADER_SIZE.checked_add(dir_size).ok_or(
        PeakError::InconsistentDirectory {
            level: 0,
            kind: DirectoryProblem::OffsetArithmeticOverflow,
        },
    )?;
    let first_payload = align_up_u64(dir_start as u64, 4);

    let mut entries = Vec::with_capacity(levels.len());
    let mut cursor = first_payload;
    for (i, level) in levels.iter().enumerate() {
        let bucket_count = u32::try_from(level.len()).map_err(|_| {
            PeakError::InconsistentDirectory {
                level: i as u8,
                kind: DirectoryProblem::OffsetArithmeticOverflow,
            }
        })?;
        let entry = LevelDir {
            samples_per_bucket: samples_per_bucket(i as u8).ok_or(
                PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::OffsetArithmeticOverflow,
                },
            )?,
            bucket_count,
            offset: cursor,
            level_idx: i as u8,
        };
        let payload_len = entry.payload_len();
        let next = cursor.checked_add(payload_len).ok_or(
            PeakError::InconsistentDirectory {
                level: i as u8,
                kind: DirectoryProblem::OffsetArithmeticOverflow,
            },
        )?;
        cursor = align_up_u64(next, 4);
        entries.push(entry);
    }
    Ok(entries)
}

fn file_byte_len(dir_entries: &[LevelDir]) -> Result<usize, PeakError> {
    let last = dir_entries
        .last()
        .expect("at least one level is enforced upstream");
    let raw = last
        .offset
        .checked_add(last.payload_len())
        .ok_or(PeakError::InconsistentDirectory {
            level: last.level_idx,
            kind: DirectoryProblem::OffsetArithmeticOverflow,
        })?;
    let aligned = align_up_u64(raw, 4);
    usize::try_from(aligned).map_err(|_| PeakError::FileTooLarge { bytes: aligned })
}

fn write_level_payload(out: &mut Vec<u8>, level: &[Bucket]) {
    // Lane-major: all mins, then all maxes, then all rms. Each lane is
    // 2-byte aligned within the payload, which is what JS Int16Array /
    // Uint16Array views require.
    for b in level {
        out.extend_from_slice(&b.min.to_le_bytes());
    }
    for b in level {
        out.extend_from_slice(&b.max.to_le_bytes());
    }
    for b in level {
        out.extend_from_slice(&b.rms.to_le_bytes());
    }
}

fn pad_to_alignment(out: &mut Vec<u8>, align: usize) {
    let cur = out.len();
    let padded = align_up(cur, align);
    out.resize(padded, 0u8);
}

const fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

const fn align_up_u64(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn sanitize(s: f32) -> f32 {
    if s.is_finite() {
        s.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn quantize_i16(x: f32) -> i16 {
    let scaled = (x.clamp(-1.0, 1.0) * 32767.0).round() as i32;
    scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn quantize_u16(x: f32) -> u16 {
    let scaled = (x.clamp(0.0, 1.0) * 65535.0).round() as i32;
    scaled.clamp(0, u16::MAX as i32) as u16
}

fn decimate(window: &[BuilderBucket]) -> BuilderBucket {
    debug_assert!(!window.is_empty());
    let mut min = i16::MAX;
    let mut max = i16::MIN;
    let mut frame_total: u64 = 0;
    let mut sumsq: f64 = 0.0;
    for b in window {
        if b.bucket.min < min {
            min = b.bucket.min;
        }
        if b.bucket.max > max {
            max = b.bucket.max;
        }
        let frames = b.frames as u64;
        let r = b.bucket.rms as f64 / 65535.0;
        sumsq += (r * r) * (frames as f64);
        frame_total += frames;
    }
    let rms_norm = if frame_total == 0 {
        0.0
    } else {
        (sumsq / frame_total as f64).sqrt()
    };
    BuilderBucket {
        bucket: Bucket {
            min,
            max,
            rms: quantize_u16(rms_norm as f32),
        },
        frames: u32::try_from(frame_total).unwrap_or(u32::MAX),
    }
}

// ---------------------- Reader ----------------------

/// Open a `.peaks` file and parse just the header + directory. The
/// payloads stay on disk; use `read_slice` to pull bucket ranges.
///
/// `PeakFile` uses positional reads (`pread`/`seek_read`) under the
/// hood, so `read_slice` takes `&self` and concurrent callers can
/// share one `PeakFile` without a Mutex.
#[derive(Debug)]
pub struct PeakFile {
    file: fs::File,
    /// Total file length, used to bounds-check slice reads.
    file_len: u64,
    /// Parsed header.
    pub header: PeakHeader,
    /// Per-level directory. Length == `header.level_count`.
    pub levels: Vec<LevelDir>,
}

impl PeakFile {
    /// Open and parse. Returns `Err` on any structural problem (bad
    /// magic, wrong version, inconsistent directory, out-of-range
    /// level pointers, overflow).
    pub fn open(path: &Path) -> Result<Self, PeakError> {
        let file = fs::File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_SIZE as u64 {
            return Err(PeakError::Truncated {
                expected: HEADER_SIZE,
                actual: file_len as usize,
            });
        }

        // Read enough to parse the header. We don't know level_count
        // until we've read it, so do this in two reads.
        let mut header_buf = [0u8; HEADER_SIZE];
        pread_exact(&file, 0, &mut header_buf)?;
        let header = PeakHeader::parse(&header_buf)?;

        let dir_bytes = LEVEL_DIR_ENTRY_SIZE * header.level_count as usize;
        let post_header = HEADER_SIZE + dir_bytes;
        if file_len < post_header as u64 {
            return Err(PeakError::Truncated {
                expected: post_header,
                actual: file_len as usize,
            });
        }
        let mut dir_buf = vec![0u8; dir_bytes];
        pread_exact(&file, HEADER_SIZE as u64, &mut dir_buf)?;

        let min_payload_offset = align_up_u64(post_header as u64, 4);
        let mut levels = Vec::with_capacity(header.level_count as usize);
        let mut prev_end = min_payload_offset;
        for i in 0..header.level_count as usize {
            let slot_bytes = &dir_buf[i * LEVEL_DIR_ENTRY_SIZE..(i + 1) * LEVEL_DIR_ENTRY_SIZE];
            let slot = LevelDir::parse(slot_bytes);

            // (a) level_idx must match the slot.
            if slot.level_idx != i as u8 {
                return Err(PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::LevelIdxMismatch {
                        found: slot.level_idx,
                        expected: i as u8,
                    },
                });
            }
            // (b) samples_per_bucket must follow the 256 × 4^level
            //     spec.
            let expected_spb = samples_per_bucket(i as u8).ok_or(
                PeakError::UnsupportedLayout {
                    lane_count: header.lane_count,
                    level_count: header.level_count,
                },
            )?;
            if slot.samples_per_bucket != expected_spb {
                return Err(PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::SamplesPerBucketMismatch {
                        found: slot.samples_per_bucket,
                        expected: expected_spb,
                    },
                });
            }
            // (c) bucket_count must equal
            //     ceil(total_samples / samples_per_bucket).
            let expected_bc = if header.total_samples == 0 {
                0u32
            } else {
                ceil_div_u64_to_u32(header.total_samples, expected_spb as u64)?
            };
            if slot.bucket_count != expected_bc {
                return Err(PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::BucketCountMismatch {
                        found: slot.bucket_count,
                        expected: expected_bc,
                    },
                });
            }
            // (d) Offset must be ≥ previous-level-end (or post-directory
            //     for level 0), and offset + payload_len must not
            //     overflow u64.
            if slot.offset < prev_end {
                return Err(PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::OverlappingOrInvalidOffset {
                        found: slot.offset,
                        minimum: prev_end,
                    },
                });
            }
            let payload_end = slot
                .offset
                .checked_add(slot.payload_len())
                .ok_or(PeakError::InconsistentDirectory {
                    level: i as u8,
                    kind: DirectoryProblem::OffsetArithmeticOverflow,
                })?;
            if payload_end > file_len {
                return Err(PeakError::LevelOutOfRange {
                    level: i as u8,
                    offset: slot.offset,
                    len: slot.payload_len(),
                    file_len,
                });
            }
            prev_end = align_up_u64(payload_end, 4);
            levels.push(slot);
        }

        Ok(Self {
            file,
            file_len,
            header,
            levels,
        })
    }

    /// Read a bucket range `[bucket_start, bucket_start + bucket_count)`
    /// from `level` and return it in IPC-ready layout:
    /// `[ min × N | max × N | rms × N ]` — three lane slices
    /// concatenated, each `2 × N` bytes. JS callers build
    /// `Int16Array` / `Uint16Array` views directly into this buffer.
    pub fn read_slice(
        &self,
        level: u8,
        bucket_start: u32,
        bucket_count: u32,
    ) -> Result<Vec<u8>, PeakError> {
        if level >= self.header.level_count {
            return Err(PeakError::InvalidLevel {
                level,
                level_count: self.header.level_count,
            });
        }
        let dir = self.levels[level as usize];
        let end = bucket_start
            .checked_add(bucket_count)
            .ok_or(PeakError::RangeOutOfBounds {
                level,
                available: dir.bucket_count,
                start: bucket_start,
                end: u32::MAX,
            })?;
        if end > dir.bucket_count {
            return Err(PeakError::RangeOutOfBounds {
                level,
                available: dir.bucket_count,
                start: bucket_start,
                end,
            });
        }
        let count = bucket_count as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut out = vec![0u8; count * BYTES_PER_BUCKET];
        let (min_dst, rest) = out.split_at_mut(count * 2);
        let (max_dst, rms_dst) = rest.split_at_mut(count * 2);

        self.read_lane(level, dir.min_lane_offset(), bucket_start, min_dst)?;
        self.read_lane(level, dir.max_lane_offset(), bucket_start, max_dst)?;
        self.read_lane(level, dir.rms_lane_offset(), bucket_start, rms_dst)?;

        Ok(out)
    }

    fn read_lane(
        &self,
        level: u8,
        lane_offset: u64,
        bucket_start: u32,
        dst: &mut [u8],
    ) -> Result<(), PeakError> {
        let lane_within = (bucket_start as u64)
            .checked_mul(2)
            .ok_or(PeakError::LevelOutOfRange {
                level,
                offset: lane_offset,
                len: dst.len() as u64,
                file_len: self.file_len,
            })?;
        let abs =
            lane_offset
                .checked_add(lane_within)
                .ok_or(PeakError::LevelOutOfRange {
                    level,
                    offset: lane_offset,
                    len: dst.len() as u64,
                    file_len: self.file_len,
                })?;
        let abs_end = abs.checked_add(dst.len() as u64).ok_or(
            PeakError::LevelOutOfRange {
                level,
                offset: abs,
                len: dst.len() as u64,
                file_len: self.file_len,
            },
        )?;
        if abs_end > self.file_len {
            return Err(PeakError::LevelOutOfRange {
                level,
                offset: abs,
                len: dst.len() as u64,
                file_len: self.file_len,
            });
        }
        pread_exact(&self.file, abs, dst)
    }
}

#[cfg(unix)]
fn pread_exact(f: &fs::File, offset: u64, dst: &mut [u8]) -> Result<(), PeakError> {
    f.read_exact_at(dst, offset).map_err(PeakError::Io)
}

#[cfg(windows)]
fn pread_exact(f: &fs::File, offset: u64, dst: &mut [u8]) -> Result<(), PeakError> {
    // FileExt::seek_read is "at least one byte" semantics; loop to fill.
    // Mirrors Unix `read_exact_at`'s stdlib retry-on-Interrupted contract
    // so callers see identical semantics on both platforms.
    let mut filled = 0;
    while filled < dst.len() {
        match f.seek_read(&mut dst[filled..], offset + filled as u64) {
            Ok(0) => {
                return Err(PeakError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "pread hit EOF before fill",
                )))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(PeakError::Io(e)),
        }
    }
    Ok(())
}

fn ceil_div_u64_to_u32(num: u64, den: u64) -> Result<u32, PeakError> {
    debug_assert!(den > 0);
    let q = num.div_ceil(den);
    u32::try_from(q).map_err(|_| PeakError::InconsistentDirectory {
        level: 0,
        kind: DirectoryProblem::OffsetArithmeticOverflow,
    })
}

/// Pick the best level for a given screen-pixel-per-second density and
/// the actual `header.level_count` of the file.
///
/// Returns the smallest level whose `samples_per_bucket / sample_rate
/// × pixels_per_second` falls in `[1.0, 4.0]` (one bucket per pixel,
/// no aliasing, no overdraw). If every level in `[0, level_count)`
/// would *over*draw (too dense at L0), returns `0` — the UI's only
/// option for very-zoomed-in scrubbing. If every level would *under*draw
/// (too sparse even at the coarsest level), returns `level_count - 1`
/// and the UI MUST further-downsample on the fly per draw column. NaN,
/// zero, and negative inputs all return `level_count - 1` (the cheapest
/// safe choice).
pub fn choose_level(sample_rate: u32, pixels_per_second: f64, level_count: u8) -> u8 {
    if level_count == 0 {
        return 0;
    }
    let last = level_count - 1;
    if sample_rate == 0 || !pixels_per_second.is_finite() || pixels_per_second <= 0.0 {
        return last;
    }
    for i in 0..level_count {
        let spb = match samples_per_bucket(i) {
            Some(s) => s as f64,
            None => return last,
        };
        let bps = spb / sample_rate as f64 * pixels_per_second;
        if (1.0..=4.0).contains(&bps) {
            return i;
        }
    }
    // Zoomed in past L0 (>4 px/bucket): use L0. Zoomed out past last
    // level (<1 px/bucket): use last. Sample the L0 density to decide.
    let l0_density = L0_SAMPLES_PER_BUCKET as f64 / sample_rate as f64 * pixels_per_second;
    if l0_density > 4.0 {
        0
    } else {
        last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_header() -> PeakHeader {
        PeakHeader {
            format_version: FORMAT_VERSION,
            lane_count: LANE_COUNT,
            level_count: 1, // overridden by encode()
            sample_rate: 48_000,
            total_samples: 0, // overridden by encode()
            content_hash_prefix: [
                0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x11, 0x22, 0x33,
            ],
        }
    }

    fn full_scale_sine(samples: usize, frequency_hz: f32, sample_rate: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let t = i as f32 / sample_rate;
            let s = (2.0 * std::f32::consts::PI * frequency_hz * t).sin();
            v.push(s); // L
            v.push(s); // R
        }
        v
    }

    #[test]
    fn header_constants_match_byte_arithmetic() {
        assert_eq!(HEADER_SIZE, 4 + 2 + 1 + 1 + 4 + 8 + 12);
        assert_eq!(LEVEL_DIR_ENTRY_SIZE, 4 + 4 + 8 + 1);
        assert_eq!(BYTES_PER_BUCKET, 2 + 2 + 2);
        assert_eq!(samples_per_bucket(0), Some(256));
        assert_eq!(samples_per_bucket(1), Some(1024));
        assert_eq!(samples_per_bucket(2), Some(4096));
        assert_eq!(samples_per_bucket(3), Some(16384));
        assert_eq!(samples_per_bucket(7), Some(4_194_304));
    }

    #[test]
    fn empty_input_produces_one_empty_level() {
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&[]);
        let finished = b.finish();
        assert_eq!(finished.total_frames, 0);
        assert_eq!(finished.levels.len(), 1, "always at least one level");
        assert!(finished.levels[0].is_empty());
        let bytes = encode(&sample_header(), &finished).unwrap();
        assert!(bytes.len() >= HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE);
    }

    #[test]
    fn dc_signal_min_eq_max() {
        let mut b = PeakBuilder::new();
        let frames = L0_SAMPLES_PER_BUCKET as usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            samples.push(0.5);
            samples.push(0.5);
        }
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 1);
        let bk = f.levels[0][0];
        let expected = quantize_i16(0.5);
        assert_eq!(bk.min, expected);
        assert_eq!(bk.max, expected);
        let expected_rms = quantize_u16(0.5);
        assert!(
            (bk.rms as i32 - expected_rms as i32).abs() <= 1,
            "rms {} ~= expected {}",
            bk.rms,
            expected_rms
        );
    }

    #[test]
    fn full_scale_sine_min_max_span_full_range() {
        let mut b = PeakBuilder::new();
        let frames = (L0_SAMPLES_PER_BUCKET * 4) as usize;
        let samples = full_scale_sine(frames, 1_000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 4);
        assert!(f.levels.len() >= 2);
        for bk in &f.levels[0] {
            assert!((bk.max as i32) > 32_000, "max near +1.0 (got {})", bk.max);
            assert!((bk.min as i32) < -32_000, "min near -1.0 (got {})", bk.min);
            let expected = quantize_u16(0.707);
            assert!(
                (bk.rms as i32 - expected as i32).abs() < 1500,
                "sine rms ~ 0.707 quantized (got {})",
                bk.rms
            );
        }
    }

    #[test]
    fn anti_phase_stereo_renders_full_excursion() {
        // L = +0.9, R = -0.9 for one full bucket. Old code (pick
        // louder-of-the-two) would yield min=max=quantize_i16(+0.9),
        // hiding R's negative swing. The fix feeds both into min/max.
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        let mut samples = Vec::with_capacity(n * 2);
        for _ in 0..n {
            samples.push(0.9);
            samples.push(-0.9);
        }
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        let bk = f.levels[0][0];
        assert_eq!(bk.min, quantize_i16(-0.9));
        assert_eq!(bk.max, quantize_i16(0.9));
    }

    #[test]
    fn asymmetric_stereo_takes_signed_envelope_from_both_channels() {
        // L = +0.3, R = -0.8 — louder channel is R; new envelope must
        // see both: min ≈ -0.8 (from R), max ≈ +0.3 (from L).
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        let mut samples = Vec::with_capacity(n * 2);
        for _ in 0..n {
            samples.push(0.3);
            samples.push(-0.8);
        }
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        let bk = f.levels[0][0];
        assert_eq!(bk.min, quantize_i16(-0.8));
        assert_eq!(bk.max, quantize_i16(0.3));
    }

    #[test]
    fn pyramid_decimation_l1_from_l0() {
        let mut b = PeakBuilder::new();
        // Four full L0 buckets, all near full-scale.
        let n = L0_SAMPLES_PER_BUCKET as usize;
        for k in 0..4 {
            let amplitude = if k % 2 == 0 { 0.9 } else { 0.5 };
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(amplitude);
                chunk.push(amplitude); // mono pair
            }
            b.push_stereo_chunk(&chunk);
        }
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 4);
        assert!(f.levels.len() >= 2);
        let l1 = f.levels[1][0];
        let expected_max = f.levels[0].iter().map(|b| b.max).max().unwrap();
        let expected_min = f.levels[0].iter().map(|b| b.min).min().unwrap();
        assert_eq!(l1.max, expected_max);
        assert_eq!(l1.min, expected_min);
        // L1 RMS = sqrt(mean((0.81 + 0.25 + 0.81 + 0.25) / 4)) = sqrt(0.53) ≈ 0.728.
        let expected_l1 = quantize_u16(0.728);
        assert!(
            (l1.rms as i32 - expected_l1 as i32).abs() < 800,
            "L1 rms ~= 0.728 quantized (got {}, expected ~{})",
            l1.rms,
            expected_l1
        );
    }

    #[test]
    fn pyramid_decimation_l2_from_l1() {
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        for k in 0..16 {
            let v = (k as f32) / 16.0;
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(v);
                chunk.push(v);
            }
            b.push_stereo_chunk(&chunk);
        }
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 16);
        assert_eq!(f.levels[1].len(), 4);
        assert!(f.levels.len() >= 3);
        let l2 = f.levels[2][0];
        let expected_max = quantize_i16(15.0 / 16.0);
        assert_eq!(l2.max, expected_max);
        assert_eq!(l2.min, 0);
    }

    #[test]
    fn frame_weighted_decimation_corrects_tail_bias() {
        // 3 full L0 buckets at amplitude 0.1, then a 100-frame tail at
        // amplitude 0.3. Unweighted decimation would over-amplify the
        // tail; the new weighted formula reports the true frame-mean.
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        for _ in 0..3 {
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(0.1);
                chunk.push(0.1);
            }
            b.push_stereo_chunk(&chunk);
        }
        let mut tail = Vec::with_capacity(200);
        for _ in 0..100 {
            tail.push(0.3);
            tail.push(0.3);
        }
        b.push_stereo_chunk(&tail);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 4);
        assert!(f.levels.len() >= 2);
        let l1 = f.levels[1][0];
        // Frame-weighted: sqrt((3*256*0.01 + 100*0.09)/(3*256+100))
        //               = sqrt(16.68 / 868) ≈ 0.1386
        let expected = quantize_u16(0.1386);
        assert!(
            (l1.rms as i32 - expected as i32).abs() < 800,
            "weighted L1 rms ~ 0.1386 (got {}, expected ~{})",
            l1.rms,
            expected
        );
        // Sanity: unweighted (old code) would have yielded ~0.173, so
        // the new value MUST be lower than that.
        let old_unweighted = quantize_u16(0.173);
        assert!(
            (l1.rms as i32) < (old_unweighted as i32),
            "weighted result should be less than unweighted ~0.173 (got {}, unweighted {})",
            l1.rms,
            old_unweighted
        );
    }

    #[test]
    fn tail_bucket_with_fewer_frames_is_recorded() {
        let mut b = PeakBuilder::new();
        let mut chunk = Vec::with_capacity(200);
        for _ in 0..100 {
            chunk.push(0.3);
            chunk.push(0.3);
        }
        b.push_stereo_chunk(&chunk);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 1, "tail L0");
        // Trim of trailing single-bucket levels: only L0 survives.
        assert_eq!(f.levels.len(), 1);
        assert_eq!(f.levels[0][0].max, quantize_i16(0.3));
    }

    #[test]
    fn last_bucket_effective_frames_via_helper() {
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        for _ in 0..3 {
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(0.1);
                chunk.push(0.1);
            }
            b.push_stereo_chunk(&chunk);
        }
        // Add 100-frame tail
        let mut tail = Vec::with_capacity(200);
        for _ in 0..100 {
            tail.push(0.3);
            tail.push(0.3);
        }
        b.push_stereo_chunk(&tail);
        let f = b.finish();
        let total = f.total_frames;
        // Last L0 bucket covers exactly 100 frames.
        let dir = LevelDir {
            samples_per_bucket: L0_SAMPLES_PER_BUCKET,
            bucket_count: f.levels[0].len() as u32,
            offset: 0,
            level_idx: 0,
        };
        assert_eq!(dir.frames_in_bucket(0, total), L0_SAMPLES_PER_BUCKET);
        assert_eq!(dir.frames_in_bucket(2, total), L0_SAMPLES_PER_BUCKET);
        assert_eq!(dir.frames_in_bucket(3, total), 100);
    }

    #[test]
    fn nan_and_inf_inputs_are_treated_as_silence() {
        let mut b = PeakBuilder::new();
        let n = L0_SAMPLES_PER_BUCKET as usize;
        let mut chunk = Vec::with_capacity(n * 2);
        for _ in 0..n {
            chunk.push(f32::NAN);
            chunk.push(f32::INFINITY);
        }
        b.push_stereo_chunk(&chunk);
        let f = b.finish();
        let bk = f.levels[0][0];
        assert_eq!(bk.min, 0);
        assert_eq!(bk.max, 0);
        assert_eq!(bk.rms, 0);
    }

    #[test]
    fn write_read_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.peaks");

        let mut b = PeakBuilder::new();
        let samples = full_scale_sine(L0_SAMPLES_PER_BUCKET as usize * 8, 1000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let finished = b.finish();
        assert_eq!(finished.levels[0].len(), 8);

        let bytes_written = write_atomic(&path, &sample_header(), &finished).unwrap();
        assert_eq!(
            bytes_written,
            fs::metadata(&path).unwrap().len() as usize,
            "actual file size matches reported"
        );

        let pf = PeakFile::open(&path).unwrap();
        assert_eq!(pf.header.sample_rate, 48_000);
        assert_eq!(pf.header.total_samples, finished.total_frames);
        assert_eq!(pf.header.level_count as usize, finished.levels.len());
        assert_eq!(pf.levels[0].bucket_count, 8);
        assert_eq!(
            pf.header.content_hash_prefix,
            sample_header().content_hash_prefix
        );

        let slice = pf.read_slice(0, 0, 8).unwrap();
        assert_eq!(slice.len(), 8 * BYTES_PER_BUCKET);
        for (i, b) in finished.levels[0].iter().enumerate() {
            let min = i16::from_le_bytes([slice[i * 2], slice[i * 2 + 1]]);
            let max = i16::from_le_bytes([slice[16 + i * 2], slice[16 + i * 2 + 1]]);
            let rms = u16::from_le_bytes([slice[32 + i * 2], slice[32 + i * 2 + 1]]);
            assert_eq!(min, b.min, "min[{i}]");
            assert_eq!(max, b.max, "max[{i}]");
            assert_eq!(rms, b.rms, "rms[{i}]");
        }
    }

    #[test]
    fn partial_slice_reads_correct_range() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.peaks");
        let mut b = PeakBuilder::new();
        let samples = full_scale_sine(L0_SAMPLES_PER_BUCKET as usize * 8, 1000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let finished = b.finish();
        write_atomic(&path, &sample_header(), &finished).unwrap();

        let pf = PeakFile::open(&path).unwrap();
        let slice = pf.read_slice(0, 2, 3).unwrap();
        assert_eq!(slice.len(), 3 * BYTES_PER_BUCKET);
        for (slot, src_idx) in (2..5).enumerate() {
            let min = i16::from_le_bytes([slice[slot * 2], slice[slot * 2 + 1]]);
            let max = i16::from_le_bytes([slice[6 + slot * 2], slice[6 + slot * 2 + 1]]);
            let rms = u16::from_le_bytes([slice[12 + slot * 2], slice[12 + slot * 2 + 1]]);
            assert_eq!(min, finished.levels[0][src_idx].min);
            assert_eq!(max, finished.levels[0][src_idx].max);
            assert_eq!(rms, finished.levels[0][src_idx].rms);
        }
    }

    #[test]
    fn read_slice_rejects_out_of_range() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(
            L0_SAMPLES_PER_BUCKET as usize,
            1000.0,
            48_000.0,
        ));
        write_atomic(&path, &sample_header(), &b.finish()).unwrap();

        let pf = PeakFile::open(&path).unwrap();
        match pf.read_slice(0, 1, 1).unwrap_err() {
            PeakError::RangeOutOfBounds {
                level: 0,
                available: 1,
                start: 1,
                end: 2,
            } => {}
            e => panic!("unexpected error: {e:?}"),
        }
        match pf.read_slice(99, 0, 1).unwrap_err() {
            PeakError::InvalidLevel { .. } => {}
            e => panic!("unexpected error: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_bad_magic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.peaks");
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE];
        bytes[0..4].copy_from_slice(b"NOPE");
        fs::write(&path, &bytes).unwrap();
        match PeakFile::open(&path).unwrap_err() {
            PeakError::BadMagic { .. } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_unsupported_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("future.peaks");
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE];
        bytes[0..4].copy_from_slice(&PEAK_MAGIC);
        bytes[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        bytes[6] = LANE_COUNT;
        bytes[7] = 1;
        fs::write(&path, &bytes).unwrap();
        match PeakFile::open(&path).unwrap_err() {
            PeakError::UnsupportedVersion { found, supported } => {
                assert_eq!(found, FORMAT_VERSION + 1);
                assert_eq!(supported, FORMAT_VERSION);
            }
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_truncated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trunc.peaks");
        fs::write(&path, b"VPK1").unwrap();
        match PeakFile::open(&path).unwrap_err() {
            PeakError::Truncated { .. } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_unsupported_layout() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("layout.peaks");
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE];
        bytes[0..4].copy_from_slice(&PEAK_MAGIC);
        bytes[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[6] = 4; // not 3
        bytes[7] = 1;
        fs::write(&path, &bytes).unwrap();
        match PeakFile::open(&path).unwrap_err() {
            PeakError::UnsupportedLayout {
                lane_count: 4,
                level_count: 1,
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_level_count_zero_or_too_big() {
        let tmp = TempDir::new().unwrap();
        for bad in [0u8, LEVEL_COUNT_MAX + 1] {
            let path = tmp.path().join(format!("lc{bad}.peaks"));
            let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE];
            bytes[0..4].copy_from_slice(&PEAK_MAGIC);
            bytes[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
            bytes[6] = LANE_COUNT;
            bytes[7] = bad;
            fs::write(&path, &bytes).unwrap();
            match PeakFile::open(&path).unwrap_err() {
                PeakError::UnsupportedLayout { .. } => {}
                e => panic!("unexpected for level_count={bad}: {e:?}"),
            }
        }
    }

    /// Build a well-formed file then mutate one byte in the directory
    /// and assert the structural validation rejects it.
    fn write_then_mutate(
        path: &Path,
        mutate: impl FnOnce(&mut [u8]),
    ) -> Result<PeakFile, PeakError> {
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(
            L0_SAMPLES_PER_BUCKET as usize * 16,
            1000.0,
            48_000.0,
        ));
        write_atomic(path, &sample_header(), &b.finish()).unwrap();
        let mut bytes = fs::read(path).unwrap();
        mutate(&mut bytes);
        fs::write(path, &bytes).unwrap();
        PeakFile::open(path)
    }

    #[test]
    fn open_rejects_misordered_level_idx() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("misord.peaks");
        let res = write_then_mutate(&path, |bytes| {
            // Set level 0's level_idx field (last byte of first dir
            // entry) to 99.
            let level0_idx_byte = HEADER_SIZE + LEVEL_DIR_ENTRY_SIZE - 1;
            bytes[level0_idx_byte] = 99;
        });
        match res.unwrap_err() {
            PeakError::InconsistentDirectory {
                level: 0,
                kind: DirectoryProblem::LevelIdxMismatch { found: 99, expected: 0 },
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_bad_samples_per_bucket() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("badspb.peaks");
        let res = write_then_mutate(&path, |bytes| {
            // Overwrite level 0's samples_per_bucket (first 4 bytes of
            // first dir entry) with 0.
            let off = HEADER_SIZE;
            bytes[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        });
        match res.unwrap_err() {
            PeakError::InconsistentDirectory {
                level: 0,
                kind: DirectoryProblem::SamplesPerBucketMismatch { found: 0, expected: 256 },
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_bad_bucket_count() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("badbc.peaks");
        let res = write_then_mutate(&path, |bytes| {
            // Set level 0's bucket_count to 99.
            let off = HEADER_SIZE + 4;
            bytes[off..off + 4].copy_from_slice(&99u32.to_le_bytes());
        });
        match res.unwrap_err() {
            PeakError::InconsistentDirectory {
                level: 0,
                kind: DirectoryProblem::BucketCountMismatch { found: 99, expected: 16 },
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_offset_below_post_directory() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("badoff.peaks");
        let res = write_then_mutate(&path, |bytes| {
            // Point level 0 at offset 0 (inside the header itself).
            let off = HEADER_SIZE + 8;
            bytes[off..off + 8].copy_from_slice(&0u64.to_le_bytes());
        });
        match res.unwrap_err() {
            PeakError::InconsistentDirectory {
                kind: DirectoryProblem::OverlappingOrInvalidOffset { found: 0, .. },
                ..
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn open_rejects_arithmetic_overflow_on_offset() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("overflow.peaks");
        let res = write_then_mutate(&path, |bytes| {
            // Set level 0's offset to near u64::MAX with a large bucket
            // count so offset + payload_len overflows u64.
            let off_field = HEADER_SIZE + 8;
            let bc_field = HEADER_SIZE + 4;
            bytes[off_field..off_field + 8]
                .copy_from_slice(&(u64::MAX - 10).to_le_bytes());
            // bucket_count = 16 (matches expected) — but offset+len overflows
            // because final offset goes past u64::MAX. Also set total_samples
            // so bucket_count check passes (total stays as-is from sample_header).
            let _ = bc_field;
        });
        // Either OverflowOnAdd or LevelOutOfRange — both are acceptable
        // rejections; the important thing is it didn't panic on overflow.
        match res.unwrap_err() {
            PeakError::InconsistentDirectory {
                kind: DirectoryProblem::OffsetArithmeticOverflow,
                ..
            }
            | PeakError::InconsistentDirectory {
                kind: DirectoryProblem::OverlappingOrInvalidOffset { .. },
                ..
            }
            | PeakError::LevelOutOfRange { .. } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn payload_starts_4_byte_aligned() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("aligned.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(
            L0_SAMPLES_PER_BUCKET as usize * 5,
            1000.0,
            48_000.0,
        ));
        let finished = b.finish();
        write_atomic(&path, &sample_header(), &finished).unwrap();

        let pf = PeakFile::open(&path).unwrap();
        for d in &pf.levels {
            assert_eq!(
                d.offset % 4,
                0,
                "level {} starts at {} which is not 4-byte aligned",
                d.level_idx,
                d.offset
            );
        }
    }

    #[test]
    fn level_dir_lane_offsets_match_layout() {
        let d = LevelDir {
            samples_per_bucket: 256,
            bucket_count: 7,
            offset: 96,
            level_idx: 0,
        };
        assert_eq!(d.min_lane_offset(), 96);
        assert_eq!(d.max_lane_offset(), 96 + 14);
        assert_eq!(d.rms_lane_offset(), 96 + 28);
        assert_eq!(d.payload_len(), 42);
    }

    #[test]
    fn choose_level_picks_appropriate_density() {
        // Default 4-level file produced from 16 L0 buckets (4096 samples).
        let level_count = 4;
        assert_eq!(choose_level(48_000, 1000.0, level_count), 0);
        assert_eq!(choose_level(48_000, 100.0, level_count), 1);
        assert_eq!(choose_level(48_000, 30.0, level_count), 2);
        assert_eq!(choose_level(48_000, 1.0, level_count), level_count - 1);

        // Pathological inputs all fall back to coarsest.
        assert_eq!(choose_level(0, 1.0, level_count), level_count - 1);
        assert_eq!(choose_level(48_000, 0.0, level_count), level_count - 1);
        assert_eq!(choose_level(48_000, f64::NAN, level_count), level_count - 1);
        assert_eq!(choose_level(48_000, -1.0, level_count), level_count - 1);
        assert_eq!(choose_level(48_000, 1e9, level_count), 0);
        assert_eq!(choose_level(48_000, 100.0, 0), 0);
    }

    #[test]
    fn write_atomic_does_not_leave_tmp_on_success() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("clean.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(512, 1000.0, 48_000.0));
        write_atomic(&path, &sample_header(), &b.finish()).unwrap();
        assert!(path.is_file());
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".peaks.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .peaks.tmp leftovers: {leftovers:?}"
        );
    }

    #[test]
    fn write_atomic_uses_unique_tmp_path_per_call() {
        let path = std::path::PathBuf::from("/tmp/whatever.peaks");
        let p1 = unique_tmp_path(&path);
        let p2 = unique_tmp_path(&path);
        assert_ne!(p1, p2, "consecutive calls give distinct tmp names");
    }

    #[test]
    fn concurrent_writers_to_same_final_path_succeed_and_leave_no_tmp() {
        // Spawn N threads each writing the same final path many times.
        // Verifies (a) AtomicU64 counter actually serializes tmp names
        // across threads, (b) the RAII guard cleans every tmp on every
        // path, (c) the final file is a valid .peaks at the end.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("race.peaks");
        let header = sample_header();

        const THREADS: usize = 8;
        const ITERS_PER_THREAD: usize = 8;

        std::thread::scope(|s| {
            for t in 0..THREADS {
                let path = path.clone();
                s.spawn(move || {
                    for i in 0..ITERS_PER_THREAD {
                        let mut b = PeakBuilder::new();
                        let freq = 100.0 + (t * 100 + i) as f32;
                        b.push_stereo_chunk(&full_scale_sine(512, freq, 48_000.0));
                        write_atomic(&path, &header, &b.finish())
                            .expect("write_atomic under contention");
                    }
                });
            }
        });

        // Final file is a valid .peaks (whichever writer landed last).
        let pf = PeakFile::open(&path).unwrap();
        assert!(pf.header.total_samples > 0);

        // No stray .peaks.tmp files in the parent directory.
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".peaks.tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .peaks.tmp leftovers after concurrent writes: {leftovers:?}"
        );
    }

    #[test]
    fn encode_rejects_empty_finished_peaks() {
        let finished = FinishedPeaks {
            total_frames: 0,
            levels: Vec::new(),
        };
        match encode(&sample_header(), &finished).unwrap_err() {
            PeakError::UnsupportedLayout {
                level_count: 0, ..
            } => {}
            e => panic!("expected UnsupportedLayout{{level_count:0}}, got {e:?}"),
        }
    }

    #[test]
    fn encode_rejects_too_many_levels() {
        let finished = FinishedPeaks {
            total_frames: 0,
            levels: (0..=LEVEL_COUNT_MAX as usize)
                .map(|_| Vec::new())
                .collect(),
        };
        match encode(&sample_header(), &finished).unwrap_err() {
            PeakError::UnsupportedLayout { level_count, .. } => {
                assert_eq!(level_count, LEVEL_COUNT_MAX + 1);
            }
            e => panic!("expected UnsupportedLayout, got {e:?}"),
        }
    }

    #[test]
    fn samples_per_bucket_returns_none_above_u32_range() {
        // 256 * 4^11 = 2^30, still fits.
        assert_eq!(samples_per_bucket(11), Some(1u32 << 30));
        // 256 * 4^12 = 2^32 — overflow — None.
        assert_eq!(samples_per_bucket(12), None);
        assert_eq!(samples_per_bucket(13), None);
        assert_eq!(samples_per_bucket(255), None);
    }

    #[test]
    fn write_atomic_cleans_tmp_on_error() {
        // Pointing at a non-existent parent directory makes the
        // create-file step fail. The guard MUST not leave a tmp file
        // behind (in this case the parent does not even exist, but the
        // logic must still attempt to remove cleanly).
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("does_not_exist").join("x.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(512, 1000.0, 48_000.0));
        let err = write_atomic(&bad, &sample_header(), &b.finish());
        assert!(err.is_err(), "expected io error");
        // No file under the temp root should be left behind by the
        // guard's cleanup path.
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains(".peaks.tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "tmp cleaned: {leftovers:?}");
    }

    #[test]
    fn mono_chunk_matches_stereo_duplicate() {
        let n = L0_SAMPLES_PER_BUCKET as usize;
        let mut mono = Vec::with_capacity(n);
        let mut stereo = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (i as f32 / n as f32) * 2.0 - 1.0;
            mono.push(s);
            stereo.push(s);
            stereo.push(s);
        }
        let mut a = PeakBuilder::new();
        a.push_mono_chunk(&mono);
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&stereo);
        let fa = a.finish();
        let fb = b.finish();
        assert_eq!(fa.levels[0], fb.levels[0]);
    }

    #[test]
    fn frames_in_bucket_handles_edge_cases() {
        let d = LevelDir {
            samples_per_bucket: 256,
            bucket_count: 5,
            offset: 96,
            level_idx: 0,
        };
        // All non-last buckets cover 256.
        for i in 0..4 {
            assert_eq!(d.frames_in_bucket(i, 5 * 256 - 100), 256);
        }
        // Last bucket covers 156 (= 5*256 - 100 - 4*256).
        assert_eq!(d.frames_in_bucket(4, 5 * 256 - 100), 156);
        // Empty file: 0.
        assert_eq!(d.frames_in_bucket(0, 0), 0);
        // Out-of-range index returns 0 gracefully.
        assert_eq!(d.frames_in_bucket(99, 100), 0);
    }

    #[test]
    fn read_slice_is_callable_with_shared_ref() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shared.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(
            L0_SAMPLES_PER_BUCKET as usize * 4,
            1000.0,
            48_000.0,
        ));
        write_atomic(&path, &sample_header(), &b.finish()).unwrap();
        let pf = PeakFile::open(&path).unwrap();
        // Two concurrent slice reads through &PeakFile (no Mutex needed
        // — the API takes &self thanks to positional reads).
        std::thread::scope(|s| {
            let a = s.spawn(|| pf.read_slice(0, 0, 2).unwrap());
            let b = s.spawn(|| pf.read_slice(0, 2, 2).unwrap());
            assert_eq!(a.join().unwrap().len(), 2 * BYTES_PER_BUCKET);
            assert_eq!(b.join().unwrap().len(), 2 * BYTES_PER_BUCKET);
        });
    }
}
