//! Cheap host CPU / RAM / NIC / GPU samples for the preview overlay. At most ~2 Hz.

use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::core::{w, Interface};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIAdapter3, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL,
    DXGI_QUERY_VIDEO_MEMORY_INFO,
};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, GetSystemTimes,
};

const MIN_DT: Duration = Duration::from_millis(400);
const PDH_FMT_NOCAP100: u32 = 0x8000;

#[derive(Clone)]
pub struct SysLoad {
    pub cpu_app_pct: f32,
    pub cpu_pct: f32,
    pub ram_pct: f32,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    pub proc_mb: f32,
    pub nic_down_kbps: f32,
    pub nic_up_kbps: f32,
    pub gpu_name: String,
    pub gpu_3d_app: f32,
    pub gpu_3d_sys: f32,
    pub gpu_copy_app: f32,
    pub gpu_copy_sys: f32,
    pub gpu_vdec_app: f32,
    pub gpu_vdec_sys: f32,
    pub gpu_vp_app: f32,
    pub gpu_vp_sys: f32,
    pub gpu_compute_app: f32,
    pub gpu_compute_sys: f32,
    pub gpu_vram_used_mb: f32,
    pub gpu_vram_budget_mb: f32,
    pub gpu_shared_used_mb: f32,
    pub gpu_shared_budget_mb: f32,
}

impl Default for SysLoad {
    fn default() -> Self {
        Self {
            cpu_app_pct: 0.0,
            cpu_pct: 0.0,
            ram_pct: 0.0,
            ram_used_gb: 0.0,
            ram_total_gb: 0.0,
            proc_mb: 0.0,
            nic_down_kbps: 0.0,
            nic_up_kbps: 0.0,
            gpu_name: String::new(),
            gpu_3d_app: 0.0,
            gpu_3d_sys: 0.0,
            gpu_copy_app: 0.0,
            gpu_copy_sys: 0.0,
            gpu_vdec_app: 0.0,
            gpu_vdec_sys: 0.0,
            gpu_vp_app: 0.0,
            gpu_vp_sys: 0.0,
            gpu_compute_app: 0.0,
            gpu_compute_sys: 0.0,
            gpu_vram_used_mb: 0.0,
            gpu_vram_budget_mb: 0.0,
            gpu_shared_used_mb: 0.0,
            gpu_shared_budget_mb: 0.0,
        }
    }
}

/// Host is too loaded to keep converting RGB. Virtual camera and record stay native.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewHostStress {
    Cpu,
    AppCpu,
    Ram,
    Proc,
    Gpu,
    Vram,
    Decode,
}

impl PreviewHostStress {
    pub fn toast(self) -> &'static str {
        match self {
            Self::Cpu | Self::AppCpu => {
                "Preview off — CPU is maxed. Virtual camera and record stay native."
            }
            Self::Ram => "Preview off — host memory is too high.",
            Self::Proc => "Preview off — PocketCam memory is too high.",
            Self::Gpu => {
                "Preview off — GPU is maxed. Virtual camera and record stay native."
            }
            Self::Vram => "Preview off — GPU memory is too high.",
            Self::Decode => {
                "Preview off — decode is too slow. Virtual camera and record stay native."
            }
        }
    }

    pub fn hold(self) -> Duration {
        match self {
            Self::Ram | Self::Proc | Self::Vram => Duration::from_millis(250),
            Self::Cpu | Self::AppCpu | Self::Gpu | Self::Decode => {
                Duration::from_millis(800)
            }
        }
    }

    pub fn is_oom(self) -> bool {
        matches!(self, Self::Ram | Self::Proc | Self::Vram)
    }
}

