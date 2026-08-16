//! Mux the phone's H.264 Annex-B into an MP4. No second encode.
//! The RTP thread clones AUs; this crate never waits on disk from decode.

use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::depay::is_idr;
use crate::sps::sps_crop_from_annex_b;

const TIMESCALE: u32 = 90_000;
const QUEUE: usize = 16;
const DEFAULT_DUR: u32 = TIMESCALE / 30;
const MIN_DUR: u32 = TIMESCALE / 120; // 120 fps
const MAX_DUR: u32 = TIMESCALE / 4; // 0.25 s, covers a camera switch gap

pub struct Recorder {
    on: Arc<AtomicBool>,
    pli: AtomicBool,
    tx: Mutex<Option<SyncSender<Msg>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    last_path: Arc<Mutex<Option<PathBuf>>>,
    notices: Arc<Mutex<Vec<String>>>,
}

enum Msg {
    Au { bytes: Vec<u8>, at: Instant },
    Stop,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            on: Arc::new(AtomicBool::new(false)),
            pli: AtomicBool::new(false),
            tx: Mutex::new(None),
            join: Mutex::new(None),
            last_path: Arc::new(Mutex::new(None)),
            notices: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn push_notice(&self, msg: impl Into<String>) {
        self.notices.lock().push(msg.into());
    }

    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.notices.lock())
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn last_path(&self) -> Option<PathBuf> {
        self.last_path.lock().clone()
    }

    pub fn take_pli(&self) -> bool {
        self.pli.swap(false, Ordering::Relaxed)
    }

    /// Clone-and-forget from the RTP thread. Drops the AU if the writer is behind.
    pub fn push(&self, annex_b: &[u8]) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let tx = self.tx.lock();
        let Some(tx) = tx.as_ref() else {
            return;
        };
        match tx.try_send(Msg::Au {
            bytes: annex_b.to_vec(),
            at: Instant::now(),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("record writer behind — dropped one AU");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn start(&self, dir: &std::path::Path) -> Result<PathBuf> {
        if self.on.load(Ordering::Relaxed) {
            bail!("already recording");
        }
        if let Some(h) = self.join.lock().take() {
            let _ = h.join();
        }
        let path = new_path(dir)?;
        let (tx, rx) = sync_channel(QUEUE);
        let path_for_thread = path.clone();
        let on_flag = Arc::clone(&self.on);
        let last_path = Arc::clone(&self.last_path);
        let notices = Arc::clone(&self.notices);
        let handle = thread::Builder::new()
            .name("pocketcam-record".into())
            .spawn(move || {
                if let Err(e) = writer_loop(rx, path_for_thread, last_path, &notices) {
                    tracing::error!("record: {e:#}");
                    notices.lock().push(format!("Recording stopped: {e:#}"));
                    on_flag.store(false, Ordering::Relaxed);
                }
            })
            .context("spawn record writer")?;
        *self.tx.lock() = Some(tx);
        *self.join.lock() = Some(handle);
        *self.last_path.lock() = Some(path.clone());
        self.pli.store(true, Ordering::Relaxed);
        self.on.store(true, Ordering::Relaxed);
        tracing::info!("record start {}", path.display());
        Ok(path)
    }

    pub fn stop(&self) -> Option<PathBuf> {
        if !self.on.swap(false, Ordering::Relaxed) {
            return self.last_path.lock().clone();
        }
        let tx = self.tx.lock().take();
        if let Some(tx) = tx {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(h) = self.join.lock().take() {
            let _ = h.join();
        }
        let path = self.last_path.lock().clone();
        if let Some(p) = &path {
            tracing::info!("record stop {}", p.display());
        }
        path
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn new_path(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = local_stamp();
    for n in 0..1000 {
        let name = if n == 0 {
            format!("pocketcam-{stamp}.mp4")
        } else {
            format!("pocketcam-{stamp}-{n}.mp4")
        };
        let p = dir.join(&name);
        if !p.exists() {
            return Ok(p);
        }
    }
    bail!("could not pick a recording name");
}

fn local_stamp() -> String {
    let st: SYSTEMTIME = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

fn sample_dur(prev: Option<Instant>, now: Instant, last_dur: u32) -> u32 {
    let Some(prev) = prev else {
        return last_dur;
    };
    let ns = now.saturating_duration_since(prev).as_nanos();
    let ticks = (ns.saturating_mul(TIMESCALE as u128) / 1_000_000_000) as u32;
    if ticks == 0 {
        last_dur
    } else {
        ticks.clamp(MIN_DUR, MAX_DUR)
    }
}

enum Push {
    Written,
    Dropped,
    /// Phone flipped portrait↔landscape. Finish this file and start another.
    OrientFlip,
    /// mdat / stco cannot grow past 4 GB. Finish and stop.
    TooLarge,
}

fn orient_swap(a_w: u16, a_h: u16, b_w: u16, b_h: u16) -> bool {
    a_w != a_h && a_w == b_h && a_h == b_w
}

fn ingest_au(
    mux: &mut Option<Mux>,
    last_at: &mut Option<Instant>,
    last_dur: &mut u32,
    path: &mut PathBuf,
    last_path: &Mutex<Option<PathBuf>>,
    notices: &Mutex<Vec<String>>,
    bytes: &[u8],
    at: Instant,
) -> Result<()> {
    if mux.is_none() {
        if !is_idr(bytes) {
            return Ok(());
        }
        *mux = Some(Mux::create(path)?);
        *last_at = Some(at);
        let _ = mux.as_mut().unwrap().push(bytes, *last_dur, true)?;
        return Ok(());
    }
    let dur = sample_dur(*last_at, at, *last_dur);
    match mux.as_mut().unwrap().push(bytes, dur, is_idr(bytes))? {
        Push::Written => {
            *last_dur = dur;
            *last_at = Some(at);
        }
        Push::Dropped => {}
        Push::TooLarge => {
            if let Some(m) = mux.take() {
                m.finish()?;
            }
            bail!("the file reached the 4 GB limit");
        }
        Push::OrientFlip => {
            if let Some(m) = mux.take() {
                m.finish()?;
            }
            *path = new_path(path.parent().unwrap_or_else(|| Path::new(".")))?;
            *last_path.lock() = Some(path.clone());
            tracing::info!(
                "record continued {} after orientation change",
                path.display()
            );
            notices.lock().push(format!(
                "Phone rotated — recording {}",
                path.display()
            ));
            *mux = Some(Mux::create(path)?);
            *last_at = Some(at);
            let _ = mux.as_mut().unwrap().push(bytes, *last_dur, true)?;
        }
    }
    Ok(())
}

fn writer_loop(
    rx: Receiver<Msg>,
    first_path: PathBuf,
    last_path: Arc<Mutex<Option<PathBuf>>>,
    notices: &Mutex<Vec<String>>,
) -> Result<()> {
    let mut path = first_path;
    let mut mux: Option<Mux> = None;
    let mut last_at: Option<Instant> = None;
    let mut last_dur = DEFAULT_DUR;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop => break,
            Msg::Au { bytes, at } => {
                if let Err(e) = ingest_au(
                    &mut mux,
                    &mut last_at,
                    &mut last_dur,
                    &mut path,
                    &last_path,
                    notices,
                    &bytes,
                    at,
                ) {
                    if let Some(m) = mux.take() {
                        let _ = m.finish();
                    }
                    return Err(e);
                }
            }
        }
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Stop => {
                    if let Some(m) = mux.take() {
                        m.finish()?;
                    } else {
                        let _ = fs::remove_file(&path);
                    }
                    return Ok(());
                }
                Msg::Au { bytes, at } => {
                    if let Err(e) = ingest_au(
                        &mut mux,
                        &mut last_at,
                        &mut last_dur,
                        &mut path,
                        &last_path,
                        notices,
                        &bytes,
                        at,
                    ) {
                        if let Some(m) = mux.take() {
                            let _ = m.finish();
                        }
                        return Err(e);
                    }
                }
            }
        }
    }
    if let Some(m) = mux.take() {
        m.finish()?;
    } else {
        let _ = fs::remove_file(&path);
        tracing::info!("record: no IDR, deleted empty file");
    }
    Ok(())
}

struct Sample {
    size: u32,
    dur: u32,
    sync: bool,
}

struct Desc {
    width: u16,
    height: u16,
    avcc: Vec<u8>,
    sps: Vec<u8>,
}

struct Chunk {
    off: u64,
    count: u32,
    desc: u32,
}

struct Mux {
    file: File,
    mdat_size_at: u64,
    payload: u64,
    descs: Vec<Desc>,
    chunks: Vec<Chunk>,
    samples: Vec<Sample>,
}

impl Mux {
    fn create(path: &Path) -> Result<Self> {
        let mut file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        file.write_all(&ftyp())?;
        let mdat_size_at = file.stream_position()?;
        file.write_all(&0u32.to_be_bytes())?;
        file.write_all(b"mdat")?;
        Ok(Self {
            file,
            mdat_size_at,
            payload: 0,
            descs: Vec::new(),
            chunks: Vec::new(),
            samples: Vec::new(),
        })
    }

    fn push(&mut self, annex_b: &[u8], dur: u32, sync: bool) -> Result<Push> {
        if let Some((sps, pps)) = sps_pps(annex_b) {
            let changed = self.descs.last().map(|d| d.sps.as_slice() != sps).unwrap_or(true);
            if changed {
                if !sync && !self.descs.is_empty() {
                    return Ok(Push::Dropped);
                }
                let crop = match sps_crop_from_annex_b(annex_b) {
                    Some(c) => c,
                    None => return Ok(Push::Dropped),
                };
                let w = crop.w.max(2) as u16;
                let h = crop.h.max(2) as u16;
                if let Some(first) = self.descs.first() {
                    if orient_swap(first.width, first.height, w, h) {
                        return Ok(Push::OrientFlip);
                    }
                }
                self.descs.push(Desc {
                    width: w,
                    height: h,
                    avcc: avcc_box(&sps, &pps)?,
                    sps,
                });
            }
        } else if self.descs.is_empty() {
            return Ok(Push::Dropped);
        }

        let avcc = annexb_to_avcc(annex_b);
        if avcc.is_empty() {
            return Ok(Push::Dropped);
        }
        if 8u64 + self.payload + avcc.len() as u64 > u32::MAX as u64 {
            return Ok(Push::TooLarge);
        }
        let desc = self.descs.len() as u32;
        let off = self.file.stream_position()?;
        self.file.write_all(&avcc)?;
        self.payload += avcc.len() as u64;
        match self.chunks.last_mut() {
            Some(c) if c.desc == desc => c.count += 1,
            _ => self.chunks.push(Chunk {
                off,
                count: 1,
                desc,
            }),
        }
        self.samples.push(Sample {
            size: avcc.len() as u32,
            dur: dur.max(1),
            sync,
        });
        Ok(Push::Written)
    }

    fn finish(mut self) -> Result<()> {
        let mdat_box = 8u64 + self.payload;
        if mdat_box > u32::MAX as u64 {
            bail!("recording larger than 4 GB");
        }
        self.file.seek(SeekFrom::Start(self.mdat_size_at))?;
        self.file.write_all(&(mdat_box as u32).to_be_bytes())?;
        self.file.seek(SeekFrom::End(0))?;
        let moov = moov_box(&self.samples, &self.descs, &self.chunks)?;
        self.file.write_all(&moov)?;
        self.file.flush()?;
        let (w, h) = self
            .descs
            .first()
            .map(|d| (d.width, d.height))
            .unwrap_or((0, 0));
        tracing::info!(
            "record wrote {} samples {}×{} {} desc(s) {} bytes",
            self.samples.len(),
            w,
            h,
            self.descs.len(),
            8 + self.payload + moov.len() as u64
        );
        Ok(())
    }
}

fn ftyp() -> Vec<u8> {
    let mut b = Vec::new();
    push_box(&mut b, *b"ftyp", |b| {
        b.extend_from_slice(b"isom");
        b.extend_from_slice(&0x200u32.to_be_bytes());
        b.extend_from_slice(b"isomiso2avc1mp41");
    });
    b
}

fn moov_box(samples: &[Sample], descs: &[Desc], chunks: &[Chunk]) -> Result<Vec<u8>> {
    if samples.is_empty() || descs.is_empty() || chunks.is_empty() {
        bail!("no samples");
    }
    let duration: u64 = samples.iter().map(|s| s.dur as u64).sum();
    let width = descs[0].width;
    let height = descs[0].height;
    let mut moov = Vec::new();
    push_box(&mut moov, *b"moov", |moov| {
        push_box(moov, *b"mvhd", |b| {
            b.extend_from_slice(&0u32.to_be_bytes()); // version/flags
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&TIMESCALE.to_be_bytes());
            b.extend_from_slice(&(duration as u32).to_be_bytes());
            b.extend_from_slice(&0x00010000u32.to_be_bytes());
            b.extend_from_slice(&0x0100u16.to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            b.extend_from_slice(&0u32.to_be_bytes());
            identity_matrix(b);
            for _ in 0..6 {
                b.extend_from_slice(&0u32.to_be_bytes());
            }
            b.extend_from_slice(&2u32.to_be_bytes()); // next track id
        });
        push_box(moov, *b"trak", |trak| {
            push_box(trak, *b"tkhd", |b| {
                b.extend_from_slice(&0x00000007u32.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&1u32.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&(duration as u32).to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&0u32.to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes());
                identity_matrix(b);
                b.extend_from_slice(&((width as u32) << 16).to_be_bytes());
                b.extend_from_slice(&((height as u32) << 16).to_be_bytes());
            });
            push_box(trak, *b"mdia", |mdia| {
                push_box(mdia, *b"mdhd", |b| {
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&TIMESCALE.to_be_bytes());
                    b.extend_from_slice(&(duration as u32).to_be_bytes());
                    b.extend_from_slice(&0x55C4u16.to_be_bytes());
                    b.extend_from_slice(&0u16.to_be_bytes());
                });
                push_box(mdia, *b"hdlr", |b| {
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(b"vide");
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(&0u32.to_be_bytes());
                    b.extend_from_slice(b"PocketCam\0");
                });
                push_box(mdia, *b"minf", |minf| {
                    push_box(minf, *b"vmhd", |b| {
                        b.extend_from_slice(&0x00000001u32.to_be_bytes());
                        b.extend_from_slice(&0u16.to_be_bytes());
                        b.extend_from_slice(&0u16.to_be_bytes());
                        b.extend_from_slice(&0u16.to_be_bytes());
                        b.extend_from_slice(&0u16.to_be_bytes());
                    });
                    push_box(minf, *b"dinf", |dinf| {
                        push_box(dinf, *b"dref", |b| {
                            b.extend_from_slice(&0u32.to_be_bytes());
                            b.extend_from_slice(&1u32.to_be_bytes());
                            push_box(b, *b"url ", |u| {
                                u.extend_from_slice(&0x00000001u32.to_be_bytes());
                            });
                        });
                    });
                    push_box(minf, *b"stbl", |stbl| {
                        stsd(stbl, descs);
                        stts(stbl, samples);
                        stss(stbl, samples);
                        stsc(stbl, chunks);
                        stsz(stbl, samples);
                        stco(stbl, chunks);
                    });
                });
            });
        });
    });
    Ok(moov)
}

