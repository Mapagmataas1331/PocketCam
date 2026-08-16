//! Persisted host settings. Lives in `%LOCALAPPDATA%\PocketCam\settings.json`.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cert::lan_named_ipv4s;

pub const DEFAULT_PORT: u16 = 8443;
pub const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Windows adapter name. `None` = auto (skip Docker / Hyper-V / VMware / VirtualBox).
    #[serde(default)]
    pub adapter: Option<String>,
    /// HTTPS listen port. Applied when PocketCam starts.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Google STUN for WebRTC ICE. Off by default so ICE stays on the LAN.
    #[serde(default)]
    pub stun: bool,
    /// Recordings folder. `None` / empty = `%USERPROFILE%\Videos\PocketCam`.
    #[serde(default)]
    pub recordings: Option<String>,
    /// Start the Windows virtual camera when PocketCam launches.
    #[serde(default)]
    pub vcam_on_launch: bool,
    /// Keep RGB preview on. Skip the auto-off watchdog.
    #[serde(default)]
    pub keep_preview: bool,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            adapter: None,
            port: DEFAULT_PORT,
            stun: false,
            recordings: None,
            vcam_on_launch: false,
            keep_preview: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Adapter {
    pub name: String,
    pub ip: Ipv4Addr,
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::cert::user_dir().join("settings.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match fs_load(&path) {
            Ok(s) => {
                tracing::info!("settings {}", path.display());
                s
            }
            Err(e) => {
                tracing::info!("settings default ({e:#})");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let mut body = serde_json::to_string_pretty(self)?;
        body.push('\n');
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn clamp_port(&mut self) {
        if self.port == 0 {
            self.port = DEFAULT_PORT;
        }
    }

    /// IPv4 printed in the QR. Auto skips Docker / Hyper-V / VMware / VirtualBox.
    pub fn resolve_lan(&self) -> Option<Ipv4Addr> {
        let nics = list_adapters();
        if let Some(name) = self.adapter.as_deref() {
            if let Some(n) = nics.iter().find(|n| n.name == name) {
                return Some(n.ip);
            }
        }
        nics.into_iter()
            .find(|n| !is_virtual_adapter(&n.name, n.ip))
            .map(|n| n.ip)
    }

    pub fn recordings_dir(&self) -> PathBuf {
        match self.recordings.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_recordings_dir(),
        }
    }
}

pub fn default_recordings_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Videos").join("PocketCam")
}

/// Empty → default folder. Non-empty paths are created so Record does not fail later.
pub fn validate_recordings_dir(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let p = PathBuf::from(trimmed);
    std::fs::create_dir_all(&p).with_context(|| format!("create {}", p.display()))?;
    Ok(Some(trimmed.to_string()))
}

pub fn open_in_explorer(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::process::Command::new("explorer")
        .arg(dir)
        .spawn()
        .context("open Explorer")?;
    Ok(())
}

pub fn pick_recordings_folder() -> Option<PathBuf> {
    unsafe { pick_folder() }.ok().flatten()
}

unsafe fn pick_folder() -> Result<Option<PathBuf>> {
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileDialog, IFileOpenDialog, IShellItem, FOS_FORCEFILESYSTEM,
        FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let dialog: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).context("FileOpenDialog")?;
    let as_dlg: IFileDialog = dialog.cast().context("IFileDialog")?;
    as_dlg
        .SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
        .context("SetOptions")?;
    let owner = FindWindowW(None, windows::core::w!("PocketCam")).unwrap_or_default();
    if as_dlg.Show(owner).is_err() {
        return Ok(None);
    }
    let item: IShellItem = as_dlg.GetResult().context("GetResult")?;
    let pw = item
        .GetDisplayName(SIGDN_FILESYSPATH)
        .context("GetDisplayName")?;
    let path = PathBuf::from(pw.to_string().unwrap_or_default());
    windows::Win32::System::Com::CoTaskMemFree(Some(pw.0 as *mut core::ffi::c_void));
    if path.as_os_str().is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

pub fn list_adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    for (name, ip) in lan_named_ipv4s() {
        out.push(Adapter { name, ip });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.ip.cmp(&b.ip)));
    out
}

pub fn is_virtual_lan(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // Docker default bridges, VirtualBox host-only / DHCP.
    (o[0] == 172 && (o[1] == 17 || o[1] == 18))
        || (o[0] == 192 && o[1] == 168 && (o[2] == 56 || o[2] == 59))
}

pub fn is_virtual_adapter(name: &str, ip: Ipv4Addr) -> bool {
    let n = name.to_ascii_lowercase();
    const HINTS: &[&str] = &[
        "wsl",
        "vethernet",
        "hyper-v",
        "vmware",
        "virtualbox",
        "vbox",
        "docker",
        "loopback",
        "bluetooth",
    ];
    HINTS.iter().any(|h| n.contains(h)) || is_virtual_lan(ip)
}

pub fn ice_servers(stun: bool) -> Vec<String> {
    if stun {
        vec![DEFAULT_STUN.to_string()]
    } else {
        Vec::new()
    }
}

fn fs_load(path: &Path) -> Result<Settings> {
    let raw = std::fs::read_to_string(path).context("read")?;
    let mut s: Settings = serde_json::from_str(&raw).context("parse")?;
    s.clamp_port();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn auto_skips_docker_vbox_and_named_virtual_nics() {
        assert!(is_virtual_lan(Ipv4Addr::new(172, 17, 0, 2)));
        assert!(is_virtual_lan(Ipv4Addr::new(172, 18, 0, 1)));
        assert!(is_virtual_lan(Ipv4Addr::new(192, 168, 56, 1)));
        assert!(!is_virtual_lan(Ipv4Addr::new(192, 168, 1, 10)));
        assert!(is_virtual_adapter("vEthernet (WSL)", Ipv4Addr::new(172, 22, 32, 1)));
        assert!(is_virtual_adapter("VMware Network Adapter VMnet8", Ipv4Addr::new(192, 168, 79, 1)));
        assert!(!is_virtual_adapter("Wi-Fi", Ipv4Addr::new(192, 168, 1, 20)));
    }

    #[test]
    fn stun_defaults_off() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(!s.stun);
        assert_eq!(s.port, DEFAULT_PORT);
    }
}
