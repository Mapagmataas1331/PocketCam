use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, ColorImage, FontId, Frame, Margin, Pos2, Rect, RichText,
    Sense, Stroke, TextFormat, TextureFilter, TextureHandle, TextureOptions, Vec2, ViewportCommand,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::health::FirewallState;
use crate::host::Host;
use crate::pairing::{display_token, PairPhase};
use crate::preview::{
    fps_avail, preview_decode_too_slow, quality_allowed, quality_by_id, quality_id_for,
    quality_long_edge, size_avail, size_label, Avail, PreviewEncoding, RgbFrame, StreamStats,
    QUALITY_FPS, QUALITY_SIZES,
};
use crate::shared::{HostCmd, ListenState, Shared};
use crate::sys::{preview_host_stress, SysLoad, SysSampler};

const BG: Color32 = Color32::from_rgb(10, 10, 12);
const PANEL: Color32 = Color32::from_rgb(22, 22, 26);
const CARD: Color32 = Color32::from_rgb(32, 32, 38);
const MUTED: Color32 = Color32::from_rgb(156, 163, 175);
const ACCENT: Color32 = Color32::from_rgb(96, 165, 250);
const LIVE: Color32 = Color32::from_rgb(74, 222, 128);
const WARN: Color32 = Color32::from_rgb(251, 191, 36);
const DANGER: Color32 = Color32::from_rgb(248, 113, 113);
const TOAST_BG: Color32 = Color32::from_rgb(22, 22, 28);
const TEXT: Color32 = Color32::from_rgb(244, 244, 245);

#[derive(Clone, Copy)]
enum ToastTone {
    Info,
    Warn,
    Danger,
}

struct Toast {
    at: Instant,
    hold: Duration,
    msg: String,
    tone: ToastTone,
}

fn toast_tone(msg: &str) -> ToastTone {
    let m = msg.to_ascii_lowercase();
    if m.contains("fail")
        || m.contains("error")
        || m.contains("could not")
        || m.contains("ran out")
    {
        ToastTone::Danger
    } else if m.contains("preview off") || m.contains("locked") || m.contains("re-select")
    {
        ToastTone::Warn
    } else {
        ToastTone::Info
    }
}

fn toast_hold(msg: &str) -> Duration {
    let n = msg.len() as u64;
    if n <= 28 {
        Duration::from_millis(4500)
    } else {
        Duration::from_millis((7500 + n.saturating_mul(40)).min(14000))
    }
}

fn toast_color(tone: ToastTone) -> Color32 {
    match tone {
        ToastTone::Info => ACCENT,
        ToastTone::Warn => WARN,
        ToastTone::Danger => DANGER,
    }
}

pub struct PocketCamApp {
    shared: Arc<Shared>,
    cmds: UnboundedSender<HostCmd>,
    host: Arc<Host>,
    allow_exit: bool,
    preview_tex: Option<TextureHandle>,
    qr_tex: Option<TextureHandle>,
    qr_url: String,
    last_seq: u64,
    toasts: Vec<Toast>,
    show_log: bool,
    color_scratch: Vec<Color32>,
    sys: SysSampler,
    heading_tex: Option<TextureHandle>,
    icon_light: Option<bool>,
    win_small: Option<windows::Win32::UI::WindowsAndMessaging::HICON>,
    win_big: Option<windows::Win32::UI::WindowsAndMessaging::HICON>,
    show_settings: bool,
    recordings_edit: String,
    preview_slow_since: Option<Instant>,
    preview_host_since: Option<Instant>,
    show_pair_qr: bool,
}