fn identity_matrix(b: &mut Vec<u8>) {
    const M: [u32; 9] = [
        0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000,
    ];
    for v in M {
        b.extend_from_slice(&v.to_be_bytes());
    }
}

fn stsd(stbl: &mut Vec<u8>, descs: &[Desc]) {
    push_box(stbl, *b"stsd", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(descs.len() as u32).to_be_bytes());
        for d in descs {
            push_box(b, *b"avc1", |avc| {
                avc.extend_from_slice(&[0; 6]);
                avc.extend_from_slice(&1u16.to_be_bytes());
                avc.extend_from_slice(&0u16.to_be_bytes());
                avc.extend_from_slice(&0u16.to_be_bytes());
                avc.extend_from_slice(&0u32.to_be_bytes());
                avc.extend_from_slice(&0u32.to_be_bytes());
                avc.extend_from_slice(&0u32.to_be_bytes());
                avc.extend_from_slice(&d.width.to_be_bytes());
                avc.extend_from_slice(&d.height.to_be_bytes());
                avc.extend_from_slice(&0x00480000u32.to_be_bytes());
                avc.extend_from_slice(&0x00480000u32.to_be_bytes());
                avc.extend_from_slice(&0u32.to_be_bytes());
                avc.extend_from_slice(&1u16.to_be_bytes());
                avc.extend_from_slice(&[0; 32]);
                avc.extend_from_slice(&0x0018u16.to_be_bytes());
                avc.extend_from_slice(&(-1i16).to_be_bytes());
                avc.extend_from_slice(&d.avcc);
            });
        }
    });
}

