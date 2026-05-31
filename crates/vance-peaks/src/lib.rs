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
//!     7..8    level_count         u8  = 3   (L0 = 256, L1 = 1024, L2 = 4096)
//!     8..12   sample_rate         u32       source sample rate (Hz)
//!    12..20   total_samples       u64       per-channel sample count (mono frames)
//!    20..32   content_hash[12]    [u8;12]   first 12 B of media's BLAKE3
//!    -------------------------- 32 B header --------------------------
//!    32..49   level_dir[0]        17 B   { samples_per_bucket u32,
//!    49..66   level_dir[1]        17 B     bucket_count u32,
//!    66..83   level_dir[2]        17 B     offset u64,
//!                                          level_idx u8 }
//!    83..PAD  alignment pad to 4-byte boundary
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
//! Per source frame `i`, the mono envelope sample is
//! `s_i = if |L_i| >= |R_i| { L_i } else { R_i }` — the *signed* sample
//! of the louder channel. This preserves the visual asymmetry of a
//! real waveform (positive vs negative excursions look different).
//! RMS uses the standard stereo formula
//! `ms_i = (L_i^2 + R_i^2) / 2`, then a bucket's `rms = sqrt(mean(ms_i))`
//! over the bucket's frames. Mono inputs are treated as `R = L`.
//!
//! ## Pyramid
//!
//! Level 0 holds the source-resolution buckets. L1 and L2 derive in
//! the same streaming pass via 4× decimation so the writer never needs
//! a second pass over the file:
//! `Ln.min = min(Ln-1[i..i+4].min)`,
//! `Ln.max = max(Ln-1[i..i+4].max)`,
//! `Ln.rms = sqrt(mean(Ln-1[i..i+4].rms^2))`.
//! The RMS derivation is exact when each child bucket holds the same
//! number of frames (always true except for the last tail bucket).
//!
//! ## Quantization
//!
//! Source samples are clamped to `[-1.0, 1.0]` and scaled by 32767 for
//! `min/max` (i16) and 65535 for `rms` (u16). NaN/inf inputs are
//! treated as 0.0.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use thiserror::Error;

/// File magic: bytes literally `V`, `P`, `K`, `1`.
pub const PEAK_MAGIC: [u8; 4] = *b"VPK1";

/// On-disk format version. Bump on breaking changes.
pub const FORMAT_VERSION: u16 = 1;

/// Number of lanes per bucket. v1 = 3 (min, max, rms). A future
/// stereo-split format would bump `lane_count`; readers MUST refuse
/// any `lane_count != 3` until the matching format_version supports it.
pub const LANE_COUNT: u8 = 3;

/// Number of pyramid levels. v1 = 3.
pub const LEVEL_COUNT: u8 = 3;

/// 6 bytes per bucket per level — `i16 + i16 + u16`. The lanes are
/// stored lane-major within each level so JS can build typed-array
/// views without copying.
pub const BYTES_PER_BUCKET: usize = 6;

/// Fixed-size file header in bytes.
pub const HEADER_SIZE: usize = 32;

/// One level-directory entry, fixed size.
pub const LEVEL_DIR_ENTRY_SIZE: usize = 17;

/// Total directory size for a v1 file.
pub const LEVEL_DIR_TOTAL_SIZE: usize = LEVEL_DIR_ENTRY_SIZE * LEVEL_COUNT as usize;

/// Samples-per-bucket for each level. 4× decimation between levels lets
/// L1 derive from L0 and L2 derive from L1 in the same streaming pass.
/// At 48 kHz: L0 ≈ 5.3 ms, L1 ≈ 21 ms, L2 ≈ 85 ms.
pub const LEVEL_SAMPLES_PER_BUCKET: [u32; LEVEL_COUNT as usize] = [256, 1024, 4096];

/// Decimation between adjacent levels.
const DECIMATION: usize = 4;

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

    /// `lane_count` or `level_count` doesn't match the v1 spec.
    #[error("unsupported layout: lane_count={lane_count}, level_count={level_count}")]
    UnsupportedLayout {
        /// What the file claims.
        lane_count: u8,
        /// What the file claims.
        level_count: u8,
    },

    /// Level directory points outside the file.
    #[error("level {level} payload out of range: offset {offset} + len {len} > file_len {file_len}")]
    LevelOutOfRange {
        /// Which level (0..LEVEL_COUNT).
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
}