impl PocketCamApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared: Arc<Shared>,
        cmds: UnboundedSender<HostCmd>,
        host: Arc<Host>,
    ) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL;
        visuals.window_fill = PANEL;
        visuals.override_text_color = Some(Color32::from_rgb(228, 228, 231));
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(39, 39, 46);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(50, 50, 58);
        visuals.selection.bg_fill = Color32::from_rgb(37, 99, 235);
        cc.egui_ctx.set_visuals(visuals);

        Self {
            shared,
            cmds,
            host,
            allow_exit: false,
            preview_tex: None,
            qr_tex: None,
            qr_url: String::new(),
            last_seq: 0,
            toasts: Vec::new(),
            show_log: true,
            color_scratch: Vec::new(),
            sys: SysSampler::new(),
            heading_tex: load_heading_icon(&cc.egui_ctx),
            icon_light: None,
            win_small: None,
            win_big: None,
            show_settings: false,
            recordings_edit: String::new(),
            preview_slow_since: None,
            preview_host_since: None,
            show_pair_qr: false,
        }
    }

    fn bump_notice(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if msg.is_empty() {
            return;
        }
        let tone = toast_tone(&msg);
        let hold = toast_hold(&msg);
        self.toasts.retain(|t| t.msg != msg);
        self.toasts.push(Toast {
            at: Instant::now(),
            hold,
            tone,
            msg,
        });
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    fn draw_toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed() < t.hold);
        if self.toasts.is_empty() {
            return;
        }
        if let Some(t) = self.toasts.first() {
            let left = t.hold.saturating_sub(t.at.elapsed());
            ctx.request_repaint_after(left.min(Duration::from_millis(250)));
        }
        let mut stack = 0.0_f32;
        let mut dismiss = None;
        for (i, toast) in self.toasts.iter().enumerate().rev() {
            let id = egui::Id::new("toast").with(i);
            let inner = egui::Area::new(id)
                .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -20.0 - stack))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    Frame::new()
                        .fill(TOAST_BG)
                        .stroke(Stroke::new(1.0_f32, toast_color(toast.tone)))
                        .inner_margin(Margin::symmetric(14, 11))
                        .corner_radius(10.0)
                        .shadow(egui::Shadow {
                            offset: [0, 4],
                            blur: 16,
                            spread: 0,
                            color: Color32::from_black_alpha(90),
                        })
                        .show(ui, |ui| {
                            ui.set_max_width(400.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&toast.msg)
                                        .size(14.0)
                                        .color(TEXT)
                                        .line_height(Some(20.0)),
                                )
                                .sense(Sense::click()),
                            )
                            .on_hover_text("Click to dismiss")
                            .clicked()
                        })
                        .inner
                });
            if inner.inner {
                dismiss = Some(i);
            }
            stack += inner.response.rect.height() + 8.0;
        }
        if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
    }

    fn drop_preview(&mut self, msg: &str, oom: bool) {
        self.shared.preview.set_encoding(PreviewEncoding::Off);
        if oom {
            self.shared.stats.lock().note_oom();
        }
        self.shared.slot.lock().frame = None;
        self.shared.stats.lock().clear_preview_rgb();
        self.preview_tex = None;
        self.color_scratch.clear();
        self.color_scratch.shrink_to_fit();
        self.preview_slow_since = None;
        self.preview_host_since = None;
        self.bump_notice(msg);
    }

    fn maybe_pause_preview_on_host(&mut self, hidden: bool, sys: &SysLoad) {
        let enc = self.shared.preview.encoding();
        if hidden
            || enc == PreviewEncoding::Off
            || self.shared.preview.keep_rgb()
            || self.shared.preview.plan().skip_always
        {
            self.preview_host_since = None;
            return;
        }
        let decode_ms = self.shared.stats.lock().decode_ms;
        let Some(why) = preview_host_stress(sys, decode_ms) else {
            self.preview_host_since = None;
            return;
        };
        let since = *self.preview_host_since.get_or_insert(Instant::now());
        if since.elapsed() < why.hold() {
            return;
        }
        self.drop_preview(why.toast(), why.is_oom());
    }

    fn maybe_pause_preview_on_slow_fps(&mut self, phase: PairPhase, hidden: bool) {
        let enc = self.shared.preview.encoding();
        if hidden
            || phase != PairPhase::Live
            || enc == PreviewEncoding::Off
            || self.shared.preview.keep_rgb()
            || self.shared.preview.plan().skip_always
        {
            self.preview_slow_since = None;
            return;
        }
        let (fps, last, quality_fps) = {
            let s = self.shared.stats.lock();
            let quality_fps = quality_by_id(&s.selected_quality)
                .map(|q| q.fps)
                .unwrap_or(30);
            (s.fps, s.last_frame, quality_fps)
        };
        let age = last.map(|t| t.elapsed());
        if !preview_decode_too_slow(fps, quality_fps, age) {
            self.preview_slow_since = None;
            return;
        }
        let since = *self.preview_slow_since.get_or_insert(Instant::now());
        if since.elapsed() < Duration::from_millis(900) {
            return;
        }
        self.drop_preview(
            "Preview off — frame rate dropped. Virtual camera and record stay native.",
            false,
        );
    }

    fn sync_os_icons(&mut self) {
        let light = crate::icon::apps_use_light_theme();
        if self.icon_light == Some(light) && self.win_big.is_some() {
            return;
        }
        let Ok(small) = crate::icon::hicon(16, light) else {
            return;
        };
        let Ok(big) = crate::icon::hicon(32, light) else {
            crate::icon::destroy_icon(small);
            return;
        };
        crate::icon::apply_window_icons(light, small, big);
        if let Some(old) = self.win_small.replace(small) {
            crate::icon::destroy_icon(old);
        }
        if let Some(old) = self.win_big.replace(big) {
            crate::icon::destroy_icon(old);
        }
        self.icon_light = Some(light);
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut notice: Option<String> = None;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                let mut next = self.shared.settings.lock().clone();
                let listen = self.shared.listen_port.load(Ordering::Relaxed);
                let mut changed = false;

                settings_section(
                    ui,
                    "Network",
                    "The phone must reach this PC on the same Wi-Fi. Auto skips Docker and VirtualBox adapters.",
                    |ui| {
                        ui.label(RichText::new("Adapter").small().color(MUTED));
                        if let Some(msg) = adapter_combo(ui, &self.shared) {
                            notice = Some(msg);
                            next = self.shared.settings.lock().clone();
                        }
                        ui.add_space(6.0);
                        ui.label(RichText::new("HTTPS port").small().color(MUTED));
                        ui.horizontal(|ui| {
                            let mut port = next.port;
                            if ui
                                .add(egui::DragValue::new(&mut port).range(1..=65535))
                                .changed()
                            {
                                next.port = port;
                                changed = true;
                            }
                            ui.label(
                                RichText::new("QR and the phone page. Applied on next launch.")
                                    .small()
                                    .color(MUTED),
                            );
                        });
                        if next.port != listen {
                            ui.label(
                                RichText::new(format!(
                                    "Still listening on {listen}. Exit to use {}.",
                                    next.port
                                ))
                                .small()
                                .color(WARN),
                            );
                            if ui
                                .add_sized(
                                    [ui.available_width(), 28.0],
                                    egui::Button::new("Exit PocketCam to apply port"),
                                )
                                .clicked()
                            {
                                self.host.request_exit();
                            }
                        }
                    },
                );

                settings_section(
                    ui,
                    "WebRTC",
                    "STUN is optional. Same-LAN setups work with it off. On sends ICE to Google.",
                    |ui| {
                        if ui
                            .checkbox(&mut next.stun, "Use Google STUN (stun.l.google.com)")
                            .changed()
                        {
                            changed = true;
                        }
                        ui.label(
                            RichText::new("Applies the next time a phone connects.")
                                .small()
                                .color(MUTED),
                        );
                    },
                );

                settings_section(
                    ui,
                    "Recordings",
                    "Record muxes the phone’s H.264 into an MP4. No second encode. Portrait↔landscape starts a new file.",
                    |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Default folder: {}",
                                crate::settings::default_recordings_dir().display()
                            ))
                            .small()
                            .color(MUTED),
                        );
                        let rec = ui.text_edit_singleline(&mut self.recordings_edit);
                        ui.horizontal(|ui| {
                            if ui.button("Use default").clicked() {
                                self.recordings_edit.clear();
                                next.recordings = None;
                                changed = true;
                            }
                            if ui.button("Browse").clicked() {
                                if let Some(p) = crate::settings::pick_recordings_folder() {
                                    self.recordings_edit = p.display().to_string();
                                    match crate::settings::validate_recordings_dir(
                                        &self.recordings_edit,
                                    ) {
                                        Ok(v) => {
                                            next.recordings = v;
                                            changed = true;
                                        }
                                        Err(e) => {
                                            notice = Some(format!("Recordings folder: {e:#}"));
                                        }
                                    }
                                }
                            }
                            if ui.button("Open folder").clicked() {
                                match crate::settings::open_in_explorer(&next.recordings_dir())
                                {
                                    Ok(()) => {}
                                    Err(e) => {
                                        notice = Some(format!("Could not open folder: {e:#}"));
                                    }
                                }
                            }
                        });
                        if rec.lost_focus() {
                            match crate::settings::validate_recordings_dir(&self.recordings_edit)
                            {
                                Ok(v) => {
                                    next.recordings = v;
                                    changed = true;
                                }
                                Err(e) => {
                                    notice = Some(format!("Recordings folder: {e:#}"));
                                    self.recordings_edit =
                                        next.recordings.clone().unwrap_or_default();
                                }
                            }
                        }
                    },
                );

                settings_section(
                    ui,
                    "Virtual camera",
                    "Lists PocketCam in OBS, Discord, and Zoom. The canvas follows Quality. After a size change, pick PocketCam again in the other app.",
                    |ui| {
                        if ui
                            .checkbox(
                                &mut next.vcam_on_launch,
                                "Start virtual camera when PocketCam launches",
                            )
                            .changed()
                        {
                            changed = true;
                        }
                    },
                );

                if changed {
                    match self.shared.apply_settings(next) {
                        Ok(msg) if msg == "Settings saved." => {}
                        Ok(msg) => notice = Some(msg),
                        Err(e) => notice = Some(format!("Could not save settings: {e:#}")),
                    }
                }
            });
        self.show_settings = open;
        if let Some(msg) = notice {
            self.bump_notice(msg);
        }
    }
}

fn load_heading_icon(ctx: &egui::Context) -> Option<TextureHandle> {
    let (s, rgba) = crate::icon::heading_rgba().ok()?;
    let n = (s as usize).saturating_mul(s as usize);
    let mut pixels = Vec::with_capacity(n);
    for p in rgba.chunks_exact(4) {
        pixels.push(Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]));
    }
    Some(ctx.load_texture(
        "brand",
        ColorImage {
            size: [s as usize, s as usize],
            pixels,
        },
        TextureOptions::LINEAR,
    ))
}

fn settings_section(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    Frame::new()
        .fill(CARD)
        .inner_margin(Margin::symmetric(12, 10))
        .corner_radius(8.0)
        .show(ui, |ui| {
            ui.label(RichText::new(title).strong().size(14.0));
            ui.label(RichText::new(desc).size(12.0).color(MUTED));
            ui.add_space(6.0);
            add_contents(ui);
        });
    ui.add_space(8.0);
}

