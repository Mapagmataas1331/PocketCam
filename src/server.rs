//! HTTPS phone page + token-gated signaling.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::sync::Mutex as TokioMutex;
use webrtc::peer_connection::RTCPeerConnection;

use crate::pairing::PairPhase;
use crate::preview::{quality_allowed, quality_by_id, CameraItem, merge_qualities};
use crate::session::SharedVideo;
use crate::shared::{HostCmd, Shared};

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const JSQR: &str = include_str!("../web/jsQR.js");
const ICON_SVG: &str = crate::icon::SVG;
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

struct PhoneLink {
    conn_id: u64,
    tx: UnboundedSender<String>,
    kill: Option<oneshot::Sender<()>>,
}

struct AppState {
    shared: Arc<Shared>,
    peer: TokioMutex<Option<Arc<RTCPeerConnection>>>,
    phone: TokioMutex<Option<PhoneLink>>,
}

#[derive(Deserialize)]
struct SignalMsg {
    #[serde(rename = "type")]
    kind: String,
    sdp: Option<String>,
    token: Option<String>,
    devices: Option<Vec<CamMsg>>,
    qualities: Option<Vec<CamMsg>>,
    #[serde(rename = "deviceId")]
    device_id: Option<String>,
    #[serde(rename = "qualityId")]
    quality_id: Option<String>,
    label: Option<String>,
}

#[derive(Deserialize)]
struct CamMsg {
    id: String,
    label: String,
    available: Option<bool>,
}

pub async fn run(shared: Arc<Shared>, mut cmds: UnboundedReceiver<HostCmd>) -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cert = crate::cert::load_or_mint().context("self-signed cert")?;
    let (cert_path, key_path) = crate::cert::write_pem(&cert)?;
    let tls = RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .context("load rustls cert")?;
    tracing::info!("cert {}", cert_path.display());
    tracing::info!("SANs {}", cert.names.join(", "));
    *shared.cert_names.lock() = cert.names.clone();

    let state = Arc::new(AppState {
        shared: shared.clone(),
        peer: TokioMutex::new(None),
        phone: TokioMutex::new(None),
    });

    let cmd_state = state.clone();
    tokio::spawn(async move {
        while let Some(cmd) = cmds.recv().await {
            match cmd {
                HostCmd::SelectCamera(id) => {
                    cmd_state.shared.stats.lock().selected_camera = id.clone();
                    if let Some(link) = cmd_state.phone.lock().await.as_ref() {
                        let _ = link.tx.send(
                            serde_json::json!({ "type": "select-camera", "deviceId": id })
                                .to_string(),
                        );
                    }
                }
                HostCmd::SelectQuality(id) => {
                    let rec = cmd_state.shared.record.is_on();
                    let vcam = cmd_state.shared.vcam.is_on();
                    let cur = cmd_state.shared.preview.camera_quality();
                    if let Some(next) = quality_by_id(&id) {
                        if !quality_allowed(cur, next, vcam, rec) {
                            tracing::info!("quality {id} blocked while vcam/record is on");
                            continue;
                        }
                    }
                    cmd_state.shared.stats.lock().selected_quality = id.clone();
                    apply_output_quality(&cmd_state.shared, &id);
                    if let Some(link) = cmd_state.phone.lock().await.as_ref() {
                        let _ = link.tx.send(
                            serde_json::json!({ "type": "select-quality", "qualityId": id })
                                .to_string(),
                        );
                    }
                    send_capture_lock(cmd_state.phone.lock().await.as_ref(), &cmd_state.shared);
                }
                HostCmd::CaptureLock => {
                    send_capture_lock(cmd_state.phone.lock().await.as_ref(), &cmd_state.shared);
                }
                HostCmd::NewSession => {
                    kick_phone(&cmd_state, "new-session").await;
                    hangup_peer(&cmd_state).await;
                    cmd_state.shared.rotate_session();
                    reset_media(&cmd_state.shared);
                    tracing::info!("new session {}", cmd_state.shared.pairing.lock().token);
                }
            }
        }
    });

    let app = Router::new()
        .route("/", get(phone_page))
        .route("/connect/{token}", get(connect_page))
        .route(
            "/app.js",
            get(|| async {
                (
                    [
                        ("content-type", "text/javascript; charset=utf-8"),
                        ("cache-control", "no-store"),
                    ],
                    APP_JS,
                )
            }),
        )
        .route(
            "/jsQR.js",
            get(|| async {
                (
                    [
                        ("content-type", "text/javascript; charset=utf-8"),
                        ("cache-control", "no-store"),
                    ],
                    JSQR,
                )
            }),
        )
        .route("/ws", get(ws_upgrade))
        .route("/pocketcam.svg", get(icon_svg))
        .route("/favicon.svg", get(icon_svg))
        .with_state(state);

    let port = shared.listen_port.load(Ordering::Relaxed);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    if let Err(e) = std::net::TcpListener::bind(addr) {
        let msg = format!(
            "Cannot listen on port {port}: {e}. Another app may be using it."
        );
        tracing::error!("{msg}");
        *shared.listen.lock() = crate::shared::ListenState::Failed(msg.clone());
        anyhow::bail!("{msg}");
    }
    tracing::info!("HTTPS listening on {addr}");
    shared.refresh_firewall();
    *shared.listen.lock() = crate::shared::ListenState::Ok;
    if let Err(e) = axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await
    {
        let msg = format!(
            "Cannot listen on port {port}: {e:#}. Another app may be using it, or Windows Firewall blocked the bind."
        );
        tracing::error!("{msg}");
        *shared.listen.lock() = crate::shared::ListenState::Failed(msg);
        anyhow::bail!("HTTPS serve");
    }
    Ok(())
}