/// Parsed `.peaks` header. Cheap to copy.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PeakHeader {
    /// File format version (currently `FORMAT_VERSION`).
    pub format_version: u16,
    /// Lanes per bucket (currently 3).
    pub lane_count: u8,
    /// Pyramid levels (currently 3).
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
        if lane_count != LANE_COUNT || level_count != LEVEL_COUNT {
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
    /// Source samples per bucket at this level (256 / 1024 / 4096).
    pub samples_per_bucket: u32,
    /// Number of buckets at this level. `ceil(total_samples / samples_per_bucket)`.
    pub bucket_count: u32,
    /// Byte offset of this level's payload start (the mins lane).
    pub offset: u64,
    /// Level index (0..LEVEL_COUNT). Stored explicitly so a misordered
    /// directory is detectable.
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
    pub fn payload_len(&self) -> u64 {
        self.bucket_count as u64 * BYTES_PER_BUCKET as u64
    }

    /// Byte offset of the `min` lane (same as `offset`).
    pub fn min_lane_offset(&self) -> u64 {
        self.offset
    }

    /// Byte offset of the `max` lane.
    pub fn max_lane_offset(&self) -> u64 {
        self.offset + self.bucket_count as u64 * 2
    }

    /// Byte offset of the `rms` lane.
    pub fn rms_lane_offset(&self) -> u64 {
        self.offset + self.bucket_count as u64 * 4
    }
}

/// One bucket of (min, max, rms) at any level.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Bucket {
    /// Most-negative envelope sample in the bucket, i16-quantized.
    pub min: i16,
    /// Most-positive envelope sample in the bucket, i16-quantized.
    pub max: i16,
    /// `sqrt(mean(L^2+R^2)/2)` × 65535, clamped to `[0, 65535]`.
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

/// Streaming peak builder. Feed PCM via `push_stereo_chunk` (or
/// `push_mono_chunk`), then `finish()` to get the three level payloads.
///
/// The builder is allocation-light: it keeps a 4-bucket sliding window
/// for each derived level rather than buffering whole levels in memory,
/// so peak extraction memory is `O(1)` in source duration.
pub struct PeakBuilder {
    samples_per_bucket: u32,
    /// L0 buckets accumulated so far.
    l0: Vec<Bucket>,
    /// L1 buckets derived in-pass.
    l1: Vec<Bucket>,
    /// L2 buckets derived in-pass.
    l2: Vec<Bucket>,
    /// In-progress L0 accumulator.
    cur_min: f32,
    cur_max: f32,
    cur_sumsq: f64,
    cur_frames: u32,
    /// Last-4 L0 buckets feeding the L1 decimator.
    l0_window: Vec<Bucket>,
    /// Last-4 L1 buckets feeding the L2 decimator.
    l1_window: Vec<Bucket>,
    /// Total mono frames consumed (== L0 sample count).
    total_frames: u64,
}

impl Default for PeakBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PeakBuilder {
    /// Create a fresh builder. `LEVEL_SAMPLES_PER_BUCKET[0]` (256) is
    /// hard-coded for v1.
    pub fn new() -> Self {
        Self {
            samples_per_bucket: LEVEL_SAMPLES_PER_BUCKET[0],
            l0: Vec::new(),
            l1: Vec::new(),
            l2: Vec::new(),
            cur_min: 0.0,
            cur_max: 0.0,
            cur_sumsq: 0.0,
            cur_frames: 0,
            l0_window: Vec::with_capacity(DECIMATION),
            l1_window: Vec::with_capacity(DECIMATION),
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
            let mono = if l.abs() >= r.abs() { l } else { r };
            self.push_mono_frame(mono, l, r);
        }
    }

    /// Feed mono PCM. Right-channel == left for the RMS formula.
    pub fn push_mono_chunk(&mut self, samples: &[f32]) {
        for &s in samples {
            let s = sanitize(s);
            self.push_mono_frame(s, s, s);
        }
    }

    fn push_mono_frame(&mut self, mono: f32, l: f32, r: f32) {
        if self.cur_frames == 0 {
            self.cur_min = mono;
            self.cur_max = mono;
        } else {
            if mono < self.cur_min {
                self.cur_min = mono;
            }
            if mono > self.cur_max {
                self.cur_max = mono;
            }
        }
        // ms_i = (L^2 + R^2) / 2  — accumulate then mean at flush.
        self.cur_sumsq += ((l as f64) * (l as f64) + (r as f64) * (r as f64)) * 0.5;
        self.cur_frames += 1;
        self.total_frames += 1;
        if self.cur_frames >= self.samples_per_bucket {
            self.flush_l0();
        }
    }

