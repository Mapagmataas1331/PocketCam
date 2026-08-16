//! Latest-wins RGB for the desktop preview. Not the virtual-camera path.
//!
//! Preview is its own fan-out branch: skip, downscale, or Rec.709, with an fps
//! cap. Virtual camera (NV12 ring) and record (phone H.264) stay native.
//! Auto is a 720p RGB cap while idle, and skips RGB while vcam/record is on
//! so 60 fps decode can keep up. Explicit preview modes keep their labeled
//! size/fps; RGB long edge still ≤ 1920 so 1440p/4K Rec.709 cannot exhaust
//! the machine.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// RGB preview mode. Applies even while idle, except Auto which follows load.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewEncoding {
    Auto = 0,
    Off = 1,
    P480_30 = 2,
    P480_60 = 3,
    P720_30 = 4,
    P720_60 = 5,
    P1080_30 = 6,
    P1080_60 = 7,
    P1440_30 = 8,
    P1440_60 = 9,
    P2160_30 = 10,
    P2160_60 = 11,
    Native = 12,
}

#[derive(Clone, Copy, Debug)]
pub struct PreviewPlan {
    pub skip_always: bool,
    pub native: bool,
    pub max_long: usize,
    pub max_fps: Option<f32>,
}

impl PreviewPlan {
    /// 4K Rec.709 RGB can exhaust the machine. Decode stays native; RGB long
    /// edge stays ≤ 1920. Auto's 720p cap is in `plan()`, not here.
    pub fn clamp_for_source(mut self, src_long: u32) -> Self {
        if src_long > 1920 {
            self.native = false;
            self.max_long = self.max_long.min(1920);
        }
        self
    }
}

/// Decode fps below this (vs Quality) means RGB preview is starving the pipeline.
pub fn slow_preview_floor(quality_fps: u32) -> f32 {
    (quality_fps as f32 * 0.7).clamp(14.0, 36.0)
}

/// Live decode is still arriving, but far below Quality. A stall (no frames)
/// is not this — that is a stream gap, not preview load.
pub fn preview_decode_too_slow(
    fps: f32,
    quality_fps: u32,
    last_frame_age: Option<Duration>,
) -> bool {
    let Some(age) = last_frame_age else {
        return false;
    };
    if age > Duration::from_millis(1500) {
        return false;
    }
    fps > 0.05 && fps < slow_preview_floor(quality_fps)
}

impl PreviewEncoding {
    pub const ALL: [Self; 13] = [
        Self::Auto,
        Self::Off,
        Self::P480_30,
        Self::P480_60,
        Self::P720_30,
        Self::P720_60,
        Self::P1080_30,
        Self::P1080_60,
        Self::P1440_30,
        Self::P1440_60,
        Self::P2160_30,
        Self::P2160_60,
        Self::Native,
    ];

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Off,
            2 => Self::P480_30,
            3 => Self::P480_60,
            4 => Self::P720_30,
            5 => Self::P720_60,
            6 => Self::P1080_30,
            7 => Self::P1080_60,
            8 => Self::P1440_30,
            9 => Self::P1440_60,
            10 => Self::P2160_30,
            11 => Self::P2160_60,
            12 => Self::Native,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Off => "Off",
            Self::P480_30 => "480p 30",
            Self::P480_60 => "480p 60",
            Self::P720_30 => "720p 30",
            Self::P720_60 => "720p 60",
            Self::P1080_30 => "1080p 30",
            Self::P1080_60 => "1080p 60",
            Self::P1440_30 => "1440p 30",
            Self::P1440_60 => "1440p 60",
            Self::P2160_30 => "4K 30",
            Self::P2160_60 => "4K 60",
            Self::Native => "Native",
        }
    }

    /// Size-capped modes need a source at least this long edge. Auto / Off / Native always fit.
    /// Gate the combo with the selected Quality long edge, not the current SPS
    /// (Safari often starts ~720p and ramps). RGB conversion may still downscale.
    pub fn min_src_long(self) -> u32 {
        match self {
            Self::Auto | Self::Off | Self::Native => 0,
            Self::P480_30 | Self::P480_60 => 854,
            Self::P720_30 | Self::P720_60 => 1280,
            Self::P1080_30 | Self::P1080_60 => 1920,
            Self::P1440_30 | Self::P1440_60 => 2560,
            Self::P2160_30 | Self::P2160_60 => 3840,
        }
    }

    pub fn fits(self, src_long: u32) -> bool {
        match self {
            Self::Auto | Self::Off | Self::Native => true,
            other => src_long == 0 || other.min_src_long() <= src_long,
        }
    }

    fn explicit_plan(self) -> PreviewPlan {
        match self {
            Self::Auto => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1280,
                max_fps: Some(30.0),
            },
            Self::Off => PreviewPlan {
                skip_always: true,
                native: false,
                max_long: 2,
                max_fps: None,
            },
            Self::P480_30 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 854,
                max_fps: Some(30.0),
            },
            Self::P480_60 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 854,
                max_fps: Some(60.0),
            },
            Self::P720_30 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1280,
                max_fps: Some(30.0),
            },
            Self::P720_60 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1280,
                max_fps: Some(60.0),
            },
            Self::P1080_30 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(30.0),
            },
            Self::P1080_60 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(60.0),
            },
            Self::P1440_30 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(30.0),
            },
            Self::P1440_60 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(60.0),
            },
            Self::P2160_30 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(30.0),
            },
            Self::P2160_60 => PreviewPlan {
                skip_always: false,
                native: false,
                max_long: 1920,
                max_fps: Some(60.0),
            },
            Self::Native => PreviewPlan {
                skip_always: false,
                native: true,
                max_long: 1920,
                max_fps: Some(60.0),
            },
        }
    }
}