/// None = RGB preview is still affordable.
pub fn preview_host_stress(sys: &SysLoad, decode_ms: f32) -> Option<PreviewHostStress> {
    if sys.ram_total_gb > 0.0 {
        let avail = (sys.ram_total_gb - sys.ram_used_gb).max(0.0);
        if avail < 1.5 || sys.ram_pct >= 80.0 {
            return Some(PreviewHostStress::Ram);
        }
        if sys.proc_mb >= 450.0 {
            return Some(PreviewHostStress::Proc);
        }
    }
    if sys.cpu_pct >= 75.0 {
        return Some(PreviewHostStress::Cpu);
    }
    if sys.cpu_app_pct >= 22.0 {
        return Some(PreviewHostStress::AppCpu);
    }
    if sys.gpu_3d_sys >= 75.0 {
        return Some(PreviewHostStress::Gpu);
    }
    if sys.gpu_vram_budget_mb > 256.0 {
        let pct = sys.gpu_vram_used_mb / sys.gpu_vram_budget_mb * 100.0;
        if pct >= 75.0 {
            return Some(PreviewHostStress::Vram);
        }
    }
    if sys.gpu_shared_budget_mb > 256.0 {
        let pct = sys.gpu_shared_used_mb / sys.gpu_shared_budget_mb * 100.0;
        if pct >= 75.0 {
            return Some(PreviewHostStress::Vram);
        }
    }
    if decode_ms >= 28.0 {
        return Some(PreviewHostStress::Decode);
    }
    None
}

struct GpuPdh {
    query: isize,
    counter: isize,
}

impl Drop for GpuPdh {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

pub struct SysSampler {
    last_idle: u64,
    last_kernel: u64,
    last_user: u64,
    last_proc_kernel: u64,
    last_proc_user: u64,
    last_nic_in: u64,
    last_nic_out: u64,
    last_at: Option<Instant>,
    cached: SysLoad,
    gpu: Option<GpuPdh>,
    pid: u32,
}

impl SysSampler {
    pub fn new() -> Self {
        Self {
            last_idle: 0,
            last_kernel: 0,
            last_user: 0,
            last_proc_kernel: 0,
            last_proc_user: 0,
            last_nic_in: 0,
            last_nic_out: 0,
            last_at: None,
            cached: SysLoad::default(),
            gpu: open_gpu_pdh(),
            pid: unsafe { GetCurrentProcessId() },
        }
    }

    pub fn sample(&mut self) -> SysLoad {
        if let Some(t) = self.last_at {
            if t.elapsed() < MIN_DT {
                return self.cached.clone();
            }
        }
        let dt = self
            .last_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        let mut load = read_load(
            &mut self.last_idle,
            &mut self.last_kernel,
            &mut self.last_user,
            &mut self.last_proc_kernel,
            &mut self.last_proc_user,
            &mut self.last_nic_in,
            &mut self.last_nic_out,
            self.last_at.is_some(),
            dt,
        );
        fill_gpu(&mut load, self.gpu.as_ref(), self.pid);
        self.cached = load;
        self.last_at = Some(Instant::now());
        self.cached.clone()
    }
}

fn filetime_u64(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

fn read_load(
    last_idle: &mut u64,
    last_kernel: &mut u64,
    last_user: &mut u64,
    last_proc_kernel: &mut u64,
    last_proc_user: &mut u64,
    last_nic_in: &mut u64,
    last_nic_out: &mut u64,
    have_prev: bool,
    dt: f32,
) -> SysLoad {
    let mut load = SysLoad::default();
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let mut sys_total = 0u64;
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            let idle_t = filetime_u64(idle);
            let kernel_t = filetime_u64(kernel);
            let user_t = filetime_u64(user);
            if have_prev {
                let di = idle_t.saturating_sub(*last_idle);
                let dk = kernel_t.saturating_sub(*last_kernel);
                let du = user_t.saturating_sub(*last_user);
                sys_total = dk.saturating_add(du);
                if sys_total > 0 {
                    // Kernel time includes idle.
                    load.cpu_pct = (1.0 - di as f32 / sys_total as f32) * 100.0;
                    load.cpu_pct = load.cpu_pct.clamp(0.0, 100.0);
                }
            }
            *last_idle = idle_t;
            *last_kernel = kernel_t;
            *last_user = user_t;
        }

        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut proc_kernel = FILETIME::default();
        let mut proc_user = FILETIME::default();
        if GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut proc_kernel,
            &mut proc_user,
        )
        .is_ok()
        {
            let pk = filetime_u64(proc_kernel);
            let pu = filetime_u64(proc_user);
            if have_prev && sys_total > 0 {
                let dp = pk
                    .saturating_sub(*last_proc_kernel)
                    .saturating_add(pu.saturating_sub(*last_proc_user));
                load.cpu_app_pct = (dp as f32 / sys_total as f32 * 100.0).clamp(0.0, 100.0);
            }
            *last_proc_kernel = pk;
            *last_proc_user = pu;
        }