async fn phone_page() -> impl IntoResponse {
    (
        [
            ("cache-control", "no-store"),
            ("referrer-policy", "no-referrer"),
        ],
        Html(INDEX),
    )
}

async fn connect_page(Path(_token): Path<String>) -> impl IntoResponse {
    phone_page().await
}

async fn icon_svg() -> impl IntoResponse {
    (
        [
            ("content-type", "image/svg+xml; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        ICON_SVG,
    )
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
    let mut kill_tx = Some(kill_tx);
    let mut authed = false;
    let mut claimed_gen = 0u64;
    let mut conn_id = 0u64;
    tracing::info!("signaling connected");

    loop {
        let msg = tokio::select! {
            _ = &mut kill_rx => {
                tracing::info!("signaling kicked");
                break;
            }
            next = stream.next() => match next {
                Some(Ok(m)) => m,
                _ => break,
            },
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        let parsed: SignalMsg = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("signal json: {e}");
                continue;
            }
        };

        if authed && state.shared.pairing.lock().generation != claimed_gen {
            let _ = out_tx.send(
                serde_json::json!({
                    "type": "bye",
                    "reason": "replaced",
                    "message": "PC started a new session. Type the new token."
                })
                .to_string(),
            );
            break;
        }

        match parsed.kind.as_str() {
            "hello" => {
                if authed {
                    let _ = out_tx.send(state.shared.hello_ok_json().to_string());
                    continue;
                }
                let token = parsed.token.unwrap_or_default();
                let result = {
                    let mut p = state.shared.pairing.lock();
                    match p.accept_token(&token) {
                        Ok(()) => {
                            p.phase = PairPhase::Connecting;
                            p.grace_until = None;
                            Ok(p.generation)
                        }
                        Err(e) => Err(e),
                    }
                };
                match result {
                    Ok(generation) => {
                        authed = true;
                        claimed_gen = generation;
                        conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
                        let kill = kill_tx.take();
                        let mut phone = state.phone.lock().await;
                        if let Some(old) = phone.take() {
                            if let Some(k) = old.kill {
                                let _ = k.send(());
                            }
                        }
                        *phone = Some(PhoneLink {
                            conn_id,
                            tx: out_tx.clone(),
                            kill,
                        });
                        drop(phone);
                        let _ = out_tx.send(state.shared.hello_ok_json().to_string());
                        tracing::info!("session claimed gen={generation} conn={conn_id}");
                    }
                    Err(e) => {
                        let _ = out_tx.send(
                            serde_json::json!({
                                "type": "error",
                                "code": e.code(),
                                "message": e.message(),
                            })
                            .to_string(),
                        );
                    }
                }
            }
            "offer" if authed => {
                let Some(sdp) = parsed.sdp else { continue };
                if handle_offer(&state, &out_tx, sdp).await {
                    break;
                }
            }
            "cameras" if authed => {
                if let Some(devices) = parsed.devices {
                    let mut s = state.shared.stats.lock();
                    s.cameras = devices
                        .into_iter()
                        .map(|d| CameraItem {
                            id: d.id,
                            label: d.label,
                            available: crate::preview::Avail::Yes,
                        })
                        .collect();
                }
            }
            "qualities" if authed => {
                if let Some(qualities) = parsed.qualities {
                    let mut s = state.shared.stats.lock();
                    s.qualities = merge_qualities(qualities.into_iter().map(|d| CameraItem {
                        id: d.id,
                        label: d.label,
                        available: crate::preview::Avail::from_opt(d.available),
                    }));
                    if let Some(id) = parsed.quality_id {
                        s.selected_quality = id.clone();
                        drop(s);
                        apply_output_quality(&state.shared, &id);
                    }
                }
            }
            "quality" if authed => {
                if let Some(id) = parsed.quality_id {
                    let mut s = state.shared.stats.lock();
                    s.selected_quality = id.clone();
                    if let Some(label) = parsed.label {
                        if !s.qualities.iter().any(|q| q.id == id) {
                            s.qualities.push(CameraItem {
                                id: id.clone(),
                                label,
                                available: crate::preview::Avail::Yes,
                            });
                        }
                    }
                    drop(s);
                    apply_output_quality(&state.shared, &id);
                }
            }
            "camera" if authed => {
                if let Some(id) = parsed.device_id {
                    let mut s = state.shared.stats.lock();
                    s.selected_camera = id.clone();
                    if let Some(label) = parsed.label {
                        if !s.cameras.iter().any(|c| c.id == id) {
                            s.cameras.push(CameraItem {
                                id,
                                label,
                                available: crate::preview::Avail::Yes,
                            });
                        }
                    }
                }
            }
            "offer" | "cameras" | "camera" | "qualities" | "quality" => {
                let _ = out_tx.send(
                    serde_json::json!({
                        "type": "error",
                        "code": "hello-first",
                        "message": "Enter the token from the PC, then start the camera.",
                    })
                    .to_string(),
                );
            }
            _ => {}
        }
    }

    tracing::info!("signaling closed");
    let ours = {
        let mut phone = state.phone.lock().await;
        let ours = phone
            .as_ref()
            .map(|p| p.conn_id == conn_id)
            .unwrap_or(false);
        if ours {
            *phone = None;
        }
        ours
    };
    if authed && ours {
        hangup_peer(&state).await;
        let deadline = {
            let mut p = state.shared.pairing.lock();
            if p.generation != claimed_gen {
                None
            } else {
                p.consumed = false;
                p.phase = PairPhase::Waiting;
                let until = std::time::Instant::now() + crate::pairing::TOKEN_GRACE;
                p.grace_until = Some(until);
                Some((claimed_gen, until))
            }
        };
        reset_media(&state.shared);
        if let Some((gen, until)) = deadline {
            tracing::info!("phone left; token kept for {:?}", crate::pairing::TOKEN_GRACE);
            let shared = state.shared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(crate::pairing::TOKEN_GRACE).await;
                let expired = {
                    let p = shared.pairing.lock();
                    p.generation == gen && p.grace_until == Some(until) && !p.consumed
                };
                if expired {
                    shared.rotate_session();
                    tracing::info!(
                        "token grace expired — new session {}",
                        shared.pairing.lock().token
                    );
                    reset_media(&shared);
                }
            });
        }
    }
}