#[derive(Clone)]
pub struct RgbFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    pub seq: u64,
}

#[derive(Default)]
pub struct FrameSlot {
    pub frame: Option<RgbFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Avail {
    Unknown,
    Yes,
    No,
}

impl Avail {
    pub fn from_opt(v: Option<bool>) -> Self {
        match v {
            Some(true) => Self::Yes,
            Some(false) => Self::No,
            None => Self::Unknown,
        }
    }

    pub fn fold(self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes, _) | (_, Self::Yes) => Self::Yes,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::No, Self::No) => Self::No,
        }
    }
}

#[derive(Clone)]
pub struct CameraItem {
    pub id: String,
    pub label: String,
    pub available: Avail,
}

impl Default for CameraItem {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            available: Avail::Unknown,
        }
    }
}

/// Shared quality catalog: phone Quality, desktop Preview encodings, and Camera.
/// Listed options stay selectable; + / x / o is availability, not disabled.
/// Same pixel size shares one ring.
pub struct QualitySpec {
    pub id: &'static str,
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub const QUALITY_SIZES: &[(u32, u32)] = &[
    (854, 480),
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

pub const QUALITY_FPS: &[u32] = &[15, 24, 25, 30, 50, 60];

const fn q(
    id: &'static str,
    label: &'static str,
    width: u32,
    height: u32,
    fps: u32,
) -> QualitySpec {
    QualitySpec {
        id,
        label,
        width,
        height,
        fps,
    }
}

pub const QUALITY_CATALOG: &[QualitySpec] = &[
    q("480p15", "480p 15 fps", 854, 480, 15),
    q("480p24", "480p 24 fps", 854, 480, 24),
    q("480p25", "480p 25 fps", 854, 480, 25),
    q("480p30", "480p 30 fps", 854, 480, 30),
    q("480p50", "480p 50 fps", 854, 480, 50),
    q("480p60", "480p 60 fps", 854, 480, 60),
    q("720p15", "720p 15 fps", 1280, 720, 15),
    q("720p24", "720p 24 fps", 1280, 720, 24),
    q("720p25", "720p 25 fps", 1280, 720, 25),
    q("720p30", "720p 30 fps", 1280, 720, 30),
    q("720p50", "720p 50 fps", 1280, 720, 50),
    q("720p60", "720p 60 fps", 1280, 720, 60),
    q("1080p15", "1080p 15 fps", 1920, 1080, 15),
    q("1080p24", "1080p 24 fps", 1920, 1080, 24),
    q("1080p25", "1080p 25 fps", 1920, 1080, 25),
    q("1080p30", "1080p 30 fps — recommended", 1920, 1080, 30),
    q("1080p50", "1080p 50 fps", 1920, 1080, 50),
    q("1080p60", "1080p 60 fps", 1920, 1080, 60),
    q("1440p15", "1440p 15 fps", 2560, 1440, 15),
    q("1440p24", "1440p 24 fps", 2560, 1440, 24),
    q("1440p25", "1440p 25 fps", 2560, 1440, 25),
    q("1440p30", "1440p 30 fps", 2560, 1440, 30),
    q("1440p50", "1440p 50 fps", 2560, 1440, 50),
    q("1440p60", "1440p 60 fps", 2560, 1440, 60),
    q("2160p15", "4K 15 fps", 3840, 2160, 15),
    q("2160p24", "4K 24 fps", 3840, 2160, 24),
    q("2160p25", "4K 25 fps", 3840, 2160, 25),
    q("2160p30", "4K 30 fps — needs strong Wi-Fi", 3840, 2160, 30),
    q("2160p50", "4K 50 fps — needs strong Wi-Fi", 3840, 2160, 50),
    q("2160p60", "4K 60 fps — needs strong Wi-Fi", 3840, 2160, 60),
];

pub const DEFAULT_QUALITY_ID: &str = "1080p30";
pub const DEFAULT_CAMERA_FMT: u8 = 15; // 1080p30

pub fn size_label(height: u32) -> &'static str {
    match height {
        480 => "480p",
        720 => "720p",
        1080 => "1080p",
        1440 => "1440p",
        _ => "4K",
    }
}

pub fn quality_id_for(height: u32, fps: u32) -> String {
    format!("{}p{fps}", if height >= 2160 { 2160 } else { height })
}

pub fn size_avail(items: &[CameraItem], height: u32) -> Avail {
    QUALITY_FPS
        .iter()
        .fold(Avail::No, |acc, fps| {
            acc.fold(avail_of(items, &quality_id_for(height, *fps)))
        })
}

pub fn fps_avail(items: &[CameraItem], height: u32, fps: u32) -> Avail {
    avail_of(items, &quality_id_for(height, fps))
}

fn avail_of(items: &[CameraItem], id: &str) -> Avail {
    items
        .iter()
        .find(|q| q.id == id)
        .map(|q| q.available)
        .unwrap_or(Avail::Unknown)
}

pub fn quality_by_id(id: &str) -> Option<&'static QualitySpec> {
    QUALITY_CATALOG.iter().find(|q| q.id == id)
}