impl eframe::App for PocketCamApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.shared.record.is_on() {
            let _ = self.shared.record.stop();
        }
        self.shared.vcam.write_waiting();
        if let Some(i) = self.win_small.take() {
            crate::icon::destroy_icon(i);
        }
        if let Some(i) = self.win_big.take() {
            crate::icon::destroy_icon(i);
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.host.take_exit() || Host::exit_requested() {
            self.allow_exit = true;
            self.shared.preview.set_window_shown(true);
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if self.host.take_theme() || self.icon_light.is_none() {
            self.sync_os_icons();
        }
        if self.host.take_show() {
            self.shared.preview.set_window_shown(true);
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        let hidden_now = !self.shared.preview.window_shown();
        if let Some(msg) = self.host.take_notice() {
            self.bump_notice(msg);
        }
        for msg in self.shared.record.take_notices() {
            if hidden_now {
                self.host.balloon(&msg);
            }
            self.bump_notice(msg);
        }
        for msg in self.shared.vcam.take_notices() {
            if hidden_now {
                self.host.balloon(&msg);
            }
            self.bump_notice(msg);
        }
        self.settings_window(ctx);
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.allow_exit || Host::exit_requested() {
                // Real quit from the tray Exit item.
            } else {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                self.shared.preview.set_window_shown(false);
                self.preview_tex = None;
                self.shared.slot.lock().frame = None;
            }
        }

        let pairing = {
            let p = self.shared.pairing.lock();
            let grace_left = p.grace_until.and_then(|until| {
                let secs = until.saturating_duration_since(Instant::now()).as_secs();
                (secs > 0).then_some(secs)
            });
            (
                p.phase,
                p.url.clone(),
                p.token.clone(),
                p.lan_ip.clone(),
                p.consumed,
                grace_left,
                p.nic_warning.clone(),
            )
        };
        let (phase, url, token, lan_ip, consumed, grace_left, nic_warning) = pairing;
        self.shared.preview.record_on.store(
            self.shared.record.is_on(),
            Ordering::Relaxed,
        );
        self.shared.preview.vcam_on.store(
            self.shared.vcam.is_on(),
            Ordering::Relaxed,
        );
        let qid = self.shared.stats.lock().selected_quality.clone();
        self.shared.preview.set_camera_quality(&qid);
        let hidden = !self.shared.preview.window_shown();
        let preview_off = self.shared.preview.rgb_off();
        let heavy_preview = matches!(
            self.shared.preview.encoding(),
            PreviewEncoding::P1080_30
                | PreviewEncoding::P1080_60
                | PreviewEncoding::P1440_30
                | PreviewEncoding::P1440_60
                | PreviewEncoding::P2160_30
                | PreviewEncoding::P2160_60
                | PreviewEncoding::Native
        );

        ctx.request_repaint_after(Duration::from_millis(if hidden {
            200
        } else {
            match phase {
                PairPhase::Live if preview_off => 200,
                PairPhase::Live if heavy_preview => 33,
                PairPhase::Live => 16,
                PairPhase::Connecting => 50,
                PairPhase::Waiting => 200,
            }
        }));

        let listen = {
            let g = self.shared.listen.lock();
            match &*g {
                ListenState::Starting => ListenState::Starting,
                ListenState::Ok => ListenState::Ok,
                ListenState::Failed(m) => ListenState::Failed(m.clone()),
            }
        };
        let bind_failed = matches!(listen, ListenState::Failed(_));
        if url.is_empty() || bind_failed {
            self.qr_tex = None;
            self.qr_url.clear();
        } else if self.qr_url != url {
            if let Ok(qr) = crate::qr::render(&url) {
                let img = egui::ColorImage::from_rgba_unmultiplied([qr.size, qr.size], &qr.rgba);
                self.qr_tex = Some(ctx.load_texture(
                    "qr",
                    img,
                    TextureOptions {
                        magnification: TextureFilter::Nearest,
                        minification: TextureFilter::Nearest,
                        ..Default::default()
                    },
                ));
                self.qr_url = url.clone();
            }
        }

        if preview_off {
            self.preview_tex = None;
        } else if let Some(frame) = take_frame(&self.shared, &mut self.last_seq) {
            match try_preview_image(&frame, &mut self.color_scratch) {
                Some(img) => {
                    let size = img.size;
                    let reuse = self
                        .preview_tex
                        .as_ref()
                        .is_some_and(|tex| tex.size() == size);
                    if reuse {
                        if let Some(tex) = self.preview_tex.as_mut() {
                            tex.set(img, TextureOptions::LINEAR);
                        }
                    } else {
                        self.preview_tex =
                            Some(ctx.load_texture("preview", img, TextureOptions::LINEAR));
                    }
                }
                None => {
                    let next = drop_preview_on_oom(&self.shared);
                    self.preview_tex = None;
                    self.color_scratch.clear();
                    self.color_scratch.shrink_to_fit();
                    self.bump_notice(format!(
                        "Preview ran out of memory. Switched to {}.",
                        next.label()
                    ));
                }
            }
        }

        let sys = self.sys.sample();
        self.maybe_pause_preview_on_host(hidden, &sys);
        self.maybe_pause_preview_on_slow_fps(phase, hidden);

        let src_long = quality_long_edge(&self.shared.stats.lock().selected_quality);

        egui::TopBottomPanel::top("top")
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(16, 10))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(39, 39, 46))),
            )
            .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                    if let Some(tex) = &self.heading_tex {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(Vec2::splat(22.0)),
                        );
                    }
                    ui.heading("PocketCam");
                    ui.add_space(8.0);
                    if ui
                        .add_sized([80.0, 26.0], egui::Button::new("Settings"))
                        .clicked()
                    {
                        self.recordings_edit = self
                            .shared
                            .settings
                            .lock()
                            .recordings
                            .clone()
                            .unwrap_or_default();
                        self.show_settings = true;
                    }
                    ui.add_space(8.0);
                    phase_chip(ui, phase, consumed, grace_left.is_some());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let vcam_on = self.shared.vcam.is_on();
                        let vcam_label = if vcam_on {
                            "Stop virtual camera"
                        } else {
                            "Start virtual camera"
                        };
                        if ui
                            .add_sized([158.0, 28.0], egui::Button::new(vcam_label))
                            .on_hover_text(
                                "Lists PocketCam in Windows. Uses the Quality you picked (same as the phone and recordings). OBS may need to re-select after a size change.",
                            )
                            .clicked()
                        {
                            self.host.request_toggle_vcam();
                        }
                        let recording = self.shared.record.is_on();
                        let rec = if recording {
                            "Stop record"
                        } else {
                            "Record"
                        };
                        let rec_hover = if recording {
                            "Stop and write the MP4 trailer. Native phone H.264 — same Quality as the virtual camera.".to_string()
                        } else if let Some(p) = self.shared.record.last_path() {
                            format!(
                                "Muxes phone H.264 into {}. Starts on the next keyframe. Last file: {}",
                                self.shared.settings.lock().recordings_dir().display(),
                                p.display()
                            )
                        } else {
                            format!(
                                "Muxes the phone H.264 into {}. Same Quality as the phone and virtual camera. Starts on the next keyframe.",
                                self.shared.settings.lock().recordings_dir().display()
                            )
                        };
                        if ui
                            .add_sized([100.0, 28.0], egui::Button::new(rec))
                            .on_hover_text(rec_hover)
                            .clicked()
                        {
                            self.host.request_toggle_record();
                        }
                        let enc = self.shared.preview.encoding();
                        let preview_label = if enc == PreviewEncoding::Auto
                            && self.shared.preview.rgb_off()
                            && !hidden
                        {
                            "Preview Auto · off".to_string()
                        } else {
                            format!("Preview {}", enc.label())
                        };
                        let mut keep = self.shared.preview.keep_rgb();
                        if ui
                            .checkbox(&mut keep, "")
                            .on_hover_text(
                                "Keep RGB preview on. PocketCam will not auto-off Preview when the machine is busy.",
                            )
                            .changed()
                        {
                            self.shared.preview.set_keep_rgb(keep);
                            self.preview_slow_since = None;
                            self.preview_host_since = None;
                            let mut next = self.shared.settings.lock().clone();
                            next.keep_preview = keep;
                            match self.shared.apply_settings(next) {
                                Ok(_) => {}
                                Err(e) => self.bump_notice(format!("Could not save: {e:#}")),
                            }
                        }
                        egui::ComboBox::from_id_salt("preview-enc")
                            .selected_text(preview_label)
                            .width(160.0)
                            .show_ui(ui, |ui| {
                                for opt in PreviewEncoding::ALL {
                                    let ok = opt.fits(src_long);
                                    ui.add_enabled_ui(ok, |ui| {
                                        if ui.selectable_label(opt == enc, opt.label()).clicked()
                                            && ok
                                        {
                                            self.shared.preview.set_encoding(opt);
                                            self.shared.stats.lock().clear_oom();
                                            self.preview_slow_since = None;
                                            self.preview_host_since = None;
                                        }
                                    });
                                }
                            })
                            .response
                            .on_hover_text(
                                "RGB preview only. Modes larger than the Quality you picked are disabled. Off skips RGB. Virtual camera and record follow Quality, not this.",
                            );
                    });
                });
            });

        let preview_off = self.shared.preview.rgb_off();
        if preview_off {
            self.preview_tex = None;
        }

        egui::SidePanel::right("pair")
            .resizable(false)
            .exact_width(276.0)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(39, 39, 46))),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("pair-scroll")
                    .auto_shrink([false, true])
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                    )
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                        ui.spacing_mut().button_padding = Vec2::new(8.0, 4.0);

                        ui.label(RichText::new("Connect").strong().size(13.0));
                        match &listen {
                            ListenState::Starting => {
                                ui.label(
                                    RichText::new("Starting HTTPS…")
                                        .small()
                                        .color(MUTED),
                                );
                            }
                            ListenState::Failed(msg) => {
                                ui.label(RichText::new(msg).small().color(DANGER));
                            }
                            ListenState::Ok => {}
                        }
                        if let Some(w) = &nic_warning {
                            ui.label(RichText::new(w).small().color(WARN));
                        }

                        ui.label(RichText::new("Network").size(11.0).color(MUTED));
                        if let Some(msg) = adapter_combo(ui, &self.shared) {
                            self.bump_notice(msg);
                        }
                        ui.label(
                            RichText::new("The phone must reach this IPv4.")
                                .size(11.5)
                                .color(MUTED),
                        );

                        let waiting = phase != PairPhase::Live;
                        if waiting {
                            ui.label(
                                RichText::new("QR is on the waiting screen.")
                                    .size(11.5)
                                    .color(MUTED),
                            );
                        }

                        let (token_note, token_note_color) = if consumed && phase == PairPhase::Live
                        {
                            ("In use by the phone".to_string(), WARN)
                        } else if consumed {
                            ("Used — New session for another phone".to_string(), WARN)
                        } else if let Some(secs) = grace_left {
                            let mins = (secs + 59) / 60;
                            (format!("Phone left. Same token ~{mins} min."), WARN)
                        } else if waiting {
                            (String::new(), MUTED)
                        } else {
                            ("Type this if the QR did not open".to_string(), MUTED)
                        };

                        let qr_title = if self.show_pair_qr {
                            "Hide QR"
                        } else {
                            "Show QR"
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), 26.0],
                                egui::Button::new(qr_title),
                            )
                            .clicked()
                        {
                            self.show_pair_qr = !self.show_pair_qr;
                        }
                        if self.show_pair_qr {
                                if let Some(tex) = &self.qr_tex {
                                    let side = 128.0_f32;
                                    ui.vertical_centered(|ui| {
                                        Frame::new()
                                            .fill(Color32::WHITE)
                                            .inner_margin(6.0)
                                            .corner_radius(6.0)
                                            .show(ui, |ui| {
                                                ui.image((tex.id(), Vec2::splat(side)));
                                            });
                                    });
                                } else if url.is_empty() || bind_failed {
                                    ui.label(
                                        RichText::new(if bind_failed {
                                            "No QR until HTTPS is listening."
                                        } else {
                                            "No QR until there is a LAN IPv4."
                                        })
                                        .small()
                                        .color(WARN),
                                    );
                                }

                                Frame::new()
                                    .fill(CARD)
                                    .inner_margin(Margin::symmetric(10, 8))
                                    .corner_radius(8.0)
                                    .show(ui, |ui| {
                                        ui.vertical_centered(|ui| {
                                            ui.label(
                                                RichText::new("Token").size(11.0).color(MUTED),
                                            );
                                            ui.label(
                                                RichText::new(display_token(&token))
                                                    .monospace()
                                                    .size(18.0)
                                                    .strong()
                                                    .color(Color32::WHITE),
                                            );
                                        });
                                        ui.add_space(4.0);
                                        ui.columns(2, |cols| {
                                            let w0 = cols[0].available_width();
                                            if cols[0]
                                                .add_sized(
                                                    [w0, 24.0],
                                                    egui::Button::new("Copy token"),
                                                )
                                                .clicked()
                                            {
                                                cols[0].ctx().copy_text(token.clone());
                                                self.bump_notice("Token copied");
                                            }
                                            let w1 = cols[1].available_width();
                                            if cols[1]
                                                .add_sized(
                                                    [w1, 24.0],
                                                    egui::Button::new("Copy URL"),
                                                )
                                                .clicked()
                                            {
                                                cols[1].ctx().copy_text(url.clone());
                                                self.bump_notice("URL copied");
                                            }
                                        });
                                    });
                        }

                        if !token_note.is_empty() {
                            ui.label(
                                RichText::new(token_note).size(12.0).color(token_note_color),
                            );
                        }

                        if ui
                            .add_sized(
                                [ui.available_width(), 26.0],
                                egui::Button::new("New session"),
                            )
                            .clicked()
                        {
                            let _ = self.cmds.send(HostCmd::NewSession);
                            self.last_seq = 0;
                            self.preview_tex = None;
                            if !waiting {
                                self.show_pair_qr = true;
                            }
                            self.bump_notice("New token — previous phone is disconnected.");
                        }

                        ui.separator();
                        ui.label(RichText::new("Phone").strong().size(13.0));
                        ui.label(RichText::new("Camera").size(11.0).color(MUTED));
                        camera_combo(ui, &self.shared, &self.cmds);
                        let gap = ui.spacing().item_spacing.x;
                        let half = ((ui.available_width() - gap) * 0.5).max(40.0);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.set_min_width(half);
                                ui.set_max_width(half);
                                ui.label(RichText::new("Resolution").size(11.0).color(MUTED));
                                if let Some(msg) =
                                    resolution_combo(ui, &self.shared, &self.cmds)
                                {
                                    self.bump_notice(msg);
                                }
                            });
                            ui.vertical(|ui| {
                                ui.set_min_width(half);
                                ui.set_max_width(half);
                                ui.label(RichText::new("FPS").size(11.0).color(MUTED));
                                if let Some(msg) = fps_combo(ui, &self.shared, &self.cmds) {
                                    self.bump_notice(msg);
                                }
                            });
                        });
                        ui.label(
                            RichText::new(quality_hint(&self.shared))
                                .size(11.5)
                                .color(MUTED),
                        );

                        ui.separator();
                        ui.checkbox(&mut self.show_log, "Stats overlay");
                        ui.label(
                            RichText::new(preview_hint(&self.shared))
                                .size(11.5)
                                .color(MUTED),
                        );

                        ui.separator();
                        egui::CollapsingHeader::new("Phone page help")
                            .default_open(false)
                            .show(ui, |ui| {
                                if !url.is_empty() {
                                    ui.label(RichText::new(&url).size(11.0).color(MUTED));
                                }
                                if !lan_ip.is_empty() {
                                    ui.label(
                                        RichText::new(format!(
                                            "LAN  {lan_ip}:{}",
                                            self.shared.listen_port.load(Ordering::Relaxed)
                                        ))
                                        .size(11.5)
                                        .color(MUTED),
                                    );
                                    let sans = self.shared.cert_names.lock();
                                    if !sans.is_empty() && !sans.iter().any(|n| n == &lan_ip) {
                                        ui.label(
                                            RichText::new(
                                                "This IP is not on the certificate. Exit and reopen PocketCam.",
                                            )
                                            .size(11.5)
                                            .color(WARN),
                                        );
                                    }
                                }
                                let saved_port = self.shared.settings.lock().port;
                                let listen_port = self.shared.listen_port.load(Ordering::Relaxed);
                                if saved_port != listen_port {
                                    ui.label(
                                        RichText::new(format!(
                                            "Listening on {listen_port}. Exit to use {saved_port}."
                                        ))
                                        .size(11.5)
                                        .color(WARN),
                                    );
                                }
                                for line in [
                                    "Safari: Show Details, then visit this website",
                                    "Chrome: Advanced, then Proceed (unsafe)",
                                    "Android Chrome: Advanced, then Proceed to the hostname",
                                    "Allow camera. Leave this page open.",
                                ] {
                                    ui.label(RichText::new(line).size(11.5).color(MUTED));
                                }
                                if *self.shared.firewall.lock() != FirewallState::Allowed {
                                    ui.label(
                                        RichText::new(format!(
                                            "Allow PocketCam in Windows Firewall (inbound TCP {listen_port}, Private)."
                                        ))
                                        .size(11.5)
                                        .color(WARN),
                                    );
                                }
                            });
                    });
            });

        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(0.0))
            .show(ctx, |ui| {
                let avail = ui.available_rect_before_wrap();
                let (rect, _) = ui.allocate_exact_size(avail.size(), Sense::hover());
                ui.painter().rect_filled(rect, 0.0, BG);

                if phase != PairPhase::Live {
                    waiting_overlay(
                        ui,
                        rect,
                        &token,
                        self.qr_tex.as_ref(),
                        bind_failed,
                        url.is_empty(),
                        {
                            let sans = self.shared.cert_names.lock();
                            !lan_ip.is_empty()
                                && !sans.is_empty()
                                && !sans.iter().any(|n| n == &lan_ip)
                        },
                    );
                } else if preview_off {
                    preview_off_overlay(
                        ui,
                        rect,
                        true,
                        self.shared.vcam.is_on(),
                        self.shared.record.is_on(),
                    );
                } else if let Some(tex) = &self.preview_tex {
                    let size = tex.size_vec2();
                    if size.x > 0.0 && size.y > 0.0 {
                        let dest = contain(size, rect);
                        // Half-texel inset so LINEAR filtering does not sample
                        // past the last texel (shows up as a 1px green line).
                        let du = 0.5 / size.x.max(1.0);
                        let dv = 0.5 / size.y.max(1.0);
                        ui.painter().image(
                            tex.id(),
                            dest,
                            Rect::from_min_max(
                                Pos2::new(du, dv),
                                Pos2::new(1.0 - du, 1.0 - dv),
                            ),
                            Color32::WHITE,
                        );
                    }
                }

                if self.show_log {
                    let job = debug_job(&self.shared, phase, sys);
                    let galley = ui.fonts(|f| f.layout_job(job));
                    let pad = Vec2::new(10.0, 7.0);
                    let text_pos = rect.left_top() + Vec2::new(12.0, 12.0);
                    let bg = Rect::from_min_size(text_pos - pad * 0.5, galley.size() + pad);
                    ui.painter()
                        .rect_filled(bg, 4.0, Color32::from_black_alpha(170));
                    ui.painter().galley(text_pos, galley, Color32::WHITE);
                }
            });
        self.draw_toasts(ctx);
    }
}

