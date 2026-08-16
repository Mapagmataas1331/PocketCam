//! webrtc-rs answerer: receive phone H.264 (Safari / Chrome), depay, decode.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};
use webrtc::api::interceptor_registry::{configure_nack, configure_rtcp_reports};
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_remote::TrackRemote;

use crate::depay::H264Depay;
use crate::decode::{error_is_oom, preview_rgb_mode, MfH264Decoder};
use crate::preview::{FrameSlot, PreviewControl, PreviewEncoding, StreamStats};
use crate::record::Recorder;
use crate::vcam::Vcam;

#[derive(Clone)]
pub struct SharedVideo {
    pub slot: Arc<Mutex<FrameSlot>>,
    pub stats: Arc<Mutex<StreamStats>>,
    pub preview: Arc<PreviewControl>,
    pub vcam: Arc<Vcam>,
    pub record: Arc<Recorder>,
}

struct AuQueue {
    buf: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
    closed: AtomicBool,
    wait_idr: AtomicBool,
    flush: AtomicBool,
}

const AU_Q_MAX: usize = 2;

impl AuQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::with_capacity(AU_Q_MAX)),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            wait_idr: AtomicBool::new(false),
            flush: AtomicBool::new(false),
        })
    }

    /// False = queued. True = dropped (overflow or waiting for a keyframe).
    fn push(&self, au: Vec<u8>) -> bool {
        let idr = crate::depay::is_idr(&au);
        if self.wait_idr.load(Ordering::Relaxed) && !idr {
            return true;
        }
        let mut g = self.buf.lock();
        if g.len() >= AU_Q_MAX {
            if idr {
                g.clear();
                self.wait_idr.store(false, Ordering::Relaxed);
                self.flush.store(true, Ordering::Relaxed);
            } else {
                // Drop this extra P-frame. Do not latch wait_idr — that
                // became a stable ~1 fps whenever 60 fps decode fell behind.
                self.cv.notify_one();
                return true;
            }
        }
        if idr {
            if self.wait_idr.swap(false, Ordering::Relaxed) {
                self.flush.store(true, Ordering::Relaxed);
            }
        }
        g.push_back(au);
        self.cv.notify_one();
        false
    }

    fn request_idr(&self) {
        self.wait_idr.store(true, Ordering::Relaxed);
        self.buf.lock().clear();
        self.cv.notify_one();
    }

    fn len(&self) -> usize {
        self.buf.lock().len()
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut g = self.buf.lock();
        loop {
            if let Some(au) = g.pop_front() {
                return Some(au);
            }
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            self.cv.wait(&mut g);
        }
    }

    fn take_flush(&self) -> bool {
        self.flush.swap(false, Ordering::Relaxed)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }
}

fn register_h264_lan(m: &mut MediaEngine) -> Result<()> {
    // No goog-remb / transport-cc: browser GCC hunts bitrate every few seconds
    // and the picture pumps between sharp and shredded.
    let fb = vec![
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
    ];
    let profiles = [
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=64001f",
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f",
        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f",
        "level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f",
    ];
    for (i, fmtp) in profiles.iter().enumerate() {
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_H264.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: (*fmtp).to_owned(),
                    rtcp_feedback: fb.clone(),
                },
                payload_type: (102 + i) as u8,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
    }
    Ok(())
}

pub async fn answer_offer(
    sdp: String,
    video: SharedVideo,
    stun: bool,
    host_ip: Option<Ipv4Addr>,
) -> Result<(String, Arc<RTCPeerConnection>)> {
    let mut m = MediaEngine::default();
    register_h264_lan(&mut m)?;

    let mut registry = Registry::new();
    registry = configure_nack(registry, &mut m);
    registry = configure_rtcp_reports(registry);

    let mut settings = SettingEngine::default();
    settings.set_ice_multicast_dns_mode(MulticastDnsMode::QueryOnly);
    settings.set_network_types(vec![NetworkType::Udp4]);
    settings.set_ip_filter(Box::new(move |ip: IpAddr| match ip {
        IpAddr::V4(v) => {
            if let Some(want) = host_ip {
                return v == want;
            }
            !v.is_loopback()
                && !v.is_link_local()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !crate::settings::is_virtual_lan(v)
        }
        IpAddr::V6(_) => false,
    }));

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();

    let ice_servers = if stun {
        vec![RTCIceServer {
            urls: crate::settings::ice_servers(true),
            ..Default::default()
        }]
    } else {
        Vec::new()
    };

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    let pc = Arc::new(api.new_peer_connection(config).await?);

    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Recvonly,
            send_encodings: vec![],
        }),
    )
    .await?;

    let video_for_track = video.clone();
    let pc_weak = Arc::downgrade(&pc);
    pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let video = video_for_track.clone();
        let pc_weak = pc_weak.clone();
        Box::pin(async move {
            on_remote_track(track, video, pc_weak).await;
        })
    }));

    let ice_stats = video.stats.clone();
    pc.on_ice_connection_state_change(Box::new(move |st: RTCIceConnectionState| {
        tracing::info!("ICE {st}");
        ice_stats.lock().ice = st.to_string();
        Box::pin(async {})
    }));

    let pc_stats = video.stats.clone();
    pc.on_peer_connection_state_change(Box::new(move |st: RTCPeerConnectionState| {
        tracing::info!("PC {st}");
        pc_stats.lock().pc = st.to_string();
        Box::pin(async {})
    }));

    let offer = RTCSessionDescription::offer(sdp)?;
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    let mut gather = pc.gathering_complete_promise().await;
    pc.set_local_description(answer).await?;
    let _ = tokio::time::timeout(Duration::from_secs(3), gather.recv()).await;

    let local = pc.local_description().await.context("local description")?;
    Ok((local.sdp, pc))
}