pub fn quality_long_edge(id: &str) -> u32 {
    quality_by_id(id)
        .map(|q| q.width.max(q.height))
        .unwrap_or(1920)
}

/// While the virtual camera or a recording is running, pixel size stays put.
/// Recording also refuses a higher frame rate (same size, fps down is ok).
pub fn quality_allowed(
    current: &QualitySpec,
    next: &QualitySpec,
    vcam: bool,
    record: bool,
) -> bool {
    if !vcam && !record {
        return true;
    }
    if next.width != current.width || next.height != current.height {
        return false;
    }
    if record && next.fps > current.fps {
        return false;
    }
    true
}

pub fn quality_by_index(i: u8) -> &'static QualitySpec {
    QUALITY_CATALOG
        .get(i as usize)
        .unwrap_or(&QUALITY_CATALOG[DEFAULT_CAMERA_FMT as usize])
}

pub fn quality_index(id: &str) -> u8 {
    QUALITY_CATALOG
        .iter()
        .position(|q| q.id == id)
        .unwrap_or(DEFAULT_CAMERA_FMT as usize) as u8
}

pub fn quality_catalog(available: Avail) -> Vec<CameraItem> {
    QUALITY_CATALOG
        .iter()
        .map(|q| CameraItem {
            id: q.id.to_string(),
            label: q.label.to_string(),
            available,
        })
        .collect()
}

pub fn merge_qualities(reported: impl IntoIterator<Item = CameraItem>) -> Vec<CameraItem> {
    let mut items = quality_catalog(Avail::Unknown);
    for r in reported {
        if let Some(slot) = items.iter_mut().find(|q| q.id == r.id) {
            slot.available = r.available;
            if !r.label.is_empty() {
                slot.label = r.label;
            }
        } else {
            items.push(r);
        }
    }
    items
}

#[derive(Default)]
pub struct StreamStats {
    pub first_frame: Option<Instant>,
    pub last_frame: Option<Instant>,
    pub decoded: u64,
    pub stalled: bool,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub fps_count: u32,
    pub fps_origin: Option<Instant>,
    pub ice: String,
    pub pc: String,
    pub rtp_gaps: u64,
    pub dropped_incomplete: u64,
    pub cameras: Vec<CameraItem>,
    pub selected_camera: String,
    pub qualities: Vec<CameraItem>,
    pub selected_quality: String,
    pub preview_w: u32,
    pub preview_h: u32,
    pub bitrate_kbps: f32,
    pub pkt_pps: f32,
    pub loss_pct: f32,
    pub decode_ms: f32,
    pub oom: bool,
    gaps_recent: u64,
    gaps_until: Option<Instant>,
    drop_recent: u64,
    drop_until: Option<Instant>,
    oom_until: Option<Instant>,
}

