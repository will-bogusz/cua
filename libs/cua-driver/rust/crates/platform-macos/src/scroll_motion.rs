//! What a scroll did to the window's pixels, measured from window frames.
//!
//! A posted wheel event proves nothing about the view under it: a window that
//! only scrolls under the real pointer drops pid-routed wheels, a pager pages
//! instead of scrolling, and a list at its end bounces back. The scroll tool
//! samples the target window while it scrolls and hands the frames to this
//! module, which answers one question per call: did the content under the
//! pointer shift rigidly, by how many points, and which way.
//!
//! Registration is generic — no hand-set content band. Rows that never change
//! across a frame pair (fixed chrome: title bars, tab bars, a status line) drop
//! out on their own; rows that change without matching any shift (a progress
//! bar, a clock) stay in the comparison as unexplained residual. When the
//! change is confined to one strip under the pointer (a carousel row or a
//! side list that moves alone), that strip is registered on its own as well,
//! so the static content beside it cannot outvote it. The search runs on
//! per-row block descriptors so a ±300 pt search stays cheap, then the
//! chosen shift is refined on full pixels. Tracking frame to frame keeps a
//! list with a constant row pitch from aliasing: the final first-vs-last
//! shift is searched only within [`NET_WINDOW_PT`] of the tracked travel.
//!
//! Sign convention: a registered shift `s > 0` means row `r` of the later frame
//! shows what row `r + s` of the earlier one did — the content moved up by `s`
//! points, which is what a `down` scroll does. Horizontal registration runs on
//! transposed frames, so `s > 0` there means the content moved left (`right`).
//!
//! Everything here is pure: frames in, verdict out. Capture and pacing live in
//! `frame_sampler` and `tools/scroll.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use cua_driver_contract::ScrollDirection;

/// Column blocks per row descriptor.
const BLOCKS: usize = 16;
/// Mean absolute gray difference above which a row counts as changed.
const ROW_CHANGE: f32 = 0.5;
/// Fewest changed rows a registration will reason about.
const MIN_BAND_ROWS: usize = 12;
/// Rows that must overlap for a candidate shift to be scored.
const MIN_OVERLAP_ROWS: i32 = 40;
/// Frame-to-frame search (the stream can arrive in bursts of 100+ pt).
pub const TRACK_SEARCH_PT: i32 = 300;
/// Horizontal frame-to-frame search.
pub const TRACK_SEARCH_H_PT: i32 = 150;
/// The first-vs-last shift is searched this close to the tracked travel.
pub const NET_WINDOW_PT: i32 = 40;
/// A frame pair whose normalized difference stays below this is noise.
const PAIR_FLOOR: f32 = 0.05;
/// Frame-to-frame acceptance: residual below this share of the difference.
const PAIR_RIGID_RATIO: f32 = 0.5;
/// Call verdict: a net shift with residual below this share is a move.
const NET_RIGID_RATIO: f32 = 0.3;
/// First-vs-last difference below this, with no frame-to-frame shift, is no
/// motion. Measured: every no-op scroll stayed ≤ 0.6 (the iPhone Mirroring
/// pointer disc appearing), every rigid move registered well above it.
const NO_MOTION_DIFF: f32 = 1.0;
/// A step smaller than this does not count toward a direction reversal.
const REVERSAL_MIN_PT: i32 = 3;
/// Descriptor minima re-scored on full pixels per registration.
const REFINE_CANDIDATES: usize = 8;
/// A competing shift this far from the best one is an alias, not a refinement.
const ALIAS_MIN_PT: u32 = 3;
/// A competing shift whose residual is within this factor of the best one's
/// (plus 5% of the difference) makes a registration ambiguous.
const ALIAS_NOISE: f32 = 1.3;
/// Below this share of the band left in common, a first-vs-last comparison
/// is not trusted and clean tracked travel stands instead.
const NET_RELIABLE_OVERLAP: f64 = 0.35;
/// Each frame-to-frame step of a clean tracked series explains at least this
/// share of its pair's difference.
const CLEAN_STEP_CONFIDENCE: f64 = 0.6;
/// A net registration further than this from a clean tracked travel yields
/// to the travel.
const TRACKED_AGREEMENT_PT: i32 = 5;
/// The residual at the tracked travel counts as competitive with the net's
/// best within this factor.
const TRAVEL_RESIDUAL_FACTOR: f32 = 1.3;
/// A per-pixel difference at or below this is stream noise when finding the
/// strip under the pointer.
const PIXEL_NOISE: u8 = 8;
/// Unchanged columns the strip under the pointer bridges.
const STRIP_GAP: usize = 24;
/// Least share of a frame pair's change the strip under the pointer holds.
const STRIP_SHARE: f64 = 0.9;
/// The strip's registration stands when its residual is below this share
/// of the whole region's.
const STRIP_ADVANTAGE: f32 = 0.5;

/// One captured window frame: 8-bit gray, row-major, one pixel per point.
#[derive(Clone, PartialEq, Eq)]
pub struct GrayFrame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for GrayFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GrayFrame({}x{})", self.width, self.height)
    }
}

impl GrayFrame {
    pub fn new(width: usize, height: usize, pixels: Vec<u8>) -> Self {
        assert_eq!(pixels.len(), width * height, "gray frame size mismatch");
        Self {
            width,
            height,
            pixels,
        }
    }

    fn at(&self, x: usize, y: usize) -> u8 {
        self.pixels[y * self.width + x]
    }

    fn transposed(&self) -> GrayFrame {
        let mut pixels = vec![0u8; self.pixels.len()];
        for y in 0..self.height {
            for x in 0..self.width {
                pixels[x * self.height + y] = self.pixels[y * self.width + x];
            }
        }
        GrayFrame::new(self.height, self.width, pixels)
    }
}

/// The part of the frame registration reads: the scroll area when
/// accessibility exposes one, else the window inside small margins, and the
/// scroll's pointer in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x0: usize,
    pub x1: usize,
    pub y0: usize,
    pub y1: usize,
    /// Where the wheel events land, `(x, y)` in frame points: registration
    /// reads the strip of change under it (see [`strip_under_pointer`]).
    pub pointer: (usize, usize),
}

impl Region {
    /// `rect` (frame points, `[x, y, w, h]`) clamped into a `width`×`height`
    /// frame. Falls back to the whole frame minus 8 pt side margins when
    /// `rect` is absent or leaves too little to register. `pointer` (frame
    /// points) is clamped into the frame.
    pub fn within(
        width: usize,
        height: usize,
        rect: Option<[f64; 4]>,
        pointer: (f64, f64),
    ) -> Region {
        let clamp = |v: f64, max: usize| v.round().clamp(0.0, max as f64) as usize;
        let pointer = (
            clamp(pointer.0, width.saturating_sub(1)),
            clamp(pointer.1, height.saturating_sub(1)),
        );
        let fallback = Region {
            x0: 8.min(width),
            x1: width.saturating_sub(8).max(8.min(width)),
            y0: 0,
            y1: height,
            pointer,
        };
        let Some([x, y, w, h]) = rect else {
            return fallback;
        };
        let region = Region {
            x0: clamp(x, width),
            x1: clamp(x + w, width),
            y0: clamp(y, height),
            y1: clamp(y + h, height),
            pointer,
        };
        if region.x1 < region.x0 + BLOCKS || region.y1 < region.y0 + MIN_OVERLAP_ROWS as usize {
            fallback
        } else {
            region
        }
    }

    fn rows(&self) -> usize {
        self.y1.saturating_sub(self.y0)
    }

    fn transposed(&self) -> Region {
        Region {
            x0: self.y0,
            x1: self.y1,
            y0: self.x0,
            y1: self.x1,
            pointer: (self.pointer.1, self.pointer.0),
        }
    }
}

/// The best rigid shift between two frames, with how well it explains them.
///
/// Both numbers are mean absolute gray levels normalized over every row of
/// the region, so `diff` is comparable across windows of any height and
/// `residual / diff` is the share of the change the shift leaves unexplained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Registration {
    pub shift: i32,
    pub residual: f32,
    pub diff: f32,
    /// Another shift at least [`ALIAS_MIN_PT`] away explains the frames
    /// within noise of this one (repeated rows: a list, numbered text), so
    /// the distance may be off by that pitch.
    pub ambiguous: bool,
    /// The shift was registered on the strip under the pointer, not the
    /// whole region ([`strip_under_pointer`]). Pixels cannot tell a strip
    /// the wheel scrolled from one that animates by itself (a banner, a
    /// shimmer), so such a shift never calibrates and is never credited
    /// when the view was already changing before the first event.
    pub local: bool,
}

impl Registration {
    fn still(diff: f32) -> Self {
        Self {
            shift: 0,
            residual: diff,
            diff,
            ambiguous: false,
            local: false,
        }
    }

    fn is_rigid(&self, ratio: f32) -> bool {
        self.shift != 0 && self.diff > 0.0 && self.residual < ratio * self.diff
    }

    fn confidence(&self) -> f64 {
        if self.diff <= 0.0 {
            return 0.0;
        }
        (1.0 - f64::from(self.residual / self.diff)).clamp(0.0, 1.0)
    }
}

fn row_diff(a: &GrayFrame, ra: usize, b: &GrayFrame, rb: usize, region: &Region) -> f32 {
    let mut sum = 0u32;
    for x in region.x0..region.x1 {
        sum += u32::from(a.at(x, ra).abs_diff(b.at(x, rb)));
    }
    sum as f32 / (region.x1 - region.x0).max(1) as f32
}

fn descriptors(frame: &GrayFrame, region: &Region) -> Vec<[f32; BLOCKS]> {
    let width = region.x1 - region.x0;
    (region.y0..region.y1)
        .map(|y| {
            let mut row = [0f32; BLOCKS];
            for (k, block) in row.iter_mut().enumerate() {
                let start = region.x0 + k * width / BLOCKS;
                let end = (region.x0 + (k + 1) * width / BLOCKS).max(start + 1);
                let sum: u32 = (start..end).map(|x| u32::from(frame.at(x, y))).sum();
                *block = sum as f32 / (end - start) as f32;
            }
            row
        })
        .collect()
}

fn descriptor_distance(a: &[f32; BLOCKS], b: &[f32; BLOCKS]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / BLOCKS as f32
}

/// Each row's difference in place, and the region's change: the changed
/// rows' differences normalized over every row.
fn row_changes(earlier: &GrayFrame, later: &GrayFrame, region: &Region) -> (Vec<f32>, f32) {
    let still_diffs: Vec<f32> = (region.y0..region.y1)
        .map(|y| row_diff(earlier, y, later, y, region))
        .collect();
    let diff =
        still_diffs.iter().filter(|d| **d > ROW_CHANGE).sum::<f32>() / region.rows().max(1) as f32;
    (still_diffs, diff)
}

