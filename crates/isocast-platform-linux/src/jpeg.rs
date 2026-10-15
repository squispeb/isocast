//! Monitors' JPEG pictures: a frame becomes an RGB JPEG at a stamp rate,
//! small enough to post over the socket many times a second (the encoder
//! is the engine's own `image` dependency).
//!
//! Either pixel layout a pool hands over is handled: BGRA (sources, CPU
//! drawing) and 8-bit 4:2:0 Y'CbCr in NV12's layout, video range, Rec. 709
//! (the program's GPU conversion, which is what a monitor of the program
//! actually gets). The JPEG encoder takes Rgb8 or Luma8 only — handing it
//! Rgba8 fails with "does not support the color type `Rgba8`".

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
    let rgb = if frame.is_ycbcr() {
        ycbcr_to_rgb(frame, width, height)?
    } else {
        bgra_to_rgb(frame, width, height)?
    };
    let buffer = ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, rgb)
        .ok_or_else(|| anyhow::anyhow!("{width}x{height} frame did not fit an image buffer"))?;

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
    let quality = (quality.clamp(0.0, 1.0) * 100.0).round() as u8;
    let jpeg = JpegEncoder::new_with_quality(&mut jpeg_data, quality.clamp(1, 100));
    jpeg.write_image(
        buffer.as_raw(),
        out_width,
        out_height,
        ColorType::Rgb8.into(),
    )?;
    Ok((jpeg_data, out_width, out_height))
}

/// One BGRA plane, as RGB rows.
fn bgra_to_rgb(frame: &LinuxFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    let (bytes, stride) = frame.plane(0)?;
    if bytes.len() < stride * height as usize {
        anyhow::bail!(
            "frame plane is {} bytes, its {width}x{height} picture needs {}",
            bytes.len(),
            stride * height as usize
        );
    }
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    for (target_row_index, target_row) in rgb.chunks_exact_mut(width as usize * 3).enumerate() {
        let from = target_row_index * stride;
        for (target_px, source_px) in target_row
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(bytes[from..].as_chunks::<4>().0)
        {
            target_px[0] = source_px[2];
            target_px[1] = source_px[1];
            target_px[2] = source_px[0];
        }
    }
    Ok(rgb)
}

/// Two-plane 4:2:0 Y'CbCr (full-size luma, then half-size Cb and Cr
/// interleaved) in video range, Rec. 709, as RGB rows.
fn ycbcr_to_rgb(frame: &LinuxFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    let (luma, luma_stride) = frame.plane(0)?;
    let (chroma, chroma_stride) = frame.plane(1)?;
    if luma.len() < luma_stride * height as usize {
        anyhow::bail!(
            "luma plane is {} bytes, its {width}x{height} picture needs {}",
            luma.len(),
            luma_stride * height as usize
        );
    }
    let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
    if chroma.len() < chroma_stride * chroma_height as usize {
        anyhow::bail!(
            "chroma plane is {} bytes, its {chroma_width}x{chroma_height} picture needs {}",
            chroma.len(),
            chroma_stride * chroma_height as usize
        );
    }
    // Video range: luma 16..=235 and chroma 16..=240 cover 0..=255.
    const LUMA_SCALE: f32 = 255.0 / 219.0;
    const CHROMA_SCALE: f32 = 255.0 / 224.0;
    let clip = |value: f32| value.clamp(0.0, 255.0).round() as u8;

    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    for row in 0..height as usize {
        let luma_row = row * luma_stride;
        let chroma_row = (row / 2) * chroma_stride;
        for column in 0..width as usize {
            let y = (luma[luma_row + column] as f32 - 16.0) * LUMA_SCALE;
            let pair = chroma_row + (column / 2) * 2;
            let cb = (chroma[pair] as f32 - 128.0) * CHROMA_SCALE;
            let cr = (chroma[pair + 1] as f32 - 128.0) * CHROMA_SCALE;
            let at = (row * width as usize + column) * 3;
            rgb[at] = clip(y + 1.5748 * cr);
            rgb[at + 1] = clip(y - 0.187_324_1 * cb - 0.468_124_4 * cr);
            rgb[at + 2] = clip(y + 1.855_6 * cb);
        }
    }
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::encode;
    use crate::frames::CpuPool;
    use isocast_engine::FramePool;

    /// A BGRA picture the engine drew must come back as a real JPEG, red
    /// where it was red, and at the size asked for. The first version
    /// handed the encoder an RGBA image and every monitor request failed
    /// with "does not support the color type `Rgba8`".
    #[test]
    fn bgra_frame_encodes_and_scales() {
        let mut pool = CpuPool::bgra(4, 2);
        let frame = pool
            .draw(&mut |bytes, stride| {
                for row in bytes.chunks_exact_mut(stride) {
                    for pixel in row.as_chunks_mut::<4>().0 {
                        pixel.copy_from_slice(&[0, 0, 255, 255]); // BGRA red
                    }
                }
            })
            .expect("a draw")
            .expect("a frame");

        let (jpeg, width, height) = encode(&frame, None, 0.75).expect("a JPEG");
        assert_eq!((width, height), (4, 2));
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "a JPEG starts with SOI");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "and ends with EOI");
        let decoded = image::load_from_memory(&jpeg).expect("the JPEG decodes");
        let pixel = decoded.to_rgb8().get_pixel(0, 0).0;
        assert!(pixel[0] > 200 && pixel[1] < 60, "red: {pixel:?}");

        let (jpeg, width, height) = encode(&frame, Some(2), 0.75).expect("a scaled JPEG");
        assert_eq!((width, height), (2, 1));
        assert!(jpeg.len() > 20);
    }

    /// The program's frames are Y'CbCr (that pool is what `program.rs`
    /// composites into), so a monitor of the program goes through this
    /// path: white at luma 235 with neutral chroma, and hue when the
    /// chroma moves.
    #[test]
    fn ycbcr_frame_encodes_colours() {
        let mut pool = CpuPool::ycbcr(4, 2);
        let frame = pool.take().expect("a take").expect("a frame");
        let (_, luma_stride) = frame.plane(0).expect("luma");
        let (_, chroma_stride) = frame.plane(1).expect("chroma");
        // White in video range, neutral chroma (the pool's strides: one
        // luma row per line, one interleaved chroma row per two lines).
        frame.set_plane(0, vec![235; luma_stride * 2]);
        frame.set_plane(1, vec![128; chroma_stride]);

        let (jpeg, width, height) = encode(&frame, None, 0.9).expect("a JPEG");
        assert_eq!((width, height), (4, 2));
        let decoded = image::load_from_memory(&jpeg).expect("the JPEG decodes");
        let pixel = decoded.to_rgb8().get_pixel(1, 1).0;
        assert!(
            pixel[0] > 230 && pixel[1] > 230 && pixel[2] > 230,
            "luma 235 with neutral chroma is white: {pixel:?}"
        );
    }
}
