//! Rasterize `images/pocketcam.svg` for the window, tray, and heading.
//! The mark stays the SVG — this file only recolors it.

use anyhow::{bail, Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HDC,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, FindWindowW, SendMessageW, ICONINFO, HICON, ICON_BIG,
    ICON_SMALL, WM_SETICON,
};

pub const SVG: &str = include_str!("../images/pocketcam.svg");

const LIGHT_FG: &str = "#18181b";
const DARK_FG: &str = "#f4f4f5";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TrayTint {
    /// Idle: no virtual camera, no record.
    Idle = 0,
    /// fps >= max, or max − 1.
    Good = 1,
    /// max − 5 … just under max − 1.
    Warn = 2,
    /// Anything slower, or no recent frames.
    Bad = 3,
}

impl TrayTint {
    pub fn hex(self) -> &'static str {
        match self {
            Self::Idle => "#ffffff",
            Self::Good => "#22c55e",
            Self::Warn => "#eab308",
            Self::Bad => "#ef4444",
        }
    }

    pub fn from_fps(fps: f32, max_fps: u32) -> Self {
        let max = max_fps.max(1) as f32;
        if fps >= max - 1.0 {
            Self::Good
        } else if fps >= (max - 5.0).max(0.0) {
            Self::Warn
        } else {
            Self::Bad
        }
    }
}

pub fn apps_use_light_theme() -> bool {
    let mut val: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut val as *mut u32).cast()),
            Some(&mut size),
        )
    };
    ok.is_ok() && val != 0
}

pub fn fg_hex(light_theme: bool) -> &'static str {
    if light_theme {
        LIGHT_FG
    } else {
        DARK_FG
    }
}

pub fn raster(size: u32, light_theme: bool) -> Result<Vec<u8>> {
    raster_color(size, fg_hex(light_theme))
}

pub fn raster_color(size: u32, color: &str) -> Result<Vec<u8>> {
    let svg = SVG.replace("currentColor", color);
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).context("parse images/pocketcam.svg")?;
    let sz = tree.size();
    let dim = sz.width().max(sz.height()).max(1.0);
    let scale = size as f32 / dim;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).context("icon pixmap")?;
    let tx = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, tx, &mut pixmap.as_mut());
    Ok(pixmap.data().to_vec())
}

pub fn egui_icon(light_theme: bool) -> Result<egui::IconData> {
    const S: u32 = 256;
    Ok(egui::IconData {
        rgba: raster(S, light_theme)?,
        width: S,
        height: S,
    })
}

pub fn heading_rgba() -> Result<(u32, Vec<u8>)> {
    const S: u32 = 64;
    // The window chrome is dark; keep the in-app mark light.
    Ok((S, raster_color(S, DARK_FG)?))
}

pub fn hicon(size: i32, light_theme: bool) -> Result<HICON> {
    hicon_hex(size, fg_hex(light_theme))
}

pub fn hicon_hex(size: i32, hex: &str) -> Result<HICON> {
    let rgba = raster_color(size as u32, hex)?;
    unsafe { hicon_from_rgba(size, size, &rgba) }
}

pub fn apply_window_icons(_light_theme: bool, small: HICON, big: HICON) {
    unsafe {
        let Ok(hwnd) = FindWindowW(None, w!("PocketCam")) else {
            return;
        };
        if hwnd.0.is_null() {
            return;
        }
        SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_SMALL as usize), LPARAM(small.0 as isize));
        SendMessageW(hwnd, WM_SETICON, WPARAM(ICON_BIG as usize), LPARAM(big.0 as isize));
    }
}

unsafe fn hicon_from_rgba(width: i32, height: i32, rgba: &[u8]) -> Result<HICON> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0 as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let hdc: HDC = GetDC(None);
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    ReleaseDC(None, hdc);
    if bits.is_null() {
        let _ = DeleteObject(color);
        bail!("CreateDIBSection bits");
    }
    let n = (width * height) as usize;
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, n * 4);
    for i in 0..n {
        dst[i * 4] = rgba[i * 4 + 2];
        dst[i * 4 + 1] = rgba[i * 4 + 1];
        dst[i * 4 + 2] = rgba[i * 4];
        dst[i * 4 + 3] = rgba[i * 4 + 3];
    }
    let mask = CreateBitmap(width, height, 1, 1, None);
    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&info)?;
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    Ok(icon)
}

pub fn destroy_icon(icon: HICON) {
    if !icon.0.is_null() {
        unsafe {
            let _ = DestroyIcon(icon);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_is_square_mark() {
        assert!(SVG.contains("viewBox=\"84 34 122 122\""));
        assert!(SVG.contains("M195,58.02v83.98h-8V58.02"));
        assert!(SVG.contains("M119.5,60c3.03,0,5.5,2.47"));
        assert!(SVG.contains("M138.5,64c.83,0,1.5.67"));
        assert!(SVG.contains("187.28 115.81 168 121.09 168 110.53"));
        assert!(SVG.contains("M147.8,105v21.63l20.17-5.54"));
        assert!(SVG.contains("currentColor"));
    }

    #[test]
    fn tray_tint_bands() {
        assert_eq!(TrayTint::from_fps(30.0, 30), TrayTint::Good);
        assert_eq!(TrayTint::from_fps(29.0, 30), TrayTint::Good);
        assert_eq!(TrayTint::from_fps(28.0, 30), TrayTint::Warn);
        assert_eq!(TrayTint::from_fps(25.0, 30), TrayTint::Warn);
        assert_eq!(TrayTint::from_fps(24.0, 30), TrayTint::Bad);
        assert_eq!(TrayTint::from_fps(0.0, 30), TrayTint::Bad);
    }
}