/// The strip of `region` a frame pair changed in under the pointer, when
/// the rest of the region held still.
///
/// A scroll can move one strip of the region only (a carousel row above a
/// static list, a side list beside static content). Compared together with
/// the static content across from it, no shift explains the strip: the
/// shifted static part leaves as large a residual as staying put. The strip
/// grows from the pointer's column through changed columns, bridging
/// unchanged gaps up to [`STRIP_GAP`] (the blank between a carousel's
/// pictures and their captions). A column counts as changed when its mean
/// difference over pixels that differ by more than [`PIXEL_NOISE`] (a video
/// stream's frame-to-frame noise) exceeds [`ROW_CHANGE`]. There is no strip
/// when it is too narrow to register, spans the whole region, or holds less
/// than [`STRIP_SHARE`] of the region's change: a strip that moved while
/// the rest was replaced is a navigation, and a small change under the
/// pointer while the whole view moved is not the scroll.
fn strip_under_pointer(earlier: &GrayFrame, later: &GrayFrame, region: &Region) -> Option<Region> {
    let width = region.x1 - region.x0;
    let pointer = region
        .pointer
        .0
        .checked_sub(region.x0)
        .filter(|p| *p < width)?;
    let mut change = vec![0u32; width];
    for y in region.y0..region.y1 {
        let start = y * earlier.width + region.x0;
        let (a, b) = (
            &earlier.pixels[start..start + width],
            &later.pixels[start..start + width],
        );
        for (sum, (p, q)) in change.iter_mut().zip(a.iter().zip(b)) {
            let d = p.abs_diff(*q);
            if d > PIXEL_NOISE {
                *sum += u32::from(d);
            }
        }
    }
    let floor = ROW_CHANGE * region.rows() as f32;
    let changed = |x: usize| change[x] as f32 > floor;
    let (mut start, mut end) = (pointer, pointer + 1);
    let mut gap = 0;
    for x in (0..pointer).rev() {
        if changed(x) {
            (start, gap) = (x, 0);
        } else if gap == STRIP_GAP {
            break;
        } else {
            gap += 1;
        }
    }
    gap = 0;
    for x in pointer + 1..width {
        if changed(x) {
            (end, gap) = (x + 1, 0);
        } else if gap == STRIP_GAP {
            break;
        } else {
            gap += 1;
        }
    }
    let total: u64 = change.iter().map(|c| u64::from(*c)).sum();
    let inside: u64 = change[start..end].iter().map(|c| u64::from(*c)).sum();
    (end - start >= BLOCKS && end - start < width && inside as f64 >= STRIP_SHARE * total as f64)
        .then_some(Region {
            x0: region.x0 + start,
            x1: region.x0 + end,
            ..*region
        })
}

/// Register `later` against `earlier` over `region`, searching shifts in
/// `lo..=hi`. A frame pair with too few changed rows, or no candidate with
/// enough overlap, registers as still (shift 0).
///
/// The whole region is registered first. When the change is confined to
/// the strip under the pointer ([`strip_under_pointer`]), the strip is
/// registered on its own too, and stands only where it explains the change
/// clearly better ([`STRIP_ADVANTAGE`]): a whole-region registration about
/// as good keeps the answer, and with it the columns that tell repeated rows
/// apart. `diff` is always the whole
/// region's change, so the no-motion and pair floors read the same either
/// way; a strip's residual is its unexplained share of that change.
///
/// Block descriptors cannot tell apart two lines of text that differ only in
/// a few glyphs (a numbered list aliases by one line pitch), so every shift
/// whose descriptor score is near the best is re-scored on full pixels; an
/// exact tie goes to the shift closest to `prefer`.
pub fn register(
    earlier: &GrayFrame,
    later: &GrayFrame,
    region: &Region,
    lo: i32,
    hi: i32,
    prefer: i32,
) -> Registration {
    if earlier.width != later.width
        || earlier.height != later.height
        || region.x1 > earlier.width
        || region.y1 > earlier.height
        || region.x1 < region.x0 + BLOCKS
    {
        return Registration::still(0.0);
    }
    if region.rows() == 0 {
        return Registration::still(0.0);
    }
    let (still_diffs, diff) = row_changes(earlier, later, region);
    if still_diffs.iter().filter(|d| **d > ROW_CHANGE).count() < MIN_BAND_ROWS {
        return Registration::still(diff);
    }
    let whole = register_rows(earlier, later, region, &still_diffs, diff, lo, hi, prefer);
    let Some(strip) = strip_under_pointer(earlier, later, region) else {
        return whole;
    };
    let (strip_diffs, _) = row_changes(earlier, later, &strip);
    let local = register_rows(earlier, later, &strip, &strip_diffs, diff, lo, hi, prefer);
    if local.shift != 0 && local.residual < STRIP_ADVANTAGE * whole.residual {
        Registration {
            local: true,
            ..local
        }
    } else {
        whole
    }
}

/// [`register`] on `region` alone, given its rows' differences in place and
/// the whole region's change `diff` to scale the residual by.
#[allow(clippy::too_many_arguments)]
fn register_rows(
    earlier: &GrayFrame,
    later: &GrayFrame,
    region: &Region,
    still_diffs: &[f32],
    diff: f32,
    lo: i32,
    hi: i32,
    prefer: i32,
) -> Registration {
    let rows = region.rows();
    let changed: Vec<bool> = still_diffs.iter().map(|d| *d > ROW_CHANGE).collect();
    let changed_rows = changed.iter().filter(|c| **c).count();
    if changed_rows < MIN_BAND_ROWS {
        return Registration::still(diff);
    }
    let limit = rows as i32 - MIN_OVERLAP_ROWS;
    let (lo, hi) = (lo.max(-limit), hi.min(limit));
    let min_pairs = MIN_BAND_ROWS.max(changed_rows / 4);
    let changed = &changed;
    let pairs = |shift: i32| {
        (0..rows).filter(move |&r| {
            let source = r as i32 + shift;
            changed[r] && source >= 0 && (source as usize) < rows && changed[source as usize]
        })
    };

    let earlier_desc = descriptors(earlier, region);
    let later_desc = descriptors(later, region);
    let mut scored: Vec<(i32, f32)> = Vec::new();
    for shift in lo..=hi {
        if shift == 0 {
            continue;
        }
        let mut sum = 0f32;
        let mut count = 0usize;
        for r in pairs(shift) {
            sum += descriptor_distance(&later_desc[r], &earlier_desc[(r as i32 + shift) as usize]);
            count += 1;
        }
        if count >= min_pairs {
            scored.push((shift, sum / count as f32));
        }
    }
    let Some(best_score) = scored.iter().map(|(_, score)| *score).reduce(f32::min) else {
        return Registration::still(diff);
    };
    // Candidates are the descriptor score's local minima near the best: a
    // smooth minimum's own shoulders would otherwise crowd out an alias one
    // line pitch away.
    let near = best_score * 1.5 + 0.1;
    let mut candidates: Vec<(i32, f32)> = scored
        .iter()
        .enumerate()
        .filter(|(i, (_, score))| {
            *score <= near
                && (*i == 0 || scored[i - 1].1 >= *score)
                && scored.get(i + 1).is_none_or(|next| next.1 >= *score)
        })
        .map(|(_, candidate)| *candidate)
        .collect();
    candidates.sort_by(|a, b| {
        a.1.total_cmp(&b.1)
            .then(a.0.abs_diff(prefer).cmp(&b.0.abs_diff(prefer)))
    });
    candidates.truncate(REFINE_CANDIDATES);

    // A scroll moves the content and leaves fixed chrome (a status bar, a
    // tab bar, a margin) where it was. Under a video or compressed stream
    // those fixed rows still differ by noise from frame to frame, so they
    // count as changed — and compared against a shifted row they leave a
    // large residual that is not the scroll's failure. Each row is explained
    // by whichever fits better: the shift, or staying put. The residual is
    // then scaled by the change in the rows it scored, not the whole frame's:
    // otherwise a shift that pushes an in-place change out of the overlap
    // scores only noise rows and looks rigid.
    let pixel_residual = |shift: i32| -> Option<f32> {
        let mut sum = 0f32;
        let mut still_sum = 0f32;
        let mut count = 0usize;
        for r in pairs(shift) {
            let source = (r as i32 + shift) as usize;
            let shifted = row_diff(earlier, region.y0 + source, later, region.y0 + r, region);
            sum += shifted.min(still_diffs[r]);
            still_sum += still_diffs[r];
            count += 1;
        }
        (count >= min_pairs && still_sum > 0.0).then(|| sum / still_sum * diff)
    };
    let mut refined: Option<(i32, f32)> = None;
    let mut tried: Vec<(i32, f32)> = Vec::new();
    for (coarse, _) in candidates {
        for shift in [coarse - 1, coarse, coarse + 1] {
            if shift == 0 || shift < lo || shift > hi || tried.iter().any(|(s, _)| *s == shift) {
                continue;
            }
            let Some(residual) = pixel_residual(shift) else {
                continue;
            };
            tried.push((shift, residual));
            let better = match refined {
                None => true,
                Some((best_shift, best)) => {
                    let tolerance = best * 1e-3 + 1e-6;
                    residual < best - tolerance
                        || (residual <= best + tolerance
                            && shift.abs_diff(prefer) < best_shift.abs_diff(prefer))
                }
            };
            if better {
                refined = Some((shift, residual));
            }
        }
    }
    match refined {
        Some((shift, residual)) => Registration {
            shift,
            residual,
            diff,
            ambiguous: tried.iter().any(|(other, r)| {
                other.abs_diff(shift) >= ALIAS_MIN_PT && *r <= residual * ALIAS_NOISE + 0.05 * diff
            }),
            local: false,
        },
        None => Registration::still(diff),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

/// One frame-to-frame rigid shift the tracker accepted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub axis: Axis,
    pub shift: i32,
    pub confidence: f64,
    /// A repeated-row alias competed with this step's shift.
    pub ambiguous: bool,
    /// Registered on the strip under the pointer ([`Registration::local`]).
    pub local: bool,
}

/// Frame-to-frame tracking state for one scroll call, fed by the sampler.
pub struct MotionTracker {
    region: Region,
    first: GrayFrame,
    prev: GrayFrame,
    travel_v: i32,
    travel_h: i32,
    series: Vec<Step>,
    chunk: ChunkStart,
    /// [`Self::begin_gesture`] ran: frames after it belong to the scroll.
    gesture_started: bool,
    /// Frames already changed before the first input event (an animation,
    /// a video, a scroll still settling): pixels alone then cannot attribute
    /// a shift to the scroll.
    baseline_moving: bool,
    /// Changed frame pairs that registered no rigid shift on either axis.
    non_rigid: usize,
}

struct ChunkStart {
    frame: GrayFrame,
    series_len: usize,
    travel_v: i32,
    travel_h: i32,
    non_rigid: usize,
}

/// What happened between two tracked frames: the tracked travel and steps,
/// and the net shift of the end frame against the start frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Motion {
    pub travel_v: i32,
    pub travel_h: i32,
    pub series: Vec<Step>,
    pub net_v: Registration,
    pub net_h: Registration,
    /// The first-vs-last registration scored at exactly the tracked travel on
    /// the vertical / horizontal axis (shift 0 when that shift cannot be
    /// scored: too little overlap).
    pub at_travel_v: Registration,
    pub at_travel_h: Registration,
    /// The window's frame changed size between the two frames, so no shift
    /// between them can be registered.
    pub reshaped: bool,
    /// Changed frame pairs in the span that registered no rigid shift (a
    /// navigation, a sheet, an animation frame).
    pub non_rigid_pairs: usize,
    /// The content was already changing before the first input event.
    pub baseline_moving: bool,
    /// Largest vertical / horizontal shift a single comparison can register
    /// (the region minus the overlap it needs).
    pub overlap_v: i32,
    pub overlap_h: i32,
}