        let mut mem = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut mem).is_ok() {
            load.ram_pct = mem.dwMemoryLoad as f32;
            load.ram_total_gb = mem.ullTotalPhys as f32 / 1e9;
            let avail = mem.ullAvailPhys as f32 / 1e9;
            load.ram_used_gb = (load.ram_total_gb - avail).max(0.0);
        }

        let mut pmc = PROCESS_MEMORY_COUNTERS::default();
        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb).is_ok() {
            load.proc_mb = pmc.WorkingSetSize as f32 / (1024.0 * 1024.0);
        }

        if let Some((inn, out)) = nic_octets() {
            if have_prev && dt > 0.05 {
                let din = inn.saturating_sub(*last_nic_in) as f32;
                let dout = out.saturating_sub(*last_nic_out) as f32;
                load.nic_down_kbps = din * 8.0 / dt / 1000.0;
                load.nic_up_kbps = dout * 8.0 / dt / 1000.0;
            }
            *last_nic_in = inn;
            *last_nic_out = out;
        }
    }
    load
}

/// Busiest up, non-loopback adapter. Wi-Fi + vEthernet can both be up; the
/// one with the most lifetime unicast octets is usually the path in use.
unsafe fn nic_octets() -> Option<(u64, u64)> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if GetIfTable2(&mut table).0 != 0 || table.is_null() {
        return None;
    }
    let n = (*table).NumEntries as usize;
    let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), n);
    let mut best: Option<(u64, u64, u64)> = None;
    for row in rows {
        if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
            continue;
        }
        if row.OperStatus != IfOperStatusUp {
            continue;
        }
        let traffic = row.InUcastOctets.saturating_add(row.OutUcastOctets);
        let better = best.map(|(t, _, _)| traffic > t).unwrap_or(true);
        if better {
            best = Some((traffic, row.InOctets, row.OutOctets));
        }
    }
    FreeMibTable(table as *const _);
    best.map(|(_, inn, out)| (inn, out))
}

fn open_gpu_pdh() -> Option<GpuPdh> {
    unsafe {
        let mut query = 0isize;
        if PdhOpenQueryW(windows::core::PCWSTR::null(), 0, &mut query) != 0 {
            return None;
        }
        let mut counter = 0isize;
        let err = PdhAddEnglishCounterW(
            query,
            w!("\\GPU Engine(*)\\Utilization Percentage"),
            0,
            &mut counter,
        );
        if err != 0 {
            let _ = PdhCloseQuery(query);
            return None;
        }
        let _ = PdhCollectQueryData(query);
        Some(GpuPdh { query, counter })
    }
}

struct EngineUse {
    d3: f32,
    copy: f32,
    vdec: f32,
    vp: f32,
    compute: f32,
    other: f32,
}

