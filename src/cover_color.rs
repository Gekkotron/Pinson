//! Dominant cover colour, for the now-playing tint and glow. Port of `web/js/color.js`.

use image::imageops::FilterType;
use image::{DynamicImage, imageops};
use serde::Serialize;

const SIZE: u32 = 32;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CoverColor {
    /// Background-safe tint (see [`tint`]).
    pub color: String,
    /// The raw dominant colour.
    pub glow: String,
}

/// Dominant colour of encoded image bytes, or `None` if they do not decode or the image is
/// fully transparent.
pub fn color_of(bytes: &[u8]) -> Option<CoverColor> {
    let img = image::load_from_memory(bytes).ok()?;
    let d = dominant(&img)?;
    Some(CoverColor {
        color: hex(tint(d)),
        glow: hex(d),
    })
}

/// Lowercase `#rrggbb`.
pub fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Dominant colour of a cover: colourful pixels grouped by hue (20 degree buckets), weighted
/// towards saturated mid-tones, heaviest group averaged. Mostly grey/black/white covers fall
/// back to the mean colour. `None` only when no pixel is opaque.
pub fn dominant(img: &DynamicImage) -> Option<[u8; 3]> {
    let small = imageops::resize(&img.to_rgba8(), SIZE, SIZE, FilterType::Triangle);
    let mut buckets = [[0f64; 4]; 18]; // weight, r, g, b
    let mut mean = [0f64; 3];
    let mut n = 0u32;
    let mut colourful = 0u32;
    for px in small.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        let rgb = [r, g, b];
        for (m, v) in mean.iter_mut().zip(rgb) {
            *m += f64::from(v);
        }
        n += 1;
        let (hue, sat, light) = to_hsl(rgb);
        if sat < 0.25 || light < 0.1 || light > 0.92 {
            continue;
        }
        let w = sat * (1.0 - (light - 0.5).abs());
        let bucket = &mut buckets[(hue / 20.0).floor() as usize % 18];
        bucket[0] += w;
        for (acc, v) in bucket[1..].iter_mut().zip(rgb) {
            *acc += f64::from(v) * w;
        }
        colourful += 1;
    }
    if n == 0 {
        return None;
    }
    if f64::from(colourful) < f64::from(n) * 0.03 {
        return Some(mean.map(|v| (v / f64::from(n)).round() as u8));
    }
    let best = buckets
        .iter()
        .fold(&buckets[0], |a, b| if b[0] > a[0] { b } else { a });
    Some([
        (best[1] / best[0]).round() as u8,
        (best[2] / best[0]).round() as u8,
        (best[3] / best[0]).round() as u8,
    ])
}

/// A background-safe version of `rgb` (kept dark and not too loud, so white text stays
/// readable). Hue is rounded to whole degrees and saturation/lightness to whole percent, as
/// the browser did with the CSS `hsl(...)` string.
pub fn tint(rgb: [u8; 3]) -> [u8; 3] {
    let (h, s, l) = to_hsl(rgb);
    let sat = s.min(0.62);
    let light = l.clamp(0.2, 0.36);
    from_hsl(
        h.round(),
        (sat * 100.0).round() / 100.0,
        (light * 100.0).round() / 100.0,
    )
}

fn to_hsl([r, g, b]: [u8; 3]) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

fn from_hsl(h: f64, s: f64, l: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0).floor() as i32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r, g, b].map(|v| ((v + m) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba, RgbaImage};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
        ImageBuffer::from_pixel(w, h, Rgba(px))
    }

    fn dyn_of(img: RgbaImage) -> DynamicImage {
        DynamicImage::ImageRgba8(img)
    }

    #[test]
    fn solid_colour_is_its_own_dominant() {
        let img = dyn_of(solid(64, 64, [40, 90, 200, 255]));
        assert_eq!(dominant(&img), Some([40, 90, 200]));
    }

    #[test]
    fn grey_cover_falls_back_to_mean() {
        let mut img = solid(32, 32, [20, 20, 20, 255]);
        for y in 0..32 {
            for x in 16..32 {
                img.put_pixel(x, y, Rgba([220, 220, 220, 255]));
            }
        }
        let d = dominant(&dyn_of(img)).unwrap();
        for c in d {
            assert!((119..=121).contains(&c), "{d:?}");
        }
    }

    #[test]
    fn few_colourful_pixels_fall_back_to_mean() {
        let mut img = solid(32, 32, [128, 128, 128, 255]);
        for x in 0..20 {
            img.put_pixel(x, 0, Rgba([255, 0, 0, 255]));
        }
        let d = dominant(&dyn_of(img)).unwrap();
        // Mean is about (128 + 20/1024*127) for red, grey for the rest; red is not picked.
        assert!(d[0] < 140 && d[1] > 120 && d[2] > 120, "{d:?}");
    }

    #[test]
    fn biggest_hue_group_wins() {
        let mut img = solid(32, 32, [128, 128, 128, 255]);
        for i in 0..1024u32 {
            let (x, y) = (i % 32, i / 32);
            if i < 205 {
                img.put_pixel(x, y, Rgba([200, 40, 40, 255]));
            } else if i < 307 {
                img.put_pixel(x, y, Rgba([40, 40, 200, 255]));
            }
        }
        let d = dominant(&dyn_of(img)).unwrap();
        assert!(d[0] > d[2], "{d:?}");
    }

    #[test]
    fn transparent_cover_has_no_colour() {
        assert_eq!(dominant(&dyn_of(solid(32, 32, [10, 200, 30, 0]))), None);
    }

    #[test]
    fn tint_clamps_like_the_web() {
        assert_eq!(hex(tint([255, 0, 0])), "#952323");
        assert_eq!(hex(tint([10, 10, 10])), "#333333");
        // Hue 200, kept; lightness lowered to 36 %.
        let [r, g, b] = tint([200, 230, 255]);
        let (h, _, l) = to_hsl([r, g, b]);
        assert!((h - 205.0).abs() <= 6.0, "{h}");
        assert!((l - 0.36).abs() < 0.01, "{l}");
    }

    #[test]
    fn color_of_rejects_garbage() {
        assert_eq!(color_of(b"not an image"), None);
    }

    #[test]
    fn color_of_reads_a_png() {
        let img = solid(64, 64, [40, 90, 200, 255]);
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        let c = color_of(&bytes).unwrap();
        assert_eq!(c.glow, "#285ac8");
    }
}
