//! Media Foundation H.264 → NV12 (DXVA when the MFT uses it).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;

use crate::preview::{FrameSlot, PreviewPlan, RgbFrame, StreamStats};
use crate::sps::{sps_crop_from_annex_b, SpsCrop};
use crate::vcam::Vcam;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PreviewRgb {
    /// Drain the MFT (DPB stays in order) but do not convert RGB.
    Skip,
    /// Native crop Rec.709 — only Native encoding, and only ≤ 1080p long edge.
    Native,
    /// Long-edge cap from NV12. Size and fps come from the preview select.
    Downscale { max_long: usize },
}

pub fn preview_rgb_mode(
    queue_empty: bool,
    plan: PreviewPlan,
    last_rgb: &mut Option<Instant>,
) -> PreviewRgb {
    if !queue_empty || plan.skip_always {
        return PreviewRgb::Skip;
    }
    if let Some(fps) = plan.max_fps {
        if fps > 0.0 {
            if let Some(t) = *last_rgb {
                let min_dt = Duration::from_secs_f32(1.0 / fps);
                if t.elapsed() < min_dt {
                    return PreviewRgb::Skip;
                }
            }
        }
    }
    *last_rgb = Some(Instant::now());
    if plan.native {
        PreviewRgb::Native
    } else {
        PreviewRgb::Downscale {
            max_long: plan.max_long.max(2).min(1920),
        }
    }
}

struct OutputLayout {
    frame_w: u32,
    frame_h: u32,
    default_stride: Option<i32>,
    aperture: Option<(i32, i32, i32, i32)>,
    sps_crop: Option<SpsCrop>,
}

impl Default for OutputLayout {
    fn default() -> Self {
        Self {
            frame_w: 0,
            frame_h: 0,
            default_stride: None,
            aperture: None,
            sps_crop: None,
        }
    }
}

pub struct MfH264Decoder {
    transform: IMFTransform,
    color: Option<IMFTransform>,
    layout: OutputLayout,
    logged_layout: bool,
    rgb_scratch: Vec<u32>,
    nv12_crop: Vec<u8>,
    ring_scratch: Vec<u8>,
    /// Native Rec.709 converter already OOM'd — skip it, still try NV12 RGB.
    color_oom: bool,
    hit_oom: bool,
    last_oom_log: Option<Instant>,
}