impl EngineUse {
    fn add(&mut self, kind: EngineKind, v: f32) {
        let slot = match kind {
            EngineKind::ThreeD => &mut self.d3,
            EngineKind::Copy => &mut self.copy,
            EngineKind::VideoDecode => &mut self.vdec,
            EngineKind::VideoProcess => &mut self.vp,
            EngineKind::Compute => &mut self.compute,
            EngineKind::Other => &mut self.other,
        };
        *slot = (*slot + v).min(100.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EngineKind {
    ThreeD,
    Copy,
    VideoDecode,
    VideoProcess,
    Compute,
    Other,
}

struct GpuHit {
    pid: u32,
    luid_key: String,
    kind: EngineKind,
    v: f32,
    luid: Option<(u32, i32)>,
}

fn fill_gpu(load: &mut SysLoad, pdh: Option<&GpuPdh>, pid: u32) {
    let mut hits: Vec<GpuHit> = Vec::new();
    if let Some(pdh) = pdh {
        unsafe {
            let _ = PdhCollectQueryData(pdh.query);
            let fmt = PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100);
            let mut bytes = 0u32;
            let mut count = 0u32;
            let st = PdhGetFormattedCounterArrayW(pdh.counter, fmt, &mut bytes, &mut count, None);
            if st == 0 || st == PDH_MORE_DATA {
                if bytes > 0 {
                    let mut buf = vec![0u8; bytes as usize];
                    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                    if PdhGetFormattedCounterArrayW(
                        pdh.counter,
                        fmt,
                        &mut bytes,
                        &mut count,
                        Some(items),
                    ) == 0
                    {
                        for i in 0..count as usize {
                            let item = &*items.add(i);
                            if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                                && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
                            {
                                continue;
                            }
                            let name = item.szName.to_string().unwrap_or_default();
                            let v = item.FmtValue.Anonymous.doubleValue as f32;
                            if !v.is_finite() || v <= 0.0 {
                                continue;
                            }
                            hits.push(GpuHit {
                                pid: instance_pid(&name).unwrap_or(0),
                                luid_key: instance_luid_key(&name)
                                    .unwrap_or_default()
                                    .to_string(),
                                kind: engine_kind(&name),
                                v,
                                luid: instance_luid(&name),
                            });
                        }
                    }
                }
            }
        }
    }

    let luid_key = pick_luid_key(&hits, pid);
    let hits: Vec<GpuHit> = match luid_key.as_deref() {
        Some(key) if !key.is_empty() => hits
            .into_iter()
            .filter(|h| h.luid_key == key)
            .collect(),
        _ => hits,
    };

    let mut app = EngineUse {
        d3: 0.0,
        copy: 0.0,
        vdec: 0.0,
        vp: 0.0,
        compute: 0.0,
        other: 0.0,
    };
    let mut sys = EngineUse {
        d3: 0.0,
        copy: 0.0,
        vdec: 0.0,
        vp: 0.0,
        compute: 0.0,
        other: 0.0,
    };
    let mut app_luid: Option<(u32, i32)> = None;
    for h in &hits {
        sys.add(h.kind, h.v);
        let ours = h.pid == pid;
        if ours {
            app.add(h.kind, h.v);
            if app_luid.is_none() {
                app_luid = h.luid;
            }
        }
    }
    load.gpu_3d_app = app.d3.min(sys.d3);
    load.gpu_3d_sys = sys.d3;
    load.gpu_copy_app = app.copy.min(sys.copy);
    load.gpu_copy_sys = sys.copy;
    load.gpu_vdec_app = app.vdec.min(sys.vdec);
    load.gpu_vdec_sys = sys.vdec;
    load.gpu_vp_app = app.vp.min(sys.vp);
    load.gpu_vp_sys = sys.vp;
    load.gpu_compute_app = app.compute.min(sys.compute);
    load.gpu_compute_sys = sys.compute;
    fill_gpu_memory(load, app_luid);
}

fn pick_luid_key(hits: &[GpuHit], pid: u32) -> Option<String> {
    let mut ours: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    let mut all: std::collections::HashMap<&str, f32> = std::collections::HashMap::new();
    for h in hits {
        if h.luid_key.is_empty() {
            continue;
        }
        *all.entry(&h.luid_key).or_insert(0.0) += h.v;
        if h.pid == pid {
            *ours.entry(&h.luid_key).or_insert(0.0) += h.v;
        }
    }
    let src = if ours.is_empty() { &all } else { &ours };
    src.iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(k, _)| (*k).to_string())
}

fn engine_suffix(name: &str) -> String {
    name.rsplit("engtype_").next().unwrap_or(name).to_string()
}

fn engine_kind(name: &str) -> EngineKind {
    let t: String = engine_suffix(name)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if t.starts_with("3d") || t.starts_with("graphics") {
        EngineKind::ThreeD
    } else if t.starts_with("copy") {
        EngineKind::Copy
    } else if t.contains("videodecode") || t.contains("decode") || t.contains("codec") {
        EngineKind::VideoDecode
    } else if t.contains("videoprocess") || t.contains("videoproc") {
        EngineKind::VideoProcess
    } else if t.starts_with("compute") {
        EngineKind::Compute
    } else {
        EngineKind::Other
    }
}

fn instance_pid(name: &str) -> Option<u32> {
    let rest = name.split("pid_").nth(1)?;
    let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    n.parse().ok()
}

fn instance_luid_key(name: &str) -> Option<&str> {
    let i = name.find("luid_")?;
    let rest = &name[i..];
    let body = rest.get(5..)?;
    let end = body.find("_phys").or_else(|| body.find("_eng"))?;
    Some(&rest[..5 + end])
}

fn instance_luid(name: &str) -> Option<(u32, i32)> {
    let i = name.find("luid_")?;
    let rest = &name[i + 5..];
    let mut parts = rest.split('_');
    let high = i32::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    let low = u32::from_str_radix(parts.next()?.trim_start_matches("0x"), 16).ok()?;
    Some((low, high))
}

fn fill_gpu_memory(load: &mut SysLoad, prefer: Option<(u32, i32)>) {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return;
        };
        let mut best: Option<(usize, String, f32, f32, f32, f32)> = None;
        let mut matched = None;
        for i in 0..8u32 {
            let Ok(adapter) = factory.EnumAdapters1(i) else {
                break;
            };
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let name = short_gpu_name(&utf16_z(&desc.Description));
            if name.is_empty() || name.contains("Microsoft Basic") {
                continue;
            }
            let (used, budget, shared, shared_budget) = adapter_memory(&adapter);
            let dedicated = desc.DedicatedVideoMemory;
            let row = (dedicated, name, used, budget, shared, shared_budget);
            let luid = (desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart);
            if prefer == Some(luid) {
                matched = Some(row.clone());
            }
            let take = best.as_ref().map(|(d, _, _, _, _, _)| dedicated > *d).unwrap_or(true);
            if take {
                best = Some(row);
            }
        }
        let picked = matched.or(best);
        if let Some((_, name, used, budget, shared, shared_budget)) = picked {
            load.gpu_name = name;
            load.gpu_vram_used_mb = used;
            load.gpu_vram_budget_mb = budget;
            load.gpu_shared_used_mb = shared;
            load.gpu_shared_budget_mb = shared_budget;
        }
    }
}

