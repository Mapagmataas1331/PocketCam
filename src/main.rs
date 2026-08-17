//! PocketCam Windows host: HTTPS + QR session + H.264 preview.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cert;
mod decode;
mod depay;
mod health;
mod host;
mod icon;
mod mf;
mod nv12;
mod pairing;
mod preview;
mod qr;
mod record;
mod server;
mod session;
mod settings;
mod shared;
mod sps;
mod sys;
mod ui;
mod vcam;

use anyhow::Result;
use eframe::egui;

use crate::shared::Shared;
use crate::ui::PocketCamApp;

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--unregister-camera") {
        let _ = crate::vcam::Vcam::unregister();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pocketcam=info,webrtc=warn".into()),
        )
        .init();
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("{info}");
        eprintln!("{info}");
    }));

    crate::mf::ensure()?;

    let Some(host) = crate::host::Host::claim()? else {
        return Ok(());
    };
    let host = std::sync::Arc::new(host);

    let shared = Shared::new();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    host.bind_shared(shared.clone(), cmd_tx.clone());
    if shared.settings.lock().vcam_on_launch {
        host.request_toggle_vcam();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("pocketcam")
        .build()?;
    let server_shared = shared.clone();
    rt.spawn(async move {
        if let Err(e) = server::run(server_shared, cmd_rx).await {
            tracing::error!("HTTPS server: {e:#}");
        }
    });

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("PocketCam")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([960.0, 600.0]);
    if let Ok(icon) = crate::icon::egui_icon(crate::icon::apps_use_light_theme()) {
        viewport = viewport.with_icon(icon);
    }
    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let result = eframe::run_native(
        "PocketCam",
        native,
        Box::new(move |cc| Ok(Box::new(PocketCamApp::new(cc, shared, cmd_tx, host)))),
    );
    drop(rt);
    result.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