async fn handle_offer(
    state: &AppState,
    out_tx: &UnboundedSender<String>,
    sdp: String,
) -> bool {
    tracing::info!("offer {} bytes", sdp.len());
    {
        let mut slot = state.peer.lock().await;
        if let Some(old) = slot.take() {
            let _ = old.close().await;
        }
    }

    let video = SharedVideo {
        slot: state.shared.slot.clone(),
        stats: state.shared.stats.clone(),
        preview: state.shared.preview.clone(),
        vcam: state.shared.vcam.clone(),
        record: state.shared.record.clone(),
    };
    let stun = state.shared.settings.lock().stun;
    let host_ip = state.shared.settings.lock().resolve_lan();
    match crate::session::answer_offer(sdp, video, stun, host_ip).await {
        Ok((answer, pc)) => {
            *state.peer.lock().await = Some(pc);
            state.shared.pairing.lock().phase = PairPhase::Live;
            let body = serde_json::json!({ "type": "answer", "sdp": answer });
            out_tx.send(body.to_string()).is_err()
        }
        Err(e) => {
            tracing::error!("answer failed: {e:#}");
            let body = serde_json::json!({
                "type": "error",
                "code": "answer-failed",
                "message": "Could not start the live session. Same Wi-Fi as the PC? Tap Start camera to retry.",
            });
            let _ = out_tx.send(body.to_string());
            false
        }
    }
}

