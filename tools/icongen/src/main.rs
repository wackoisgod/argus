//! Generates the Argus app icon: a shattered dark world with glowing
//! fissures that form a watching eye. Original procedural art.
//!
//! Usage: cargo run -p icongen  → writes assets/argus.ico + a 512px preview.

use image::imageops;
use image::{Rgba, RgbaImage};

const S: u32 = 1024;
const CX: f32 = 512.0;
const CY: f32 = 512.0;
const R: f32 = 440.0;

fn main() {
    let planet = draw_planet();
    let (glow, core) = draw_cracks();

    let mut img = RgbaImage::new(S, S);
    // Composite: planet, then additive glow, then bright crack cores.
    for y in 0..S {
        for x in 0..S {
            let mut px = *planet.get_pixel(x, y);
            let g = glow.get_pixel(x, y)[0] as f32 / 255.0;
            let c = core.get_pixel(x, y)[0] as f32 / 255.0;
            // Fel glow: sickly green; core: bright green-white.
            let (gr, gg, gb) = (0.38, 1.0, 0.30);
            add(&mut px, gr * g * 240.0, gg * g * 240.0, gb * g * 240.0, g * 240.0);
            add(&mut px, 190.0 * c, 255.0 * c, 175.0 * c, c * 255.0);
            img.put_pixel(x, y, px);
        }
    }

    std::fs::create_dir_all("assets").unwrap();
    let preview = imageops::resize(&img, 512, 512, imageops::FilterType::Lanczos3);
    preview.save("assets/argus-preview.png").unwrap();
    write_ico(&img, "assets/argus.ico");
    println!("wrote assets/argus.ico and assets/argus-preview.png");
}

fn add(px: &mut Rgba<u8>, r: f32, g: f32, b: f32, a: f32) {
    px[0] = (px[0] as f32 + r).min(255.0) as u8;
    px[1] = (px[1] as f32 + g).min(255.0) as u8;
    px[2] = (px[2] as f32 + b).min(255.0) as u8;
    px[3] = (px[3] as f32 + a).min(255.0) as u8;
}

fn draw_planet() -> RgbaImage {
    let mut img = RgbaImage::new(S, S);
    for y in 0..S {
        for x in 0..S {
            let dx = (x as f32 - CX) / R;
            let dy = (y as f32 - CY) / R;
            let d = (dx * dx + dy * dy).sqrt();
            if d > 1.0 {
                continue;
            }
            // Fake sphere normal → light from upper-left.
            let nz = (1.0 - d * d).sqrt();
            let ndotl = (-dx * 0.55 - dy * 0.55 + nz * 0.63).max(0.0);
            // Deep violet base with subtle warm shadowed side.
            let base = (18.0, 12.0, 34.0);
            let lit = (86.0, 62.0, 132.0);
            let t = ndotl.powf(1.35);
            let mut r = base.0 + (lit.0 - base.0) * t;
            let mut g = base.1 + (lit.1 - base.1) * t;
            let mut b = base.2 + (lit.2 - base.2) * t;
            // Rim light on the upper-left edge.
            let rim = ((d - 0.82) / 0.18).clamp(0.0, 1.0) * (-dx - dy).clamp(0.0, 1.4) / 1.4;
            r += rim * 70.0;
            g += rim * 55.0;
            b += rim * 110.0;
            // Soft edge anti-aliasing.
            let edge = ((1.0 - d) * R).clamp(0.0, 1.5) / 1.5;
            img.put_pixel(
                x,
                y,
                Rgba([r as u8, g as u8, b as u8, (edge * 255.0) as u8]),
            );
        }
    }
    img
}

#[derive(Clone, Copy, PartialEq)]
enum Taper {
    None,
    /// Widest in the middle — the eye-slit profile.
    Lens,
    /// Wide at the start, thinning to a point.
    Fade,
}

/// Stamp a jagged polyline into a grayscale layer with a given width profile.
fn stamp_path(layer: &mut RgbaImage, pts: &[(f32, f32)], width: f32, taper: Taper) {
    let total = pts.len().max(2) - 1;
    for (i, w) in pts.windows(2).enumerate() {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        let seg_t = i as f32 / total as f32;
        let wfac = match taper {
            Taper::Lens => (1.0 - (seg_t * 2.0 - 1.0).abs()).powf(0.4).max(0.06),
            Taper::Fade => (1.0 - seg_t * 0.85).max(0.1),
            Taper::None => 1.0,
        };
        let len = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
        let steps = (len * 2.0) as i32 + 1;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let px = x0 + (x1 - x0) * t;
            let py = y0 + (y1 - y0) * t;
            let rad = (width * wfac).max(1.0);
            let ir = rad as i32 + 1;
            for oy in -ir..=ir {
                for ox in -ir..=ir {
                    let dist = ((ox * ox + oy * oy) as f32).sqrt();
                    if dist > rad {
                        continue;
                    }
                    let (ux, uy) = ((px as i32 + ox) as u32, (py as i32 + oy) as u32);
                    if ux < S && uy < S {
                        let v = ((1.0 - dist / rad) * 255.0) as u8;
                        let old = layer.get_pixel(ux, uy)[0];
                        layer.put_pixel(ux, uy, Rgba([old.max(v); 4]));
                    }
                }
            }
        }
    }
}

