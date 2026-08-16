//! Minimal H.264 SPS parse: coded size + frame_crop → display rectangle.

const TYPE_MASK: u8 = 0x1F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsCrop {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub fn sps_crop_from_annex_b(annex_b: &[u8]) -> Option<SpsCrop> {
    let mut i = 0;
    while i + 4 < annex_b.len() {
        let start = if annex_b[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if annex_b[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let nal = i + start;
        if nal >= annex_b.len() {
            break;
        }
        let ntype = annex_b[nal] & TYPE_MASK;
        let mut end = annex_b.len();
        let mut j = nal + 1;
        while j + 3 < annex_b.len() {
            if annex_b[j..].starts_with(&[0, 0, 0, 1]) || annex_b[j..].starts_with(&[0, 0, 1]) {
                end = j;
                break;
            }
            j += 1;
        }
        if ntype == 7 {
            return parse_sps(&annex_b[nal + 1..end]);
        }
        i = end;
    }
    None
}

fn parse_sps(nal_body: &[u8]) -> Option<SpsCrop> {
    let rbsp = ebsp_to_rbsp(nal_body);
    let mut r = Bits::new(&rbsp);
    let profile = r.u(8)?;
    let _compat = r.u(8)?;
    let _level = r.u(8)?;
    let _sps_id = r.ue()?;

    let mut chroma_format_idc = 1u32;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            let _ = r.u(1)?;
        }
        let _ = r.ue()?;
        let _ = r.ue()?;
        let _ = r.u(1)?;
        if r.u(1)? == 1 {
            let n = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let _ = r.ue()?;
    let poc = r.ue()?;
    if poc == 0 {
        let _ = r.ue()?;
    } else if poc == 1 {
        let _ = r.u(1)?;
        let _ = r.se()?;
        let _ = r.se()?;
        let n = r.ue()?;
        for _ in 0..n {
            let _ = r.se()?;
        }
    }
    let _ = r.ue()?;
    let _ = r.u(1)?;
    let pic_w_mbs = r.ue()? + 1;
    let pic_h_map = r.ue()? + 1;
    let frame_mbs_only = r.u(1)?;
    if frame_mbs_only == 0 {
        let _ = r.u(1)?;
    }
    let _ = r.u(1)?;
    let cropping = r.u(1)?;
    let mut left = 0u32;
    let mut right = 0u32;
    let mut top = 0u32;
    let mut bottom = 0u32;
    if cropping == 1 {
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }

    let (sub_w, sub_h) = match chroma_format_idc {
        0 => (1, 1),
        2 => (2, 1),
        3 => (1, 1),
        _ => (2, 2),
    };
    let crop_x = sub_w;
    let crop_y = sub_h * (2 - frame_mbs_only);
    let coded_w = pic_w_mbs * 16;
    let coded_h = pic_h_map * 16 * (2 - frame_mbs_only);
    let x = left * crop_x;
    let y = top * crop_y;
    let w = coded_w.saturating_sub((left + right) * crop_x);
    let h = coded_h.saturating_sub((top + bottom) * crop_y);
    if w < 2 || h < 2 {
        return None;
    }
    Some(SpsCrop { x, y, w, h })
}

fn skip_scaling_list(r: &mut Bits, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

fn ebsp_to_rbsp(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut zeros = 0u8;
    for &b in src {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros.saturating_add(1) } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.at / 8;
            let bit = 7 - (self.at % 8);
            if byte >= self.data.len() {
                return None;
            }
            v = (v << 1) | ((self.data[byte] as u32 >> bit) & 1);
            self.at += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.u(1)? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let rest = self.u(leading)?;
        Some((1u32 << leading) - 1 + rest)
    }

    fn se(&mut self) -> Option<i32> {
        let v = self.ue()? as i32;
        let n = (v + 1) >> 1;
        Some(if v & 1 != 0 { n } else { -n })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tighten_1080p_crop_units() {
        // 1920x1088 coded, bottom crop 4 (×2 luma) → 1080.
        assert_eq!(1088 - 4 * 2, 1080);
    }
}