    fn flush_l0(&mut self) {
        if self.cur_frames == 0 {
            return;
        }
        let ms_mean = self.cur_sumsq / self.cur_frames as f64;
        let rms = (ms_mean.max(0.0)).sqrt();
        let b = Bucket {
            min: quantize_i16(self.cur_min),
            max: quantize_i16(self.cur_max),
            rms: quantize_u16(rms as f32),
        };
        self.l0.push(b);
        self.feed_l1(b);
        self.cur_min = 0.0;
        self.cur_max = 0.0;
        self.cur_sumsq = 0.0;
        self.cur_frames = 0;
    }

    fn feed_l1(&mut self, b: Bucket) {
        self.l0_window.push(b);
        if self.l0_window.len() == DECIMATION {
            let derived = decimate(&self.l0_window);
            self.l1.push(derived);
            self.feed_l2(derived);
            self.l0_window.clear();
        }
    }

    fn feed_l2(&mut self, b: Bucket) {
        self.l1_window.push(b);
        if self.l1_window.len() == DECIMATION {
            let derived = decimate(&self.l1_window);
            self.l2.push(derived);
            self.l1_window.clear();
        }
    }

    /// Finalize: flush the partial L0 bucket, drain partial L1/L2
    /// windows so every L0 bucket is represented at every level.
    pub fn finish(mut self) -> FinishedPeaks {
        // Tail L0 with < samples_per_bucket frames is still recorded.
        self.flush_l0();
        if !self.l0_window.is_empty() {
            let derived = decimate(&self.l0_window);
            self.l1.push(derived);
            self.feed_l2(derived);
            self.l0_window.clear();
        }
        if !self.l1_window.is_empty() {
            let derived = decimate(&self.l1_window);
            self.l2.push(derived);
            self.l1_window.clear();
        }
        FinishedPeaks {
            total_frames: self.total_frames,
            levels: [self.l0, self.l1, self.l2],
        }
    }

    /// Live progress: total source frames consumed so far. Useful for
    /// the `peaks://progress` event the PeaksService emits.
    pub fn frames_consumed(&self) -> u64 {
        self.total_frames
    }
}

/// Output of `PeakBuilder::finish`: the three level payloads ready to
/// write. Stored as `Vec<Bucket>` per level rather than as raw bytes so
/// tests can assert structurally; the on-disk encoding happens inside
/// `write_atomic`.
#[derive(Debug, Clone)]
pub struct FinishedPeaks {
    /// Total per-channel frames consumed by the builder.
    pub total_frames: u64,
    /// Buckets per level: `levels[0]` = L0, `levels[1]` = L1, `levels[2]` = L2.
    pub levels: [Vec<Bucket>; LEVEL_COUNT as usize],
}