impl MotionTracker {
    pub fn new(first: GrayFrame, region: Region) -> Self {
        Self {
            region,
            prev: first.clone(),
            chunk: ChunkStart {
                frame: first.clone(),
                series_len: 0,
                travel_v: 0,
                travel_h: 0,
                non_rigid: 0,
            },
            first,
            travel_v: 0,
            travel_h: 0,
            series: Vec::new(),
            gesture_started: false,
            baseline_moving: false,
            non_rigid: 0,
        }
    }

    /// The gesture starts now: the latest frame becomes the call's first
    /// frame. `baseline_moving`: the content was still changing when the
    /// gesture started (the caller watched it before any input).
    pub fn begin_gesture(&mut self, baseline_moving: bool) {
        self.gesture_started = true;
        self.baseline_moving = baseline_moving;
        self.first = self.prev.clone();
        self.travel_v = 0;
        self.travel_h = 0;
        self.series.clear();
        self.non_rigid = 0;
        self.begin_chunk();
    }

    /// Feed the next captured frame. Returns whether it differed from the
    /// previous one (identical frames are skipped without registration).
    pub fn push(&mut self, frame: GrayFrame) -> bool {
        if frame == self.prev {
            return false;
        }
        if !self.gesture_started {
            self.prev = frame;
            return true;
        }
        let vertical = register(
            &self.prev,
            &frame,
            &self.region,
            -TRACK_SEARCH_PT,
            TRACK_SEARCH_PT,
            0,
        );
        if vertical.diff > PAIR_FLOOR && vertical.is_rigid(PAIR_RIGID_RATIO) {
            self.travel_v += vertical.shift;
            self.series.push(Step {
                axis: Axis::Vertical,
                shift: vertical.shift,
                confidence: vertical.confidence(),
                ambiguous: vertical.ambiguous,
                local: vertical.local,
            });
        } else if vertical.diff > PAIR_FLOOR {
            let horizontal = register(
                &self.prev.transposed(),
                &frame.transposed(),
                &self.region.transposed(),
                -TRACK_SEARCH_H_PT,
                TRACK_SEARCH_H_PT,
                0,
            );
            if horizontal.is_rigid(PAIR_RIGID_RATIO) {
                self.travel_h += horizontal.shift;
                self.series.push(Step {
                    axis: Axis::Horizontal,
                    shift: horizontal.shift,
                    confidence: horizontal.confidence(),
                    ambiguous: horizontal.ambiguous,
                    local: horizontal.local,
                });
            } else if vertical.diff >= NO_MOTION_DIFF {
                // A change above the no-motion floor that no shift explains:
                // a navigation, a sheet, an animation frame. Fainter pairs (a
                // scroller fading in) are below what the classifier counts
                // as change at all.
                self.non_rigid += 1;
            }
        }
        self.prev = frame;
        true
    }

    /// Start a new chunk at the current frame.
    pub fn begin_chunk(&mut self) {
        self.chunk = ChunkStart {
            frame: self.prev.clone(),
            series_len: self.series.len(),
            travel_v: self.travel_v,
            travel_h: self.travel_h,
            non_rigid: self.non_rigid,
        };
    }

    /// Motion since [`Self::begin_chunk`].
    pub fn chunk_motion(&self) -> Motion {
        self.motion_between(
            &self.chunk.frame,
            self.travel_v - self.chunk.travel_v,
            self.travel_h - self.chunk.travel_h,
            &self.series[self.chunk.series_len..],
            self.non_rigid - self.chunk.non_rigid,
        )
    }

    /// Motion over the whole call.
    pub fn call_motion(&self) -> Motion {
        self.motion_between(&self.first, self.travel_v, self.travel_h, &self.series, self.non_rigid)
    }

    /// Points tracked frame to frame since the gesture began, along the axis
    /// and signed for `direction` (positive is the way asked).
    pub fn tracked_along(&self, direction: ScrollDirection) -> i32 {
        match axis_of(direction) {
            (Axis::Vertical, sign) => sign * self.travel_v,
            (Axis::Horizontal, sign) => sign * self.travel_h,
        }
    }

    fn motion_between(
        &self,
        start: &GrayFrame,
        travel_v: i32,
        travel_h: i32,
        series: &[Step],
        non_rigid_pairs: usize,
    ) -> Motion {
        let net_v = register(
            start,
            &self.prev,
            &self.region,
            travel_v - NET_WINDOW_PT,
            travel_v + NET_WINDOW_PT,
            travel_v,
        );
        let net_h = register(
            &start.transposed(),
            &self.prev.transposed(),
            &self.region.transposed(),
            travel_h - NET_WINDOW_PT,
            travel_h + NET_WINDOW_PT,
            travel_h,
        );
        let at_travel_v = register(start, &self.prev, &self.region, travel_v, travel_v, travel_v);
        let at_travel_h = register(
            &start.transposed(),
            &self.prev.transposed(),
            &self.region.transposed(),
            travel_h,
            travel_h,
            travel_h,
        );
        Motion {
            travel_v,
            travel_h,
            series: series.to_vec(),
            net_v,
            net_h,
            at_travel_v,
            at_travel_h,
            reshaped: start.width != self.prev.width || start.height != self.prev.height,
            non_rigid_pairs,
            baseline_moving: self.baseline_moving,
            overlap_v: self.region.rows() as i32 - MIN_OVERLAP_ROWS,
            overlap_h: (self.region.x1 - self.region.x0) as i32 - MIN_OVERLAP_ROWS,
        }
    }
}

/// The measured verdict of a scroll, before any requested-distance rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    Moved,
    AtEnd,
    NoMotion,
    ChangedInPlace,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Classified {
    pub kind: OutcomeKind,
    /// Displacement along the requested direction, window points; positive
    /// is the way the caller asked.
    pub along: i32,
    /// Perpendicular displacement, window points; positive is content moving
    /// up (for a horizontal request) or left (for a vertical one).
    pub across: i32,
    /// `1 - residual/diff` of the shift the verdict rests on; `None` when no
    /// shift was registered.
    pub confidence: Option<f64>,
    /// The content moved one way and then the other (overshoot and bounce).
    pub bounced: bool,
    /// The distance rests on a registration a repeated-row alias competes
    /// with, so it may be off by one pitch: never correct or calibrate by it.
    pub ambiguous: bool,
    /// The distance stands (tracked-only, or content already moving) but must
    /// not size later chunks.
    pub uncalibrated: bool,
}

fn axis_of(direction: ScrollDirection) -> (Axis, i32) {
    match direction {
        ScrollDirection::Down => (Axis::Vertical, 1),
        ScrollDirection::Up => (Axis::Vertical, -1),
        ScrollDirection::Right => (Axis::Horizontal, 1),
        ScrollDirection::Left => (Axis::Horizontal, -1),
    }
}

/// Classify `motion` for a scroll in `direction`.
pub fn classify(motion: &Motion, direction: ScrollDirection) -> Classified {
    let (axis, sign) = axis_of(direction);
    let (net, travel, net_across, travel_across, overlap, at_travel) = match axis {
        Axis::Vertical => (
            motion.net_v,
            motion.travel_v,
            motion.net_h,
            motion.travel_h,
            motion.overlap_v,
            motion.at_travel_v,
        ),
        Axis::Horizontal => (
            motion.net_h,
            motion.travel_h,
            motion.net_v,
            motion.travel_v,
            motion.overlap_h,
            motion.at_travel_h,
        ),
    };
    let steps: Vec<&Step> = motion.series.iter().filter(|s| s.axis == axis).collect();
    let bounced = steps.iter().any(|s| s.shift >= REVERSAL_MIN_PT)
        && steps.iter().any(|s| s.shift <= -REVERSAL_MIN_PT);
    let step_confidence = (!steps.is_empty())
        .then(|| steps.iter().map(|s| s.confidence).sum::<f64>() / steps.len() as f64);
    let across = if net_across.is_rigid(NET_RIGID_RATIO) {
        net_across.shift
    } else {
        travel_across
    };
    // The first-vs-last difference is read in the vertical orientation; it is
    // the same pixels whichever axis the request named.
    let diff = motion.net_v.diff;
    let verdict = |kind, along: i32, confidence, ambiguous, uncalibrated| Classified {
        kind,
        along: sign * along,
        across,
        confidence,
        bounced,
        ambiguous,
        uncalibrated,
    };

    if motion.reshaped {
        return verdict(OutcomeKind::ChangedInPlace, 0, None, false, true);
    }
    // Steps registered on a strip under the pointer do not lift a change
    // below the no-motion floor: a faint shimmer or a pointer disc moving
    // there is not a scroll.
    if diff < NO_MOTION_DIFF && motion.series.iter().all(|s| s.local) {
        return verdict(OutcomeKind::NoMotion, 0, None, false, true);
    }
    // A clean tracked series: every changed pair a confident, unambiguous
    // rigid shift the same way on the requested axis only, and nothing
    // moving before the first event.
    let clean_steps = !steps.is_empty()
        && !motion.baseline_moving
        && motion.non_rigid_pairs == 0
        && motion.series.iter().all(|s| s.axis == axis)
        && steps.iter().all(|s| {
            s.shift.signum() == travel.signum()
                && s.confidence >= CLEAN_STEP_CONFIDENCE
                && !s.ambiguous
        });
    // Content that was already moving must agree with the tracked travel
    // before its shift is credited to the scroll, and a strip under the
    // pointer that was already moving (a banner, a marquee) never is.
    let attributable =
        !motion.baseline_moving || (!net.local && (net.shift - travel).abs() <= 8);
    if net.is_rigid(NET_RIGID_RATIO) && attributable {
        let kind = if bounced {
            OutcomeKind::AtEnd
        } else {
            OutcomeKind::Moved
        };
        // Over repeated rows the first-vs-last search can settle on an alias
        // a few line pitches from the travel every frame pair registered.
        // The tracked travel wins only where the net cannot be trusted over
        // it: the shift at the travel is beyond what first and last frames
        // can score, or scores about as well as the alias the net picked.
        let travel_unscorable = at_travel.shift != travel;
        let travel_competitive =
            at_travel.residual <= net.residual * TRAVEL_RESIDUAL_FACTOR + 0.05 * net.diff;
        if clean_steps
            && !bounced
            && (net.shift - travel).abs() > TRACKED_AGREEMENT_PT
            && (travel_unscorable || travel_competitive)
        {
            return verdict(kind, travel, step_confidence, false, true);
        }
        return verdict(
            kind,
            net.shift,
            Some(net.confidence()),
            net.ambiguous,
            motion.baseline_moving || net.local,
        );
    }
    let unambiguous_steps = steps.iter().all(|s| !s.ambiguous);
    if bounced && unambiguous_steps && !motion.baseline_moving {
        return verdict(OutcomeKind::AtEnd, travel, step_confidence, false, true);
    }
    // Two independent registrations agreeing: the tracked frame-to-frame
    // travel (one direction, on the requested axis only) and a first-vs-last
    // shift at that travel that explains most of the change. A noisy stream
    // (video, compression, bursty frame delivery) keeps the residual above
    // the strict rigid ratio. A frame pair that did not register (bursty
    // delivery, or a partial navigation) demands the strict ratio instead,
    // and such a verdict never calibrates.
    let monotonic = !steps.is_empty()
        && motion.series.iter().all(|s| s.axis == axis)
        && steps.iter().all(|s| s.shift.signum() == travel.signum())
        && unambiguous_steps;
    let agreement_ratio = if motion.non_rigid_pairs > 0 {
        NET_RIGID_RATIO
    } else {
        PAIR_RIGID_RATIO
    };
    if travel != 0
        && monotonic
        && !motion.baseline_moving
        && (net.shift - travel).abs() <= TRACKED_AGREEMENT_PT
        && net.is_rigid(agreement_ratio)
    {
        return verdict(
            OutcomeKind::Moved,
            net.shift,
            Some(net.confidence()),
            false,
            motion.non_rigid_pairs > 0 || net.local,
        );
    }
    // Tracked but not registered first-vs-last: accept a clean tracked
    // travel when the first and last frames share too little of the band for
    // one comparison to be reliable, or so little that the shift at the
    // travel cannot be scored at all.
    let band = overlap + MIN_OVERLAP_ROWS;
    let little_overlap = f64::from(band - travel.abs()) < NET_RELIABLE_OVERLAP * f64::from(band)
        || at_travel.shift != travel;
    if travel != 0 && clean_steps && little_overlap {
        return verdict(OutcomeKind::Moved, travel, step_confidence, false, true);
    }
    if diff >= NO_MOTION_DIFF || travel != 0 {
        return verdict(OutcomeKind::ChangedInPlace, 0, None, false, true);
    }
    verdict(OutcomeKind::NoMotion, 0, None, false, true)
}

