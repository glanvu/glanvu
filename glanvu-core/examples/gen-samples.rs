// SPDX-License-Identifier: Apache-2.0

//! Generate one small sample image per Phase 1 base format, for manual testing.
//!
//! Usage: `cargo run -p glanvu-core --example gen-samples -- [OUT_DIR]`
//! (OUT_DIR defaults to `./glanvu-samples`). Formats the local `image` build cannot encode are
//! skipped with a note.

use std::path::PathBuf;

use image::{DynamicImage, ImageFormat, RgbaImage};

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("glanvu-samples"));

    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        std::process::exit(1);
    }

    let img = gradient(256, 160);

    let targets = [
        (ImageFormat::Png, "sample.png"),
        (ImageFormat::Jpeg, "sample.jpg"),
        (ImageFormat::Gif, "sample.gif"),
        (ImageFormat::Bmp, "sample.bmp"),
        (ImageFormat::Tiff, "sample.tiff"),
        (ImageFormat::WebP, "sample.webp"),
    ];

    for (format, name) in targets {
        let path = out_dir.join(name);
        match img.save_with_format(&path, format) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => println!("skipped {name} ({format:?} not encodable here): {e}"),
        }
    }

    println!(
        "\nTry: cargo run -p glanvu -- info {}",
        out_dir.join("sample.png").display()
    );
}

/// A simple RGBA gradient so the samples are visually distinct (useful once the viewer exists).
fn gradient(w: u32, h: u32) -> DynamicImage {
    let mut img = RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let r = (x * 255 / w.max(1)) as u8;
        let g = (y * 255 / h.max(1)) as u8;
        *px = image::Rgba([r, g, 128, 255]);
    }
    DynamicImage::ImageRgba8(img)
}
