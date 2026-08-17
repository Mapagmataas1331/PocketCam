//! Windows virtual camera: NV12 ring + IMFVirtualCamera.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use pocketcam_ipc::{peek_layout, ring_bytes, RingWriter, DEFAULT_PATH};
use windows::core::{w, Interface};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::nv12::{contain_fit, waiting_still};
use crate::preview::QualitySpec;

const CLSID: windows::core::PCWSTR = w!("{7B89B92E-FE71-42D0-8A41-E137D06EA184}");
const FRIENDLY: windows::core::PCWSTR = w!("PocketCam");
/// Name used by an earlier prototype. Remove() so a leftover registration
/// does not sit next to PocketCam in the camera list.
const LEGACY_VCAM_NAME: windows::core::PCWSTR = w!("SWVCamMediaSource");
const RING_WAIT: Duration = Duration::from_secs(2);

/// {C7F7C57B-DF30-41D0-AFFC-15201CDF920D} — VirtualCameraKind::Synthetic
const VCAM_KIND: windows::core::GUID =
    windows::core::GUID::from_u128(0xc7f7c57b_df30_41d0_affc_1520_1cdf920d);

/// COM camera handle. Start/Stop/Remove run on the tray thread so they
/// still work while the preview window is hidden.
struct Camera(IMFVirtualCamera);
unsafe impl Send for Camera {}
unsafe impl Sync for Camera {}