/// Atomically write `header + finished` to `final_path` via
/// `<final_path>.tmp` + fsync + rename. The source `sample_rate` and
/// `content_hash_prefix` are taken from the header; `total_samples` is
/// overridden to match the builder so the caller can't desync them.
///
/// Returns the bytes written (header + dir + pad + payloads).
pub fn write_atomic(
    final_path: &Path,
    header: &PeakHeader,
    finished: &FinishedPeaks,
) -> Result<usize, PeakError> {
    let bytes = encode(header, finished);
    let tmp_path = with_extension(final_path, "peaks.tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, final_path)?;
    Ok(bytes.len())
}

/// Pure-Rust encoder — exposed for round-trip testing and for callers
/// that need to ship the bytes over IPC without touching disk.
pub fn encode(header: &PeakHeader, finished: &FinishedPeaks) -> Vec<u8> {
    let mut header = *header;
    header.total_samples = finished.total_frames;

    let dir_entries = build_directory(&finished.levels);
    let mut out = Vec::with_capacity(file_byte_len(&dir_entries));

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

    out
}

fn build_directory(levels: &[Vec<Bucket>; LEVEL_COUNT as usize]) -> [LevelDir; LEVEL_COUNT as usize] {
    let dir_start = HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE;
    let first_payload = align_up(dir_start, 4);
    let mut entries = [LevelDir {
        samples_per_bucket: 0,
        bucket_count: 0,
        offset: 0,
        level_idx: 0,
    }; LEVEL_COUNT as usize];
    let mut cursor = first_payload as u64;
    for i in 0..LEVEL_COUNT as usize {
        let bucket_count = levels[i].len() as u32;
        entries[i] = LevelDir {
            samples_per_bucket: LEVEL_SAMPLES_PER_BUCKET[i],
            bucket_count,
            offset: cursor,
            level_idx: i as u8,
        };
        let payload_len = bucket_count as u64 * BYTES_PER_BUCKET as u64;
        cursor += payload_len;
        cursor = align_up(cursor as usize, 4) as u64;
    }
    entries
}

fn file_byte_len(dir_entries: &[LevelDir; LEVEL_COUNT as usize]) -> usize {
    let last = &dir_entries[LEVEL_COUNT as usize - 1];
    align_up((last.offset + last.payload_len()) as usize, 4)
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

fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn with_extension(path: &Path, new_ext: &str) -> std::path::PathBuf {
    let mut p = path.to_path_buf();
    p.set_extension(new_ext);
    p
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

fn decimate(window: &[Bucket]) -> Bucket {
    debug_assert!(!window.is_empty());
    let mut min = i16::MAX;
    let mut max = i16::MIN;
    let mut sumsq = 0.0f64;
    for b in window {
        if b.min < min {
            min = b.min;
        }
        if b.max > max {
            max = b.max;
        }
        let r = b.rms as f64 / 65535.0;
        sumsq += r * r;
    }
    let rms_norm = (sumsq / window.len() as f64).sqrt();
    Bucket {
        min,
        max,
        rms: quantize_u16(rms_norm as f32),
    }
}

// ---------------------- Reader ----------------------

/// Open a `.peaks` file and parse just the header + directory. The
/// payloads stay on disk; use `read_slice` to pull bucket ranges.
#[derive(Debug)]
pub struct PeakFile {
    file: fs::File,
    /// Total file length, used to bounds-check slice reads.
    file_len: u64,
    /// Parsed header.
    pub header: PeakHeader,
    /// Per-level directory.
    pub levels: [LevelDir; LEVEL_COUNT as usize],
}

impl PeakFile {
    /// Open and parse. Returns `Err` on any structural problem (bad
    /// magic, wrong version, out-of-range level pointers).
    pub fn open(path: &Path) -> Result<Self, PeakError> {
        let mut file = fs::File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < (HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE) as u64 {
            return Err(PeakError::Truncated {
                expected: HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE,
                actual: file_len as usize,
            });
        }
        let mut buf = vec![0u8; HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut buf)?;
        let header = PeakHeader::parse(&buf[..HEADER_SIZE])?;
        let mut levels = [LevelDir {
            samples_per_bucket: 0,
            bucket_count: 0,
            offset: 0,
            level_idx: 0,
        }; LEVEL_COUNT as usize];
        for i in 0..LEVEL_COUNT as usize {
            let start = HEADER_SIZE + i * LEVEL_DIR_ENTRY_SIZE;
            levels[i] = LevelDir::parse(&buf[start..start + LEVEL_DIR_ENTRY_SIZE]);
            let payload_end = levels[i].offset + levels[i].payload_len();
            if payload_end > file_len {
                return Err(PeakError::LevelOutOfRange {
                    level: i as u8,
                    offset: levels[i].offset,
                    len: levels[i].payload_len(),
                    file_len,
                });
            }
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
    /// concatenated, each 2*N bytes. The buffer is 4-byte aligned at
    /// the start (the IPC layer guarantees this for typed-array view
    /// safety, and JS only needs 2-byte alignment for Int16/Uint16
    /// anyway).
    pub fn read_slice(
        &mut self,
        level: u8,
        bucket_start: u32,
        bucket_count: u32,
    ) -> Result<Vec<u8>, PeakError> {
        if (level as u8) >= LEVEL_COUNT {
            return Err(PeakError::InvalidLevel {
                level,
                level_count: LEVEL_COUNT,
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

        self.read_lane(dir.min_lane_offset(), bucket_start, min_dst)?;
        self.read_lane(dir.max_lane_offset(), bucket_start, max_dst)?;
        self.read_lane(dir.rms_lane_offset(), bucket_start, rms_dst)?;

        Ok(out)
    }

    fn read_lane(&mut self, lane_offset: u64, bucket_start: u32, dst: &mut [u8]) -> Result<(), PeakError> {
        let abs = lane_offset + bucket_start as u64 * 2;
        let abs_end = abs + dst.len() as u64;
        if abs_end > self.file_len {
            return Err(PeakError::LevelOutOfRange {
                level: 0,
                offset: abs,
                len: dst.len() as u64,
                file_len: self.file_len,
            });
        }
        self.file.seek(SeekFrom::Start(abs))?;
        self.file.read_exact(dst)?;
        Ok(())
    }
}

/// Pick the best level for a given screen-pixel-per-second density.
/// Returns the smallest level whose `samples_per_bucket / sample_rate
/// * pixels_per_second` falls in `[1.0, 4.0]` (one bucket per pixel,
/// no aliasing, no overdraw). Falls back to the coarsest level if
/// every level would overdraw at this zoom (zoomed-way-out case).
pub fn choose_level(sample_rate: u32, pixels_per_second: f64) -> u8 {
    if sample_rate == 0 || pixels_per_second <= 0.0 {
        return LEVEL_COUNT - 1;
    }
    for (i, &spb) in LEVEL_SAMPLES_PER_BUCKET.iter().enumerate() {
        let bps = spb as f64 / sample_rate as f64 * pixels_per_second;
        if (1.0..=4.0).contains(&bps) {
            return i as u8;
        }
    }
    // Zoomed in past L0 (>4 px/bucket): use L0 anyway. Zoomed out past
    // L2 (<1 px/bucket): use L2 and accept the aliasing — there is no
    // L3.
    let l0_density = LEVEL_SAMPLES_PER_BUCKET[0] as f64 / sample_rate as f64 * pixels_per_second;
    if l0_density > 4.0 {
        0
    } else {
        LEVEL_COUNT - 1
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
            level_count: LEVEL_COUNT,
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
        assert_eq!(LEVEL_SAMPLES_PER_BUCKET, [256, 1024, 4096]);
    }

    #[test]
    fn empty_input_produces_zero_bucket_directory() {
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&[]);
        let finished = b.finish();
        assert_eq!(finished.total_frames, 0);
        for lvl in &finished.levels {
            assert!(lvl.is_empty(), "no buckets on empty input");
        }
        let bytes = encode(&sample_header(), &finished);
        assert_eq!(
            bytes.len(),
            align_up(HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE, 4),
            "header + dir + final pad only"
        );
    }

    #[test]
    fn dc_signal_min_eq_max() {
        let mut b = PeakBuilder::new();
        // Exactly one L0 bucket of constant DC at +0.5.
        let frames = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            samples.push(0.5);
            samples.push(0.5);
        }
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 1);
        let bk = f.levels[0][0];
        // mono = 0.5 for every frame → min == max == 0.5 quantized
        let expected = quantize_i16(0.5);
        assert_eq!(bk.min, expected);
        assert_eq!(bk.max, expected);
        // ms = (0.25 + 0.25)/2 = 0.25; rms = 0.5
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
        // 4 L0 buckets worth so we also exercise the L1 decimator.
        let frames = (LEVEL_SAMPLES_PER_BUCKET[0] * 4) as usize;
        let samples = full_scale_sine(frames, 1_000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 4);
        assert_eq!(f.levels[1].len(), 1);
        for bk in &f.levels[0] {
            assert!((bk.max as i32) > 32_000, "max near +1.0 (got {})", bk.max);
            assert!((bk.min as i32) < -32_000, "min near -1.0 (got {})", bk.min);
            // sine RMS = sqrt(0.5) ≈ 0.707. Tolerance accommodates the
            // partial-cycle truncation at 1kHz over 256 samples / 48k.
            let expected = quantize_u16(0.707);
            assert!(
                (bk.rms as i32 - expected as i32).abs() < 1500,
                "sine rms ~ 0.707 quantized (got {})",
                bk.rms
            );
        }
    }

    #[test]
    fn pyramid_decimation_l1_from_l0() {
        let mut b = PeakBuilder::new();
        // Four distinct L0 buckets: alternating loud/quiet.
        let n = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
        for k in 0..4 {
            let amplitude = if k % 2 == 0 { 0.9 } else { 0.1 };
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(amplitude);
                chunk.push(-amplitude);
            }
            b.push_stereo_chunk(&chunk);
        }
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 4);
        assert_eq!(f.levels[1].len(), 1);
        let l1 = f.levels[1][0];
        // L1 min/max should equal the most extreme L0 bucket's min/max.
        let expected_max = f.levels[0].iter().map(|b| b.max).max().unwrap();
        let expected_min = f.levels[0].iter().map(|b| b.min).min().unwrap();
        assert_eq!(l1.max, expected_max);
        assert_eq!(l1.min, expected_min);
        // L1 rms ≈ sqrt(mean((rms_i/65535)^2)) * 65535. Two buckets at
        // ~0.9 (mono=L since |L|>=|R|... wait L=+0.9, R=-0.9, so
        // |L|=|R| and mono=L=+0.9; ms = (0.81+0.81)/2 = 0.81; rms=0.9)
        // and two at 0.1 (rms ~= 0.1). Mean square = (0.81+0.01)*0.5 = 0.41
        // -> rms ≈ 0.640.
        let expected_l1 = quantize_u16(0.640);
        assert!(
            (l1.rms as i32 - expected_l1 as i32).abs() < 800,
            "L1 rms ~= 0.640 quantized (got {}, expected ~{})",
            l1.rms,
            expected_l1
        );
    }

    #[test]
    fn pyramid_decimation_l2_from_l1() {
        let mut b = PeakBuilder::new();
        // 16 L0 buckets → 4 L1 → 1 L2.
        let n = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
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
        assert_eq!(f.levels[2].len(), 1);
        let l2 = f.levels[2][0];
        // Max DC: k=15 → mono = 15/16 = 0.9375
        let expected_max = quantize_i16(15.0 / 16.0);
        assert_eq!(l2.max, expected_max);
        // Min DC: k=0 → mono = 0
        assert_eq!(l2.min, 0);
    }

    #[test]
    fn tail_bucket_with_fewer_frames_is_recorded() {
        let mut b = PeakBuilder::new();
        // 100 frames (less than 256) — should produce one tail L0
        // bucket, one tail L1, one tail L2.
        let mut chunk = Vec::with_capacity(200);
        for _ in 0..100 {
            chunk.push(0.3);
            chunk.push(0.3);
        }
        b.push_stereo_chunk(&chunk);
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 1, "tail L0");
        assert_eq!(f.levels[1].len(), 1, "tail L1 = single L0");
        assert_eq!(f.levels[2].len(), 1, "tail L2 = single L1");
        for lvl in &f.levels {
            assert_eq!(lvl[0].max, quantize_i16(0.3));
        }
    }

    #[test]
    fn nan_and_inf_inputs_are_treated_as_silence() {
        let mut b = PeakBuilder::new();
        let n = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
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
        let samples = full_scale_sine(LEVEL_SAMPLES_PER_BUCKET[0] as usize * 8, 1000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let finished = b.finish();
        assert_eq!(finished.levels[0].len(), 8);
        assert_eq!(finished.levels[1].len(), 2);
        assert_eq!(finished.levels[2].len(), 1); // partial L1 window flushed

        let bytes_written = write_atomic(&path, &sample_header(), &finished).unwrap();
        assert_eq!(
            bytes_written,
            fs::metadata(&path).unwrap().len() as usize,
            "actual file size matches reported"
        );

        let mut pf = PeakFile::open(&path).unwrap();
        assert_eq!(pf.header.sample_rate, 48_000);
        assert_eq!(pf.header.total_samples, finished.total_frames);
        assert_eq!(pf.levels[0].bucket_count, 8);
        assert_eq!(pf.levels[1].bucket_count, 2);
        assert_eq!(pf.levels[2].bucket_count, 1);
        assert_eq!(
            pf.header.content_hash_prefix,
            sample_header().content_hash_prefix
        );

        // Pull all of L0 and check it matches the in-memory buckets.
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
        let samples = full_scale_sine(LEVEL_SAMPLES_PER_BUCKET[0] as usize * 8, 1000.0, 48_000.0);
        b.push_stereo_chunk(&samples);
        let finished = b.finish();
        write_atomic(&path, &sample_header(), &finished).unwrap();

        let mut pf = PeakFile::open(&path).unwrap();
        // Buckets 2..5 (3 buckets) at L0.
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
            LEVEL_SAMPLES_PER_BUCKET[0] as usize,
            1000.0,
            48_000.0,
        ));
        write_atomic(&path, &sample_header(), &b.finish()).unwrap();

        let mut pf = PeakFile::open(&path).unwrap();
        match pf.read_slice(0, 1, 1).unwrap_err() {
            PeakError::RangeOutOfBounds { level: 0, available: 1, start: 1, end: 2 } => {}
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
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE];
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
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE];
        bytes[0..4].copy_from_slice(&PEAK_MAGIC);
        bytes[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        bytes[6] = LANE_COUNT;
        bytes[7] = LEVEL_COUNT;
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
        let mut bytes = vec![0u8; HEADER_SIZE + LEVEL_DIR_TOTAL_SIZE];
        bytes[0..4].copy_from_slice(&PEAK_MAGIC);
        bytes[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[6] = 4; // not 3
        bytes[7] = LEVEL_COUNT;
        fs::write(&path, &bytes).unwrap();
        match PeakFile::open(&path).unwrap_err() {
            PeakError::UnsupportedLayout {
                lane_count: 4,
                level_count: 3,
            } => {}
            e => panic!("unexpected: {e:?}"),
        }
    }

    #[test]
    fn payload_starts_4_byte_aligned() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("aligned.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(
            LEVEL_SAMPLES_PER_BUCKET[0] as usize * 5,
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
        // 7 buckets at L0 → mins at offset, maxes at offset+14, rms at offset+28.
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
        // 48k samples, asking for 1 px per millisecond (1000 px/s):
        //   L0 = 256/48000 * 1000 = 5.33 px/bucket (> 4) → L0 anyway
        //   L1 = 1024/48000 * 1000 = 21.3 px/bucket — too dense
        //   L2 = 4096/48000 * 1000 = 85.3 px/bucket — too dense
        //   None match [1..4] → fall back to L0 (densest source).
        assert_eq!(choose_level(48_000, 1000.0), 0);

        // 100 px/s:
        //   L0 = 256/48000 * 100 = 0.53 — too sparse
        //   L1 = 1024/48000 * 100 = 2.13 → IN RANGE
        //   L2 = 4096/48000 * 100 = 8.53 — too dense
        assert_eq!(choose_level(48_000, 100.0), 1);

        // 30 px/s:
        //   L0 = 256/48000 * 30 = 0.16
        //   L1 = 1024/48000 * 30 = 0.64
        //   L2 = 4096/48000 * 30 = 2.56 → IN RANGE
        assert_eq!(choose_level(48_000, 30.0), 2);

        // Pathologically zoomed-out: 1 px/s → no level in [1..4]; L0 is the
        // densest source so it's not the "too dense at L0" case → coarsest.
        assert_eq!(choose_level(48_000, 1.0), LEVEL_COUNT - 1);

        // Sanity: zero arguments don't panic.
        assert_eq!(choose_level(0, 1.0), LEVEL_COUNT - 1);
        assert_eq!(choose_level(48_000, 0.0), LEVEL_COUNT - 1);
    }

    #[test]
    fn finish_handles_partial_l1_window() {
        // 5 L0 buckets → L1 gets {0..4} = 1, plus tail-drain of {4} = 1
        // → 2 L1 buckets. L2 gets the same 2 → tail drain into 1 L2.
        let mut b = PeakBuilder::new();
        let n = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
        for _ in 0..5 {
            let mut chunk = Vec::with_capacity(n * 2);
            for _ in 0..n {
                chunk.push(0.7);
                chunk.push(0.7);
            }
            b.push_stereo_chunk(&chunk);
        }
        let f = b.finish();
        assert_eq!(f.levels[0].len(), 5);
        assert_eq!(f.levels[1].len(), 2);
        assert_eq!(f.levels[2].len(), 1);
    }

    #[test]
    fn mono_chunk_matches_stereo_duplicate() {
        let n = LEVEL_SAMPLES_PER_BUCKET[0] as usize;
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
    fn write_atomic_does_not_leave_tmp_on_success() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("clean.peaks");
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&full_scale_sine(512, 1000.0, 48_000.0));
        write_atomic(&path, &sample_header(), &b.finish()).unwrap();
        assert!(path.is_file());
        assert!(
            !tmp.path().join("clean.peaks.tmp").exists(),
            ".tmp must be renamed away on success"
        );
    }
}