impl MfH264Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            crate::mf::ensure()?;

            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("CLSID_MSH264DecoderMFT")?;

            if let Ok(api) = transform.cast::<ICodecAPI>() {
                let low = VARIANT::from(true);
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &low);
            }
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }

            let input: IMFMediaType = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &input, 0)?;

            let mut dec = Self {
                transform,
                color: None,
                layout: OutputLayout::default(),
                logged_layout: false,
                rgb_scratch: Vec::new(),
                nv12_crop: Vec::new(),
                ring_scratch: Vec::new(),
                color_oom: false,
                hit_oom: false,
                last_oom_log: None,
            };
            dec.pick_nv12()?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            dec.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(dec)
        }
    }

    pub fn flush(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Some(color) = &self.color {
                let _ = color.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
        }
    }

    pub fn take_oom(&mut self) -> bool {
        std::mem::replace(&mut self.hit_oom, false)
    }

    pub fn src_long(&self) -> u32 {
        let fw = self.layout.frame_w;
        let fh = self.layout.frame_h;
        let sps = self.layout.sps_crop.map(|c| c.w.max(c.h)).unwrap_or(0);
        fw.max(fh).max(sps)
    }

    fn note_oom(&mut self, where_: &'static str) {
        self.hit_oom = true;
        self.color_oom = true;
        self.rgb_scratch.clear();
        self.rgb_scratch.shrink_to_fit();
        if let Some(color) = self.color.take() {
            unsafe {
                let _ = color.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
        }
        let now = Instant::now();
        if self
            .last_oom_log
            .map(|t| t.elapsed() >= Duration::from_secs(2))
            .unwrap_or(true)
        {
            self.last_oom_log = Some(now);
            tracing::warn!("decode OOM at {where_} — drop preview RGB, keep the stream");
        }
    }

    unsafe fn pick_nv12(&mut self) -> Result<()> {
        if let Some(color) = self.color.take() {
            let _ = color.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
        for i in 0..32u32 {
            let t = match self.transform.GetOutputAvailableType(0, i) {
                Ok(t) => t,
                Err(_) => break,
            };
            let sub: GUID = t.GetGUID(&MF_MT_SUBTYPE)?;
            if sub == MFVideoFormat_NV12 {
                t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
                self.transform.SetOutputType(0, &t, 0)?;
                self.refresh_output_info()?;
                return Ok(());
            }
        }
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        let _ = self.transform.SetOutputType(0, &t, 0);
        self.refresh_output_info()?;
        Ok(())
    }

    unsafe fn refresh_output_info(&mut self) -> Result<()> {
        let t = self.transform.GetOutputCurrentType(0)?;
        let sps_crop = self.layout.sps_crop;
        let mut layout = OutputLayout::default();
        if let Ok(size) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
            layout.frame_w = (size >> 32) as u32;
            layout.frame_h = size as u32;
        }
        if let Ok(stride) = t.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
            layout.default_stride = Some(stride as i32);
        }
        layout.aperture = read_aperture(&t);
        layout.sps_crop = sps_crop;
        tracing::info!(
            "MF output NV12 {}x{} stride={:?} aperture={:?} sps={:?}",
            layout.frame_w,
            layout.frame_h,
            layout.default_stride,
            layout.aperture,
            layout.sps_crop.map(|c| (c.w, c.h))
        );
        self.layout = layout;
        self.logged_layout = false;
        Ok(())
    }

    unsafe fn attach_color_converter(&mut self) {
        if self.color.is_some() || self.color_oom || self.src_long() > 1920 {
            return;
        }
        match self.build_color_converter() {
            Ok(c) => {
                tracing::warn!("MF color converter RGB32 ready");
                self.color = Some(c);
            }
            Err(e) => {
                tracing::warn!("MF color converter unavailable ({e:#}); NV12 fallback");
                self.color = None;
            }
        }
    }

    unsafe fn build_color_converter(&self) -> Result<IMFTransform> {
        let nv12 = self.transform.GetOutputCurrentType(0)?;
        let conv: IMFTransform =
            CoCreateInstance(&CColorConvertDMO, None, CLSCTX_INPROC_SERVER)
                .context("CColorConvertDMO")?;
        conv.SetInputType(0, &nv12, 0)?;

        let size = nv12.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
        let w = (size >> 32) as u32;
        let h = size as u32;
        let rgb: IMFMediaType = MFCreateMediaType()?;
        rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        rgb.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
        if let Ok(rate) = nv12.GetUINT64(&MF_MT_FRAME_RATE) {
            let _ = rgb.SetUINT64(&MF_MT_FRAME_RATE, rate);
        }
        rgb.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        rgb.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        rgb.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        rgb.SetUINT32(&MF_MT_DEFAULT_STRIDE, w.saturating_mul(4))?;
        rgb.SetUINT32(&MF_MT_SAMPLE_SIZE, w.saturating_mul(h).saturating_mul(4))?;
        conv.SetOutputType(0, &rgb, 0)?;
        conv.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        conv.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        Ok(conv)
    }

    unsafe fn color_convert_sample(
        &mut self,
        nv12: &IMFSample,
        out: &mut Vec<u32>,
    ) -> Result<(u32, u32)> {
        let input_result = self
            .color
            .as_ref()
            .context("no color converter")?
            .ProcessInput(0, nv12, 0);
        match input_result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                let _ = self.drain_color_rgb(false, out)?;
                    match self
                    .color
                    .as_ref()
                    .context("no color converter")?
                    .ProcessInput(0, nv12, 0)
                {
                    Ok(()) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e.into()),
        }
        self.drain_color_rgb(true, out)?
            .context("color converter produced no RGB")
    }

    unsafe fn drain_color_rgb(
        &mut self,
        keep: bool,
        out: &mut Vec<u32>,
    ) -> Result<Option<(u32, u32)>> {
        let color = self.color.as_ref().context("no color converter")?;
        let info = color.GetOutputStreamInfo(0)?;
        let mut last = None;
        loop {
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                ..Default::default()
            }];
            let size = info.cbSize.max(1);
            let sample: IMFSample = MFCreateSample()?;
            let buf: IMFMediaBuffer = match MFCreateMemoryBuffer(size) {
                Ok(b) => b,
                Err(e) => return Err(e.into()),
            };
            sample.AddBuffer(&buf)?;
            bufs[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
            let mut status = 0u32;
            match color.ProcessOutput(0, &mut bufs, &mut status) {
                Ok(()) => {
                    let sample = take_mft_sample(&mut bufs).context("RGB sample")?;
                    if keep {
                        last = Some(copy_rgb32(&sample, &self.layout, out)?);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    release_mft_out(&mut bufs);
                    break;
                }
                Err(e) => {
                    release_mft_out(&mut bufs);
                    return Err(e.into());
                }
            }
        }
        Ok(last)
    }

    pub fn decode_access_unit(
        &mut self,
        annex_b: &[u8],
        slot: &Arc<Mutex<FrameSlot>>,
        stats: &Arc<Mutex<StreamStats>>,
        seq: &mut u64,
        rgb: PreviewRgb,
        vcam: Option<&Vcam>,
    ) -> Result<()> {
        if annex_b.is_empty() {
            return Ok(());
        }
        if let Some(crop) = sps_crop_from_annex_b(annex_b) {
            if self.layout.sps_crop != Some(crop) {
                if self.layout.sps_crop.is_some() {
                    self.flush();
                    self.color = None;
                }
                tracing::info!(
                    "SPS display crop {}x{}+{}+{}",
                    crop.w,
                    crop.h,
                    crop.x,
                    crop.y
                );
                self.layout.sps_crop = Some(crop);
            }
        }
        unsafe {
            let sample = match sample_from_bytes(annex_b) {
                Ok(s) => s,
                Err(e) if is_oom(&e) => {
                    self.note_oom("sample_from_bytes");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    self.drain(slot, stats, seq, rgb, vcam)?;
                    match self.transform.ProcessInput(0, &sample, 0) {
                        Ok(()) => {}
                        Err(e) if hresult_oom(&e) => {
                            self.note_oom("ProcessInput");
                            return Ok(());
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) if hresult_oom(&e) => {
                    self.note_oom("ProcessInput");
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
            self.drain(slot, stats, seq, rgb, vcam)?;
        }
        Ok(())
    }

    unsafe fn drain(
        &mut self,
        slot: &Arc<Mutex<FrameSlot>>,
        stats: &Arc<Mutex<StreamStats>>,
        seq: &mut u64,
        rgb: PreviewRgb,
        vcam: Option<&Vcam>,
    ) -> Result<()> {
        loop {
            match self.process_output(rgb, vcam) {
                Ok(Some(decoded)) => {
                    let now = Instant::now();
                    {
                        let mut s = stats.lock();
                        if s.first_frame.is_none() {
                            s.first_frame = Some(now);
                        }
                        s.last_frame = Some(now);
                        s.decoded += 1;
                        s.fps_count += 1;
                        if s.fps_origin.map(|t| t.elapsed() >= Duration::from_secs(1)).unwrap_or(true)
                        {
                            let dt = s
                                .fps_origin
                                .map(|t| t.elapsed().as_secs_f32())
                                .unwrap_or(1.0)
                                .max(0.001);
                            s.fps = s.fps_count as f32 / dt;
                            s.fps_count = 0;
                            s.fps_origin = Some(now);
                        }
                    }
                    let Some((pixels, cw, ch)) = decoded else {
                        continue;
                    };
                    if cw == 0 || ch == 0 {
                        continue;
                    }
                    *seq += 1;
                    let native = native_display_size(&self.layout, cw, ch);
                    {
                        let mut g = slot.lock();
                        if let Some(old) = g.frame.take() {
                            self.rgb_scratch = old.pixels;
                        }
                        g.frame = Some(RgbFrame {
                            width: cw,
                            height: ch,
                            pixels,
                            seq: *seq,
                        });
                    }
                    let mut s = stats.lock();
                    if s.width == 0 {
                        tracing::info!(
                            "first decoded frame crop {}x{} (preview {}x{})",
                            native.0,
                            native.1,
                            cw,
                            ch
                        );
                    }
                    s.width = native.0;
                    s.height = native.1;
                    s.preview_w = cw;
                    s.preview_h = ch;
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// RGB is optional. The sample is always consumed so the DPB stays in order.
    /// Packed NV12 is written to the ring on every decoded frame when vcam is on.
    unsafe fn process_output(
        &mut self,
        rgb: PreviewRgb,
        vcam: Option<&Vcam>,
    ) -> Result<Option<Option<(Vec<u32>, u32, u32)>>> {
        if self.src_long() > 1920 {
            if let Some(color) = self.color.take() {
                let _ = color.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
        }
        let info = self.transform.GetOutputStreamInfo(0)?;
        let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0
            || (info.dwFlags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;

        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            ..Default::default()
        }];
        if !provides {
            let size = info.cbSize.max(1);
            let sample: IMFSample = match MFCreateSample() {
                Ok(s) => s,
                Err(e) if hresult_oom(&e) => {
                    self.note_oom("MFCreateSample");
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            };
            let buf: IMFMediaBuffer = match MFCreateMemoryBuffer(size) {
                Ok(b) => b,
                Err(e) if hresult_oom(&e) => {
                    self.note_oom("MFCreateMemoryBuffer");
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            };
            sample.AddBuffer(&buf)?;
            out[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
        }

        let mut status = 0u32;
        match self.transform.ProcessOutput(0, &mut out, &mut status) {
            Ok(()) => {
                let sample = take_mft_sample(&mut out).context("ProcessOutput sample")?;
                if let Some(vcam) = vcam {
                    if vcam.is_on() {
                        if let Err(e) = self.pack_to_ring(&sample, vcam) {
                            tracing::debug!("ring NV12: {e:#}");
                        }
                    }
                }
                if rgb == PreviewRgb::Skip {
                    return Ok(Some(None));
                }
                let mut pixels = std::mem::take(&mut self.rgb_scratch);
                pixels.clear();
                let src_long = self.src_long();
                let native_ok =
                    rgb == PreviewRgb::Native && src_long <= 1920 && !self.color_oom;
                if native_ok {
                    self.attach_color_converter();
                }
                if native_ok && self.color.is_some() {
                    match self.color_convert_sample(&sample, &mut pixels) {
                        Ok((w, h)) => return Ok(Some(Some((pixels, w, h)))),
                        Err(e) => {
                            if is_oom(&e) {
                                self.note_oom("color convert");
                                pixels.clear();
                                self.rgb_scratch = pixels;
                                return Ok(Some(None));
                            }
                            tracing::warn!("color convert failed, NV12 fallback: {e:#}");
                            pixels.clear();
                        }
                    }
                }
                let max_long = match rgb {
                    PreviewRgb::Downscale { max_long } => Some(max_long),
                    PreviewRgb::Native if src_long > 1920 => Some(1920),
                    _ => None,
                };
                match copy_nv12_cropped(
                    &sample,
                    &self.layout,
                    &mut self.logged_layout,
                    max_long,
                    &mut pixels,
                ) {
                    Ok((w, h)) => Ok(Some(Some((pixels, w, h)))),
                    Err(e) => {
                        if is_oom(&e) {
                            self.note_oom("NV12 RGB");
                        } else {
                            tracing::warn!("preview RGB skipped: {e:#}");
                        }
                        self.rgb_scratch = pixels;
                        Ok(Some(None))
                    }
                }
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                release_mft_out(&mut out);
                Ok(None)
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                release_mft_out(&mut out);
                self.color = None;
                tracing::info!("MF stream change — reselect NV12");
                self.pick_nv12()?;
                Ok(None)
            }
            Err(e) if hresult_oom(&e) => {
                release_mft_out(&mut out);
                self.note_oom("ProcessOutput");
                Ok(None)
            }
            Err(e) => {
                release_mft_out(&mut out);
                Err(e.into())
            }
        }
    }

    unsafe fn pack_to_ring(&mut self, sample: &IMFSample, vcam: &Vcam) -> Result<()> {
        let (cw, ch) = pack_nv12_contiguous(
            sample,
            &self.layout,
            &mut self.nv12_crop,
        )?;
        if cw < 2 || ch < 2 {
            return Ok(());
        }
        vcam.write_contain(&self.nv12_crop, cw, ch, &mut self.ring_scratch);
        Ok(())
    }
}

unsafe fn sample_from_bytes(data: &[u8]) -> Result<IMFSample> {
    let buf: IMFMediaBuffer = MFCreateMemoryBuffer(data.len() as u32)?;
    let mut max = 0u32;
    let mut cur = 0u32;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur))?;
    if ptr.is_null() {
        bail!("IMFMediaBuffer::Lock null");
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    buf.Unlock()?;
    buf.SetCurrentLength(data.len() as u32)?;
    let sample: IMFSample = MFCreateSample()?;
    sample.AddBuffer(&buf)?;
    Ok(sample)
}

unsafe fn read_aperture(t: &IMFMediaType) -> Option<(i32, i32, i32, i32)> {
    for key in [&MF_MT_MINIMUM_DISPLAY_APERTURE, &MF_MT_GEOMETRIC_APERTURE] {
        let Ok(size) = t.GetBlobSize(key) else { continue };
        if (size as usize) < std::mem::size_of::<MFVideoArea>() {
            continue;
        }
        let mut blob = vec![0u8; size as usize];
        if t.GetBlob(key, &mut blob, None).is_err() {
            continue;
        }
        let area = std::ptr::read_unaligned(blob.as_ptr() as *const MFVideoArea);
        let w = area.Area.cx;
        let h = area.Area.cy;
        if w > 0 && h > 0 {
            return Some((area.OffsetX.value as i32, area.OffsetY.value as i32, w, h));
        }
    }
    None
}

unsafe fn copy_rgb32(
    sample: &IMFSample,
    layout: &OutputLayout,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let buf = sample
        .GetBufferByIndex(0)
        .or_else(|_| sample.ConvertToContiguousBuffer())?;
    let cur_len = buf.GetCurrentLength().unwrap_or(0) as usize;
    if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
        let mut scan0: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        buf2d.Lock2D(&mut scan0, &mut pitch)?;
        let result = (|| {
            if scan0.is_null() || pitch == 0 {
                bail!("RGB Lock2D null");
            }
            let stride = pitch.unsigned_abs() as usize;
            let plane_w = if stride >= 4 {
                stride / 4
            } else {
                layout.frame_w as usize
            };
            let plane_h = if stride >= 4 {
                cur_len
                    .max(stride)
                    .saturating_div(stride)
                    .max(layout.frame_h as usize)
            } else {
                layout.frame_h as usize
            };
            rgb_from_bgrx(scan0, stride, plane_w, plane_h, layout, out)
        })();
        let _ = buf2d.Unlock2D();
        if result.is_ok() {
            return result;
        }
    }
    let mut max = 0u32;
    let mut cur = 0u32;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur))?;
    let stride = (layout.frame_w as usize).saturating_mul(4).max(4);
    let plane_h = (cur as usize)
        .saturating_div(stride)
        .max(layout.frame_h as usize);
    let result = rgb_from_bgrx(ptr, stride, layout.frame_w as usize, plane_h, layout, out);
    let _ = buf.Unlock();
    result
}

unsafe fn rgb_from_bgrx(
    scan0: *const u8,
    stride: usize,
    plane_w: usize,
    plane_h: usize,
    layout: &OutputLayout,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let crop = resolve_crop(layout, plane_w.max(1), plane_h.max(1));
    if crop.w == 0 || crop.h == 0 {
        bail!("RGB empty");
    }
    let safe_w = mb_safe(crop.w).max(2);
    let safe_h = mb_safe(crop.h).max(2);
    resize_rgb(out, crop.w.saturating_mul(crop.h))?;
    for j in 0..crop.h {
        let sj = if j >= safe_h { safe_h - 1 } else { j };
        let row = scan0.add((crop.y + sj) * stride);
        for i in 0..crop.w {
            let si = if i >= safe_w { safe_w - 1 } else { i };
            let p = row.add((crop.x + si) * 4);
            let b = *p as u32;
            let g = *p.add(1) as u32;
            let r = *p.add(2) as u32;
            out[j * crop.w + i] = (r << 16) | (g << 8) | b;
        }
    }
    Ok((crop.w as u32, crop.h as u32))
}

struct Crop {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

fn align16(n: usize) -> usize {
    (n + 15) & !15
}

fn even(n: usize) -> usize {
    n & !1
}

/// 1080 is not 16-aligned. The last 8 luma rows share a macroblock with
/// zero-UV padding, which shows as a green strip. Repeat the last full MB.
fn mb_safe(n: usize) -> usize {
    let r = n % 16;
    if r == 0 {
        n
    } else {
        n - r
    }
}

/// 4K RGB32 is 33 MB. A failed `resize` aborts the process (0xc0000409) with no backtrace.
fn resize_rgb(out: &mut Vec<u32>, n: usize) -> Result<()> {
    const MAX: usize = 3840 * 2160;
    if n == 0 || n > MAX {
        bail!("RGB size {n} out of range");
    }
    if out.try_reserve(n.saturating_sub(out.len())).is_err() {
        out.clear();
        out.shrink_to_fit();
        bail!("RGB alloc {n} failed");
    }
    out.resize(n, 0);
    Ok(())
}

fn is_oom(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("0x8007000E") || s.contains("RGB alloc") || s.contains("Not enough memory")
}

pub(crate) fn error_is_oom(e: &anyhow::Error) -> bool {
    is_oom(e)
}

fn hresult_oom(e: &windows::core::Error) -> bool {
    const E_OUTOFMEMORY: i32 = 0x8007_000E_u32 as i32;
    e.code().0 == E_OUTOFMEMORY
}

/// windows-rs stores IMFSample in ManuallyDrop. Dropping the struct does not
/// Release the COM object — every NEED_MORE_INPUT used to leak the output buffer.
unsafe fn release_mft_out(bufs: &mut [MFT_OUTPUT_DATA_BUFFER]) {
    let _ = bufs[0].pSample.take();
    let _ = bufs[0].pEvents.take();
}

unsafe fn take_mft_sample(bufs: &mut [MFT_OUTPUT_DATA_BUFFER]) -> Option<IMFSample> {
    let sample = bufs[0].pSample.take();
    let _ = bufs[0].pEvents.take();
    sample
}

/// 1080p is coded as 1088 (16-aligned). Same for portrait 1080-wide.
fn visible_luma(reported: usize, plane: usize) -> usize {
    let coded = reported.max(plane);
    if reported > 0 && reported < plane {
        return tighten_mb(even(reported).max(2));
    }
    if coded == align16(reported) && reported % 16 != 0 {
        return tighten_mb(even(reported).max(2));
    }
    tighten_mb(even(reported.min(plane).max(2)))
}

/// Macroblock padding is zeroed UV → green. Crop before MF aperture arrives.
fn tighten_mb(n: usize) -> usize {
    match n {
        1088 => 1080,
        2176 => 2160,
        _ => n,
    }
}

fn resolve_crop(layout: &OutputLayout, stride: usize, coded_h: usize) -> Crop {
    let fw = layout.frame_w as usize;
    let fh = layout.frame_h as usize;
    let (mut x, mut y, mut w, mut h) = if let Some(sps) = layout.sps_crop {
        (
            sps.x as usize,
            sps.y as usize,
            sps.w as usize,
            sps.h as usize,
        )
    } else if let Some((ax, ay, aw, ah)) = layout.aperture {
        if aw > 0 && ah > 0 {
            (
                ax.max(0) as usize,
                ay.max(0) as usize,
                tighten_mb(aw as usize),
                tighten_mb(ah as usize),
            )
        } else {
            (0, 0, visible_luma(fw, stride), visible_luma(fh, coded_h))
        }
    } else {
        (0, 0, visible_luma(fw, stride), visible_luma(fh, coded_h))
    };
    w = tighten_mb(w);
    h = tighten_mb(h);
    x = even(x);
    y = even(y);
    w = even(w).max(2);
    h = even(h).max(2);
    if x >= stride {
        x = 0;
    }
    if y >= coded_h {
        y = 0;
    }
    w = w.min(even(stride.saturating_sub(x))).max(2);
    h = h.min(even(coded_h.saturating_sub(y))).max(2);
    Crop { x, y, w, h }
}

/// Rows of Y before the UV plane. Trust the media type height unless the
/// packed size clearly includes 16-aligned padding (1920×1088). Forcing
/// align16(1080)=1088 when UV actually starts at 1080 is a black grid.
fn uv_plane_rows(frame_h: usize, stride: usize, packed_len: usize) -> usize {
    let h = even(frame_h).max(2);
    let aligned = align16(h);
    if stride < 2 {
        return h;
    }
    let packed_h = if packed_len >= stride * 3 / 2 {
        even(packed_len * 2 / (stride * 3))
    } else {
        0
    };
    if aligned > h && packed_h >= aligned {
        aligned
    } else {
        h
    }
}

unsafe fn pack_nv12_contiguous(
    sample: &IMFSample,
    layout: &OutputLayout,
    out: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    let buf = sample
        .GetBufferByIndex(0)
        .or_else(|_| sample.ConvertToContiguousBuffer())?;
    if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
        match pack_from_2d(&buf2d, layout, out) {
            Ok(v) => return Ok(v),
            Err(e) => tracing::debug!("NV12 pack Lock2D fallback: {e:#}"),
        }
    }
    pack_from_1d(&buf, layout, out)
}

unsafe fn pack_from_2d(
    buf2d: &IMF2DBuffer,
    layout: &OutputLayout,
    out: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    let mut scan0: *mut u8 = std::ptr::null_mut();
    let mut pitch = 0i32;
    buf2d.Lock2D(&mut scan0, &mut pitch)?;
    let result = (|| {
        if scan0.is_null() || pitch <= 0 {
            bail!("Lock2D null/zero pitch");
        }
        let stride = pitch as usize;
        let contig = buf2d.GetContiguousLength().unwrap_or(0) as usize;
        let uv_rows = uv_plane_rows(layout.frame_h as usize, stride, contig);
        let crop = resolve_crop(layout, stride, uv_rows);
        copy_crop_nv12(scan0, stride, uv_rows, crop, out)
    })();
    let _ = buf2d.Unlock2D();
    result
}

unsafe fn pack_from_1d(
    buf: &IMFMediaBuffer,
    layout: &OutputLayout,
    out: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    let mut max = 0u32;
    let mut cur = 0u32;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur))?;
    let result = (|| {
        if ptr.is_null() {
            bail!("IMFMediaBuffer::Lock null");
        }
        let len = cur as usize;
        let stride = layout
            .default_stride
            .map(|s| s.unsigned_abs() as usize)
            .filter(|s| *s >= layout.frame_w as usize)
            .unwrap_or_else(|| infer_stride(layout.frame_w as usize, layout.frame_h as usize, len));
        let uv_rows = uv_plane_rows(layout.frame_h as usize, stride, len);
        let crop = resolve_crop(layout, stride, uv_rows);
        copy_crop_nv12(ptr, stride, uv_rows, crop, out)
    })();
    let _ = buf.Unlock();
    result
}

unsafe fn copy_crop_nv12(
    scan0: *const u8,
    stride: usize,
    uv_rows: usize,
    crop: Crop,
    out: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    if crop.w < 2 || crop.h < 2 {
        bail!("empty NV12 crop");
    }
    if crop.x + crop.w > stride || crop.y + crop.h > uv_rows {
        bail!(
            "NV12 crop {}x{}+{}+{} outside stride={stride} uv_rows={uv_rows}",
            crop.w,
            crop.h,
            crop.x,
            crop.y
        );
    }
    let need = crop.w * crop.h + crop.w * crop.h / 2;
    if out.try_reserve(need.saturating_sub(out.len())).is_err() {
        out.clear();
        out.shrink_to_fit();
        bail!("NV12 crop alloc {need} failed");
    }
    out.resize(need, 0);
    let uv_off = stride * uv_rows;
    for j in 0..crop.h {
        let src = scan0.add((crop.y + j) * stride + crop.x);
        let dst = out.as_mut_ptr().add(j * crop.w);
        std::ptr::copy_nonoverlapping(src, dst, crop.w);
    }
    let dst_uv = crop.w * crop.h;
    for j in 0..crop.h / 2 {
        let src = scan0.add(uv_off + (crop.y / 2 + j) * stride + (crop.x & !1));
        let dst = out.as_mut_ptr().add(dst_uv + j * crop.w);
        std::ptr::copy_nonoverlapping(src, dst, crop.w);
    }
    Ok((crop.w as u32, crop.h as u32))
}

unsafe fn copy_nv12_cropped(
    sample: &IMFSample,
    layout: &OutputLayout,
    logged: &mut bool,
    max_long: Option<usize>,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let buf = sample
        .GetBufferByIndex(0)
        .or_else(|_| sample.ConvertToContiguousBuffer())?;

    let max_len = buf.GetMaxLength().unwrap_or(0) as usize;
    let cur_len = buf.GetCurrentLength().unwrap_or(0) as usize;

    if let Ok(buf2d) = buf.cast::<IMF2DBuffer>() {
        match copy_from_2d(
            &buf2d,
            layout,
            max_len.max(cur_len),
            logged,
            max_long,
            out,
        ) {
            Ok(v) => return Ok(v),
            Err(e) => tracing::debug!("NV12 Lock2D fallback: {e:#}"),
        }
    }
    copy_from_1d(&buf, layout, logged, max_long, out)
}

unsafe fn copy_from_2d(
    buf2d: &IMF2DBuffer,
    layout: &OutputLayout,
    buf_len: usize,
    logged: &mut bool,
    max_long: Option<usize>,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let mut scan0: *mut u8 = std::ptr::null_mut();
    let mut pitch = 0i32;
    buf2d.Lock2D(&mut scan0, &mut pitch)?;
    let result = (|| {
        if scan0.is_null() || pitch == 0 {
            bail!("Lock2D null/zero pitch");
        }
        if pitch < 0 {
            bail!("bottom-up NV12 pitch {pitch}");
        }
        let stride = pitch as usize;
        let contig = buf2d.GetContiguousLength().unwrap_or(0) as usize;
        let uv_rows = uv_plane_rows(layout.frame_h as usize, stride, contig);
        let crop = resolve_crop(layout, stride, uv_rows);
        if !*logged {
            tracing::warn!(
                "NV12 Lock2D pitch={pitch} uv_rows={uv_rows} contig={contig} buflen={buf_len} frame={}x{} crop={}x{}+{}+{}",
                layout.frame_w,
                layout.frame_h,
                crop.w,
                crop.h,
                crop.x,
                crop.y
            );
            *logged = true;
        }
        rgb_from_nv12_ptr(scan0, stride, uv_rows, crop, max_long, out)
    })();
    let _ = buf2d.Unlock2D();
    result
}

unsafe fn copy_from_1d(
    buf: &IMFMediaBuffer,
    layout: &OutputLayout,
    logged: &mut bool,
    max_long: Option<usize>,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let mut max = 0u32;
    let mut cur = 0u32;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buf.Lock(&mut ptr, Some(&mut max), Some(&mut cur))?;
    let result = (|| {
        if ptr.is_null() {
            bail!("IMFMediaBuffer::Lock null");
        }
        let len = cur as usize;
        let stride = layout
            .default_stride
            .map(|s| s.unsigned_abs() as usize)
            .filter(|s| *s >= layout.frame_w as usize)
            .unwrap_or_else(|| infer_stride(layout.frame_w as usize, layout.frame_h as usize, len));
        let uv_rows = uv_plane_rows(layout.frame_h as usize, stride, len);
        let crop = resolve_crop(layout, stride, uv_rows);
        if !*logged {
            tracing::warn!(
                "NV12 1D len={len} stride={stride} uv_rows={uv_rows} frame={}x{} crop={}x{}+{}+{}",
                layout.frame_w,
                layout.frame_h,
                crop.w,
                crop.h,
                crop.x,
                crop.y
            );
            *logged = true;
        }
        rgb_from_nv12_ptr(ptr, stride, uv_rows, crop, max_long, out)
    })();
    let _ = buf.Unlock();
    result
}

fn scaled_size(w: usize, h: usize, max_long: usize) -> (usize, usize) {
    let long = w.max(h);
    if long <= max_long {
        return (even(w).max(2), even(h).max(2));
    }
    let dw = even(((w * max_long) / long).max(2));
    let dh = even(((h * max_long) / long).max(2));
    (dw.max(2), dh.max(2))
}

fn native_display_size(layout: &OutputLayout, fallback_w: u32, fallback_h: u32) -> (u32, u32) {
    if let Some(sps) = layout.sps_crop {
        return (
            tighten_mb(even(sps.w as usize).max(2)) as u32,
            tighten_mb(even(sps.h as usize).max(2)) as u32,
        );
    }
    if let Some((_, _, aw, ah)) = layout.aperture {
        if aw > 0 && ah > 0 {
            return (
                tighten_mb(aw as usize) as u32,
                tighten_mb(ah as usize) as u32,
            );
        }
    }
    if layout.frame_w > 0 && layout.frame_h > 0 {
        return (
            tighten_mb(layout.frame_w as usize) as u32,
            tighten_mb(layout.frame_h as usize) as u32,
        );
    }
    (fallback_w, fallback_h)
}

unsafe fn rgb_from_nv12_ptr(
    scan0: *const u8,
    stride: usize,
    uv_rows: usize,
    crop: Crop,
    max_long: Option<usize>,
    out: &mut Vec<u32>,
) -> Result<(u32, u32)> {
    let uv_off = stride * uv_rows;
    if crop.x + crop.w > stride || crop.y + crop.h > uv_rows {
        bail!(
            "NV12 crop {}x{}+{}+{} outside stride={stride} uv_rows={uv_rows}",
            crop.w,
            crop.h,
            crop.x,
            crop.y
        );
    }
    let (dw, dh) = match max_long {
        Some(m) if crop.w.max(crop.h) > m => scaled_size(crop.w, crop.h, m),
        _ => (crop.w, crop.h),
    };
    let safe_w = mb_safe(crop.w).max(2);
    let safe_h = mb_safe(crop.h).max(2);
    resize_rgb(out, dw.saturating_mul(dh))?;
    if dw == crop.w && dh == crop.h {
        for j in 0..dh {
            let sj = if j >= safe_h { safe_h - 1 } else { j };
            let y_row = (crop.y + sj) * stride + crop.x;
            let uv_row = uv_off + ((crop.y + sj) / 2) * stride + (crop.x & !1);
            for i in 0..dw {
                let si = if i >= safe_w { safe_w - 1 } else { i };
                let y = *scan0.add(y_row + si) as i32;
                let u = *scan0.add(uv_row + (si & !1)) as i32;
                let v = *scan0.add(uv_row + (si & !1) + 1) as i32;
                out[j * dw + i] = yuv709_full(y, u, v);
            }
        }
    } else {
        let y_last = crop.y + safe_h - 1;
        let x_last = crop.x + safe_w - 1;
        for j in 0..dh {
            let sy = (crop.y + j * crop.h / dh).min(y_last);
            for i in 0..dw {
                let sx = (crop.x + i * crop.w / dw).min(x_last);
                let y = *scan0.add(sy * stride + sx) as i32;
                let uv_row = uv_off + (sy / 2) * stride + (sx & !1);
                let u = *scan0.add(uv_row) as i32;
                let v = *scan0.add(uv_row + 1) as i32;
                out[j * dw + i] = yuv709_full(y, u, v);
            }
        }
    }
    Ok((dw as u32, dh as u32))
}

fn infer_stride(width: usize, height: usize, len: usize) -> usize {
    if height == 0 {
        return width.max(2);
    }
    let denom = height * 3 / 2;
    if denom > 0 && len % denom == 0 {
        let stride = len / denom;
        if stride >= width {
            return stride;
        }
    }
    let aligned_h = align16(height);
    let denom16 = aligned_h * 3 / 2;
    if denom16 > 0 && len % denom16 == 0 {
        let stride = len / denom16;
        if stride >= width {
            return stride;
        }
    }
    width.max(2)
}

#[inline]
fn yuv709_full(y: i32, u: i32, v: i32) -> u32 {
    let u = u - 128;
    let v = v - 128;
    let r = (y + (459 * v) / 256).clamp(0, 255) as u32;
    let g = (y - (55 * u) / 256 - (136 * v) / 256).clamp(0, 255) as u32;
    let b = (y + (541 * u) / 256).clamp(0, 255) as u32;
    (r << 16) | (g << 8) | b
}
