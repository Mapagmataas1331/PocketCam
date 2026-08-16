//! File-backed 3-slot NV12 latest-wins ring.
//! Mapping is from a real file, not a named kernel object, so Frame Server
//! (Local Service) can read it.

use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, SetEndOfFile, SetFilePointerEx, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
    SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;

pub const MAGIC: u32 = 0x4D41_4350; // 'PCAM'
pub const VERSION: u32 = 1;
pub const SLOTS: u32 = 3;
pub const HEADER_BYTES: u32 = 256;
pub const SLOT_META_BYTES: u32 = 64;
pub const DEFAULT_PATH: &str = r"C:\ProgramData\PocketCam\nv12.ring";
pub const DEFAULT_WIDTH: u32 = 1920;
pub const DEFAULT_HEIGHT: u32 = 1080;
pub const DEFAULT_FPS: u32 = 30;

pub fn nv12_size(width: u32, height: u32) -> u32 {
    width * height + (width * height) / 2
}

pub fn slot_stride(width: u32, height: u32) -> u32 {
    let size = nv12_size(width, height);
    (size + 63) & !63
}

pub fn ring_bytes(width: u32, height: u32) -> u32 {
    HEADER_BYTES + SLOTS * (SLOT_META_BYTES + slot_stride(width, height))
}

fn win32_code(e: windows::core::Error) -> i32 {
    let c = e.code().0 as u32;
    if c & 0xFFFF_0000 == 0x8007_0000 {
        (c & 0xFFFF) as i32
    } else {
        e.code().0
    }
}

fn is_user_mapped(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(1224)
}

fn read_u32(p: *const u8) -> u32 {
    let mut b = [0u8; 4];
    unsafe {
        ptr::copy_nonoverlapping(p, b.as_mut_ptr(), 4);
    }
    u32::from_le_bytes(b)
}

fn header_layout(base: *mut u8, map_bytes: u32) -> Option<(u32, u32, u32)> {
    if map_bytes < HEADER_BYTES {
        return None;
    }
    let magic = read_u32(base);
    let ver = read_u32(unsafe { base.add(4) });
    let w = read_u32(unsafe { base.add(8) });
    let h = read_u32(unsafe { base.add(12) });
    let fps = read_u32(unsafe { base.add(40) }).max(1);
    if magic != MAGIC || ver != VERSION || w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
        return None;
    }
    if ring_bytes(w, h) > map_bytes {
        return None;
    }
    Some((w, h, fps))
}

/// Header only — does not map the file, so Frame Server can still hold it.
pub fn peek_layout(path: &Path) -> Option<(u32, u32, u32)> {
    let mut buf = [0u8; 48];
    let mut file = std::fs::File::open(path).ok()?;
    file.read_exact(&mut buf).ok()?;
    let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    let ver = u32::from_le_bytes(buf[4..8].try_into().ok()?);
    let w = u32::from_le_bytes(buf[8..12].try_into().ok()?);
    let h = u32::from_le_bytes(buf[12..16].try_into().ok()?);
    let fps = u32::from_le_bytes(buf[40..44].try_into().ok()?).max(1);
    if magic != MAGIC || ver != VERSION || w < 2 || h < 2 || w % 2 != 0 || h % 2 != 0 {
        return None;
    }
    Some((w, h, fps))
}

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn grant_frame_server_acl(path: &Path) {
    let wide = to_wide(path);
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:(A;OICI;FA;;;SY)(A;OICI;FRFW;;;LS)(A;OICI;FRFW;;;BU)(A;OICI;FA;;;BA)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .is_err()
        {
            return;
        }
        let mut present = windows::Win32::Foundation::BOOL::default();
        let mut defaulted = windows::Win32::Foundation::BOOL::default();
        let mut dacl: *mut ACL = ptr::null_mut();
        let ok = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
        if ok.is_ok() && !dacl.is_null() {
            let _ = SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            );
        }
        let _ = LocalFree(windows::Win32::Foundation::HLOCAL(sd.0));
    }
}

