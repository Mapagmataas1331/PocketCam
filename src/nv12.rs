//! NV12 contain-fit into a landscape Camera canvas. Never stretch.
//! Portrait into landscape (or the reverse) is rotated 90° clockwise so the
//! frame fills, instead of being letterboxed or scaled into the wrong aspect.

use std::sync::OnceLock;

use pocketcam_ipc::nv12_size;

const MARK_PX: u32 = 256;

fn even(n: usize) -> usize {
    n & !1
}

thread_local! {
    static ROTATE: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
}

/// Letterbox or downscale-contain `src` (contiguous NV12, stride = src_w) into `dst`.
/// Orientation mismatch is a 90° clockwise rotate, then contain.
pub fn contain_fit(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let sw = even(src_w as usize).max(2);
    let sh = even(src_h as usize).max(2);
    let dw = even(dst_w as usize).max(2);
    let dh = even(dst_h as usize).max(2);
    let need = nv12_size(dw as u32, dh as u32) as usize;
    if dst.len() < need {
        return;
    }
    let src_need = nv12_size(sw as u32, sh as u32) as usize;
    if src.len() < src_need {
        fill_black(dst, dw, dh);
        return;
    }
    let src_port = sh > sw;
    let dst_port = dh > dw;
    if src_port != dst_port {
        let rw = sh;
        let rh = sw;
        if rw == dw && rh == dh {
            rotate_nv12_90_cw(src, sw, sh, dst);
            return;
        }
        ROTATE.with(|cell| {
            let mut rot = cell.borrow_mut();
            let n = nv12_size(rw as u32, rh as u32) as usize;
            rot.resize(n, 0);
            rotate_nv12_90_cw(src, sw, sh, &mut rot);
            contain_copy(&rot, rw, rh, dst, dw, dh);
        });
        return;
    }
    contain_copy(src, sw, sh, dst, dw, dh);
}

fn contain_copy(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    let need = nv12_size(dw as u32, dh as u32) as usize;
    fill_black(dst, dw, dh);
    if sw == dw && sh == dh {
        let src_need = nv12_size(sw as u32, sh as u32) as usize;
        dst[..need].copy_from_slice(&src[..src_need.min(need)]);
        return;
    }
    let scale = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
    let fw = even(((sw as f32 * scale).round() as usize).max(2)).min(dw);
    let fh = even(((sh as f32 * scale).round() as usize).max(2)).min(dh);
    let ox = even((dw.saturating_sub(fw)) / 2);
    let oy = even((dh.saturating_sub(fh)) / 2);
    let src_uv = sw * sh;
    let dst_uv = dw * dh;
    for y in 0..fh {
        let sy = (y * sh / fh).min(sh - 1);
        let dy = oy + y;
        let src_row = sy * sw;
        let dst_row = dy * dw + ox;
        for x in 0..fw {
            let sx = (x * sw / fw).min(sw - 1);
            dst[dst_row + x] = src[src_row + sx];
        }
    }
    for y in (0..fh).step_by(2) {
        let sy = (y * sh / fh).min(sh - 2) & !1;
        let dy = (oy + y) / 2;
        for x in (0..fw).step_by(2) {
            let sx = (x * sw / fw).min(sw - 2) & !1;
            let si = src_uv + (sy / 2) * sw + sx;
            let di = dst_uv + dy * dw + ox + x;
            dst[di] = src[si];
            dst[di + 1] = src[si + 1];
        }
    }
}

/// Clockwise 90°: dest size is (src_h, src_w). dest(x, y) = src(y, src_h - 1 - x).
fn rotate_nv12_90_cw(src: &[u8], sw: usize, sh: usize, dst: &mut [u8]) {
    let dw = sh;
    let dh = sw;
    let need = nv12_size(dw as u32, dh as u32) as usize;
    if dst.len() < need {
        return;
    }
    let src_uv = sw * sh;
    let dst_uv = dw * dh;
    for y in 0..dh {
        for x in 0..dw {
            let sx = y;
            let sy = sh - 1 - x;
            dst[y * dw + x] = src[sy * sw + sx];
        }
    }
    for y in (0..dh).step_by(2) {
        for x in (0..dw).step_by(2) {
            let sx = y & !1;
            let sy = (sh - 1 - x) & !1;
            let si = src_uv + (sy / 2) * sw + sx;
            let di = dst_uv + (y / 2) * dw + x;
            dst[di] = src[si];
            dst[di + 1] = src[si + 1];
        }
    }
}

pub fn fill_black(dst: &mut [u8], w: usize, h: usize) {
    let y = w * h;
    let n = nv12_size(w as u32, h as u32) as usize;
    if dst.len() < n {
        return;
    }
    dst[..y].fill(16);
    dst[y..n].fill(128);
}

fn waiting_mark() -> &'static [u8] {
    static MARK: OnceLock<Vec<u8>> = OnceLock::new();
    MARK.get_or_init(|| {
        crate::icon::raster_color(MARK_PX, "#f4f4f5").unwrap_or_default()
    })
}