fn waiting_overlay(
    ui: &egui::Ui,
    rect: Rect,
    token: &str,
    qr: Option<&TextureHandle>,
    bind_failed: bool,
    no_url: bool,
    cert_stale: bool,
) {
    let c = rect.center();
    let qr_side = (rect.height() * 0.42)
        .clamp(148.0, 240.0)
        .min(rect.width() - 72.0);
    let pad = 10.0_f32;
    let qr_center = Pos2::new(c.x, c.y - 18.0);
    let qr_rect = Rect::from_center_size(qr_center, Vec2::splat(qr_side));

    ui.painter().text(
        Pos2::new(c.x, qr_rect.top() - pad - 28.0),
        Align2::CENTER_BOTTOM,
        "Waiting for phone",
        FontId::proportional(22.0),
        MUTED,
    );

    if let Some(tex) = qr {
        ui.painter().rect_filled(qr_rect.expand(pad), 10.0, Color32::WHITE);
        ui.painter().image(
            tex.id(),
            qr_rect,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter().rect_filled(qr_rect.expand(pad), 10.0, CARD);
        let msg = if bind_failed {
            "No QR until HTTPS is listening"
        } else if no_url {
            "No QR until there is a LAN IPv4"
        } else {
            "QR is generating…"
        };
        ui.painter().text(
            qr_center,
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(14.0),
            WARN,
        );
    }

    ui.painter().text(
        Pos2::new(c.x, qr_rect.bottom() + pad + 18.0),
        Align2::CENTER_TOP,
        display_token(token),
        FontId::monospace(28.0),
        Color32::WHITE,
    );
    ui.painter().text(
        Pos2::new(c.x, qr_rect.bottom() + pad + 72.0),
        Align2::CENTER_TOP,
        if cert_stale {
            "This IP is not on the certificate. Exit and reopen PocketCam."
        } else {
            "Scan this QR, or type the token on the phone"
        },
        FontId::proportional(14.0),
        if cert_stale { WARN } else { MUTED },
    );
}

fn preview_off_overlay(ui: &egui::Ui, rect: Rect, live: bool, vcam: bool, record: bool) {
    let c = rect.center();
    ui.painter().rect_filled(rect, 0.0, Color32::from_black_alpha(120));
    ui.painter().text(
        c - Vec2::new(0.0, 12.0),
        Align2::CENTER_CENTER,
        "Preview off",
        FontId::proportional(22.0),
        Color32::WHITE,
    );
    let sub = if live && vcam {
        "Decode still feeds the virtual camera"
    } else if live && record {
        "Recording native H.264 — decode is off"
    } else if live {
        "Turn preview on in the toolbar to see the phone"
    } else {
        "Waiting for phone — preview stays off until you turn it on"
    };
    ui.painter().text(
        c + Vec2::new(0.0, 16.0),
        Align2::CENTER_CENTER,
        sub,
        FontId::proportional(14.0),
        MUTED,
    );
}

fn take_frame(shared: &Shared, last_seq: &mut u64) -> Option<RgbFrame> {
    let mut g = shared.slot.lock();
    let seq = g.frame.as_ref().map(|f| f.seq)?;
    if seq == *last_seq {
        return None;
    }
    let frame = g.frame.take()?;
    *last_seq = frame.seq;
    Some(frame)
}

fn try_preview_image(frame: &RgbFrame, scratch: &mut Vec<Color32>) -> Option<egui::ColorImage> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let n = w.saturating_mul(h);
    if n == 0 || n != frame.pixels.len() {
        return None;
    }
    scratch.clear();
    if scratch.try_reserve(n).is_err() {
        scratch.shrink_to_fit();
        return None;
    }
    for p in &frame.pixels {
        scratch.push(Color32::from_rgb(
            ((p >> 16) & 0xff) as u8,
            ((p >> 8) & 0xff) as u8,
            (p & 0xff) as u8,
        ));
    }
    Some(egui::ColorImage {
        size: [w, h],
        pixels: std::mem::take(scratch),
    })
}