/// Latest-wins writer. Frame Server maps the same file read-only.
pub struct RingWriter {
    file: HANDLE,
    mapping: HANDLE,
    base: *mut u8,
    mapped_bytes: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

unsafe impl Send for RingWriter {}
unsafe impl Sync for RingWriter {}

impl RingWriter {
    pub fn open(path: &Path, width: u32, height: u32, fps: u32) -> std::io::Result<Self> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("NV12 size {width}x{height} must be even"),
            ));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            grant_frame_server_acl(dir);
        }
        let bytes = ring_bytes(width, height);
        let wide = to_wide(path);
        unsafe {
            let file = CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| std::io::Error::from_raw_os_error(win32_code(e)))?;
            if file.is_invalid() || file == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut existing = 0i64;
            if let Err(e) = GetFileSizeEx(file, &mut existing) {
                let _ = CloseHandle(file);
                return Err(std::io::Error::from_raw_os_error(win32_code(e)));
            }
            let want = bytes as i64;
            let resized = existing != want;
            if resized {
                if let Err(e) = SetFilePointerEx(file, want, None, FILE_BEGIN) {
                    let _ = CloseHandle(file);
                    return Err(std::io::Error::from_raw_os_error(win32_code(e)));
                }
                if let Err(e) = SetEndOfFile(file) {
                    let err = std::io::Error::from_raw_os_error(win32_code(e));
                    let _ = CloseHandle(file);
                    return Err(err);
                }
            }
            grant_frame_server_acl(path);

            let mapping = CreateFileMappingW(
                file,
                None,
                PAGE_READWRITE,
                0,
                bytes,
                PCWSTR::null(),
            )
            .map_err(|e| {
                let _ = CloseHandle(file);
                std::io::Error::from_raw_os_error(win32_code(e))
            })?;
            let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, bytes as usize);
            if view.Value.is_null() {
                let err = std::io::Error::last_os_error();
                let _ = CloseHandle(mapping);
                let _ = CloseHandle(file);
                return Err(err);
            }
            let base = view.Value.cast::<u8>();
            let old = header_layout(base, bytes);
            let reset = resized
                || existing == 0
                || old.map(|(w, h, _)| w != width || h != height).unwrap_or(true);
            let mut writer = Self {
                file,
                mapping,
                base,
                mapped_bytes: bytes,
                width,
                height,
                fps: fps.max(1),
            };
            writer.write_header(reset);
            Ok(writer)
        }
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps.max(1);
        unsafe { self.write_header(false) };
    }

    /// Open at exact `width`×`height`. Frame Server may still hold a mapping
    /// for a few milliseconds after Remove — poll instead of keeping the old size.
    pub fn open_poll(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        budget: Duration,
    ) -> std::io::Result<Self> {
        let start = std::time::Instant::now();
        let mut wait = Duration::from_millis(8);
        loop {
            match Self::open(path, width, height, fps) {
                Ok(s) => return Ok(s),
                Err(e) if is_user_mapped(&e) => {
                    if start.elapsed() >= budget {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!(
                                "nv12.ring still mapped after {}ms (os error 1224)",
                                budget.as_millis()
                            ),
                        ));
                    }
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_millis(40));
                }
                Err(e) => return Err(e),
            }
        }
    }

    unsafe fn write_header(&mut self, reset_slots: bool) {
        let h = self.base;
        let w = self.width;
        let ht = self.height;
        let fps = self.fps.max(1);
        if reset_slots {
            ptr::write_bytes(h, 0, self.mapped_bytes as usize);
        }
        ptr::copy_nonoverlapping(MAGIC.to_le_bytes().as_ptr(), h, 4);
        ptr::copy_nonoverlapping(VERSION.to_le_bytes().as_ptr(), h.add(4), 4);
        ptr::copy_nonoverlapping(w.to_le_bytes().as_ptr(), h.add(8), 4);
        ptr::copy_nonoverlapping(ht.to_le_bytes().as_ptr(), h.add(12), 4);
        ptr::copy_nonoverlapping(SLOTS.to_le_bytes().as_ptr(), h.add(16), 4);
        ptr::copy_nonoverlapping(nv12_size(w, ht).to_le_bytes().as_ptr(), h.add(20), 4);
        ptr::copy_nonoverlapping(slot_stride(w, ht).to_le_bytes().as_ptr(), h.add(24), 4);
        ptr::copy_nonoverlapping(HEADER_BYTES.to_le_bytes().as_ptr(), h.add(28), 4);
        ptr::copy_nonoverlapping(fps.to_le_bytes().as_ptr(), h.add(40), 4);
        ptr::copy_nonoverlapping(1u32.to_le_bytes().as_ptr(), h.add(44), 4);
        if reset_slots {
            self.store_u64(h.add(32).cast(), 0);
        }
    }

    fn store_u64(&self, p: *mut u64, v: u64) {
        unsafe { AtomicU64::from_ptr(p).store(v, Ordering::SeqCst) };
    }

    fn load_u64(&self, p: *mut u64) -> u64 {
        unsafe { AtomicU64::from_ptr(p).load(Ordering::SeqCst) }
    }

    fn slot_ptr(&self, index: u32) -> *mut u8 {
        let stride = slot_stride(self.width, self.height);
        unsafe {
            self.base
                .add(HEADER_BYTES as usize + index as usize * (SLOT_META_BYTES + stride) as usize)
        }
    }

    pub fn write_nv12(&self, nv12: &[u8]) {
        let expected = nv12_size(self.width, self.height) as usize;
        if nv12.len() < expected {
            return;
        }
        let published = unsafe { self.base.add(32).cast::<u64>() };
        let next = self.load_u64(published).saturating_add(2);
        let index = ((next / 2) % SLOTS as u64) as u32;
        let slot = self.slot_ptr(index);
        let seq = slot.cast::<u64>();
        self.store_u64(seq, next - 1);
        std::sync::atomic::fence(Ordering::SeqCst);
        unsafe {
            let mut qpc = 0i64;
            let _ = QueryPerformanceCounter(&mut qpc);
            ptr::copy_nonoverlapping(
                (qpc as u64).to_le_bytes().as_ptr(),
                slot.add(8),
                8,
            );
            ptr::copy_nonoverlapping(self.width.to_le_bytes().as_ptr(), slot.add(16), 4);
            ptr::copy_nonoverlapping(self.height.to_le_bytes().as_ptr(), slot.add(20), 4);
            ptr::copy_nonoverlapping((expected as u32).to_le_bytes().as_ptr(), slot.add(24), 4);
            ptr::copy_nonoverlapping(nv12.as_ptr(), slot.add(SLOT_META_BYTES as usize), expected);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        self.store_u64(seq, next);
        self.store_u64(published, next);
    }

    fn close(&mut self) {
        unsafe {
            if !self.base.is_null() {
                let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.base.cast(),
                });
                self.base = ptr::null_mut();
            }
            if !self.mapping.is_invalid() && self.mapping != HANDLE::default() {
                let _ = CloseHandle(self.mapping);
                self.mapping = HANDLE::default();
            }
            if !self.file.is_invalid() && self.file != HANDLE::default() && self.file != INVALID_HANDLE_VALUE
            {
                let _ = CloseHandle(self.file);
                self.file = INVALID_HANDLE_VALUE;
            }
        }
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_640x480() {
        assert_eq!(nv12_size(640, 480), 460800);
        assert_eq!(slot_stride(640, 480), 460800);
        assert_eq!(ring_bytes(640, 480), 256 + 3 * (64 + 460800));
    }

    #[test]
    fn sizes_catalog() {
        assert_eq!(nv12_size(854, 480), 614_880);
        assert_eq!(slot_stride(854, 480), 614_912);
        assert_eq!(nv12_size(1920, 1080), 3_110_400);
        assert_eq!(nv12_size(3840, 2160), 12_441_600);
        assert_eq!(ring_bytes(1920, 1080), 256 + 3 * (64 + 3_110_400));
    }

    #[test]
    fn peek_roundtrip() {
        let dir = std::env::temp_dir().join("pocketcam-ipc-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("nv12.ring");
        let _ = std::fs::remove_file(&path);
        let writer = RingWriter::open(&path, 1920, 1080, 30).unwrap();
        drop(writer);
        assert_eq!(peek_layout(&path), Some((1920, 1080, 30)));
        let _ = std::fs::remove_file(&path);
    }
}