/// Scroll units a caller can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollUnit {
    Line,
    Page,
    Points,
}

impl ScrollUnit {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "line" => Some(Self::Line),
            "page" => Some(Self::Page),
            "points" => Some(Self::Points),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Page => "page",
            Self::Points => "points",
        }
    }

    /// The amount a call that names none asks for: three line notches, or
    /// one page. `points` has no default; the tool refuses it without one.
    pub fn default_amount(self) -> u64 {
        match self {
            Self::Line => 3,
            Self::Page | Self::Points => 1,
        }
    }

    /// Clamp a requested amount into the unit's accepted range.
    pub fn clamp_amount(self, requested: u64) -> u32 {
        let max = match self {
            Self::Line | Self::Page => NOTCH_AMOUNT_MAX,
            Self::Points => POINTS_AMOUNT_MAX,
        };
        requested.clamp(1, max) as u32
    }
}

/// Largest `amount` for `line` and `page`.
pub const NOTCH_AMOUNT_MAX: u64 = 50;
/// Largest `amount` for `points`.
pub const POINTS_AMOUNT_MAX: u64 = 5000;
/// Points one `line` asks for.
pub const LINE_PT: f64 = 40.0;
/// Share of the visible extent along the scroll (height, or width sideways)
/// one `page` asks for.
pub const PAGE_FRACTION: f64 = 0.8;
/// Largest distance one measured chunk asks for.
pub const CHUNK_MAX_PT: f64 = 400.0;
/// Largest delta of one pixel wheel event (one event saturates near 99 pt).
pub const WHEEL_EVENT_MAX_PX: u32 = 30;
/// Spacing of the wheel events inside one measured chunk.
pub const WHEEL_EVENT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
/// Chunks one call may run before it stops regardless.
const MAX_CHUNKS: u32 = 24;
/// Accepted range of the points-per-pixel calibration.
const K_RANGE: (f64, f64) = (0.05, 20.0);

/// The distance a foreground scroll asks for, in window points.
pub fn requested_distance(unit: ScrollUnit, amount: u32, visible_extent: f64) -> f64 {
    let amount = f64::from(amount);
    match unit {
        ScrollUnit::Points => amount,
        ScrollUnit::Line => amount * LINE_PT,
        ScrollUnit::Page => amount * PAGE_FRACTION * visible_extent.max(1.0),
    }
}

/// Background line-wheel ticks and the lines each carries.
pub fn background_ticks(unit: ScrollUnit, amount: u32) -> (u32, u32) {
    match unit {
        ScrollUnit::Line => (amount, 1),
        ScrollUnit::Page => (amount, 5),
        ScrollUnit::Points => ((f64::from(amount) / LINE_PT).ceil() as u32, 1),
    }
}

/// Split `px` into pixel wheel deltas of at most [`WHEEL_EVENT_MAX_PX`],
/// the remainder in the last event, signed for `direction`: `(wheel1,
/// wheel2)` with down → wheel1 negative, up → positive, right → wheel2
/// negative, left → positive.
pub fn wheel_events(px: u32, direction: ScrollDirection) -> Vec<(i32, i32)> {
    wheel_events_of(px, WHEEL_EVENT_MAX_PX, direction)
}

fn wheel_events_of(px: u32, max_px: u32, direction: ScrollDirection) -> Vec<(i32, i32)> {
    let full = px / max_px;
    let rest = px % max_px;
    let sizes = std::iter::repeat_n(max_px, full as usize)
        .chain((rest > 0).then_some(rest))
        .map(|size| size as i32);
    sizes
        .map(|size| match direction {
            ScrollDirection::Down => (-size, 0),
            ScrollDirection::Up => (size, 0),
            ScrollDirection::Right => (0, -size),
            ScrollDirection::Left => (0, size),
        })
        .collect()
}

/// Delta of one stroke wheel event.
pub const STROKE_EVENT_PX: u32 = 48;
/// Spacing of stroke wheel events. Measured on iPhone Mirroring (macOS 26.1,
/// 2026-10-02): at 40 ms the mirrored view follows the stream about 180 pt
/// behind and at most 119 pt between sampled frames; at 33 ms and faster the
/// lag grows through the stream and the view lands in one late jump of up to
/// 495 pt; a pause of 60 ms or more ends the phone's gesture, and the next
/// event loses about 40 pt to touch slop.
pub const STROKE_EVENT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);
/// Share of the stroke's expected travel the view must have followed before
/// a settle back short of it is a snap rather than the end's spring.
const SNAP_FOLLOW: f64 = 0.8;

/// The wheel events of one stroke: `requested_pt` at `k` points per pixel,
/// in deltas of at most [`STROKE_EVENT_PX`].
pub fn stroke_events(requested_pt: f64, k: f64, direction: ScrollDirection) -> Vec<(i32, i32)> {
    let k = k.clamp(K_RANGE.0, K_RANGE.1);
    let px = (requested_pt / k).round().max(1.0) as u32;
    wheel_events_of(px, STROKE_EVENT_PX, direction)
}

/// A stroke's verdict: the classified landing, and where the view went
/// before it settled when that differs from the landing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokeVerdict {
    pub classified: Classified,
    /// The view followed the stroke to this many points the way asked, then
    /// settled at `classified.along` (a page or card snap). `None` when the
    /// view came to rest where it travelled.
    pub snapped_from: Option<i32>,
    /// The frames tracked the move frame to frame but the first and last
    /// frames do not register as one shift: part of the view (a large title
    /// collapsing or expanding, a bar fading in) changed shape while the
    /// content moved. `along` is the tracked travel.
    pub reshaped_header: bool,
}

/// Classify one stroke's `motion` for a scroll in `direction` that was
/// expected to move `expected_pt` (the pixels posted times the calibration).
///
/// On top of [`classify`]: a view that followed the stroke to at least
/// [`SNAP_FOLLOW`] of its distance and settled somewhere else snapped, so it
/// is `Moved` to where it landed rather than `AtEnd`; and a tracked series
/// that moved one way on the requested axis is `Moved` by its travel even
/// when the first-vs-last comparison fails on a reshaped header.
pub fn classify_stroke(
    motion: &Motion,
    direction: ScrollDirection,
    expected_pt: f64,
) -> StrokeVerdict {
    let mut classified = classify(motion, direction);
    let (axis, sign) = axis_of(direction);
    let steps: Vec<&Step> = motion.series.iter().filter(|s| s.axis == axis).collect();
    let mut position = 0i32;
    let mut peak = 0i32;
    for step in &steps {
        position += sign * step.shift;
        peak = peak.max(position);
    }
    let followed = f64::from(peak) >= SNAP_FOLLOW * expected_pt && peak > 0;
    let mut verdict = StrokeVerdict {
        classified,
        snapped_from: None,
        reshaped_header: false,
    };
    match classified.kind {
        OutcomeKind::AtEnd if followed && classified.along > -REVERSAL_MIN_PT => {
            classified.kind = OutcomeKind::Moved;
            classified.uncalibrated = true;
            verdict.snapped_from = (peak != classified.along).then_some(peak);
        }
        OutcomeKind::ChangedInPlace if !motion.reshaped && !motion.baseline_moving => {
            let travel = match axis {
                Axis::Vertical => motion.travel_v,
                Axis::Horizontal => motion.travel_h,
            };
            let one_way = travel != 0
                && motion.series.iter().all(|s| s.axis == axis)
                && steps.iter().all(|s| {
                    s.shift.signum() == travel.signum()
                        && s.confidence >= CLEAN_STEP_CONFIDENCE
                        && !s.ambiguous
                });
            if one_way {
                classified.kind = OutcomeKind::Moved;
                classified.along = sign * travel;
                classified.confidence = Some(
                    steps.iter().map(|s| s.confidence).sum::<f64>() / steps.len() as f64,
                );
                classified.uncalibrated = true;
                verdict.reshaped_header = true;
            }
        }
        _ => {}
    }
    verdict.classified = classified;
    verdict
}

/// The points-per-pixel a finished stroke measured: its landing over the
/// pixels posted, from a clean, confident move that came to rest where it
/// travelled and landed within a factor of two of what `k` predicted.
/// `None` for anything else (a snap, an end, a reshaped header, an alias).
pub fn stroke_calibration(classified: &Classified, posted_px: u32, k: f64) -> Option<f64> {
    if posted_px == 0
        || classified.kind != OutcomeKind::Moved
        || classified.along <= 0
        || classified.ambiguous
        || classified.uncalibrated
        || classified.bounced
        || !classified
            .confidence
            .is_some_and(|confidence| confidence >= CALIBRATION_CONFIDENCE)
    {
        return None;
    }
    let measured = f64::from(classified.along) / f64::from(posted_px);
    let ratio = measured / k;
    ((0.5..=2.0).contains(&ratio)).then(|| measured.clamp(K_RANGE.0, K_RANGE.1))
}

/// One measured chunk as the loop sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkMeasure {
    pub classified: Classified,
    /// The sampler saw the frames settle (not stopped by its hard cap).
    pub settled: bool,
}

/// What one turn of the closed loop measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChunkStep {
    /// The frames were measured.
    Measured(ChunkMeasure),
    /// The frames could not be measured (capture failure, cancel, input error).
    Unmeasured,
}

/// One turn of the closed loop: what was posted and what it measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChunkRun {
    pub step: ChunkStep,
    /// Wheel pixels and events actually posted (fewer than asked when the
    /// burst was stopped).
    pub posted_px: u32,
    pub posted_events: u32,
    /// Input must stop after this turn (takeover, destination changed).
    pub stop: bool,
}

/// What the closed loop did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopSummary {
    pub chunks: u32,
    pub total_px: u32,
    pub events: u32,
    /// Points per pixel from a chunk whose successor's own ratio agreed with
    /// it within 15% — evidence it ran its whole course.
    pub calibration: Option<f64>,
    /// The last chunk moved less than 20% of what it was expected to, with
    /// frames settled.
    pub stalled: bool,
    /// A chunk could not be measured; the loop stopped there.
    pub unmeasured: bool,
    /// The caller asked the loop to stop ([`ChunkRun::stop`]).
    pub halted: bool,
    /// The loop stopped short for its own reason the outcome does not show.
    pub short: Option<&'static str>,
}

