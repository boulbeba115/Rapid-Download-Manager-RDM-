//! The application icon, drawn in code so it can be produced at any size
//! (window, tray, and the `.ico` embedded by the packaging tool).
//! A white download arrow over a tray, on a blue rounded square.

/// Straight-alpha RGBA pixels, `size` × `size`, antialiased by supersampling.
pub fn render(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            // Accumulate premultiplied colour over the sub-samples.
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if let Some([pr, pg, pb]) = sample(u, v) {
                        r += pr;
                        g += pg;
                        b += pb;
                        a += 1.0;
                    }
                }
            }
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                out[i] = (r / a).round() as u8;
                out[i + 1] = (g / a).round() as u8;
                out[i + 2] = (b / a).round() as u8;
                out[i + 3] = (a / (SS * SS) as f32 * 255.0).round() as u8;
            }
        }
    }
    out
}

/// Colour at a point of the unit square, or `None` outside the shape.
fn sample(u: f32, v: f32) -> Option<[f32; 3]> {
    // Rounded square with a small margin.
    let (lo, hi, radius) = (0.03, 0.97, 0.20);
    if !(lo..=hi).contains(&u) || !(lo..=hi).contains(&v) {
        return None;
    }
    let cx = u.clamp(lo + radius, hi - radius);
    let cy = v.clamp(lo + radius, hi - radius);
    if (u - cx).hypot(v - cy) > radius {
        return None;
    }

    let shaft = (0.415..=0.585).contains(&u) && (0.20..=0.50).contains(&v);
    let head = (0.46..=0.70).contains(&v) && (u - 0.5).abs() <= (0.70 - v) / 0.24 * 0.245;
    let tray = (0.25..=0.75).contains(&u) && (0.765..=0.835).contains(&v);
    if shaft || head || tray {
        return Some([255.0, 255.0, 255.0]);
    }
    // Vertical gradient, lighter at the top.
    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * t;
    Some([mix(0x3A as f32, 0x12 as f32), mix(0x9B as f32, 0x62 as f32), mix(0xEE as f32, 0xC0 as f32)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_the_expected_shape_at_every_size() {
        for size in [16u32, 32, 48, 256] {
            let px = render(size);
            assert_eq!(px.len(), (size * size * 4) as usize);
            let at = |u: f32, v: f32| {
                let (x, y) = ((u * size as f32) as u32, (v * size as f32) as u32);
                let i = ((y * size + x) * 4) as usize;
                [px[i], px[i + 1], px[i + 2], px[i + 3]]
            };
            assert_eq!(at(0.0, 0.0)[3], 0, "corners are transparent ({size}px)");
            assert_eq!(at(0.5, 0.35), [255, 255, 255, 255], "arrow shaft is white ({size}px)");
            let bg = at(0.15, 0.5);
            assert!(bg[3] == 255 && bg[2] > bg[0], "background is opaque blue ({size}px): {bg:?}");
        }
    }
}
