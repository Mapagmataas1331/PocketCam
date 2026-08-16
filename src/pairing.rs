//! One-phone session token + LAN URL for the QR.

use std::time::{Duration, Instant};

use rand::Rng;

use crate::settings::Settings;

pub const TOKEN_GRACE: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PairPhase {
    Waiting,
    Connecting,
    Live,
}

#[derive(Debug)]
pub enum AcceptError {
    Unknown,
    Used,
}

impl AcceptError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown-token",
            Self::Used => "token-used",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::Unknown => {
                "That token does not match. Type the code shown on the PC."
            }
            Self::Used => {
                "This token was already used. Tap New session on the PC and type the new code."
            }
        }
    }
}

pub struct Pairing {
    pub token: String,
    pub url: String,
    pub lan_ip: String,
    /// Saved adapter gone, or no LAN IPv4. Empty when the QR IP is honest.
    pub nic_warning: Option<String>,
    /// After a phone is accepted, further clients are rejected until a new session.
    pub consumed: bool,
    pub phase: PairPhase,
    /// Bumped on every rotate so a closing WebSocket cannot burn a fresh QR.
    pub generation: u64,
    /// After the phone drops, the same token works until this instant.
    pub grace_until: Option<Instant>,
}

impl Pairing {
    pub fn new(settings: &Settings, listen_port: u16) -> Self {
        Self::with_generation(0, settings, listen_port)
    }

    fn with_generation(generation: u64, settings: &Settings, listen_port: u16) -> Self {
        let token = mint_token();
        let (lan_ip, url, nic_warning) = endpoint(settings, listen_port, &token);
        Self {
            token,
            url,
            lan_ip,
            nic_warning,
            consumed: false,
            phase: PairPhase::Waiting,
            generation,
            grace_until: None,
        }
    }

    pub fn rotate(&mut self, settings: &Settings, listen_port: u16) {
        *self = Self::with_generation(self.generation.wrapping_add(1), settings, listen_port);
    }

    /// Keep the token; rewrite the QR for a new adapter or listen port.
    pub fn set_endpoint(&mut self, settings: &Settings, listen_port: u16) {
        let (lan_ip, url, nic_warning) = endpoint(settings, listen_port, &self.token);
        self.lan_ip = lan_ip;
        self.url = url;
        self.nic_warning = nic_warning;
    }

    pub fn accept_token(&mut self, offered: &str) -> Result<(), AcceptError> {
        if normalize_token(offered) != self.token {
            return Err(AcceptError::Unknown);
        }
        if self.consumed {
            return Err(AcceptError::Used);
        }
        self.consumed = true;
        Ok(())
    }
}

fn endpoint(settings: &Settings, listen_port: u16, token: &str) -> (String, String, Option<String>) {
    let nics = crate::settings::list_adapters();
    let stale = settings
        .adapter
        .as_ref()
        .map(|name| !nics.iter().any(|n| &n.name == name))
        .unwrap_or(false);
    let ip = settings.resolve_lan();
    match ip {
        Some(ip) => {
            let lan_ip = ip.to_string();
            let url = format!("https://{lan_ip}:{listen_port}/connect/{token}");
            let nic_warning = if stale {
                Some(format!(
                    "Saved adapter is gone. QR uses Auto {lan_ip}."
                ))
            } else {
                None
            };
            (lan_ip, url, nic_warning)
        }
        None => (
            String::new(),
            String::new(),
            Some("No LAN IPv4. Pick an adapter after Wi-Fi is up.".into()),
        ),
    }
}

pub fn mint_token() -> String {
    const ALPH: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| ALPH[rng.gen_range(0..ALPH.len())] as char)
        .collect()
}

pub fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn display_token(token: &str) -> String {
    if token.len() == 8 {
        format!("{}  {}", &token[..4], &token[4..])
    } else {
        token.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_hello_is_used() {
        let mut p = Pairing {
            token: "ABCD2345".into(),
            url: String::new(),
            lan_ip: String::new(),
            nic_warning: None,
            consumed: false,
            phase: PairPhase::Waiting,
            generation: 0,
            grace_until: None,
        };
        p.accept_token("abcd 2345").unwrap();
        assert!(p.consumed);
        match p.accept_token("ABCD2345") {
            Err(AcceptError::Used) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_token() {
        let mut p = Pairing {
            token: "ABCD2345".into(),
            url: String::new(),
            lan_ip: String::new(),
            nic_warning: None,
            consumed: false,
            phase: PairPhase::Waiting,
            generation: 0,
            grace_until: None,
        };
        match p.accept_token("ZZZZZZZZ") {
            Err(AcceptError::Unknown) => {}
            other => panic!("{other:?}"),
        }
    }
}