fn rotate(pts: &[(f32, f32)], deg: f32) -> Vec<(f32, f32)> {
    let (s, c) = deg.to_radians().sin_cos();
    pts.iter()
        .map(|&(x, y)| {
            let (dx, dy) = (x - CX, y - CY);
            (CX + dx * c - dy * s, CY + dx * s + dy * c)
        })
        .collect()
}

fn draw_cracks() -> (RgbaImage, RgbaImage) {
    let mut layer = RgbaImage::new(S, S);
    let tilt = -7.0;

    // The wound: an irregular molten core at the planet's heart — reads as
    // a bright eye at small sizes.
    let wound_blobs: Vec<(f32, f32, f32)> = vec![
        (512.0, 512.0, 58.0),
        (487.0, 502.0, 50.0),
        (540.0, 520.0, 52.0),
        (524.0, 488.0, 46.0),
        (494.0, 534.0, 44.0),
        (516.0, 540.0, 42.0),
        (538.0, 496.0, 40.0),
        (470.0, 516.0, 38.0),
        (554.0, 512.0, 36.0),
    ];
    for &(bx, by, r) in &wound_blobs {
        stamp_path(&mut layer, &[(bx - 4.0, by), (bx + 4.0, by)], r, Taper::None);
    }

    // Primary shatter cracks radiating from the wound to (and past) the rim.
    let majors: Vec<(Vec<(f32, f32)>, f32)> = vec![
        (
            vec![
                (480.0, 500.0),
                (390.0, 520.0),
                (310.0, 490.0),
                (225.0, 515.0),
                (130.0, 495.0),
                (60.0, 510.0),
            ],
            15.0,
        ),
        (
            vec![
                (550.0, 515.0),
                (640.0, 495.0),
                (720.0, 525.0),
                (815.0, 500.0),
                (910.0, 520.0),
                (970.0, 505.0),
            ],
            15.0,
        ),
        (
            vec![
                (520.0, 480.0),
                (560.0, 390.0),
                (530.0, 300.0),
                (585.0, 205.0),
                (560.0, 120.0),
            ],
            11.0,
        ),
        (
            vec![
                (495.0, 550.0),
                (445.0, 645.0),
                (485.0, 730.0),
                (430.0, 830.0),
                (455.0, 910.0),
            ],
            11.0,
        ),
    ];
    for (path, w) in &majors {
        stamp_path(&mut layer, &rotate(path, tilt), *w, Taper::Fade);
    }

    // Fine fractures.
    let minors: Vec<Vec<(f32, f32)>> = vec![
        vec![(390.0, 520.0), (360.0, 600.0), (300.0, 655.0)],
        vec![(640.0, 495.0), (690.0, 415.0), (755.0, 380.0)],
        vec![(310.0, 490.0), (280.0, 420.0), (215.0, 380.0)],
        vec![(720.0, 525.0), (760.0, 600.0), (825.0, 645.0)],
        vec![(560.0, 390.0), (490.0, 350.0)],
        vec![(445.0, 645.0), (530.0, 690.0)],
    ];
    for path in &minors {
        stamp_path(&mut layer, &rotate(path, tilt), 5.0, Taper::Fade);
    }

    // Mask cracks to the planet disc (tiny feather beyond the rim).
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - CX;
            let dy = y as f32 - CY;
            let d = (dx * dx + dy * dy).sqrt() / R;
            if d > 1.01 {
                layer.put_pixel(x, y, Rgba([0; 4]));
            }
        }
    }

    let glow = imageops::blur(&layer, 16.0);
    let core = imageops::blur(&layer, 1.6);
    (glow, core)
}

/// ICO container with embedded PNGs at standard sizes.
fn write_ico(src: &RgbaImage, path: &str) {
    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let mut pngs: Vec<Vec<u8>> = Vec::new();
    for &size in &sizes {
        let scaled = imageops::resize(src, size, size, imageops::FilterType::Lanczos3);
        let mut buf = Vec::new();
        scaled
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        pngs.push(buf);
    }
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (i, &size) in sizes.iter().enumerate() {
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0); // palette
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(pngs[i].len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += pngs[i].len() as u32;
    }
    for png in &pngs {
        out.extend_from_slice(png);
    }
    std::fs::write(path, out).unwrap();
}