fn drop_preview_on_oom(shared: &Shared) -> PreviewEncoding {
    shared.preview.set_encoding(PreviewEncoding::Off);
    {
        let mut s = shared.stats.lock();
        s.note_oom();
        s.clear_preview_rgb();
    }
    PreviewEncoding::Off
}

fn contain(src: Vec2, dst: Rect) -> Rect {
    let scale = (dst.width() / src.x).min(dst.height() / src.y);
    Rect::from_center_size(dst.center(), src * scale)
}

fn phase_chip(ui: &mut egui::Ui, phase: PairPhase, consumed: bool, grace: bool) {
    let (label, color) = if consumed && phase == PairPhase::Waiting {
        ("Token used — new session", DANGER)
    } else if grace && phase == PairPhase::Waiting {
        ("Waiting — same token", WARN)
    } else {
        match phase {
            PairPhase::Waiting => ("Waiting for phone", ACCENT),
            PairPhase::Connecting => ("Connecting", WARN),
            PairPhase::Live => ("Live", LIVE),
        }
    };
    ui.colored_label(color, label);
}

fn preview_hint(shared: &Shared) -> String {
    let enc = shared.preview.encoding();
    let plan = shared.preview.plan();
    if plan.skip_always {
        if shared.preview.needs_nv12() {
            "Preview off. Decode still feeds the virtual camera.".into()
        } else if shared.record.is_on() {
            "Preview off. Recording native H.264 — decode is off.".into()
        } else {
            "Preview off. Decode is off.".into()
        }
    } else if shared.preview.keep_rgb() {
        "Keep is on — RGB preview will not auto-off.".into()
    } else if enc == PreviewEncoding::Auto && shared.preview.loaded() {
        "Auto: RGB off while virtual camera or record is on.".into()
    } else if enc == PreviewEncoding::Auto {
        "Auto: 720p 30. GPU contain-fit. Phone stream stays native.".into()
    } else if plan.native {
        if let Some(fps) = plan.max_fps {
            format!("Native Rec.709, capped at {fps:.0} fps.")
        } else {
            "Native Rec.709, every decoded frame.".into()
        }
    } else {
        let fps = plan
            .max_fps
            .map(|f| format!("{f:.0} fps"))
            .unwrap_or_else(|| "source fps".into());
        format!(
            "Preview {} (long edge ≤ {}), {}. GPU contain-fit.",
            enc.label(),
            plan.max_long,
            fps
        )
    }
}