fn stts(stbl: &mut Vec<u8>, samples: &[Sample]) {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match runs.last_mut() {
            Some((count, dur)) if *dur == s.dur => *count += 1,
            _ => runs.push((1, s.dur)),
        }
    }
    push_box(stbl, *b"stts", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        for (count, dur) in runs {
            b.extend_from_slice(&count.to_be_bytes());
            b.extend_from_slice(&dur.to_be_bytes());
        }
    });
}

fn stss(stbl: &mut Vec<u8>, samples: &[Sample]) {
    let idx: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.sync)
        .map(|(i, _)| (i + 1) as u32)
        .collect();
    push_box(stbl, *b"stss", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(idx.len() as u32).to_be_bytes());
        for n in idx {
            b.extend_from_slice(&n.to_be_bytes());
        }
    });
}

fn stsc(stbl: &mut Vec<u8>, chunks: &[Chunk]) {
    push_box(stbl, *b"stsc", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for (i, c) in chunks.iter().enumerate() {
            b.extend_from_slice(&((i + 1) as u32).to_be_bytes());
            b.extend_from_slice(&c.count.to_be_bytes());
            b.extend_from_slice(&c.desc.to_be_bytes());
        }
    });
}

fn stsz(stbl: &mut Vec<u8>, samples: &[Sample]) {
    push_box(stbl, *b"stsz", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        for s in samples {
            b.extend_from_slice(&s.size.to_be_bytes());
        }
    });
}