pub struct VcamStart {
    pub replaced: bool,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct Vcam {
    on: AtomicBool,
    ring: Mutex<Option<RingWriter>>,
    cam: Mutex<Option<Camera>>,
    canvas: Mutex<(u32, u32, u32)>,
    notices: Mutex<Vec<String>>,
}

impl Vcam {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            on: AtomicBool::new(false),
            ring: Mutex::new(None),
            cam: Mutex::new(None),
            canvas: Mutex::new((1920, 1080, 30)),
            notices: Mutex::new(Vec::new()),
        })
    }

    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }

    /// Drop the Windows camera so uninstall can delete the DLL.
    pub fn unregister() -> Result<()> {
        ensure_mf()?;
        unsafe {
            remove_named(FRIENDLY);
            remove_named(LEGACY_VCAM_NAME);
        }
        Ok(())
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn canvas(&self) -> (u32, u32) {
        let g = self.canvas.lock();
        (g.0, g.1)
    }

    pub fn start(&self, spec: &QualitySpec) -> Result<VcamStart> {
        ensure_mf()?;
        let path = Path::new(DEFAULT_PATH);
        let existed = peek_layout(path).is_some();
        let replace = must_replace(path, spec.width, spec.height);

        // Unmap ourselves first — we are a mapper, same as Frame Server.
        *self.ring.lock() = None;
        if replace {
            self.evict_camera();
        }

        let ring = RingWriter::open_poll(path, spec.width, spec.height, spec.fps, RING_WAIT)
            .with_context(|| {
                format!(
                    "nv12.ring could not become {}×{} — click Start again",
                    spec.width, spec.height
                )
            })?;
        if ring.width != spec.width || ring.height != spec.height {
            bail!(
                "nv12.ring opened at {}×{}, not {}×{}",
                ring.width,
                ring.height,
                spec.width,
                spec.height
            );
        }

        let (width, height, fps) = (ring.width, ring.height, ring.fps);
        *self.ring.lock() = Some(ring);
        *self.canvas.lock() = (width, height, fps);
        self.write_waiting();
        let recreated = self.publish(replace)?;
        self.on.store(true, Ordering::Relaxed);
        tracing::info!(
            "virtual camera on {width}×{height} @ {fps} (NV12 ring {DEFAULT_PATH})"
        );
        Ok(VcamStart {
            replaced: recreated && existed,
            width,
            height,
            fps,
        })
    }

    pub fn stop(&self) {
        self.write_waiting();
        self.on.store(false, Ordering::Relaxed);
        self.evict_camera();
        *self.ring.lock() = None;
        tracing::info!("virtual camera off");
    }

    pub fn set_format(&self, spec: &QualitySpec) -> Result<()> {
        let (w, h, fps) = *self.canvas.lock();
        if w == spec.width && h == spec.height && fps == spec.fps {
            return Ok(());
        }
        if w == spec.width && h == spec.height {
            if let Some(ring) = self.ring.lock().as_mut() {
                ring.set_fps(spec.fps);
            }
            self.canvas.lock().2 = spec.fps;
            return Ok(());
        }
        if self.is_on() {
            // Live size stays put. Contain-fit handles a different phone frame.
            return Ok(());
        }
        *self.canvas.lock() = (spec.width, spec.height, spec.fps);
        Ok(())
    }

    /// Tear down Frame Server's mapping so `nv12.ring` can change size.
    fn evict_camera(&self) {
        if let Some(cam) = self.cam.lock().take() {
            unsafe {
                let _ = cam.0.Stop();
                let _ = cam.0.Remove();
                let _ = cam.0.Shutdown();
            }
        }
        unsafe {
            remove_named(FRIENDLY);
            remove_named(LEGACY_VCAM_NAME);
        }
    }

    /// `force_replace` after a canvas change: new IMFVirtualCamera so Initialize
    /// peeks the new ring. Same size attaches to a parked PocketCam (no re-select).
    fn publish(&self, force_replace: bool) -> Result<bool> {
        unsafe {
            remove_named(LEGACY_VCAM_NAME);
            if !force_replace {
                if let Some(cam) = self.cam.lock().as_ref() {
                    cam.0.Start(None).context("IMFVirtualCamera::Start")?;
                    return Ok(false);
                }
                if let Ok(cam) = MFCreateVirtualCamera(
                    MFVirtualCameraType_SoftwareCameraSource,
                    MFVirtualCameraLifetime_System,
                    MFVirtualCameraAccess_CurrentUser,
                    FRIENDLY,
                    CLSID,
                    None,
                ) {
                    if let Ok(attrs) = cam.cast::<IMFAttributes>() {
                        let _ = attrs.SetUINT32(&VCAM_KIND, 0);
                    }
                    if cam.Start(None).is_ok() {
                        *self.cam.lock() = Some(Camera(cam));
                        return Ok(false);
                    }
                    let _ = cam.Remove();
                    let _ = cam.Shutdown();
                }
            }

            if let Some(old) = self.cam.lock().take() {
                let _ = old.0.Stop();
                let _ = old.0.Remove();
                let _ = old.0.Shutdown();
            }
            remove_named(FRIENDLY);
            let cam = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_System,
                MFVirtualCameraAccess_CurrentUser,
                FRIENDLY,
                CLSID,
                None,
            )
            .context("MFCreateVirtualCamera — is VirtualCameraMediaSource.dll registered in HKLM?")?;
            if let Ok(attrs) = cam.cast::<IMFAttributes>() {
                let _ = attrs.SetUINT32(&VCAM_KIND, 0);
            }
            cam.Start(None)
                .context("IMFVirtualCamera::Start")?;
            *self.cam.lock() = Some(Camera(cam));
            Ok(true)
        }
    }

    pub fn write_waiting(&self) {
        let (w, h, _) = *self.canvas.lock();
        let mut buf = vec![0u8; pocketcam_ipc::nv12_size(w, h) as usize];
        waiting_still(&mut buf, w, h);
        if let Some(ring) = self.ring.lock().as_ref() {
            ring.write_nv12(&buf);
        }
    }

    pub fn write_contain(
        &self,
        src: &[u8],
        src_w: u32,
        src_h: u32,
        dst: &mut Vec<u8>,
    ) {
        if !self.is_on() {
            return;
        }
        let (dw, dh) = self.canvas();
        let need = pocketcam_ipc::nv12_size(dw, dh) as usize;
        if dst.len() != need {
            dst.clear();
            dst.resize(need, 0);
        }
        contain_fit(src, src_w, src_h, dst, dw, dh);
        if let Some(ring) = self.ring.lock().as_ref() {
            ring.write_nv12(dst);
        }
    }
}

impl Drop for Vcam {
    fn drop(&mut self) {
        self.write_waiting();
        self.on.store(false, Ordering::Relaxed);
        *self.ring.lock() = None;
        // Leave PocketCam registered. Frame Server keeps serving the waiting
        // still from nv12.ring after this process exits. Next Start evicts
        // if the canvas size changed.
    }
}

fn must_replace(path: &Path, width: u32, height: u32) -> bool {
    let want = u64::from(ring_bytes(width, height));
    let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match peek_layout(path) {
        None => true,
        Some((w, h, _)) => w != width || h != height || (file_len != 0 && file_len != want),
    }
}

fn ensure_mf() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    crate::mf::ensure()
}

unsafe fn remove_named(name: windows::core::PCWSTR) {
    if let Ok(cam) = MFCreateVirtualCamera(
        MFVirtualCameraType_SoftwareCameraSource,
        MFVirtualCameraLifetime_System,
        MFVirtualCameraAccess_CurrentUser,
        name,
        CLSID,
        None,
    ) {
        let _ = cam.Remove();
        let _ = cam.Shutdown();
    }
}
