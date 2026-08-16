//! QR bitmap for the connect URL.

use qrcode::Color;
use qrcode::QrCode;

pub struct QrBitmap {
    pub size: usize,
    pub rgba: Vec<u8>,
}

pub fn render(url: &str) -> anyhow::Result<QrBitmap> {
    let qr = QrCode::new(url.as_bytes())?;
    let dim = qr.width();
    let colors = qr.to_colors();
    let quiet = 4usize;
    let module = 6usize;
    let n = dim + quiet * 2;
    let size = n * module;
    let mut rgba = vec![255u8; size * size * 4];
    for y in 0..dim {
        for x in 0..dim {
            if colors[y * dim + x] != Color::Dark {
                continue;
            }
            let x0 = (x + quiet) * module;
            let y0 = (y + quiet) * module;
            for py in 0..module {
                for px in 0..module {
                    let i = ((y0 + py) * size + (x0 + px)) * 4;
                    rgba[i] = 17;
                    rgba[i + 1] = 17;
                    rgba[i + 2] = 17;
                    rgba[i + 3] = 255;
                }
            }
        }
    }
    Ok(QrBitmap { size, rgba })
}