/// Waiting still: brand mark, POCKETCAM, WAITING. Not a frozen last frame.
pub fn waiting_still(dst: &mut [u8], width: u32, height: u32) {
    let w = even(width as usize).max(2);
    let h = even(height as usize).max(2);
    let n = nv12_size(w as u32, h as u32) as usize;
    if dst.len() < n {
        return;
    }
    fill_black(dst, w, h);
    let scale = ((h / 100) as i32).max(2);
    let line_h = 7 * scale;
    let gap = 5 * scale;
    let icon = even(((h as i32 / 6).clamp(48, 280)) as usize).max(2) as i32;
    let stack = icon + gap + line_h + gap + line_h;
    let y0 = (h as i32 - stack) / 2;
    let mark = waiting_mark();
    if mark.len() == (MARK_PX * MARK_PX * 4) as usize {
        blit_mark(
            dst,
            w,
            h,
            mark,
            (w as i32 - icon) / 2,
            y0,
            icon,
        );
    }
    blit_text(dst, w, h, "POCKETCAM", scale, y0 + icon + gap);
    blit_text(
        dst,
        w,
        h,
        "WAITING",
        scale,
        y0 + icon + gap + line_h + gap,
    );
}

fn blit_mark(dst: &mut [u8], w: usize, h: usize, rgba: &[u8], x0: i32, y0: i32, size: i32) {
    if size <= 0 {
        return;
    }
    for dy in 0..size {
        let y = y0 + dy;
        if y < 0 || y >= h as i32 {
            continue;
        }
        let sy = (dy as u32 * MARK_PX / size as u32).min(MARK_PX - 1);
        for dx in 0..size {
            let x = x0 + dx;
            if x < 0 || x >= w as i32 {
                continue;
            }
            let sx = (dx as u32 * MARK_PX / size as u32).min(MARK_PX - 1);
            let i = ((sy * MARK_PX + sx) * 4) as usize;
            let a = rgba[i + 3] as u32;
            if a < 16 {
                continue;
            }
            let r = rgba[i] as u32;
            let g = rgba[i + 1] as u32;
            let b = rgba[i + 2] as u32;
            let luma = (77 * r + 150 * g + 29 * b) / 255;
            dst[y as usize * w + x as usize] = (16 + 219 * luma * a / (255 * 255)) as u8;
        }
    }
}

fn blit_text(dst: &mut [u8], w: usize, h: usize, text: &str, scale: i32, cy: i32) {
    let gw = 6 * scale;
    let total = text.len() as i32 * gw;
    let x0 = (w as i32 - total) / 2;
    let mut x = x0;
    for ch in text.bytes() {
        blit_glyph(dst, w, h, ch, scale, x, cy);
        x += gw;
    }
}

fn blit_glyph(dst: &mut [u8], w: usize, h: usize, ch: u8, scale: i32, x0: i32, y0: i32) {
    let bits = glyph(ch);
    for row in 0..7 {
        let line = bits[row];
        for col in 0..5 {
            if (line & (1 << (4 - col))) == 0 {
                continue;
            }
            for dy in 0..scale {
                let y = y0 + row as i32 * scale + dy;
                if y < 0 || y >= h as i32 {
                    continue;
                }
                for dx in 0..scale {
                    let x = x0 + col as i32 * scale + dx;
                    if x < 0 || x >= w as i32 {
                        continue;
                    }
                    dst[y as usize * w + x as usize] = 235;
                }
            }
        }
    }
}

fn glyph(ch: u8) -> [u8; 7] {
    match ch {
        b'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        b'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        b'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        b'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        b'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        b'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        b'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        b'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        b'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        b'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        b'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        b'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        b'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        b'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        b'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010],
        b'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        b' ' => [0; 7],
        _ => [0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_rotates_to_fill_landscape() {
        let src_w = 1080u32;
        let src_h = 1920u32;
        let mut src = vec![0u8; nv12_size(src_w, src_h) as usize];
        src[..(src_w * src_h) as usize].fill(200);
        src[0] = 99;
        let mut dst = vec![0u8; nv12_size(1920, 1080) as usize];
        contain_fit(&src, src_w, src_h, &mut dst, 1920, 1080);
        assert_eq!(dst[1919], 99, "src(0,0) → dest top-right");
        assert_eq!(dst[1920 * 540 + 960], 200);
        assert_ne!(dst[0], 16, "no letterbox after rotate-to-fill");
    }

    #[test]
    fn landscape_copy_is_1to1() {
        let mut src = vec![0u8; nv12_size(1920, 1080) as usize];
        src[..1920 * 1080].fill(180);
        src[0] = 40;
        let mut dst = vec![0u8; nv12_size(1920, 1080) as usize];
        contain_fit(&src, 1920, 1080, &mut dst, 1920, 1080);
        assert_eq!(dst[0], 40);
        assert_eq!(dst[100], 180);
    }

    #[test]
    fn waiting_still_paints_mark_and_words() {
        let mut dst = vec![0u8; nv12_size(1280, 720) as usize];
        waiting_still(&mut dst, 1280, 720);
        let y = &dst[..1280 * 720];
        let bright = y.iter().filter(|&&p| p > 180).count();
        assert!(bright > 800, "icon + POCKETCAM + WAITING should light pixels, got {bright}");
        assert!(y.iter().all(|&p| p >= 16), "limited-range luma");
    }
}