fn stco(stbl: &mut Vec<u8>, chunks: &[Chunk]) {
    push_box(stbl, *b"stco", |b| {
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&(chunks.len() as u32).to_be_bytes());
        for c in chunks {
            b.extend_from_slice(&(c.off as u32).to_be_bytes());
        }
    });
}

fn push_box<F: FnOnce(&mut Vec<u8>)>(out: &mut Vec<u8>, typ: [u8; 4], body: F) {
    let start = out.len();
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&typ);
    body(out);
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

fn avcc_box(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    if sps.len() < 4 || pps.is_empty() {
        bail!("bad SPS/PPS");
    }
    let mut avcc = Vec::new();
    push_box(&mut avcc, *b"avcC", |b| {
        b.push(1);
        b.push(sps[1]);
        b.push(sps[2]);
        b.push(sps[3]);
        b.push(0xFF);
        b.push(0xE1);
        b.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        b.extend_from_slice(sps);
        b.push(1);
        b.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        b.extend_from_slice(pps);
    });
    Ok(avcc)
}

fn sps_pps(annex_b: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut sps = None;
    let mut pps = None;
    for nal in nals(annex_b) {
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1F {
            7 => sps = Some(nal.to_vec()),
            8 => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    Some((sps?, pps?))
}

fn annexb_to_avcc(annex_b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals(annex_b) {
        if nal.is_empty() {
            continue;
        }
        let t = nal[0] & 0x1F;
        if t == 7 || t == 8 || t == 9 || t == 12 {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn nals(annex_b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push(i + 4);
            i += 4;
        } else if annex_b[i..].starts_with(&[0, 0, 1]) {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = starts.get(k + 1).copied().unwrap_or(annex_b.len());
        let mut end = e;
        if k + 1 < starts.len() {
            let sc = if annex_b[starts[k + 1] - 4..starts[k + 1]].starts_with(&[0, 0, 0, 1]) {
                4
            } else {
                3
            };
            end = starts[k + 1] - sc;
        }
        if s < end {
            out.push(&annex_b[s..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let mut buf = vec![0, 0, 0, 1, 0x67, 1, 2];
        buf.extend_from_slice(&[0, 0, 0, 1, 0x68, 3]);
        buf.extend_from_slice(&[0, 0, 0, 1, 0x65, 9, 9, 9]);
        let n = nals(&buf);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0], &[0x67, 1, 2]);
        assert_eq!(n[2], &[0x65, 9, 9, 9]);
        let avcc = annexb_to_avcc(&buf);
        assert_eq!(&avcc[..4], &4u32.to_be_bytes());
        assert_eq!(&avcc[4..], &[0x65, 9, 9, 9]);
    }

    #[test]
    fn sample_dur_clamps_rtp_like_jumps() {
        let a = Instant::now();
        let b = a + std::time::Duration::from_millis(33);
        let d = sample_dur(Some(a), b, DEFAULT_DUR);
        assert!((2800..=3200).contains(&d), "33ms → {d}");
        let c = a + std::time::Duration::from_millis(370);
        let stretched = sample_dur(Some(a), c, DEFAULT_DUR);
        assert_eq!(stretched, MAX_DUR);
        assert_eq!(sample_dur(None, a, DEFAULT_DUR), DEFAULT_DUR);
    }

    #[test]
    fn orient_swap_is_wh_flip_only() {
        assert!(orient_swap(1920, 1080, 1080, 1920));
        assert!(orient_swap(1080, 1920, 1920, 1080));
        assert!(!orient_swap(1920, 1080, 1920, 1080));
        assert!(!orient_swap(1920, 1080, 1280, 720));
        assert!(!orient_swap(1080, 1080, 1080, 1080));
    }
}
