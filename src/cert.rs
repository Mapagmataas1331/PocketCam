//! Self-signed TLS. Phones must continue past the certificate warning
//! until a local CA ships with the installer.

use std::fs;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, Ia5String, KeyPair, SanType};

pub struct AppCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub names: Vec<String>,
}

pub fn user_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("PocketCam")
}

pub fn cert_dir() -> PathBuf {
    user_dir()
}

fn migrate_legacy_app_dir() {
    let dest = user_dir();
    let src = dest.join("app");
    if !src.is_dir() {
        return;
    }
    for name in ["cert.pem", "key.pem", "sans.txt"] {
        let from = src.join(name);
        let to = dest.join(name);
        if from.exists() && !to.exists() {
            if fs::rename(&from, &to).is_err() {
                if let Ok(bytes) = fs::read(&from) {
                    let _ = fs::write(&to, bytes);
                }
            }
        }
    }
}

pub fn lan_ipv4s() -> Vec<Ipv4Addr> {
    let mut ips: Vec<Ipv4Addr> = lan_named_ipv4s().into_iter().map(|(_, ip)| ip).collect();
    ips.sort();
    ips.dedup();
    ips
}

pub fn lan_named_ipv4s() -> Vec<(String, Ipv4Addr)> {
    let mut out = Vec::new();
    if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in ifaces {
            if let std::net::IpAddr::V4(v4) = ip {
                if !v4.is_loopback() && !v4.is_link_local() && !v4.is_multicast() {
                    out.push((name, v4));
                }
            }
        }
    }
    out
}

fn wanted_names() -> Vec<String> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "pocketcam.local".to_string(),
    ];
    for ip in lan_ipv4s() {
        names.push(ip.to_string());
    }
    names
}

pub fn load_or_mint() -> Result<AppCert> {
    migrate_legacy_app_dir();
    let dir = cert_dir();
    fs::create_dir_all(&dir).context("create cert dir")?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let names_path = dir.join("sans.txt");
    let names = wanted_names();
    let names_blob = names.join("\n");

    if cert_path.exists() && key_path.exists() && names_path.exists() {
        let prev = fs::read_to_string(&names_path).unwrap_or_default();
        if prev.trim() == names_blob.trim() {
            return Ok(AppCert {
                cert_pem: fs::read_to_string(&cert_path)?,
                key_pem: fs::read_to_string(&key_path)?,
                names,
            });
        }
    }

    let minted = mint(&names)?;
    fs::write(&cert_path, &minted.cert_pem)?;
    fs::write(&key_path, &minted.key_pem)?;
    restrict_key(&key_path);
    fs::write(&names_path, names_blob)?;
    Ok(minted)
}

fn mint(names: &[String]) -> Result<AppCert> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "PocketCam");
    params.subject_alt_names = names
        .iter()
        .map(|n| {
            if let Ok(ip) = n.parse::<std::net::IpAddr>() {
                Ok(SanType::IpAddress(ip))
            } else {
                Ok(SanType::DnsName(
                    Ia5String::try_from(n.as_str()).context("SAN DNS")?,
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(AppCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        names: names.to_vec(),
    })
}

pub fn write_pem(cert: &AppCert) -> Result<(PathBuf, PathBuf)> {
    let dir = cert_dir();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    fs::write(&cert_path, &cert.cert_pem)?;
    fs::write(&key_path, &cert.key_pem)?;
    restrict_key(&key_path);
    Ok((cert_path, key_path))
}

fn restrict_key(path: &std::path::Path) {
    let Ok(user) = std::env::var("USERNAME") else {
        return;
    };
    if user.is_empty() {
        return;
    }
    let _ = std::process::Command::new("icacls.exe")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(R,W)"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
