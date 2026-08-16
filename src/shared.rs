use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::health::FirewallState;
use crate::pairing::Pairing;
use crate::preview::{FrameSlot, PreviewControl, PreviewEncoding, StreamStats};
use crate::record::Recorder;
use crate::settings::Settings;
use crate::vcam::Vcam;

pub enum HostCmd {
    SelectCamera(String),
    SelectQuality(String),
    CaptureLock,
    NewSession,
}

pub enum ListenState {
    Starting,
    Ok,
    Failed(String),
}

pub struct Shared {
    pub slot: Arc<Mutex<FrameSlot>>,
    pub stats: Arc<Mutex<StreamStats>>,
    pub pairing: Arc<Mutex<Pairing>>,
    pub preview: Arc<PreviewControl>,
    pub vcam: Arc<Vcam>,
    pub record: Arc<Recorder>,
    pub settings: Arc<Mutex<Settings>>,
    pub listen_port: AtomicU16,
    pub listen: Mutex<ListenState>,
    pub firewall: Mutex<FirewallState>,
    pub cert_names: Mutex<Vec<String>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        let settings = Settings::load();
        let port = settings.port;
        let pairing = Pairing::new(&settings, port);
        let preview = PreviewControl::new();
        preview.set_keep_rgb(settings.keep_preview);
        Arc::new(Self {
            slot: Arc::new(Mutex::new(FrameSlot::default())),
            stats: Arc::new(Mutex::new(StreamStats::fresh())),
            pairing: Arc::new(Mutex::new(pairing)),
            preview,
            vcam: Vcam::new(),
            record: Recorder::new(),
            settings: Arc::new(Mutex::new(settings)),
            listen_port: AtomicU16::new(port),
            listen: Mutex::new(ListenState::Starting),
            firewall: Mutex::new(FirewallState::inbound_tcp(port)),
            cert_names: Mutex::new(Vec::new()),
        })
    }

    pub fn refresh_firewall(&self) {
        let port = self.listen_port.load(Ordering::Relaxed);
        *self.firewall.lock() = FirewallState::inbound_tcp(port);
    }

    pub fn apply_settings(&self, mut next: Settings) -> anyhow::Result<String> {
        next.clamp_port();
        next.save()?;
        let listen = self.listen_port.load(Ordering::Relaxed);
        self.pairing.lock().set_endpoint(&next, listen);
        let port_changed = next.port != listen;
        *self.settings.lock() = next;
        if port_changed {
            Ok(format!(
                "Settings saved. Restart PocketCam to listen on port {} (now {listen}).",
                self.settings.lock().port
            ))
        } else {
            Ok("Settings saved.".into())
        }
    }

    pub fn rotate_session(&self) {
        let settings = self.settings.lock().clone();
        let port = self.listen_port.load(Ordering::Relaxed);
        self.pairing.lock().rotate(&settings, port);
    }

    pub fn hello_ok_json(&self) -> serde_json::Value {
        let stun = self.settings.lock().stun;
        serde_json::json!({
            "type": "hello-ok",
            "stun": stun,
            "stunUrl": crate::settings::DEFAULT_STUN,
        })
    }

    /// Start/stop the Windows camera. Safe from the tray thread while hidden.
    pub fn toggle_vcam(&self) -> String {
        if self.preview.vcam_on.load(Ordering::Relaxed) {
            self.vcam.stop();
            self.preview.vcam_on.store(false, Ordering::Relaxed);
            return "Virtual camera off.".into();
        }
        let id = self.stats.lock().selected_quality.clone();
        self.preview.set_camera_quality(&id);
        let spec = self.preview.camera_quality();
        match self.vcam.start(spec) {
            Ok(info) => {
                self.preview.vcam_on.store(true, Ordering::Relaxed);
                let mut msg = if info.replaced {
                    format!(
                        "PocketCam is {}×{} @ {} fps. Select it again in OBS or Discord if the picture went black.",
                        info.width, info.height, info.fps
                    )
                } else {
                    format!(
                        "Virtual camera on {}×{} @ {} fps. Pick PocketCam in OBS, Discord, or Zoom.",
                        info.width, info.height, info.fps
                    )
                };
                if self.preview.encoding() == PreviewEncoding::Auto && self.preview.loaded() {
                    msg.push_str(
                        " Auto preview skipped RGB so the virtual camera can hold frame rate.",
                    );
                }
                msg
            }
            Err(e) => {
                tracing::error!("virtual camera: {e:#}");
                format!("Virtual camera failed: {e:#}")
            }
        }
    }

    /// Start/stop the MP4 mux. Safe from the tray thread while hidden.
    pub fn toggle_record(&self) -> String {
        if self.record.is_on() {
            if let Some(path) = self.record.stop() {
                self.preview.record_on.store(false, Ordering::Relaxed);
                return format!("Saved {}", path.display());
            }
            return "Record already stopped.".into();
        }
        let dir = self.settings.lock().recordings_dir();
        match self.record.start(&dir) {
            Ok(path) => {
                self.preview.record_on.store(true, Ordering::Relaxed);
                format!(
                    "Recording {} — starts on the next keyframe. Quality size is locked.",
                    path.display()
                )
            }
            Err(e) => {
                tracing::error!("record: {e:#}");
                format!("Record failed: {e:#}")
            }
        }
    }
}
