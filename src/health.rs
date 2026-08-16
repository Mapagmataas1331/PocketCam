//! Launch checks the installer will later take over (firewall rule).
//! Certificate trust on the phone cannot be probed from here.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegQueryValueExW, HKEY,
    HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirewallState {
    /// Inbound TCP for this exe or port is allowed, or the firewall is off.
    Allowed,
    /// Firewall is on and no matching allow rule. Windows may still prompt.
    Unknown,
}

impl FirewallState {
    pub fn inbound_tcp(port: u16) -> Self {
        match inbound_allowed(port) {
            Ok(true) => Self::Allowed,
            _ => Self::Unknown,
        }
    }
}

fn inbound_allowed(port: u16) -> anyhow::Result<bool> {
    if !any_profile_on()? {
        return Ok(true);
    }
    let exe = std::env::current_exe().ok();
    let exe_name = exe
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_ascii_lowercase());
    let exe_s = exe.as_ref().map(|p| normalize_path(p));

    for raw in enum_firewall_rules()? {
        let rule = parse_rule(&raw);
        if !rule.active || !rule.inbound {
            continue;
        }
        if rule.action != Action::Allow {
            continue;
        }
        if !rule.tcp_ok {
            continue;
        }
        let port_ok = rule.any_port || rule.ports.contains(&port);
        let app_ok = match (&rule.app, &exe_s, &exe_name) {
            (None, _, _) => true,
            (Some(app), Some(exe), _) if app == exe => true,
            (Some(app), _, Some(name)) if app.rsplit(['\\', '/']).next() == Some(name.as_str()) => {
                true
            }
            _ => false,
        };
        if app_ok && port_ok {
            return Ok(true);
        }
        if rule.app.is_none() && port_ok {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Allow,
    Other,
}

struct Rule {
    active: bool,
    inbound: bool,
    action: Action,
    tcp_ok: bool,
    any_port: bool,
    ports: Vec<u16>,
    app: Option<String>,
}

fn parse_rule(raw: &str) -> Rule {
    let mut active = false;
    let mut inbound = false;
    let mut action = Action::Other;
    let mut tcp_ok = true;
    let mut any_port = true;
    let mut ports = Vec::new();
    let mut app = None;
    for part in raw.split('|') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k {
            "Active" => active = v.eq_ignore_ascii_case("TRUE"),
            "Dir" => inbound = v.eq_ignore_ascii_case("In"),
            "Action" => {
                action = if v.eq_ignore_ascii_case("Allow") {
                    Action::Allow
                } else {
                    Action::Other
                };
            }
            "Protocol" => {
                tcp_ok = v.is_empty() || v == "*" || v == "6";
            }
            "LPort" => {
                any_port = false;
                if v == "*" {
                    any_port = true;
                } else if let Ok(p) = v.parse() {
                    ports.push(p);
                }
            }
            "App" => {
                if v != "*" && !v.is_empty() {
                    app = Some(normalize_path(Path::new(v)));
                }
            }
            _ => {}
        }
    }
    Rule {
        active,
        inbound,
        action,
        tcp_ok,
        any_port,
        ports,
        app,
    }
}

fn normalize_path(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").to_ascii_lowercase()
}

fn any_profile_on() -> anyhow::Result<bool> {
    const PROFILES: &[&str] = &[
        r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\DomainProfile",
        r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\StandardProfile",
        r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\PublicProfile",
    ];
    let mut saw = false;
    for path in PROFILES {
        if let Ok(v) = dword(path, "EnableFirewall") {
            saw = true;
            if v != 0 {
                return Ok(true);
            }
        }
    }
    Ok(if saw { false } else { true })
}

fn dword(subkey: &str, name: &str) -> anyhow::Result<u32> {
    unsafe {
        let mut h = HKEY::default();
        let key = wide(subkey);
        let val = wide(name);
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            0,
            KEY_READ,
            &mut h,
        ) != ERROR_SUCCESS
        {
            anyhow::bail!("open {subkey}");
        }
        let mut kind = 0u32;
        let mut buf = [0u8; 4];
        let mut len = buf.len() as u32;
        let st = RegQueryValueExW(
            h,
            PCWSTR(val.as_ptr()),
            None,
            Some(&mut kind as *mut u32 as *mut REG_VALUE_TYPE),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = RegCloseKey(h);
        if st != ERROR_SUCCESS {
            anyhow::bail!("query {name}");
        }
        if kind != REG_DWORD.0 || len < 4 {
            anyhow::bail!("not dword");
        }
        Ok(u32::from_le_bytes(buf))
    }
}

fn enum_firewall_rules() -> anyhow::Result<Vec<String>> {
    const PATH: &str =
        r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
    unsafe {
        let mut h = HKEY::default();
        let key = wide(PATH);
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            0,
            KEY_READ,
            &mut h,
        ) != ERROR_SUCCESS
        {
            anyhow::bail!("open FirewallRules");
        }
        let mut value_count = 0u32;
        let st = RegQueryInfoKeyW(
            h,
            windows::core::PWSTR::null(),
            None,
            None,
            None,
            None,
            None,
            Some(&mut value_count),
            None,
            None,
            None,
            None,
        );
        if st != ERROR_SUCCESS {
            let _ = RegCloseKey(h);
            anyhow::bail!("RegQueryInfoKeyW");
        }
        let mut out = Vec::new();
        for i in 0..value_count {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let mut kind = 0u32;
            let mut data = [0u16; 2048];
            let mut data_bytes = (data.len() * 2) as u32;
            let st = RegEnumValueW(
                h,
                i,
                windows::core::PWSTR(name.as_mut_ptr()),
                &mut name_len,
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr() as *mut u8),
                Some(&mut data_bytes),
            );
            if st != ERROR_SUCCESS || kind != REG_SZ.0 {
                continue;
            }
            let nchars = (data_bytes as usize / 2).saturating_sub(1);
            let s = String::from_utf16_lossy(&data[..nchars.min(data.len())]);
            out.push(s);
        }
        let _ = RegCloseKey(h);
        Ok(out)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_allow_all_ports() {
        let r = parse_rule(
            "v2.32|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=C:\\Program Files\\PocketCam\\pocketcam.exe|Name=PocketCam HTTPS|",
        );
        assert!(r.active && r.inbound && r.any_port);
        assert_eq!(r.action, Action::Allow);
        assert!(r.app.unwrap().contains("pocketcam.exe"));
    }

    #[test]
    fn parses_port_rule() {
        let r = parse_rule("v2.10|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=8443|Name=x|");
        assert!(!r.any_port);
        assert_eq!(r.ports, vec![8443]);
        assert!(r.app.is_none());
    }
}