fn adapter_memory(adapter: &IDXGIAdapter1) -> (f32, f32, f32, f32) {
    unsafe {
        let Ok(a3) = adapter.cast::<IDXGIAdapter3>() else {
            return (0.0, 0.0, 0.0, 0.0);
        };
        let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let _ = a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local);
        let _ = a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared);
        (
            local.CurrentUsage as f32 / (1024.0 * 1024.0),
            local.Budget as f32 / (1024.0 * 1024.0),
            shared.CurrentUsage as f32 / (1024.0 * 1024.0),
            shared.Budget as f32 / (1024.0 * 1024.0),
        )
    }
}

fn utf16_z(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn short_gpu_name(raw: &str) -> String {
    raw.replace("NVIDIA GeForce ", "")
        .replace("NVIDIA ", "")
        .replace("Intel(R) ", "")
        .replace("Intel ", "")
        .replace("AMD Radeon ", "")
        .replace("AMD ", "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load() -> SysLoad {
        let mut s = SysLoad::default();
        s.ram_total_gb = 16.0;
        s.ram_used_gb = 8.0;
        s.ram_pct = 50.0;
        s.proc_mb = 200.0;
        s.cpu_pct = 40.0;
        s.cpu_app_pct = 10.0;
        s
    }

    #[test]
    fn idle_host_keeps_preview() {
        assert_eq!(preview_host_stress(&load(), 8.0), None);
    }

    #[test]
    fn system_cpu_75_drops_preview() {
        let mut s = load();
        s.cpu_pct = 76.0;
        assert_eq!(
            preview_host_stress(&s, 8.0),
            Some(PreviewHostStress::Cpu)
        );
    }

    #[test]
    fn ram_and_vram_drop_preview() {
        let mut s = load();
        s.ram_used_gb = 15.5;
        s.ram_pct = 96.0;
        assert_eq!(
            preview_host_stress(&s, 8.0),
            Some(PreviewHostStress::Ram)
        );
        s = load();
        s.gpu_vram_used_mb = 7800.0;
        s.gpu_vram_budget_mb = 8000.0;
        assert_eq!(
            preview_host_stress(&s, 8.0),
            Some(PreviewHostStress::Vram)
        );
        assert_eq!(
            preview_host_stress(&load(), 30.0),
            Some(PreviewHostStress::Decode)
        );
    }
}
