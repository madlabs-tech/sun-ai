//! PNG helpers for design-canvas evidence: crop a pin's box out of a
//! preview screenshot and measure how much two screenshots of the
//! same screen differ (the `visual_diff` gate). Screenshots are
//! taken at device pixel ratio 1, so CSS pixels are image pixels.

use std::io::Cursor;

use png::{BitDepth, ColorType, Decoder, Encoder, Transformations};
use sun_core::model::PinRect;
use sun_core::{Error, Result};

/// CSS pixels of context kept around a pinned element.
const PAD: f64 = 8.0;
/// A channel must move more than this to count a pixel as changed
/// (anti-aliasing and font hinting jitter between renders).
const TOLERANCE: u8 = 16;

/// An 8-bit RGBA image.
pub(crate) struct Image {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

fn bad(e: impl std::fmt::Display) -> Error {
    Error::InvalidInput(format!("screenshot: {e}"))
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Image> {
    let mut decoder = Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(bad)?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| bad("too large"))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(bad)?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        ColorType::Rgba => buf,
        ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        ColorType::Indexed => return Err(bad("indexed colour after expansion")),
    };
    Ok(Image {
        width: info.width,
        height: info.height,
        rgba,
    })
}

pub(crate) fn encode(image: &Image) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(bad)?;
    writer.write_image_data(&image.rgba).map_err(bad)?;
    writer.finish().map_err(bad)?;
    Ok(out)
}

/// The viewport height that shows all of `rect` (at least a screen's worth).
pub(crate) fn height_for(rect: &PinRect) -> u32 {
    ((rect.y + rect.height + PAD).ceil()).clamp(800.0, 8000.0) as u32
}

/// `rect` plus [`PAD`], clipped to the screenshot, as a PNG.
pub(crate) fn crop(png: &[u8], rect: &PinRect) -> Result<Vec<u8>> {
    let image = decode(png)?;
    let clip = |v: f64, max: u32| v.clamp(0.0, f64::from(max)) as u32;
    let (x0, y0) = (
        clip((rect.x - PAD).floor(), image.width),
        clip((rect.y - PAD).floor(), image.height),
    );
    let (x1, y1) = (
        clip((rect.x + rect.width + PAD).ceil(), image.width),
        clip((rect.y + rect.height + PAD).ceil(), image.height),
    );
    if x1 <= x0 || y1 <= y0 {
        return Err(bad("the pinned element is outside the page"));
    }
    let stride = image.width as usize * 4;
    let mut rgba = Vec::with_capacity((x1 - x0) as usize * (y1 - y0) as usize * 4);
    for y in y0..y1 {
        let row = y as usize * stride;
        rgba.extend_from_slice(&image.rgba[row + x0 as usize * 4..row + x1 as usize * 4]);
    }
    encode(&Image {
        width: x1 - x0,
        height: y1 - y0,
        rgba,
    })
}

/// Share of pixels (0..=1) that differ between two screenshots. Pixels
/// that only one image has (a size change) count as changed.
pub(crate) fn changed(a: &[u8], b: &[u8]) -> Result<f64> {
    let (a, b) = (decode(a)?, decode(b)?);
    let (width, height) = (a.width.max(b.width), a.height.max(b.height));
    fn pixel(image: &Image, x: u32, y: u32) -> Option<&[u8]> {
        let i = (y as usize * image.width as usize + x as usize) * 4;
        (x < image.width && y < image.height).then(|| &image.rgba[i..i + 4])
    }
    let mut differ = 0u64;
    for y in 0..height {
        for x in 0..width {
            let same = match (pixel(&a, x, y), pixel(&b, x, y)) {
                (Some(p), Some(q)) => p.iter().zip(q).all(|(u, v)| u.abs_diff(*v) <= TOLERANCE),
                _ => false,
            };
            differ += u64::from(!same);
        }
    }
    let total = u64::from(width) * u64::from(height);
    Ok(if total == 0 {
        0.0
    } else {
        differ as f64 / total as f64
    })
}

/// A test PNG whose pixel `(x, y)` is `paint(x, y)`.
#[cfg(test)]
pub(crate) fn test_png(width: u32, height: u32, paint: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let rgba = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .flat_map(|(x, y)| paint(x, y))
        .collect();
    encode(&Image {
        width,
        height,
        rgba,
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn crops_the_padded_box_and_counts_changed_pixels() {
        let shot = test_png(100, 100, |x, y| [x as u8, y as u8, 0, 255]);
        let rect = PinRect {
            x: 20.0,
            y: 30.0,
            width: 10.0,
            height: 5.0,
        };
        let cut = decode(&crop(&shot, &rect).unwrap()).unwrap();
        assert_eq!((cut.width, cut.height), (26, 21));
        assert_eq!(&cut.rgba[..4], &[12, 22, 0, 255]);
        let off_page = PinRect { x: 500.0, ..rect };
        assert!(crop(&shot, &off_page).is_err());

        assert_eq!(changed(&shot, &shot).unwrap(), 0.0);
        let half = test_png(100, 100, |x, y| {
            [if x < 50 { 255 } else { x as u8 }, y as u8, 0, 255]
        });
        assert!((changed(&shot, &half).unwrap() - 0.5).abs() < 0.01);
    }
}