fn quality_hint(shared: &Shared) -> &'static str {
    let rec = shared.record.is_on();
    let vcam = shared.vcam.is_on();
    let s = shared.stats.lock();
    if rec {
        "Recording: size is locked. You can only drop fps."
    } else if vcam {
        "Virtual camera is on: size is locked (same ring). You can still drop fps."
    } else if s.selected_quality.contains("2160") {
        "4K needs strong Wi-Fi and runs the phone hot. Drop back if it stutters."
    } else if s.selected_quality.contains("1440") {
        "1440p uses more Wi-Fi than 1080p. Drop back if it stutters."
    } else if s.selected_quality.contains("60") {
        "60 fps uses more Wi-Fi and battery. Drop back if it stutters."
    } else {
        "Phone, recording, and virtual camera share this. Preview is separate."
    }
}

fn combo_width(ui: &egui::Ui) -> f32 {
    (ui.available_width() - ui.spacing().button_padding.x).max(24.0)
}

fn adapter_combo(ui: &mut egui::Ui, shared: &Shared) -> Option<String> {
    let nics = crate::settings::list_adapters();
    let mut next = shared.settings.lock().clone();
    let selected = next.adapter.clone();
    let label = match selected.as_deref() {
        Some(name) => nics
            .iter()
            .find(|n| n.name == name)
            .map(|n| format!("{}  {}", n.name, n.ip))
            .unwrap_or_else(|| format!("{name} (not found)")),
        None => match next.resolve_lan() {
            Some(ip) => format!("Auto  {ip}"),
            None => "Auto  (no LAN IPv4)".into(),
        },
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("nic")
        .selected_text(label)
        .width(combo_width(ui))
        .truncate()
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(selected.is_none(), "Auto")
                .clicked()
            {
                next.adapter = None;
                changed = true;
            }
            for nic in &nics {
                let row = format!("{}  {}", nic.name, nic.ip);
                if ui
                    .selectable_label(selected.as_deref() == Some(nic.name.as_str()), row)
                    .clicked()
                {
                    next.adapter = Some(nic.name.clone());
                    changed = true;
                }
            }
        });
    if !changed {
        return None;
    }
    match shared.apply_settings(next) {
        Ok(msg) if msg == "Settings saved." => Some("QR updated for this adapter.".into()),
        Ok(msg) => Some(msg),
        Err(e) => Some(format!("Could not save adapter: {e:#}")),
    }
}

fn quality_items(shared: &Shared) -> (Vec<crate::preview::CameraItem>, String) {
    let s = shared.stats.lock();
    let qualities = if s.qualities.is_empty() {
        crate::preview::quality_catalog(Avail::Unknown)
    } else {
        s.qualities.clone()
    };
    (qualities, s.selected_quality.clone())
}

fn avail_color(a: Avail) -> Color32 {
    match a {
        Avail::Yes => Color32::from_rgb(114, 168, 126),
        Avail::Unknown => Color32::from_rgb(196, 168, 92),
        Avail::No => Color32::from_rgb(196, 118, 118),
    }
}

fn avail_label(text: impl Into<String>, a: Avail) -> RichText {
    RichText::new(text.into()).color(avail_color(a))
}

fn pick_quality(height: u32, fps: u32) -> String {
    let id = quality_id_for(height, fps);
    if quality_by_id(&id).is_some() {
        return id;
    }
    quality_id_for(height, 30)
}