fn chunk_px(remaining_pt: f64, k: f64, max_chunk_pt: f64) -> u32 {
    (remaining_pt.min(max_chunk_pt) / k).round().max(1.0) as u32
}

fn keep_going(remaining_pt: f64, requested_pt: f64) -> bool {
    remaining_pt > (0.05 * requested_pt).max(8.0)
}

/// Largest chunk for a registration band of `band_pt`: at most
/// [`CHUNK_MAX_PT`] and 0.6 of the band, so first and last frame of a chunk
/// keep enough in common to register.
pub fn max_chunk_pt(band_pt: f64) -> f64 {
    (0.6 * band_pt).clamp(40.0, CHUNK_MAX_PT)
}

/// Lowest registration confidence a chunk may calibrate from.
const CALIBRATION_CONFIDENCE: f64 = 0.85;
/// Largest spread between a pending calibration and its successor's ratio.
const CALIBRATION_AGREEMENT: f64 = 1.15;

/// Scroll `requested_pt` in chunks of at most `max_chunk_pt`, each sized from
/// the points-per-pixel `k` (starting at `k0`) and re-sized from a chunk that
/// measured cleanly. `run(px)` posts one chunk and says what it posted and
/// measured. The loop stops on a chunk that could not be measured, did not
/// move the way asked, reached the end, changed in place, or rests on an
/// alias-ambiguous registration (a correction for a possibly aliased
/// distance could overshoot by that alias), and when `run` asks it to stop.
pub fn drive_chunks(
    requested_pt: f64,
    k0: f64,
    max_chunk_pt: f64,
    mut run: impl FnMut(u32) -> anyhow::Result<ChunkRun>,
) -> anyhow::Result<LoopSummary> {
    let mut k = k0.clamp(K_RANGE.0, K_RANGE.1);
    let mut remaining = requested_pt;
    let mut summary = LoopSummary {
        chunks: 0,
        total_px: 0,
        events: 0,
        calibration: None,
        stalled: false,
        unmeasured: false,
        halted: false,
        short: None,
    };
    // The latest clean chunk's k, committed once its successor agrees.
    let mut pending: Option<f64> = None;
    loop {
        if summary.chunks >= MAX_CHUNKS {
            summary.short = Some("the chunk limit was reached");
            break;
        }
        let px = chunk_px(remaining, k, max_chunk_pt);
        let turn = run(px)?;
        if turn.posted_px > 0 {
            summary.chunks += 1;
            summary.total_px += turn.posted_px;
            summary.events += turn.posted_events;
        }
        if turn.stop && turn.posted_px == 0 {
            summary.halted = true;
            break;
        }
        let measure = match turn.step {
            ChunkStep::Measured(measure) => measure,
            ChunkStep::Unmeasured => {
                summary.unmeasured = true;
                summary.halted = turn.stop;
                break;
            }
        };
        let expected = f64::from(turn.posted_px) * k;
        let c = measure.classified;
        let moved = f64::from(c.along);
        summary.stalled = measure.settled && moved < 0.2 * expected;
        let full_move = c.kind == OutcomeKind::Moved && moved > 0.0 && !summary.stalled;
        if turn.stop {
            summary.halted = true;
            break;
        }
        if !full_move {
            break;
        }
        remaining -= moved;
        let own = (moved / f64::from(turn.posted_px)).clamp(K_RANGE.0, K_RANGE.1);
        let ratio = moved / expected;
        let clean = !c.ambiguous
            && !c.uncalibrated
            && !c.bounced
            && c.confidence.is_some_and(|confidence| confidence >= CALIBRATION_CONFIDENCE)
            && (0.5..=2.0).contains(&ratio);
        if let Some(previous) = pending.take() {
            if clean && own.max(previous) <= own.min(previous) * CALIBRATION_AGREEMENT {
                summary.calibration = Some(previous);
            }
        }
        if c.ambiguous {
            summary.short = Some(
                "the distance registered ambiguously (repeated rows), so no correcting chunk \
                 was sent",
            );
            break;
        }
        if clean {
            pending = Some(own);
            k = own;
        }
        if !keep_going(remaining, requested_pt) {
            break;
        }
    }
    Ok(summary)
}

/// Points-per-pixel calibration per `(pid, window_id)`, learned from earlier
/// measured scrolls so the first chunk of the next call is sized right.
#[derive(Default)]
pub struct ScrollCalibrations {
    by_window: Mutex<HashMap<(i32, u32), f64>>,
}

impl ScrollCalibrations {
    pub fn get(&self, pid: i32, window_id: u32) -> Option<f64> {
        self.by_window
            .lock()
            .ok()
            .and_then(|map| map.get(&(pid, window_id)).copied())
    }

