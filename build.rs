//! Embed `images/pocketcam.svg` as the PE icon (Explorer / Start menu / shortcut).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=images/pocketcam.svg");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let svg_path = manifest.join("images/pocketcam.svg");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("pocketcam.ico");
    let ico = svg_to_ico(&svg_path);
    fs::write(&out, &ico).expect("write PE icon");
    let _ = fs::create_dir_all(manifest.join("target"));
    fs::write(manifest.join("target/pocketcam.ico"), &ico).expect("write target icon");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(out.to_str().expect("utf-8 icon path"));
    res.set("ProductName", "PocketCam");
    res.set("FileDescription", "Phone browser as a Windows webcam");
    res.compile().expect("embed PE icon");
}

fn svg_to_ico(svg_path: &Path) -> Vec<u8> {
    let svg = fs::read_to_string(svg_path).expect("read images/pocketcam.svg");
    // Explorer / shortcut: brand purple, same mark as the SVG.
    let svg = svg.replace("currentColor", "#7c3aed");
    let tree = usvg::Tree::from_str(&svg, &usvg::Options::default()).expect("parse svg");
    let dim = tree.size().width().max(tree.size().height()).max(1.0);
    let sizes = [16u32, 32, 48, 256];
    let pngs: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&size| {
            let scale = size as f32 / dim;
            let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
            resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
            (size, rgba_png(pixmap.data(), size, size))
        })
        .collect();
    pack_ico(&pngs)
}

fn rgba_png(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(rgba).expect("png data");
    }
    bytes
}

fn pack_ico(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = pngs.len() as u16;
    let header = 6 + 16 * pngs.len();
    let mut offset = header as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for &(size, ref png) in pngs {
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in pngs {
        out.extend_from_slice(png);
    }
    out
}