fn resolution_combo(
    ui: &mut egui::Ui,
    shared: &Shared,
    cmds: &UnboundedSender<HostCmd>,
) -> Option<String> {
    let rec = shared.record.is_on();
    let vcam = shared.vcam.is_on();
    let current_spec = shared.preview.camera_quality();
    let (qualities, selected) = quality_items(shared);
    let spec = quality_by_id(&selected);
    let height = spec.map(|s| s.height).unwrap_or(1080);
    let fps = spec.map(|s| s.fps).unwrap_or(30);
    let avail = size_avail(&qualities, height);
    let mut notice = None;
    egui::ComboBox::from_id_salt("quality-res")
        .selected_text(avail_label(size_label(height), avail))
        .width(combo_width(ui))
        .truncate()
        .show_ui(ui, |ui| {
            for &(_w, h) in QUALITY_SIZES {
                let id = pick_quality(h, fps);
                let next = quality_by_id(&id);
                let locked_ok = next
                    .map(|n| quality_allowed(current_spec, n, vcam, rec))
                    .unwrap_or(true);
                let mut row_avail = size_avail(&qualities, h);
                if !locked_ok {
                    row_avail = Avail::No;
                }
                let rec_mark = if h == 1080 { " — recommended" } else { "" };
                let row = avail_label(format!("{}{rec_mark}", size_label(h)), row_avail);
                if ui.selectable_label(h == height, row).clicked() && h != height {
                    if !locked_ok {
                        notice = Some(if rec {
                            "Recording: size is locked. You can only drop fps.".into()
                        } else {
                            "Virtual camera is on: size is locked.".into()
                        });
                    } else if let Some(n) = next {
                        let _ = cmds.send(HostCmd::SelectQuality(n.id.to_string()));
                    }
                }
            }
        })
        .response
        .on_hover_text(
            "Phone encode size. Green: this camera. Yellow: not tested yet. Red: not available.",
        );
    notice
}

fn fps_combo(
    ui: &mut egui::Ui,
    shared: &Shared,
    cmds: &UnboundedSender<HostCmd>,
) -> Option<String> {
    let rec = shared.record.is_on();
    let vcam = shared.vcam.is_on();
    let current_spec = shared.preview.camera_quality();
    let (qualities, selected) = quality_items(shared);
    let spec = quality_by_id(&selected);
    let height = spec.map(|s| s.height).unwrap_or(1080);
    let fps = spec.map(|s| s.fps).unwrap_or(30);
    let avail = fps_avail(&qualities, height, fps);
    let mut notice = None;
    egui::ComboBox::from_id_salt("quality-fps")
        .selected_text(avail_label(format!("{fps}"), avail))
        .width(combo_width(ui))
        .truncate()
        .show_ui(ui, |ui| {
            for &f in QUALITY_FPS {
                let id = pick_quality(height, f);
                let next = quality_by_id(&id);
                let locked_ok = next
                    .map(|n| quality_allowed(current_spec, n, vcam, rec))
                    .unwrap_or(true);
                let mut row_avail = fps_avail(&qualities, height, f);
                if !locked_ok {
                    row_avail = Avail::No;
                }
                let row = avail_label(format!("{f}"), row_avail);
                if ui.selectable_label(f == fps, row).clicked() && f != fps {
                    if !locked_ok {
                        notice = Some("Recording: cannot raise fps while a file is open.".into());
                    } else if let Some(n) = next {
                        let _ = cmds.send(HostCmd::SelectQuality(n.id.to_string()));
                    }
                }
            }
        })
        .response
        .on_hover_text(
            "Phone encode frame rate. Green: this camera. Yellow: not tested yet. Red: not available.",
        );
    notice
}

fn camera_combo(ui: &mut egui::Ui, shared: &Shared, cmds: &UnboundedSender<HostCmd>) {
    let (cameras, selected) = {
        let s = shared.stats.lock();
        (s.cameras.clone(), s.selected_camera.clone())
    };
    let current = cameras
        .iter()
        .find(|c| c.id == selected)
        .map(|c| c.label.clone())
        .unwrap_or_else(|| {
            if cameras.is_empty() {
                "No cameras yet".into()
            } else {
                "Select…".into()
            }
        });
    let w = combo_width(ui);
    ui.scope(|ui| {
        ui.set_max_width(w);
        egui::ComboBox::from_id_salt("camera")
            .selected_text(current)
            .width(w)
            .truncate()
            .show_ui(ui, |ui| {
                for cam in &cameras {
                    if ui
                        .selectable_label(cam.id == selected, &cam.label)
                        .clicked()
                    {
                        let _ = cmds.send(HostCmd::SelectCamera(cam.id.clone()));
                    }
                }
            });
    });
}

fn debug_job(shared: &Shared, phase: PairPhase, sys: SysLoad) -> LayoutJob {
    let mut s = shared.stats.lock();
    s.stalled = s.decoded > 0
        && s.last_frame
            .map(|t| t.elapsed() > Duration::from_secs(2))
            .unwrap_or(false);

    let font = FontId::monospace(11.5);
    let mut job = LayoutJob::default();
    job.wrap.max_width = 1100.0;

    let live = phase == PairPhase::Live;
    let phase_s = match phase {
        PairPhase::Waiting => "wait",
        PairPhase::Connecting => "ice",
        PairPhase::Live => "live",
    };
    let phase_c = if phase == PairPhase::Connecting {
        WARN
    } else {
        Color32::WHITE
    };
    append(&mut job, phase_s, phase_c, &font);
    let want = quality_by_id(&s.selected_quality);
    let live_long = s.width.max(s.height);
    let res_c = if live && s.width >= 2 {
        if let Some(q) = want {
            let want_long = q.width.max(q.height).max(1);
            let ratio = live_long as f32 / want_long as f32;
            if ratio >= 0.90 {
                Color32::WHITE
            } else if ratio >= 0.58 {
                WARN
            } else {
                DANGER
            }
        } else {
            Color32::WHITE
        }
    } else {
        Color32::WHITE
    };
    append(
        &mut job,
        &format!("  {}×{}", s.width.max(1), s.height.max(1)),
        res_c,
        &font,
    );

    let fps_c = if live && s.decoded > 0 {
        if s.stalled || s.fps < 8.0 {
            DANGER
        } else if s.fps < 20.0 {
            WARN
        } else {
            Color32::WHITE
        }
    } else {
        Color32::WHITE
    };
    append(&mut job, &format!("  {:>4.0} fps", s.fps), fps_c, &font);

    let rgb_on = !shared.preview.rgb_off();
    if rgb_on && s.preview_w > 0 && (s.preview_w != s.width || s.preview_h != s.height) {
        append(
            &mut job,
            &format!("  prev {}×{}", s.preview_w, s.preview_h),
            Color32::WHITE,
            &font,
        );
    }

    let enc = shared.preview.encoding();
    let enc_label = if rgb_on {
        enc.label().to_string()
    } else if enc == PreviewEncoding::Off {
        "Off".into()
    } else {
        format!("{}·off", enc.label())
    };
    append(
        &mut job,
        &format!("  enc {enc_label}"),
        Color32::WHITE,
        &font,
    );
    let cam = shared.preview.camera_quality();
    let cam_c = if shared.vcam.is_on() {
        LIVE
    } else {
        Color32::WHITE
    };
    append(
        &mut job,
        &format!("  cam {}×{}@{}", cam.width, cam.height, cam.fps),
        cam_c,
        &font,
    );
    if rgb_on && s.preview_w > 0 && s.preview_h > 0 {
        let src_long = s.width.max(s.height);
        let path = if enc == PreviewEncoding::Native && src_long > 0 && src_long <= 1920 {
            "rec709"
        } else {
            "nv12"
        };
        append(&mut job, &format!("  rgb {path}"), Color32::WHITE, &font);
        let rgb_fps = shared
            .preview
            .plan()
            .max_fps
            .unwrap_or(s.fps)
            .min(s.fps.max(0.0));
        let rgb_mbs = s.preview_w as f32 * s.preview_h as f32 * 4.0 * rgb_fps / 1e6;
        if rgb_mbs >= 1.0 {
            // 1080p60 RGB32 is ~498 MB/s — that is the pixel math, not a GPU alarm.
            append(
                &mut job,
                &format!(" {rgb_mbs:.0} MB/s"),
                level(rgb_mbs, 800.0, 1200.0),
                &font,
            );
        }
    }

    if s.loss_pct >= 0.05 {
        let c = if s.loss_pct >= 2.0 { DANGER } else { WARN };
        append(&mut job, &format!("  loss {:>3.1}%", s.loss_pct), c, &font);
    }
    if s.decode_ms > 0.5 {
        let c = if s.decode_ms >= 80.0 {
            DANGER
        } else if s.decode_ms >= 40.0 {
            WARN
        } else {
            Color32::WHITE
        };
        append(&mut job, &format!("  dec {:>4.0} ms", s.decode_ms), c, &font);
    }
    if s.stalled {
        append(&mut job, "  STALL", DANGER, &font);
    }
    if s.oom_visible() {
        append(&mut job, "  OOM", DANGER, &font);
    }
    if let Some(n) = s.visible_gaps() {
        let c = if n >= 10 { DANGER } else { WARN };
        append(&mut job, &format!("  gaps {n}"), c, &font);
    }
    if let Some(n) = s.visible_drops() {
        let c = if n >= 5 { DANGER } else { WARN };
        append(&mut job, &format!("  drop {n}"), c, &font);
    }
    let ice = s.ice.as_str();
    if !ice.is_empty() && ice != "connected" && ice != "completed" && ice != "—" {
        let c = if ice.contains("fail") || ice.contains("disconnect") {
            DANGER
        } else {
            WARN
        };
        append(&mut job, &format!("  ice {ice}"), c, &font);
    }

    append(&mut job, "\n", Color32::WHITE, &font);

    append(&mut job, "CPU ", Color32::WHITE, &font);
    append(
        &mut job,
        &format!("{:>4.0}%", sys.cpu_app_pct),
        level(sys.cpu_app_pct, 25.0, 50.0),
        &font,
    );
    append(&mut job, " (", Color32::WHITE, &font);
    append(
        &mut job,
        &format!("{:.0}%", sys.cpu_pct),
        level(sys.cpu_pct, 80.0, 92.0),
        &font,
    );
    append(&mut job, ")", Color32::WHITE, &font);

    append(&mut job, "   RAM ", Color32::WHITE, &font);
    append(
        &mut job,
        &format!("{:>5.0} MB", sys.proc_mb),
        level(sys.proc_mb, 500.0, 800.0),
        &font,
    );
    append(&mut job, " (", Color32::WHITE, &font);
    append(
        &mut job,
        &format!("{:.0}% {:.1}/{:.0} GB", sys.ram_pct, sys.ram_used_gb, sys.ram_total_gb),
        level(sys.ram_pct, 80.0, 90.0),
        &font,
    );
    append(&mut job, ")", Color32::WHITE, &font);

    append_net_line(&mut job, &s, &sys, live, &font);
    append_gpu_line(&mut job, &sys, live, &font);

    job
}

