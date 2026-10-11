//! Derive a subtle card tint color from a repository's stored icon.
//!
//! The color is computed once, when the icon is first fetched at add time and
//! backfilled for older archives, then stored on the repo manifest. Rendering
//! never decodes an image. When there is no icon to read we fall back to a
//! stable hash of the repo path so every card still gets its own identity
//! color.

/// Fallback palette, matching the folder dots in `style.css`.
const PALETTE: [&str; 8] = [
    "#e5534b", "#e0823d", "#bb8009", "#57ab5a", "#3992c4", "#8250df", "#d6318c", "#768390",
];

/// Stable fallback color for a repo with no usable icon: FNV-1a of the repo
/// path, mapped onto the palette. The same path always yields the same color.
/// This is only used when there is nothing to extract from.
pub fn hash_color(rel: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rel.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    PALETTE[(h % PALETTE.len() as u64) as usize].to_string()
}

/// Extract a normalized `#rrggbb` tint from encoded image bytes (PNG, JPEG,
/// GIF or WebP). Returns `None` when the image can't be decoded or has no
/// saturated, mid-tone color to work with (e.g. a white or grayscale avatar).
///
/// The hue is kept faithful to the icon (a banana stays yellow, a red hat
/// stays red) while saturation and value are clamped so the same hex reads on
/// both the light and dark themes, like the hand-picked palette colors do.
pub fn extract_auto_color(bytes: &[u8]) -> Option<String> {
    let img = image::load_from_memory(bytes).ok()?;
    // A 32x32 thumbnail is plenty for a dominant-color read and keeps the
    // decode cheap.
    let small = img.thumbnail_exact(32, 32).to_rgba8();

    // Bucket pixels by 30-degree hue and average the RGB within each bucket.
    let mut buckets: [(u64, f64, f64, f64); 12] = [(0, 0.0, 0.0, 0.0); 12];
    for px in small.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        let (rf, gf, bf) = (f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0);
        let (hue, sat, value) = rgb_to_hsv(rf, gf, bf);
        let min = rf.min(gf).min(bf);
        // Drop near-black, near-white and desaturated (gray) pixels. "White"
        // means every channel is high, not merely a high value: a bright
        // saturated yellow (a banana) has value near 1.0 and must survive.
        if value < 0.08 || min > 0.90 || sat < 0.15 {
            continue;
        }
        let idx = ((hue / 30.0) as usize).min(11);
        let bucket = &mut buckets[idx];
        bucket.0 += 1;
        bucket.1 += rf;
        bucket.2 += gf;
        bucket.3 += bf;
    }

    let (idx, count) = buckets
        .iter()
        .enumerate()
        .fold((None, 0u64), |(best, best_n), (i, b)| {
            if b.0 > best_n {
                (Some(i), b.0)
            } else {
                (best, best_n)
            }
        });
    let idx = idx?;
    if count == 0 {
        return None;
    }
    let (_, sum_r, sum_g, sum_b) = buckets[idx];
    let (r, g, b) = (sum_r / count as f64, sum_g / count as f64, sum_b / count as f64);
    let (hue, mut sat, mut value) = rgb_to_hsv(r, g, b);
    sat = sat.clamp(0.45, 0.85);
    value = value.clamp(0.55, 0.80);
    Some(hsv_to_hex(hue, sat, value))
}

/// RGB (each 0.0..=1.0) to HSV, hue in degrees 0.0..360.0.
fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let hue = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };
    let sat = if max == 0.0 { 0.0 } else { d / max };
    (hue, sat, max)
}

/// HSV (hue degrees, saturation/value 0.0..=1.0) to a `#rrggbb` string.
fn hsv_to_hex(hue: f64, sat: f64, value: f64) -> String {
    let c = value * sat;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = value - c;
    let (r, g, b) = match hue {
        h if h < 60.0 => (c, x, 0.0),
        h if h < 120.0 => (x, c, 0.0),
        h if h < 180.0 => (0.0, c, x),
        h if h < 240.0 => (0.0, x, c),
        h if h < 300.0 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |f: f64| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", to(r), to(g), to(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(img: &image::RgbaImage) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn channels(hex: &str) -> (u8, u8, u8) {
        let r = u8::from_str_radix(&hex[1..3], 16).unwrap();
        let g = u8::from_str_radix(&hex[3..5], 16).unwrap();
        let b = u8::from_str_radix(&hex[5..7], 16).unwrap();
        (r, g, b)
    }

    #[test]
    fn hash_color_is_stable_and_in_palette() {
        let a = hash_color("games/mario");
        assert_eq!(a, hash_color("games/mario"));
        assert!(PALETTE.contains(&a.as_str()));
        assert_ne!(hash_color("games/mario"), "");
    }

    #[test]
    fn extracts_the_dominant_hue() {
        let mut img = image::RgbaImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = image::Rgba([220, 40, 30, 255]);
        }
        let hex = extract_auto_color(&png_bytes(&img)).unwrap();
        let (r, g, b) = channels(&hex);
        assert!(r > g && r > b, "expected a red tint, got {hex}");
    }

    #[test]
    fn keeps_a_bright_saturated_yellow() {
        // Regression: value near 1.0 alone must not count as "near white".
        let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([240, 200, 40, 255]));
        let hex = extract_auto_color(&png_bytes(&img)).unwrap();
        let (r, g, b) = channels(&hex);
        assert!(r > b && g > b, "expected a yellow tint, got {hex}");
    }

    #[test]
    fn ignores_white_and_transparent_pixels() {
        let white = image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255]));
        assert!(extract_auto_color(&png_bytes(&white)).is_none());
        let clear = image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 0]));
        assert!(extract_auto_color(&png_bytes(&clear)).is_none());
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(extract_auto_color(b"not an image").is_none());
    }
}