impl StreamStats {
    pub fn fresh() -> Self {
        Self {
            qualities: quality_catalog(Avail::Unknown),
            selected_quality: DEFAULT_QUALITY_ID.into(),
            ..Default::default()
        }
    }

    const ALERT_HOLD: Duration = Duration::from_secs(8);

    /// Stream is alive (RTP/AU) even if we are not converting RGB.
    pub fn pulse(&mut self) {
        let now = Instant::now();
        if self.first_frame.is_none() {
            self.first_frame = Some(now);
        }
        self.last_frame = Some(now);
        self.fps_count += 1;
        if self
            .fps_origin
            .map(|t| t.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true)
        {
            let dt = self
                .fps_origin
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(1.0)
                .max(0.001);
            self.fps = self.fps_count as f32 / dt;
            self.fps_count = 0;
            self.fps_origin = Some(now);
        }
    }

    pub fn note_gaps(&mut self, total: u64) {
        let added = total.saturating_sub(self.rtp_gaps);
        self.rtp_gaps = total;
        Self::bump(&mut self.gaps_recent, &mut self.gaps_until, added);
    }

    pub fn note_drops(&mut self, total: u64) {
        let added = total.saturating_sub(self.dropped_incomplete);
        self.dropped_incomplete = total;
        Self::bump(&mut self.drop_recent, &mut self.drop_until, added);
    }

    pub fn note_oom(&mut self) {
        self.oom = true;
        self.oom_until = Some(Instant::now() + Duration::from_secs(12));
    }

    pub fn clear_oom(&mut self) {
        self.oom = false;
        self.oom_until = None;
    }

    pub fn clear_preview_rgb(&mut self) {
        self.preview_w = 0;
        self.preview_h = 0;
    }

    pub fn visible_gaps(&self) -> Option<u64> {
        Self::visible(self.gaps_recent, self.gaps_until)
    }

    pub fn visible_drops(&self) -> Option<u64> {
        Self::visible(self.drop_recent, self.drop_until)
    }

    pub fn oom_visible(&self) -> bool {
        self.oom
            && self
                .oom_until
                .map(|t| Instant::now() < t)
                .unwrap_or(false)
    }

    fn bump(recent: &mut u64, until: &mut Option<Instant>, n: u64) {
        if n == 0 {
            return;
        }
        let now = Instant::now();
        if until.map(|t| now >= t).unwrap_or(true) {
            *recent = 0;
        }
        *recent = recent.saturating_add(n);
        *until = Some(now + Self::ALERT_HOLD);
    }

    fn visible(recent: u64, until: Option<Instant>) -> Option<u64> {
        until
            .filter(|&t| Instant::now() < t)
            .and_then(|_| (recent > 0).then_some(recent))
    }
}

/// Flags the decode thread reads every frame. UI writes; no lock.
pub struct PreviewControl {
    pub vcam_on: AtomicBool,
    pub record_on: AtomicBool,
    encoding: AtomicU8,
    camera_fmt: AtomicU8,
    window_shown: AtomicBool,
    keep_rgb: AtomicBool,
}