fn append_net_line(
    job: &mut LayoutJob,
    s: &StreamStats,
    sys: &SysLoad,
    live: bool,
    font: &FontId,
) {
    append(job, "\n", Color32::WHITE, font);
    append(job, "NET ↓", Color32::WHITE, font);
    let app_down = if live { s.bitrate_kbps } else { 0.0 };
    let net_c = if live && s.decoded > 0 && app_down < 200.0 && app_down > 0.0 {
        WARN
    } else {
        Color32::WHITE
    };
    append(job, &fmt_rate(app_down), net_c, font);
    if s.pkt_pps > 0.0 && live {
        append(job, &format!(" {:>3.0}p/s", s.pkt_pps), Color32::WHITE, font);
    }
    append(job, "  (↓", Color32::WHITE, font);
    append(job, &fmt_rate(sys.nic_down_kbps), Color32::WHITE, font);
    append(job, " ↑", Color32::WHITE, font);
    append(job, &fmt_rate(sys.nic_up_kbps), Color32::WHITE, font);
    append(job, ")", Color32::WHITE, font);
}

fn append_gpu_line(job: &mut LayoutJob, sys: &SysLoad, _live: bool, font: &FontId) {
    append(job, "\n", Color32::WHITE, font);
    append(job, "GPU ", Color32::WHITE, font);
    if sys.gpu_name.is_empty() {
        append(job, "—", MUTED, font);
        return;
    }
    append(job, &sys.gpu_name, Color32::WHITE, font);
    append_eng(job, font, "  3D ", sys.gpu_3d_app, sys.gpu_3d_sys);
    append_eng(job, font, "  copy ", sys.gpu_copy_app, sys.gpu_copy_sys);
    append_eng(job, font, "  vdec ", sys.gpu_vdec_app, sys.gpu_vdec_sys);
    append_eng(job, font, "  vp ", sys.gpu_vp_app, sys.gpu_vp_sys);
    if sys.gpu_compute_sys >= 0.5 || sys.gpu_compute_app >= 0.5 {
        append_eng(job, font, "  cmp ", sys.gpu_compute_app, sys.gpu_compute_sys);
    }
    if sys.gpu_vram_budget_mb > 1.0 {
        append(job, "  ", Color32::WHITE, font);
        let pct = if sys.gpu_vram_budget_mb > 1.0 {
            sys.gpu_vram_used_mb / sys.gpu_vram_budget_mb * 100.0
        } else {
            0.0
        };
        append(
            job,
            &fmt_vram(sys.gpu_vram_used_mb, sys.gpu_vram_budget_mb),
            level(pct, 75.0, 90.0),
            font,
        );
    }
    if sys.gpu_shared_used_mb >= 8.0 {
        append(job, "  sh ", Color32::WHITE, font);
        let sh_pct = if sys.gpu_shared_budget_mb > 1.0 {
            sys.gpu_shared_used_mb / sys.gpu_shared_budget_mb * 100.0
        } else {
            0.0
        };
        append(
            job,
            &format!("{:.1}G", sys.gpu_shared_used_mb / 1024.0),
            level(sh_pct, 40.0, 70.0),
            font,
        );
    }
}

fn append_eng(job: &mut LayoutJob, font: &FontId, label: &str, app: f32, sys: f32) {
    append(job, label, Color32::WHITE, font);
    append(job, &format!("{:.0}%", app), level(app, 70.0, 90.0), font);
    append(job, "(", Color32::WHITE, font);
    append(job, &format!("{:.0}%", sys), level(sys, 70.0, 90.0), font);
    append(job, ")", Color32::WHITE, font);
}

fn fmt_vram(used: f32, budget: f32) -> String {
    if budget >= 1024.0 {
        format!("{:.1}/{:.0}G", used / 1024.0, budget / 1024.0)
    } else {
        format!("{:.0}/{:.0} MB", used, budget)
    }
}

fn append(job: &mut LayoutJob, text: &str, color: Color32, font: &FontId) {
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font.clone(),
            color,
            ..Default::default()
        },
    );
}

fn level(v: f32, warn: f32, bad: f32) -> Color32 {
    if v >= bad {
        DANGER
    } else if v >= warn {
        WARN
    } else {
        Color32::WHITE
    }
}

fn fmt_rate(kbps: f32) -> String {
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", kbps / 1000.0)
    } else {
        format!("{:.0} kbps", kbps.max(0.0))
    }
}
