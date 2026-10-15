//! Monitors' JPEG pictures: a BGRA frame becomes an RGB JPEG at a
//! stamp rate, small enough to post over the socket many times a
//! second (the encoder is the engine's own `image` dependency).

use anyhow::Result;
use image::ImageEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ColorType, ImageBuffer};

use crate::frames::LinuxFrame;

/// A frame as JPEG bytes, scaled to fit `max_width` when asked.
pub(crate) fn encode(
    frame: &LinuxFrame,
    max_width: Option<u32>,
    quality: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    let (width, height) = frame.size();
    let (bytes, stride) = frame.plane(0)?;
    if bytes.len() < stride * height as usize {
        anyhow::bail!(
            "frame plane is {} bytes, its {}x{} picture needs {}",
            bytes.len(),
            width,
            height,
            stride * height as usize
        );
    }
    // BGRA in planes of u8s without padding (stride is width * 4):
    // build the RGBA buffer the image crate encodes to JPEG from.
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for (target_row_index, target_row) in rgba.chunks_exact_mut(width as usize * 4).enumerate() {
        let from = target_row_index * stride;
        for (target_px, source_px) in target_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(bytes[from..].as_chunks::<4>().0)
        {
            target_px[0] = source_px[2];
            target_px[1] = source_px[1];
            target_px[2] = source_px[0];
            target_px[3] = source_px[3];
        }
    }
    let buffer = ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .ok_or_else(|| anyhow::anyhow!("{}x{} frame did not fit an image buffer", width, height))?;

    // Scale for monitors (area is the same filter the compositor uses
    // when it shrinks): ceil-keeping the shape.
    let buffer = match max_width {
        Some(max) if max < width => {
            let scale = max as f64 / width as f64;
            let new_height = (height as f64 * scale).round() as u32;

            image::imageops::resize(&buffer, max.max(1), new_height.max(1), FilterType::Triangle)
        }
        _ => buffer,
    };
    let (out_width, out_height) = (buffer.width(), buffer.height());

    let mut jpeg_data = Vec::new();
    let jpeg =
        JpegEncoder::new_with_quality(&mut jpeg_data, (quality.clamp(0.0, 1.0) * 100.0) as u8);
    // The image crate's RGBA JPEG path converts to RGB; the encoder
    // takes the RGBA buffer at ColorType::Rgba8.
    jpeg.write_image(
        buffer.as_raw(),
        out_width,
        out_height,
        ColorType::Rgba8.into(),
    )?;
    Ok((jpeg_data, out_width, out_height))
}