    pub fn record(&self, pid: i32, window_id: u32, k: f64) {
        if let Ok(mut map) = self.by_window.lock() {
            map.insert((pid, window_id), k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 120;
    const H: usize = 400;
    const CHROME_TOP: usize = 30;
    const CHROME_BOTTOM: usize = 40;

    /// Deterministic texture: value at content row `y` (document space).
    fn texture(x: usize, y: i64) -> u8 {
        let mut v = (x as u64).wrapping_mul(0x9E37_79B9) ^ (y as u64).wrapping_mul(0x85EB_CA6B);
        v ^= v >> 13;
        v = v.wrapping_mul(0xC2B2_AE35);
        v ^= v >> 16;
        // Rows of "text": dense in bands, blank between them.
        if y.rem_euclid(18) < 12 {
            (v % 200) as u8 + 30
        } else {
            245
        }
    }

    /// A window with fixed chrome rows and content scrolled to `offset`
    /// (content moved up by `offset`), drawn by `content(x, doc_y)`.
    fn frame_with(offset: i64, content: impl Fn(usize, i64) -> u8) -> GrayFrame {
        let mut pixels = vec![0u8; W * H];
        for y in 0..H {
            for x in 0..W {
                pixels[y * W + x] = if y < CHROME_TOP {
                    40 + (x % 7) as u8
                } else if y >= H - CHROME_BOTTOM {
                    200 - (x % 5) as u8
                } else {
                    content(x, y as i64 + offset)
                };
            }
        }
        GrayFrame::new(W, H, pixels)
    }

    fn frame(offset: i64) -> GrayFrame {
        frame_with(offset, texture)
    }

    /// The window with the pointer at its centre.
    fn region() -> Region {
        Region::within(W, H, None, (W as f64 / 2.0, H as f64 / 2.0))
    }

    fn track(frames: impl IntoIterator<Item = GrayFrame>) -> MotionTracker {
        let mut frames = frames.into_iter();
        let mut tracker = MotionTracker::new(frames.next().unwrap(), region());
        tracker.begin_gesture(false);
        for frame in frames {
            tracker.push(frame);
        }
        tracker
    }

    fn down(offsets: &[i64]) -> Classified {
        classify(
            &track(offsets.iter().map(|o| frame(*o))).call_motion(),
            ScrollDirection::Down,
        )
    }

    #[test]
    fn a_known_shift_registers_exactly_with_fixed_chrome_rows() {
        for shift in [7, 45, 120, -33] {
            let r = register(&frame(0), &frame(shift), &region(), -300, 300, 0);
            assert_eq!(r.shift, shift as i32, "{r:?}");
            assert!(r.residual < 0.05 * r.diff, "{r:?}");
        }
    }

    #[test]
    fn a_tracked_scroll_down_is_moved_by_its_distance_and_up_is_negative_for_down() {
        let moved = down(&[0, 20, 50, 90, 130, 150, 150]);
        assert_eq!(moved.kind, OutcomeKind::Moved);
        assert_eq!(moved.along, 150);
        assert!(moved.confidence.unwrap() > 0.9);

        let tracker = track([0, -20, -60, -60].map(frame));
        let up = classify(&tracker.call_motion(), ScrollDirection::Up);
        assert_eq!((up.kind, up.along), (OutcomeKind::Moved, 60));
        let asked_down = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(asked_down.along, -60, "moved against the request is negative");
    }

    #[test]
    fn identical_frames_are_no_motion() {
        let tracker = track([frame(0), frame(0), frame(0)]);
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(verdict.kind, OutcomeKind::NoMotion);
        assert_eq!(verdict.along, 0);
        assert_eq!(verdict.confidence, None);
    }

    #[test]
    fn a_constantly_changing_progress_row_neither_moves_nor_breaks_a_move() {
        // A 3-row progress bar inside the bottom chrome advances 3 pt a frame.
        let with_progress = |offset: i64, progress: usize| {
            let mut f = frame(offset);
            for y in H - 30..H - 27 {
                for x in 0..W {
                    f.pixels[y * W + x] = if x < progress { 20 } else { 230 };
                }
            }
            f
        };
        let still = track((0..6).map(|i| with_progress(0, 10 + i * 3)));
        let verdict = classify(&still.call_motion(), ScrollDirection::Down);
        assert_eq!(verdict.kind, OutcomeKind::NoMotion, "{:?}", still.call_motion());

        let moving = track((0..6).map(|i| with_progress(i as i64 * 25, 10 + i * 3)));
        let verdict = classify(&moving.call_motion(), ScrollDirection::Down);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, 125));
    }

    #[test]
    fn a_constant_row_pitch_list_aliases_without_tracking_and_not_with_it() {
        const PITCH: i64 = 44;
        let list = |x: usize, y: i64| {
            let row = y.rem_euclid(PITCH);
            if row < 30 {
                ((x * 37 + row as usize * 11) % 180) as u8 + 40
            } else {
                250
            }
        };
        let frames: Vec<GrayFrame> = (0..=15).map(|i| frame_with(i * 10, list)).collect();
        let unconstrained = register(&frames[0], &frames[15], &region(), -300, 300, 0);
        assert_ne!(unconstrained.shift, 150, "the pitch aliases a single comparison");

        let verdict = classify(&track(frames).call_motion(), ScrollDirection::Down);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, 150));
    }

    #[test]
    fn an_overshoot_and_bounce_is_at_end() {
        // Scrolled up to the top: overshoots 40 pt past the edge, springs back.
        let bounce = |offset: i64| {
            frame_with(0, move |x, y| if y - offset < 0 { 250 } else { texture(x, y - offset) })
        };
        let tracker = track([0, 20, 40, 25, 10, 0].map(bounce));
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Up);
        assert_eq!(verdict.kind, OutcomeKind::AtEnd, "{:?}", tracker.call_motion());
        assert!(verdict.bounced);
        assert_eq!(verdict.along, 0);

        // A move that ran on and bounced back part of the way.
        let verdict = down(&[0, 60, 120, 140, 125, 120]);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::AtEnd, 120));
    }

    /// Live on TextEdit: a page (0.8 of the visible area) left too little in
    /// common between first and last frame to register, and the clean
    /// tracked travel was refused, so every page read "changed in place".
    #[test]
    fn a_page_sized_move_stands_on_its_clean_tracked_travel() {
        for total in [320i64, 340] {
            let offsets: Vec<i64> = (0..=10).map(|i| i * total / 10).collect();
            let verdict = down(&offsets);
            assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, total as i32));
            assert!(verdict.uncalibrated && !verdict.ambiguous, "tracked-only distances never calibrate");
        }
    }

    #[test]
    fn a_pager_like_in_place_change_is_changed_in_place() {
        let page_two = |x: usize, y: i64| texture(x + 1000, y + 5000);
        let tracker = track([frame(0), frame_with(0, page_two)]);
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(verdict.kind, OutcomeKind::ChangedInPlace);
        assert_eq!(verdict.confidence, None);
    }

    #[test]
    fn a_horizontal_scroll_registers_on_columns() {
        let at = |dx: usize| {
            let mut pixels = vec![0u8; W * H];
            for y in 0..H {
                for x in 0..W {
                    pixels[y * W + x] = texture(y, (x + dx) as i64);
                }
            }
            GrayFrame::new(W, H, pixels)
        };
        let tracker = track([at(0), at(10), at(25), at(25)]);
        let right = classify(&tracker.call_motion(), ScrollDirection::Right);
        assert_eq!((right.kind, right.along), (OutcomeKind::Moved, 25));
        let down = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(down.across, 25);
    }

    /// Adds `±amp` gray-level noise, as a compressed video stream does to
    /// every pixel from frame to frame.
    fn noise(f: &mut GrayFrame, seed: u64, amp: i16) {
        for (i, p) in f.pixels.iter_mut().enumerate() {
            let mut v = (i as u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
                .wrapping_mul(0xBF58_476D_1CE4_E5B9);
            v ^= v >> 31;
            let n = (v % (2 * amp as u64 + 1)) as i16 - amp;
            *p = (i16::from(*p) + n).clamp(0, 255) as u8;
        }
    }

    /// The static window of [`frame`] with rows `strip` drawn by
    /// `strip_content(x, y)`, under `±2` noise seeded by `seed`.
    fn with_strip(
        strip: std::ops::Range<usize>,
        seed: u64,
        strip_content: impl Fn(usize, usize) -> u8,
    ) -> GrayFrame {
        let mut f = frame(0);
        for y in strip {
            for x in 0..W {
                f.pixels[y * W + x] = strip_content(x, y);
            }
        }
        noise(&mut f, seed, 2);
        f
    }

    /// Live on iPhone Mirroring: a carousel row about a fifth of the window
    /// tall scrolled 93 pt sideways above a static list, and both calls read
    /// "changed in place". Every column compared the moving strip together
    /// with the static rows around it, so no sideways shift explained it.
    #[test]
    fn a_thin_strip_scrolled_sideways_is_moved() {
        let at = |dx: usize, seed: u64| {
            with_strip(150..210, seed, |x, y| texture(y, (x + dx + 700) as i64))
        };
        let offsets = [0usize, 6, 14, 20, 25];
        let right = track(
            offsets
                .iter()
                .enumerate()
                .map(|(i, dx)| at(*dx, i as u64 + 1)),
        );
        let verdict = classify(&right.call_motion(), ScrollDirection::Right);
        assert_eq!(
            (verdict.kind, verdict.along),
            (OutcomeKind::Moved, 25),
            "{verdict:?}"
        );
        let left = track(
            offsets
                .iter()
                .rev()
                .enumerate()
                .map(|(i, dx)| at(*dx, i as u64 + 1)),
        );
        let verdict = classify(&left.call_motion(), ScrollDirection::Left);
        assert_eq!(
            (verdict.kind, verdict.along),
            (OutcomeKind::Moved, 25),
            "{verdict:?}"
        );
    }

    /// The vertical counterpart: a narrow list beside static content.
    #[test]
    fn a_narrow_column_scrolled_down_is_moved() {
        let at = |offset: i64, seed: u64| {
            let mut f = frame(0);
            for y in CHROME_TOP..H - CHROME_BOTTOM {
                for x in 44..68 {
                    f.pixels[y * W + x] = texture(x + 300, y as i64 + offset);
                }
            }
            noise(&mut f, seed, 2);
            f
        };
        let offsets = [0i64, 9, 21, 33, 40];
        let tracker = track(
            offsets
                .iter()
                .enumerate()
                .map(|(i, o)| at(*o, i as u64 + 1)),
        );
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(
            (verdict.kind, verdict.along),
            (OutcomeKind::Moved, 40),
            "{verdict:?}"
        );
    }

    /// A strip whose content is replaced in place (a carousel page tap)
    /// stays changed in place on either axis.
    #[test]
    fn a_thin_strip_replaced_in_place_is_not_a_move() {
        let before = with_strip(150..210, 1, |x, y| texture(y, (x + 700) as i64));
        let after = with_strip(150..210, 2, |x, y| texture(y + 40, (x + 5000) as i64));
        let motion = track([before, after]).call_motion();
        for direction in [
            ScrollDirection::Right,
            ScrollDirection::Left,
            ScrollDirection::Down,
            ScrollDirection::Up,
        ] {
            let verdict = classify(&motion, direction);
            assert_eq!(
                verdict.kind,
                OutcomeKind::ChangedInPlace,
                "{direction:?} {verdict:?}"
            );
        }
    }

    /// A strip that slides by itself (a marquee, a rotating banner) is not
    /// what the scroll moved: not away from the pointer, and not under it
    /// when it was already moving before the first event.
    #[test]
    fn a_strip_sliding_by_itself_is_not_the_scroll() {
        let at = |dx: usize, seed: u64| {
            with_strip(150..210, seed, |x, y| texture(y, (x + dx + 700) as i64))
        };
        for (pointer, baseline_moving) in [((60.0, 300.0), false), ((60.0, 180.0), true)] {
            let mut tracker = MotionTracker::new(at(0, 1), Region::within(W, H, None, pointer));
            tracker.begin_gesture(baseline_moving);
            for (i, dx) in [6, 14, 20, 25].into_iter().enumerate() {
                tracker.push(at(dx, i as u64 + 2));
            }
            let verdict = classify(&tracker.call_motion(), ScrollDirection::Right);
            assert_eq!(
                verdict.kind,
                OutcomeKind::ChangedInPlace,
                "{pointer:?} {verdict:?}"
            );
        }
    }

    /// Audit of 45d71f18b: a faint highlight sweeping across a strip under
    /// the pointer (a skeleton loader's shimmer) changes less than a no-op
    /// scroll's floor; the strip's rigid steps must not make it a move.
    #[test]
    fn a_shimmer_under_the_pointer_is_no_motion() {
        let shimmer = |offset: i64| {
            let mut f = frame(0);
            for y in 180..222 {
                for x in 0..W {
                    let d = (x as i64 + offset - 60).abs() as f32 / 20.0;
                    f.pixels[y * W + x] = 210 + (20.0 * (1.0 - d).max(0.0)) as u8;
                }
            }
            f
        };
        let tracker = track([0, 12, 24, 36].map(shimmer));
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Right);
        assert_eq!(verdict.kind, OutcomeKind::NoMotion, "{verdict:?}");
    }

    /// Audit of 45d71f18b: a strip under the pointer that slides while the
    /// rest of the window is replaced (a navigation carrying a shared
    /// header) is not a scroll of the strip.
    #[test]
    fn a_strip_that_moves_while_the_rest_is_replaced_is_changed_in_place() {
        let before = with_strip(150..210, 1, |x, y| texture(y, (x + 700) as i64));
        let mut after = with_strip(150..210, 2, |x, y| texture(y, (x + 725) as i64));
        for y in 250..360 {
            for x in 0..W {
                after.pixels[y * W + x] = texture(x + 500, y as i64 + 3000);
            }
        }
        let verdict = classify(
            &track([before, after]).call_motion(),
            ScrollDirection::Right,
        );
        assert_eq!(verdict.kind, OutcomeKind::ChangedInPlace, "{verdict:?}");
    }

    /// Audit of 45d71f18b: a pointer disc appearing over a blank margin
    /// while the whole document scrolls must not narrow the measurement to
    /// the disc.
    #[test]
    fn a_disc_appearing_at_the_pointer_does_not_hide_a_whole_view_scroll() {
        let at = |offset: i64, disc: bool| {
            let mut f = frame_with(offset, |x, y| if x < 80 { texture(x, y) } else { 250 });
            if disc {
                for y in 192..208 {
                    for x in 92..108 {
                        f.pixels[y * W + x] = 150;
                    }
                }
            }
            f
        };
        let mut tracker =
            MotionTracker::new(at(0, false), Region::within(W, H, None, (100.0, 200.0)));
        tracker.begin_gesture(false);
        for offset in [20, 50, 93] {
            tracker.push(at(offset, true));
        }
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!(
            (verdict.kind, verdict.along),
            (OutcomeKind::Moved, 93),
            "{verdict:?}"
        );
    }

    /// Audit of 45d71f18b: identical line bodies under the pointer, their
    /// numbers more than a strip gap away. The bodies alone alias by one
    /// pitch; the whole region, numbers included, does not.
    #[test]
    fn line_numbers_beyond_the_strip_gap_still_break_the_pitch_alias() {
        const PITCH: i64 = 13;
        let numbered = |x: usize, y: i64| {
            let line = y.div_euclid(PITCH);
            let row = y.rem_euclid(PITCH);
            if row >= 10 || (14..44).contains(&x) {
                250
            } else if x < 14 {
                ((line * 7919 + x as i64 * 31 + row * 17).rem_euclid(200)) as u8 + 30
            } else {
                ((x * 13 + row as usize * 29) % 190) as u8 + 40
            }
        };
        let frames = (0..=10).map(|i| frame_with(i * 15, numbered));
        let verdict = classify(&track(frames).call_motion(), ScrollDirection::Down);
        assert_eq!(
            (verdict.kind, verdict.along, verdict.ambiguous),
            (OutcomeKind::Moved, 150, false),
            "{verdict:?}"
        );
    }

    /// Audit of 45d71f18b: a video stream whose static areas are partly
    /// bit-for-bit still (a letterbox, codec skip blocks) and partly noisy.
    #[test]
    fn a_thin_strip_moves_under_partly_noise_free_static_content() {
        let at = |dx: usize, seed: u64| {
            let mut f = with_strip(150..210, seed, |x, y| texture(y, (x + dx + 700) as i64));
            let clean = frame(0);
            f.pixels[..100 * W].copy_from_slice(&clean.pixels[..100 * W]);
            f
        };
        let tracker = track(
            [0usize, 6, 14, 20, 25]
                .into_iter()
                .enumerate()
                .map(|(i, dx)| at(dx, i as u64 + 1)),
        );
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Right);
        assert_eq!(
            (verdict.kind, verdict.along),
            (OutcomeKind::Moved, 25),
            "{verdict:?}"
        );
    }

    #[test]
    fn distance_forms_and_background_ticks() {
        assert_eq!(requested_distance(ScrollUnit::Points, 231, 500.0), 231.0);
        assert_eq!(requested_distance(ScrollUnit::Line, 3, 500.0), 120.0);
        assert_eq!(requested_distance(ScrollUnit::Page, 2, 500.0), 800.0);
        assert_eq!(background_ticks(ScrollUnit::Points, 81), (3, 1));
        assert_eq!(background_ticks(ScrollUnit::Page, 2), (2, 5));
        assert_eq!(ScrollUnit::Line.clamp_amount(1100), 50);
        assert_eq!(ScrollUnit::Points.clamp_amount(1100), 1100);
        assert_eq!(ScrollUnit::Points.clamp_amount(9000), 5000);
        assert_eq!(ScrollUnit::Page.clamp_amount(0), 1);
    }

    #[test]
    fn a_page_scroll_that_names_no_amount_asks_for_one_page() {
        let page = ScrollUnit::Page.clamp_amount(ScrollUnit::Page.default_amount());
        assert_eq!(requested_distance(ScrollUnit::Page, page, 500.0), 400.0);
        let lines = ScrollUnit::Line.clamp_amount(ScrollUnit::Line.default_amount());
        assert_eq!(requested_distance(ScrollUnit::Line, lines, 500.0), 120.0);
    }

    #[test]
    fn wheel_events_cap_each_delta_and_sign_by_direction() {
        assert_eq!(
            wheel_events(75, ScrollDirection::Down),
            vec![(-30, 0), (-30, 0), (-15, 0)]
        );
        assert_eq!(wheel_events(60, ScrollDirection::Up), vec![(30, 0), (30, 0)]);
        assert_eq!(wheel_events(5, ScrollDirection::Right), vec![(0, -5)]);
        assert_eq!(wheel_events(31, ScrollDirection::Left), vec![(0, 30), (0, 1)]);
    }

    fn measured(px: u32, along: i32, confidence: f64, ambiguous: bool, settled: bool) -> ChunkRun {
        let kind = if along == 0 {
            OutcomeKind::NoMotion
        } else {
            OutcomeKind::Moved
        };
        ChunkRun {
            step: ChunkStep::Measured(ChunkMeasure {
                classified: Classified {
                    kind,
                    along,
                    across: 0,
                    confidence: (along != 0).then_some(confidence),
                    bounced: false,
                    ambiguous,
                    uncalibrated: false,
                },
                settled,
            }),
            posted_px: px,
            posted_events: px.div_ceil(WHEEL_EVENT_MAX_PX),
            stop: false,
        }
    }

    /// A view that moves `k` pt per wheel pixel and ends after `room` pt.
    fn view(k: f64, room: f64) -> impl FnMut(u32) -> anyhow::Result<ChunkRun> {
        let mut position = 0.0f64;
        move |px| {
            let before = position;
            position = (position + f64::from(px) * k).min(room);
            Ok(measured(px, (position - before).round() as i32, 0.95, false, true))
        }
    }

    #[test]
    fn the_loop_calibrates_from_its_first_chunk_and_lands_on_the_distance() {
        let mut sizes = Vec::new();
        let mut inner = view(0.771, 10_000.0);
        let summary = drive_chunks(600.0, 1.0, 400.0, |px| {
            sizes.push(px);
            inner(px)
        })
        .unwrap();
        assert_eq!(sizes[0], 400, "first chunk capped at 400 pt with k=1");
        assert_eq!(sizes.len(), 2, "400 px moved 308 pt; the second chunk sized at k=0.771");
        let k = summary.calibration.unwrap();
        assert!((k - 0.771).abs() < 0.01, "{k}");
        assert!(!summary.stalled);
    }

    #[test]
    fn a_cached_calibration_sizes_the_first_chunk() {
        let mut sizes = Vec::new();
        let mut inner = view(0.771, 10_000.0);
        drive_chunks(231.0, 0.771, 400.0, |px| {
            sizes.push(px);
            inner(px)
        })
        .unwrap();
        assert_eq!(sizes, vec![300]);
    }

    /// Audit N3: in a 300 pt scroll area a 400 pt chunk leaves first and last
    /// frame too little in common; chunks stay at 0.6 of the band.
    #[test]
    fn chunks_stay_within_six_tenths_of_the_registration_band() {
        assert_eq!(max_chunk_pt(300.0), 180.0);
        assert_eq!(max_chunk_pt(900.0), CHUNK_MAX_PT);
        let mut sizes = Vec::new();
        let mut inner = view(1.0, 10_000.0);
        drive_chunks(1000.0, 1.0, max_chunk_pt(300.0), |px| {
            sizes.push(px);
            inner(px)
        })
        .unwrap();
        assert!(sizes.iter().all(|px| *px <= 180), "{sizes:?}");
        assert_eq!(sizes.iter().sum::<u32>(), 1000);
    }

    #[test]
    fn the_loop_stops_at_the_end_and_reports_the_stall() {
        let mut calls = 0;
        let mut inner = view(1.0, 450.0);
        let summary = drive_chunks(1200.0, 1.0, 400.0, |px| {
            calls += 1;
            inner(px)
        })
        .unwrap();
        assert_eq!(calls, 2, "a chunk that barely moves ends the call");
        assert!(summary.stalled);
    }

    /// Before: an unmeasured chunk counted as its expected distance and the
    /// loop kept sending blind (400, 400, 100 px for 900 pt).
    #[test]
    fn an_unmeasured_chunk_stops_the_loop_and_counts_what_was_sent() {
        let mut sizes = Vec::new();
        let summary = drive_chunks(900.0, 1.0, 400.0, |px| {
            sizes.push(px);
            Ok(ChunkRun {
                step: ChunkStep::Unmeasured,
                posted_px: px,
                posted_events: 14,
                stop: false,
            })
        })
        .unwrap();
        assert_eq!(sizes, vec![400]);
        assert!(summary.unmeasured);
        assert_eq!((summary.chunks, summary.total_px, summary.events), (1, 400, 14));
    }

    #[test]
    fn a_stop_counts_only_what_was_posted() {
        let mut inner = view(1.0, 10_000.0);
        let mut calls = 0;
        let summary = drive_chunks(1000.0, 1.0, 400.0, |px| {
            calls += 1;
            if calls == 2 {
                let mut partial = measured(90, 90, 0.95, false, true);
                partial.stop = true;
                return Ok(partial);
            }
            inner(px)
        })
        .unwrap();
        assert!(summary.halted);
        assert_eq!((summary.chunks, summary.total_px), (2, 490));
    }

    /// Reviewer M3: a first chunk cut short by the end (240 of 400 pt, inside
    /// the 0.5..2 ratio filter) must not become the window's calibration.
    #[test]
    fn a_chunk_that_may_have_hit_the_end_never_calibrates() {
        let summary = drive_chunks(1200.0, 1.0, 400.0, view(1.0, 240.0)).unwrap();
        assert_eq!(summary.calibration, None);
    }

    /// Audit M3 residual: chunk A moves 240 of 400 pt; chunk B trickles 10 pt
    /// without settling (a loading feed). B's own ratio is far from A's, so
    /// A's calibration is never committed.
    #[test]
    fn a_successor_that_barely_moves_does_not_confirm_the_calibration() {
        let mut calls = 0;
        let summary = drive_chunks(1200.0, 1.0, 400.0, |px| {
            calls += 1;
            Ok(match calls {
                1 => measured(px, 240, 0.95, false, true),
                _ => measured(px, 10, 0.95, false, false),
            })
        })
        .unwrap();
        assert_eq!(summary.calibration, None);
    }

    /// Skeptic R5: an aliased distance must neither calibrate nor trigger a
    /// correction chunk that would overshoot by the alias; the reply says the
    /// loop stopped short.
    #[test]
    fn an_ambiguous_chunk_ends_the_loop_without_calibrating() {
        let mut calls = 0;
        let summary = drive_chunks(300.0, 1.0, 400.0, |px| {
            calls += 1;
            Ok(measured(px, 287, 0.98, true, true))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(summary.calibration, None);
        assert!(summary.short.is_some());
    }

    /// Audit N3: a tracked-only distance stands but never stops the loop.
    #[test]
    fn a_tracked_only_chunk_continues_without_calibrating() {
        let summary = drive_chunks(600.0, 1.0, 400.0, |px| {
            let mut run = measured(px, px as i32, 0.95, false, true);
            if let ChunkStep::Measured(m) = &mut run.step {
                m.classified.uncalibrated = true;
            }
            Ok(run)
        })
        .unwrap();
        assert_eq!((summary.chunks, summary.total_px), (2, 600));
        assert_eq!(summary.calibration, None);
    }

    #[test]
    fn chunks_that_disagree_leave_the_calibration_unset() {
        let mut ratios = [0.5, 1.0, 1.0].into_iter();
        let summary = drive_chunks(1000.0, 1.0, 400.0, |px| {
            let k = ratios.next().unwrap_or(1.0);
            Ok(measured(px, (f64::from(px) * k).round() as i32, 0.95, false, true))
        })
        .unwrap();
        assert_eq!(summary.calibration, None);
    }

    /// Audit R6: content already moving before the first event never sizes a
    /// chunk, and its shift must agree with the tracked travel.
    #[test]
    fn content_moving_before_the_gesture_is_never_calibrated_from() {
        let with_progress = |offset: i64, progress: usize| {
            let mut f = frame(offset);
            for y in H - 30..H - 27 {
                for x in 0..W {
                    f.pixels[y * W + x] = if x < progress { 20 } else { 230 };
                }
            }
            f
        };
        let mut tracker = MotionTracker::new(with_progress(0, 10), region());
        tracker.push(with_progress(0, 30));
        tracker.begin_gesture(true);
        for (i, offset) in [30, 60, 90].into_iter().enumerate() {
            tracker.push(with_progress(offset, 50 + i * 20));
        }
        let verdict = classify(&tracker.call_motion(), ScrollDirection::Down);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, 90));
        assert!(verdict.uncalibrated);
    }

    /// Live on TextEdit (13 pt numbered lines): every chunk registered 534 pt,
    /// but the whole-call search within ±40 pt of that travel settled on an
    /// alias three line pitches short and the reply said 495.
    #[test]
    fn a_net_alias_three_pitches_off_yields_to_the_clean_tracked_travel() {
        let step = |shift| Step {
            axis: Axis::Vertical,
            shift,
            confidence: 0.97,
            ambiguous: false,
            local: false,
        };
        let net = Registration {
            shift: 495,
            residual: 0.5,
            diff: 20.0,
            ambiguous: false,
            local: false,
        };
        let motion = Motion {
            travel_v: 534,
            travel_h: 0,
            series: vec![step(200), step(200), step(134)],
            net_v: net,
            net_h: Registration::still(20.0),
            // 534 is past the overlap first and last frames can score.
            at_travel_v: Registration::still(20.0),
            at_travel_h: Registration::still(20.0),
            reshaped: false,
            non_rigid_pairs: 0,
            baseline_moving: false,
            overlap_v: 632,
            overlap_h: 600,
        };
        let verdict = classify(&motion, ScrollDirection::Down);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, 534));
        assert!(verdict.uncalibrated);

        // One pair that did not register rigidly, an aliased step, or a step
        // on the other axis withdraws the tracked travel's standing.
        let dirty = Motion {
            non_rigid_pairs: 1,
            ..motion.clone()
        };
        assert_eq!(classify(&dirty, ScrollDirection::Down).along, 495);
        let mut aliased = motion.clone();
        aliased.series[1].ambiguous = true;
        assert_eq!(classify(&aliased, ScrollDirection::Down).along, 495);
        let mut sideways = motion.clone();
        sideways.series.push(Step {
            axis: Axis::Horizontal,
            ..step(20)
        });
        assert_eq!(classify(&sideways, ScrollDirection::Down).along, 495);
    }

    /// Audit M2: per-frame steps that each round the same way drift from the
    /// truth; where the net scores the travel clearly worse than its own
    /// shift, the net stands.
    #[test]
    fn a_drifted_tracked_sum_yields_to_a_clearly_better_net() {
        let step = |shift| Step {
            axis: Axis::Vertical,
            shift,
            confidence: 0.97,
            ambiguous: false,
            local: false,
        };
        let motion = Motion {
            travel_v: 240,
            travel_h: 0,
            series: (0..20).map(|_| step(12)).collect(),
            net_v: Registration {
                shift: 246,
                residual: 0.1,
                diff: 20.0,
                ambiguous: false,
                local: false,
            },
            net_h: Registration::still(20.0),
            at_travel_v: Registration {
                shift: 240,
                residual: 6.0,
                diff: 20.0,
                ambiguous: false,
                local: false,
            },
            at_travel_h: Registration::still(20.0),
            reshaped: false,
            non_rigid_pairs: 0,
            baseline_moving: false,
            overlap_v: 632,
            overlap_h: 600,
        };
        assert_eq!(classify(&motion, ScrollDirection::Down).along, 246);
    }

    /// Live on iPhone Mirroring: every foreground scroll that moved read
    /// "changed in place". The mirror is a compressed video: every pixel,
    /// fixed chrome and black margin included, differs a little from frame
    /// to frame, and the stream arrives in bursts. Chrome rows compared
    /// against shifted rows kept the residual above the rigid ratio, and the
    /// tracked travel was refused because first and last frames still shared
    /// part of the band.
    #[test]
    fn a_bursty_move_through_codec_noise_and_fixed_chrome_is_moved() {
        let noisy = |offset: i64, seed: u64| {
            let mut f = frame(offset);
            for (i, p) in f.pixels.iter_mut().enumerate() {
                let mut v = (i as u64 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                v ^= v >> 31;
                let n = (v % 9) as i16 - 4;
                *p = (i16::from(*p) + n).clamp(0, 255) as u8;
            }
            // A black margin down both sides, as around a mirrored phone.
            for y in 0..H {
                for x in (0..10).chain(W - 10..W) {
                    f.pixels[y * W + x] = (seed as u8) & 3;
                }
            }
            f
        };
        for offsets in [[0i64, 12, 81, 93], [0, 13, 92, 230]] {
            let frames = offsets.iter().enumerate().map(|(i, o)| noisy(*o, i as u64 + 1));
            let verdict = classify(&track(frames).call_motion(), ScrollDirection::Down);
            assert_eq!(
                (verdict.kind, verdict.along),
                (OutcomeKind::Moved, offsets[3] as i32),
                "{offsets:?}"
            );
        }
    }

    /// Audit M1 of cda887bd2: with a little change on every row (codec
    /// noise, or a slight dim), a region changing in place while nothing
    /// scrolled registered as a confirmed ~200 pt move — the chosen shift
    /// pushed the changed band out of the overlap and scored only noise rows
    /// against the whole frame's change.
    #[test]
    fn an_in_place_change_under_noise_or_a_dim_is_not_a_move() {
        let swapped = |f: &mut GrayFrame, rows: std::ops::Range<usize>| {
            for y in rows {
                for x in 0..W {
                    f.pixels[y * W + x] = texture(x, y as i64 + 3000);
                }
            }
        };
        // Rows 200..360 swapped in place under ±2 noise, no scroll.
        let mut before = frame(0);
        noise(&mut before, 1, 2);
        let mut after = frame(0);
        swapped(&mut after, 200..360);
        noise(&mut after, 2, 2);
        let verdict = classify(&track([before, after]).call_motion(), ScrollDirection::Down);
        assert_ne!(verdict.kind, OutcomeKind::Moved, "{verdict:?}");
        assert_ne!(verdict.kind, OutcomeKind::AtEnd, "{verdict:?}");

        // The window dims 6% while rows 200..360 swap, no noise at all.
        let mut dimmed = frame(0);
        swapped(&mut dimmed, 200..360);
        for p in dimmed.pixels.iter_mut() {
            *p = (f32::from(*p) * 0.94) as u8;
        }
        let verdict = classify(&track([frame(0), dimmed]).call_motion(), ScrollDirection::Down);
        assert_ne!(verdict.kind, OutcomeKind::Moved, "{verdict:?}");
        assert_ne!(verdict.kind, OutcomeKind::AtEnd, "{verdict:?}");
    }

    /// Audit: a clean move followed by one navigation frame (unrelated
    /// content) read as a confirmed move of the tracked distance.
    #[test]
    fn a_move_then_a_navigation_is_changed_in_place() {
        let navigated = frame_with(0, |x, y| texture(x + 1000, y + 5000));
        let mut frames: Vec<GrayFrame> = [0, 100, 200, 280].map(frame).to_vec();
        frames.push(navigated);
        let verdict = classify(&track(frames).call_motion(), ScrollDirection::Down);
        assert_eq!(verdict.kind, OutcomeKind::ChangedInPlace);
    }

    /// Measured on TextEdit: a numbered document whose lines differ only in
    /// their number registered a 300 pt scroll as 287 — one line pitch short —
    /// because block descriptors cannot see the digits.
    #[test]
    fn lines_that_differ_only_in_their_number_do_not_alias_by_one_pitch() {
        const PITCH: i64 = 13;
        let numbered = |x: usize, y: i64| {
            let line = y.div_euclid(PITCH);
            let row = y.rem_euclid(PITCH);
            if row >= 10 {
                250
            } else if x < 14 {
                ((line * 7919 + x as i64 * 31 + row * 17).rem_euclid(200)) as u8 + 30
            } else {
                ((x * 13 + row as usize * 29) % 190) as u8 + 40
            }
        };
        let frames: Vec<GrayFrame> = (0..=10).map(|i| frame_with(i * 15, numbered)).collect();
        let net = register(&frames[0], &frames[10], &region(), 110, 190, 140);
        assert_eq!(net.shift, 150, "{net:?}");
        let verdict = classify(&track(frames).call_motion(), ScrollDirection::Down);
        assert_eq!((verdict.kind, verdict.along), (OutcomeKind::Moved, 150));
    }

    /// Measured on TextEdit: a chunk that ran into the top moved 99 of 400 pt
    /// and was taken as k = 0.25, so the next chunk asked for 1616 px and the
    /// next call started from the same bad calibration.
    #[test]
    fn a_chunk_that_hits_the_end_does_not_recalibrate() {
        let mut sizes = Vec::new();
        let mut inner = view(1.0, 500.0);
        let summary = drive_chunks(5000.0, 1.0, 400.0, |px| {
            sizes.push(px);
            inner(px)
        })
        .unwrap();
        assert_eq!(sizes, vec![400, 400, 400], "chunks keep the full chunk's size");
        assert!(summary.stalled);
        assert_eq!(summary.calibration, None, "the end-cut chunk disagrees, so nothing commits");
    }

    fn stroke_down(offsets: &[i64], expected_pt: f64) -> (Classified, StrokeVerdict) {
        let motion = track(offsets.iter().map(|o| frame(*o))).call_motion();
        (classify(&motion, ScrollDirection::Down), classify_stroke(&motion, ScrollDirection::Down, expected_pt))
    }

    /// A card rail that snaps: the stroke carried the view 210 pt, then it
    /// settled back to the card edge at 151. A stepped chunk reads that as
    /// the end; a stroke reports where the view landed.
    #[test]
    fn a_stroke_that_snaps_short_reports_its_landing_not_the_end() {
        let offsets = [0, 40, 80, 120, 160, 200, 210, 190, 170, 151, 151];
        let (stepped, stroke) = stroke_down(&offsets, 210.0);
        assert_eq!((stepped.kind, stepped.along), (OutcomeKind::AtEnd, 151));
        assert_eq!(
            (stroke.classified.kind, stroke.classified.along),
            (OutcomeKind::Moved, 151)
        );
        assert_eq!(stroke.snapped_from, Some(210));
        assert_eq!(stroke_calibration(&stroke.classified, 210, 1.0), None, "a snap never calibrates");
    }

    #[test]
    fn a_stroke_that_snaps_back_to_its_start_is_a_move_of_zero_from_its_peak() {
        let offsets = [0, 40, 80, 120, 160, 200, 150, 100, 50, 0, 0];
        let (_, stroke) = stroke_down(&offsets, 210.0);
        assert_eq!(
            (stroke.classified.kind, stroke.classified.along),
            (OutcomeKind::Moved, 0)
        );
        assert_eq!(stroke.snapped_from, Some(200));
    }

    /// The end's spring: the view stopped following the stroke long before
    /// its distance, overshot a little and came back.
    #[test]
    fn a_stroke_that_springs_at_the_end_stays_at_end() {
        let (_, stroke) = stroke_down(&[0, 20, 40, 60, 50, 40, 40], 300.0);
        assert_eq!(
            (stroke.classified.kind, stroke.classified.along),
            (OutcomeKind::AtEnd, 40)
        );
        assert_eq!(stroke.snapped_from, None);
    }

    #[test]
    fn a_clean_stroke_lands_on_its_distance_and_calibrates() {
        let offsets: Vec<i64> = (0..=10).map(|i| i * 24).collect();
        let (_, stroke) = stroke_down(&offsets, 240.0);
        assert_eq!(
            (stroke.classified.kind, stroke.classified.along),
            (OutcomeKind::Moved, 240)
        );
        assert_eq!((stroke.snapped_from, stroke.reshaped_header), (None, false));
        assert_eq!(stroke_calibration(&stroke.classified, 480, 0.5), Some(0.5));
        assert_eq!(stroke_calibration(&stroke.classified, 480, 2.0), None, "4x off the prediction");
    }

    /// A large title above the list collapses while the list scrolls: it
    /// shrinks with the scroll, then swaps for the bar's small title in one
    /// frame pair, so that pair registers no shift and the first and last
    /// frames do not register as one.
    #[test]
    fn a_stroke_under_a_collapsing_title_is_a_move_by_its_tracked_travel() {
        const TITLE: usize = 260;
        let titled = |offset: i64| {
            let mut f = frame(offset);
            let large = offset < 80;
            let height = if large { TITLE - offset as usize } else { 30 };
            for y in CHROME_TOP..CHROME_TOP + TITLE {
                for x in 0..W {
                    f.pixels[y * W + x] = if y >= CHROME_TOP + height {
                        texture(x, y as i64 + offset)
                    } else if large {
                        texture(x + 500, (y - CHROME_TOP) as i64 + offset)
                    } else {
                        90
                    };
                }
            }
            f
        };
        let offsets = [0i64, 20, 40, 60, 80, 100, 120, 140, 160];
        let motion = track(offsets.map(titled)).call_motion();
        let stepped = classify(&motion, ScrollDirection::Down);
        let stroke = classify_stroke(&motion, ScrollDirection::Down, 160.0);
        assert_eq!(stepped.kind, OutcomeKind::ChangedInPlace, "{motion:?}");
        assert_eq!(stroke.classified.kind, OutcomeKind::Moved, "{motion:?}");
        assert!(stroke.reshaped_header);
        assert!(stroke.classified.along > 0 && stroke.classified.along <= 160, "{stroke:?}");
        assert_eq!(stroke_calibration(&stroke.classified, 160, 1.0), None);
    }

    #[test]
    fn a_stroke_posts_the_distance_over_its_calibration_in_capped_events() {
        let events = stroke_events(500.0, 1.0, ScrollDirection::Down);
        assert_eq!(events.len(), 11);
        assert!(events.iter().all(|&(w1, w2)| w2 == 0 && -w1 <= STROKE_EVENT_PX as i32));
        assert_eq!(events.iter().map(|e| -e.0).sum::<i32>(), 500);
        let half = stroke_events(500.0, 2.0, ScrollDirection::Left);
        assert_eq!(half.iter().map(|e| e.1).sum::<i32>(), 250);
    }
}