impl PreviewControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            vcam_on: AtomicBool::new(false),
            record_on: AtomicBool::new(false),
            encoding: AtomicU8::new(PreviewEncoding::Auto as u8),
            camera_fmt: AtomicU8::new(DEFAULT_CAMERA_FMT),
            window_shown: AtomicBool::new(true),
            keep_rgb: AtomicBool::new(false),
        })
    }

    pub fn camera_quality(&self) -> &'static QualitySpec {
        quality_by_index(self.camera_fmt.load(Ordering::Relaxed))
    }

    pub fn set_camera_quality(&self, id: &str) {
        self.camera_fmt.store(quality_index(id), Ordering::Relaxed);
    }

    /// Decode NV12 even when RGB preview is Off.
    pub fn needs_nv12(&self) -> bool {
        self.vcam_on.load(Ordering::Relaxed)
    }

    pub fn encoding(&self) -> PreviewEncoding {
        PreviewEncoding::from_u8(self.encoding.load(Ordering::Relaxed))
    }

    pub fn set_encoding(&self, enc: PreviewEncoding) {
        self.encoding.store(enc as u8, Ordering::Relaxed);
    }

    pub fn window_shown(&self) -> bool {
        self.window_shown.load(Ordering::Relaxed)
    }

    pub fn set_window_shown(&self, shown: bool) {
        self.window_shown.store(shown, Ordering::Relaxed);
    }

    pub fn keep_rgb(&self) -> bool {
        self.keep_rgb.load(Ordering::Relaxed)
    }

    pub fn set_keep_rgb(&self, keep: bool) {
        self.keep_rgb.store(keep, Ordering::Relaxed);
    }

    pub fn loaded(&self) -> bool {
        self.vcam_on.load(Ordering::Relaxed) || self.record_on.load(Ordering::Relaxed)
    }

    /// True when the desktop pane should show the Preview-off veil — Off,
    /// a hidden window, or Auto skipping RGB while vcam/record is on.
    /// Encoding stays Auto in that last case so RGB resumes when load drops.
    pub fn rgb_off(&self) -> bool {
        self.plan().skip_always
    }

    /// RGB plan for this frame. Auto skips RGB while vcam/record is on, unless
    /// the user pinned preview with Keep.
    /// A hidden window skips RGB (same GPU win as Preview Off) without changing
    /// the encoding the user picked.
    pub fn plan(&self) -> PreviewPlan {
        if !self.window_shown() {
            return PreviewPlan {
                skip_always: true,
                native: false,
                max_long: 2,
                max_fps: None,
            };
        }
        let enc = self.encoding();
        if enc == PreviewEncoding::Auto && self.loaded() && !self.keep_rgb() {
            return PreviewPlan {
                skip_always: true,
                native: false,
                max_long: 2,
                max_fps: None,
            };
        }
        enc.explicit_plan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_fits_selected_quality_not_warmup_sps() {
        let q = quality_long_edge("1080p30");
        assert_eq!(q, 1920);
        assert!(PreviewEncoding::P480_30.fits(q));
        assert!(PreviewEncoding::P720_30.fits(q));
        assert!(PreviewEncoding::P1080_30.fits(q));
        assert!(!PreviewEncoding::P1440_30.fits(q));
        assert!(!PreviewEncoding::P2160_30.fits(q));
        assert!(PreviewEncoding::Auto.fits(678));
        assert!(!PreviewEncoding::P1080_30.fits(678));
    }

    #[test]
    fn hidden_window_skips_rgb_without_changing_encoding() {
        let p = PreviewControl::new();
        p.set_encoding(PreviewEncoding::P1080_30);
        p.set_window_shown(false);
        let plan = p.plan();
        assert!(plan.skip_always);
        assert_eq!(p.encoding(), PreviewEncoding::P1080_30);
        p.set_window_shown(true);
        assert!(!p.plan().skip_always);
        assert_eq!(p.plan().max_long, 1920);
    }

    #[test]
    fn auto_skips_rgb_while_vcam_is_on() {
        let p = PreviewControl::new();
        p.vcam_on.store(true, Ordering::Relaxed);
        assert!(p.plan().skip_always);
        assert_eq!(p.encoding(), PreviewEncoding::Auto);
        p.vcam_on.store(false, Ordering::Relaxed);
        assert!(!p.plan().skip_always);
        assert_eq!(p.plan().max_long, 1280);
        p.vcam_on.store(true, Ordering::Relaxed);
        p.set_keep_rgb(true);
        assert!(!p.plan().skip_always);
        assert!(!p.rgb_off());
        p.set_keep_rgb(false);
        assert!(p.rgb_off());
        assert_eq!(p.encoding(), PreviewEncoding::Auto);
    }

    #[test]
    fn default_quality_is_1080p30() {
        assert_eq!(QUALITY_CATALOG[DEFAULT_CAMERA_FMT as usize].id, "1080p30");
        assert_eq!(quality_index("1080p30"), DEFAULT_CAMERA_FMT);
        assert_eq!(
            QUALITY_CATALOG.len(),
            QUALITY_SIZES.len() * QUALITY_FPS.len()
        );
    }

    #[test]
    fn four_k_source_does_not_force_720p_on_explicit_1080p() {
        let p = PreviewPlan {
            skip_always: false,
            native: true,
            max_long: 1920,
            max_fps: Some(60.0),
        }
        .clamp_for_source(3840);
        assert!(!p.native);
        assert_eq!(p.max_long, 1920);
        assert_eq!(p.max_fps, Some(60.0));
    }

    #[test]
    fn one_fps_is_too_slow_for_1080p30() {
        assert!(preview_decode_too_slow(
            1.0,
            30,
            Some(Duration::from_millis(200))
        ));
        assert!(preview_decode_too_slow(
            15.0,
            30,
            Some(Duration::from_millis(200))
        ));
        assert!(!preview_decode_too_slow(
            28.0,
            30,
            Some(Duration::from_millis(200))
        ));
        assert!(!preview_decode_too_slow(
            1.0,
            30,
            Some(Duration::from_secs(3))
        ));
        assert!(!preview_decode_too_slow(1.0, 30, None));
    }
}