async fn kick_phone(state: &AppState, reason: &str) {
    let old = state.phone.lock().await.take();
    if let Some(link) = old {
        let _ = link.tx.send(
            serde_json::json!({
                "type": "bye",
                "reason": reason,
                "message": "PC started a new session. Type the new token shown on the PC.",
            })
            .to_string(),
        );
        if let Some(kill) = link.kill {
            let _ = kill.send(());
        }
        tokio::task::yield_now().await;
    }
}

async fn hangup_peer(state: &AppState) {
    if let Some(pc) = state.peer.lock().await.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), pc.close()).await;
    }
}

fn apply_output_quality(shared: &Shared, id: &str) {
    shared.preview.set_camera_quality(id);
    if let Some(spec) = quality_by_id(id) {
        if let Err(e) = shared.vcam.set_format(spec) {
            tracing::error!("vcam format from quality: {e:#}");
        }
    }
}

fn send_capture_lock(phone: Option<&PhoneLink>, shared: &Shared) {
    let Some(link) = phone else {
        return;
    };
    let rec = shared.record.is_on();
    let vcam = shared.vcam.is_on();
    let spec = shared.preview.camera_quality();
    let msg = if !rec && !vcam {
        serde_json::json!({ "type": "capture-lock" })
    } else {
        serde_json::json!({
            "type": "capture-lock",
            "width": spec.width,
            "height": spec.height,
            "maxFps": if rec { serde_json::Value::from(spec.fps) } else { serde_json::Value::Null },
        })
    };
    let _ = link.tx.send(msg.to_string());
}

fn reset_media(shared: &Shared) {
    shared.slot.lock().frame = None;
    let mut s = shared.stats.lock();
    *s = crate::preview::StreamStats::fresh();
    if shared.vcam.is_on() {
        shared.vcam.write_waiting();
    }
    if shared.record.is_on() {
        shared.preview.record_on.store(false, std::sync::atomic::Ordering::Relaxed);
        match shared.record.stop() {
            Some(path) if path.exists() => {
                shared.record.push_notice(format!(
                    "Recording stopped — phone disconnected. Saved {}",
                    path.display()
                ));
            }
            Some(_) => {
                shared
                    .record
                    .push_notice("Recording stopped — no video in the file yet.");
            }
            None => {}
        }
    }
}
