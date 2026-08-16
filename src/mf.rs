//! Process-wide Media Foundation startup. Decode threads and the virtual
//! camera only need per-thread `CoInitializeEx`.

use std::sync::OnceLock;

use anyhow::{bail, Result};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_NOSOCKET, MF_VERSION};

static START: OnceLock<Result<(), String>> = OnceLock::new();

pub fn ensure() -> Result<()> {
    match START.get_or_init(|| {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| e.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(e) => bail!("MFStartup: {e}"),
    }
}