async fn on_remote_track(
    track: Arc<TrackRemote>,
    video: SharedVideo,
    pc: std::sync::Weak<RTCPeerConnection>,
) {
    let codec = track.codec();
    let mime = codec.capability.mime_type.clone();
    tracing::info!(
        "remote track {} pt={} fmtp={}",
        mime,
        codec.payload_type,
        codec.capability.sdp_fmtp_line
    );
    video.stats.lock().codec = format!("{} {}", mime, codec.capability.sdp_fmtp_line);

    if !mime.eq_ignore_ascii_case(MIME_TYPE_H264) {
        tracing::error!("negotiated {mime}, not H.264");
        return;
    }

    let media_ssrc = track.ssrc();
    if let Some(pc) = pc.upgrade() {
        let _ = pc
            .write_rtcp(&[Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc,
            })])
            .await;
    }

    let need_pli = Arc::new(AtomicBool::new(false));
    let latest = AuQueue::new();
    let decode_slot = video.slot.clone();
    let decode_stats = video.stats.clone();
    let decode_preview = video.preview.clone();
    let decode_vcam = video.vcam.clone();
    let latest_dec = latest.clone();
    let need_pli_dec = need_pli.clone();
    let _ = std::thread::Builder::new()
        .name("pocketcam-decode".into())
        .spawn(move || {
            let mut decoder = match MfH264Decoder::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("MF H.264 decoder failed: {e:#}");
                    return;
                }
            };
            let mut seq = 0u64;
            let mut last_rgb: Option<Instant> = None;
            let mut was_skipping = false;
            while let Some(au) = latest_dec.pop() {
                let plan = decode_preview.plan();
                if plan.skip_always && !decode_preview.needs_nv12() {
                    was_skipping = true;
                    {
                        let mut s = decode_stats.lock();
                        if crate::depay::is_idr(&au) {
                            if let Some(crop) = crate::sps::sps_crop_from_annex_b(&au) {
                                if s.width == 0 {
                                    tracing::info!(
                                        "preview off — {}x{} stream, decode paused",
                                        crop.w,
                                        crop.h
                                    );
                                }
                                s.width = crop.w;
                                s.height = crop.h;
                            }
                        }
                        s.pulse();
                    }
                    continue;
                }
                if was_skipping {
                    was_skipping = false;
                    decoder.flush();
                    if !crate::depay::is_idr(&au) {
                        latest_dec.request_idr();
                        need_pli_dec.store(true, Ordering::Relaxed);
                        continue;
                    }
                }
                if crate::depay::is_idr(&au) && latest_dec.take_flush() {
                    decoder.flush();
                }
                let rgb = preview_rgb_mode(
                    latest_dec.len() == 0,
                    plan.clamp_for_source(decoder.src_long()),
                    &mut last_rgb,
                );
                let t0 = Instant::now();
                let decoded = decoder.decode_access_unit(
                    &au,
                    &decode_slot,
                    &decode_stats,
                    &mut seq,
                    rgb,
                    Some(decode_vcam.as_ref()),
                );
                let decode_ms = t0.elapsed().as_secs_f32() * 1000.0;
                {
                    let mut s = decode_stats.lock();
                    s.decode_ms = if s.decode_ms <= 0.0 {
                        decode_ms
                    } else {
                        s.decode_ms * 0.85 + decode_ms * 0.15
                    };
                    if decoder.take_oom() {
                        s.note_oom();
                        decode_preview.set_encoding(PreviewEncoding::Off);
                        decode_slot.lock().frame = None;
                    }
                }
                if let Err(e) = decoded {
                    if error_is_oom(&e) {
                        decode_preview.set_encoding(PreviewEncoding::Off);
                        decode_slot.lock().frame = None;
                        decode_stats.lock().note_oom();
                    } else {
                        tracing::warn!("decode: {e:#}");
                        latest_dec.request_idr();
                        need_pli_dec.store(true, Ordering::Relaxed);
                    }
                }
            }
        });

    let mut depay = H264Depay::default();
    let mut last_gap_log = 0u64;
    let mut last_drop_log = 0u64;
    let mut rtp_count = 0u64;
    let mut au_count = 0u64;
    let mut last_pli = Instant::now() - Duration::from_secs(10);
    let mut net_bytes = 0u64;
    let mut net_pkts = 0u64;
    let mut net_gaps0 = 0u64;
    let mut net_at = Instant::now();
    loop {
        match track.read_rtp().await {
            Ok((pkt, _)) => {
                rtp_count += 1;
                net_bytes += 12 + pkt.payload.len() as u64;
                net_pkts += 1;
                if rtp_count == 1 {
                    tracing::info!(
                        "first RTP seq={} ts={} marker={} payload={}",
                        pkt.header.sequence_number,
                        pkt.header.timestamp,
                        pkt.header.marker,
                        pkt.payload.len()
                    );
                }
                let aus = depay.push(
                    pkt.header.sequence_number,
                    pkt.header.timestamp,
                    pkt.header.marker,
                    &pkt.payload,
                );
                if depay.seq_gaps != last_gap_log {
                    last_gap_log = depay.seq_gaps;
                    if last_gap_log == 1 || last_gap_log % 60 == 0 {
                        tracing::warn!("RTP seq gaps={}", last_gap_log);
                    }
                    video.stats.lock().note_gaps(last_gap_log);
                }
                if depay.dropped_incomplete != last_drop_log {
                    last_drop_log = depay.dropped_incomplete;
                    latest.request_idr();
                    need_pli.store(true, Ordering::Relaxed);
                    if last_drop_log == 1 || last_drop_log % 30 == 0 {
                        tracing::warn!(
                            "dropped incomplete AU (total={}) — wait IDR",
                            last_drop_log
                        );
                    }
                    video.stats.lock().note_drops(last_drop_log);
                }
                for (au, _rtp_ts) in aus {
                    au_count += 1;
                    if au_count == 1 || au_count % 300 == 0 {
                        tracing::info!("H.264 AU #{au_count} {} bytes (rtp={rtp_count})", au.len());
                    }
                    video.record.push(&au);
                    if latest.push(au) {
                        need_pli.store(true, Ordering::Relaxed);
                    }
                }
                if net_at.elapsed() >= Duration::from_secs(1) {
                    let dt = net_at.elapsed().as_secs_f32().max(0.001);
                    let gaps = depay.seq_gaps;
                    let dg = gaps.saturating_sub(net_gaps0);
                    {
                        let mut s = video.stats.lock();
                        s.bitrate_kbps = net_bytes as f32 * 8.0 / dt / 1000.0;
                        s.pkt_pps = net_pkts as f32 / dt;
                        s.loss_pct = if net_pkts + dg > 0 {
                            dg as f32 / (net_pkts + dg) as f32 * 100.0
                        } else {
                            0.0
                        };
                    }
                    net_bytes = 0;
                    net_pkts = 0;
                    net_gaps0 = gaps;
                    net_at = Instant::now();
                }
                if video.record.take_pli() {
                    need_pli.store(true, Ordering::Relaxed);
                }
                if need_pli.swap(false, Ordering::Relaxed)
                    && last_pli.elapsed() >= Duration::from_millis(800)
                {
                    last_pli = Instant::now();
                    if let Some(pc) = pc.upgrade() {
                        let _ = pc
                            .write_rtcp(&[Box::new(PictureLossIndication {
                                sender_ssrc: 0,
                                media_ssrc,
                            })])
                            .await;
                    }
                }
            }
            Err(e) => {
                tracing::info!("track ended: {e}");
                break;
            }
        }
    }
    latest.close();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn p_frame() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x01]
    }

    fn idr_frame() -> Vec<u8> {
        vec![0, 0, 0, 1, 0x05]
    }

    #[test]
    fn overflow_drops_p_frames_without_latching_idr() {
        let q = AuQueue::new();
        assert!(!q.push(p_frame()));
        assert!(!q.push(p_frame()));
        assert!(q.push(p_frame()), "third P-frame overflows");
        assert!(
            !q.wait_idr.load(Ordering::Relaxed),
            "overflow must not wait for the next keyframe"
        );
        assert_eq!(q.len(), 2);
        assert!(q.pop().is_some());
        assert!(!q.push(p_frame()), "queue accepts again after a pop");
    }

    #[test]
    fn overflow_idr_replaces_queue() {
        let q = AuQueue::new();
        assert!(!q.push(p_frame()));
        assert!(!q.push(p_frame()));
        assert!(!q.push(idr_frame()));
        assert!(!q.wait_idr.load(Ordering::Relaxed));
        assert_eq!(q.len(), 1);
    }
}
