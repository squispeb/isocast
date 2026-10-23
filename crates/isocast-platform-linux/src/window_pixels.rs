//! Putting a window's picture on the canvas a take records at.
//!
//! A window's size is the compositor's to change, but a take's size is
//! fixed when its encoders open, so the picture is placed on the canvas
//! the window had at the start: copied as it is when the size is (within
//! a pixel of) the same, scaled to fit with bars otherwise: the same
//! "keeps its size if the window is resized, scaling the window to fit"
//! macOS does. An app's windows are placed where the compositor has them.
//!
//! Everything here is plain arithmetic on byte slices.

/// How the compositor's pixels read: the pools hold BGRA bytes
/// (premultiplied where there is alpha), and the shm formats a compositor
/// offers are those four-byte orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// The format carries alpha (an `x` format's fourth byte is padding).
    pub alpha: bool,
    /// Red comes first in memory (the `abgr`/`xbgr` formats).
    pub swapped: bool,
}

/// The `wl_shm` formats this build copies from, best first, with their
/// layouts: the codes are DRM fourccs, or 0 and 1 for ARGB and XRGB.
pub(crate) const FORMATS: [(u32, Layout); 4] = [
    (
        0,
        Layout {
            alpha: true,
            swapped: false,
        },
    ),
    (
        1,
        Layout {
            alpha: false,
            swapped: false,
        },
    ),
    (
        0x3432_4241,
        Layout {
            alpha: true,
            swapped: true,
        },
    ),
    (
        0x3432_4258,
        Layout {
            alpha: false,
            swapped: true,
        },
    ),
];

/// The first of the offered format codes this build can copy from.
pub(crate) fn choose_format(offered: &[u32]) -> Option<(u32, Layout)> {
    FORMATS
        .iter()
        .find(|(code, _)| offered.contains(code))
        .copied()
}

/// A picture as the compositor wrote it.
#[derive(Clone, Copy)]
pub(crate) struct Picture<'a> {
    pub bytes: &'a [u8],
    pub stride: usize,
    pub width: u32,
    pub height: u32,
    pub layout: Layout,
}

/// Where a picture goes on the canvas, in pixels (the corner may be
/// off the canvas; the part outside is cut).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// A size an encoder can take: rounded down to even, at least 2 (what
/// macOS does with a window's pixel size).
pub(crate) fn even(value: u32) -> u32 {
    value.max(2) & !1
}

/// Where a picture of size `source` goes on a canvas of size `canvas`:
/// the corner, unscaled, when it is within a pixel of the canvas's size
/// (an odd-sized window is cut to even, not rescaled), else scaled to fit
/// and centred.
pub(crate) fn fit(source: (u32, u32), canvas: (u32, u32)) -> Rect {
    let (sw, sh) = (source.0.max(1), source.1.max(1));
    let (cw, ch) = canvas;
    if sw.abs_diff(cw) <= 1 && sh.abs_diff(ch) <= 1 {
        return Rect {
            x: 0,
            y: 0,
            w: sw.min(cw),
            h: sh.min(ch),
        };
    }
    // Compared as fractions: cw/sw against ch/sh.
    let (w, h) = if u64::from(cw) * u64::from(sh) <= u64::from(ch) * u64::from(sw) {
        let h = (u64::from(sh) * u64::from(cw) + u64::from(sw) / 2) / u64::from(sw);
        (cw, (h as u32).clamp(1, ch))
    } else {
        let w = (u64::from(sw) * u64::from(ch) + u64::from(sh) / 2) / u64::from(sh);
        ((w as u32).clamp(1, cw), ch)
    };
    Rect {
        x: ((cw - w) / 2) as i32,
        y: ((ch - h) / 2) as i32,
        w,
        h,
    }
}

/// Draw `picture` into `rect` of the BGRA canvas `canvas_bytes`
/// (`canvas_stride` bytes per row, `canvas` pixels), nearest-neighbour
/// when the rectangle's size is not the picture's. `over` blends it over
/// what is there (premultiplied); otherwise it replaces it. Pixels
/// outside the canvas, or a picture shorter than it says, are skipped.
pub(crate) fn blit(
    canvas_bytes: &mut [u8],
    canvas_stride: usize,
    canvas: (u32, u32),
    rect: Rect,
    picture: &Picture<'_>,
    over: bool,
) {
    if rect.w == 0 || rect.h == 0 || picture.width == 0 || picture.height == 0 {
        return;
    }
    let first_row = i64::from(rect.y).min(0).unsigned_abs() as i64;
    let last_row = (i64::from(canvas.1) - i64::from(rect.y)).min(i64::from(rect.h));
    let first_col = i64::from(rect.x).min(0).unsigned_abs() as i64;
    let last_col = (i64::from(canvas.0) - i64::from(rect.x)).min(i64::from(rect.w));
    if first_row >= last_row || first_col >= last_col {
        return;
    }
    let same_width = rect.w == picture.width;
    let straight = !over && !picture.layout.swapped && picture.layout.alpha && same_width;
    for row in first_row..last_row {
        let source_row = if rect.h == picture.height {
            row as usize
        } else {
            (row as u64 * u64::from(picture.height) / u64::from(rect.h)) as usize
        };
        let from = source_row * picture.stride;
        let Some(source) = picture.bytes.get(from..from + picture.width as usize * 4) else {
            continue;
        };
        let to = (i64::from(rect.y) + row) as usize * canvas_stride
            + (i64::from(rect.x) + first_col) as usize * 4;
        let count = (last_col - first_col) as usize;
        let Some(dest) = canvas_bytes.get_mut(to..to + count * 4) else {
            continue;
        };
        if straight {
            let start = first_col as usize * 4;
            dest.copy_from_slice(&source[start..start + count * 4]);
            continue;
        }
        for (i, out) in dest.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let column = first_col as usize + i;
            let source_column = if same_width {
                column
            } else {
                (column as u64 * u64::from(picture.width) / u64::from(rect.w)) as usize
            };
            let pixel = &source[source_column * 4..source_column * 4 + 4];
            let (mut b, g, mut r) = (pixel[0], pixel[1], pixel[2]);
            if picture.layout.swapped {
                std::mem::swap(&mut b, &mut r);
            }
            let a = if picture.layout.alpha { pixel[3] } else { 255 };
            if !over || a == 255 {
                out.copy_from_slice(&[b, g, r, a]);
            } else if a != 0 {
                let keep = u32::from(255 - a);
                for (out, add) in out.iter_mut().zip([b, g, r, a]) {
                    *out = (u32::from(add) + (u32::from(*out) * keep + 127) / 255).min(255) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGB: Layout = Layout {
        alpha: true,
        swapped: false,
    };

    #[test]
    fn even_rounds_down_with_a_floor_of_two() {
        assert_eq!(even(1), 2);
        assert_eq!(even(2), 2);
        assert_eq!(even(1023), 1022);
        assert_eq!(even(1080), 1080);
        assert_eq!(even(0), 2);
    }

    #[test]
    fn near_sizes_are_cut_and_others_scaled_to_fit() {
        let canvas = (1022, 1100);
        // The odd size a fractional scale makes: cut, not rescaled.
        assert_eq!(
            fit((1023, 1101), canvas),
            Rect {
                x: 0,
                y: 0,
                w: 1022,
                h: 1100
            }
        );
        // Smaller by a pixel: the whole picture, at the corner.
        assert_eq!(
            fit((1021, 1100), canvas),
            Rect {
                x: 0,
                y: 0,
                w: 1021,
                h: 1100
            }
        );
        // Half the width, same height: scaled up to the height, centred.
        let rect = fit((511, 1100), canvas);
        assert_eq!((rect.h, rect.w), (1100, 511));
        let wide = fit((2000, 500), (1000, 1000));
        assert_eq!(
            wide,
            Rect {
                x: 0,
                y: 375,
                w: 1000,
                h: 250
            }
        );
        let tall = fit((500, 2000), (1000, 1000));
        assert_eq!(
            tall,
            Rect {
                x: 375,
                y: 0,
                w: 250,
                h: 1000
            }
        );
    }

    #[test]
    fn straight_copy_keeps_pixels_and_cuts_the_overhang() {
        // 3x2 source, 2x2 canvas, source rows padded to 16 bytes.
        let mut source = vec![0u8; 32];
        for row in 0..2 {
            for col in 0..3 {
                let at = row * 16 + col * 4;
                source[at..at + 4].copy_from_slice(&[col as u8, row as u8, 7, 255]);
            }
        }
        let picture = Picture {
            bytes: &source,
            stride: 16,
            width: 3,
            height: 2,
            layout: ARGB,
        };
        let mut canvas = vec![9u8; 16];
        blit(
            &mut canvas,
            8,
            (2, 2),
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            &picture,
            false,
        );
        // rect.w (2) != picture.width (3) so this one is the scaled path:
        // dest column 1 takes source column 1*3/2 = 1.
        assert_eq!(&canvas[0..4], &[0, 0, 7, 255]);
        assert_eq!(&canvas[4..8], &[1, 0, 7, 255]);
        assert_eq!(&canvas[8..12], &[0, 1, 7, 255]);
    }

    #[test]
    fn same_width_rows_copy_whole_and_negative_corners_clip() {
        let source: Vec<u8> = (0..16).collect(); // 2x2, stride 8
        let picture = Picture {
            bytes: &source,
            stride: 8,
            width: 2,
            height: 2,
            layout: ARGB,
        };
        let mut canvas = vec![0u8; 16];
        // Corner at (-1, -1): only the source's bottom-right pixel lands,
        // at the canvas's (0, 0).
        blit(
            &mut canvas,
            8,
            (2, 2),
            Rect {
                x: -1,
                y: -1,
                w: 2,
                h: 2,
            },
            &picture,
            false,
        );
        assert_eq!(&canvas[0..4], &[12, 13, 14, 15]);
        assert_eq!(&canvas[4..8], &[0, 0, 0, 0]);
        assert_eq!(&canvas[8..16], &[0; 8]);
    }

    #[test]
    fn x_formats_become_opaque_and_swapped_ones_reorder() {
        // XRGB with garbage in the pad byte.
        let source = [10u8, 20, 30, 0];
        let xrgb = Picture {
            bytes: &source,
            stride: 4,
            width: 1,
            height: 1,
            layout: Layout {
                alpha: false,
                swapped: false,
            },
        };
        let mut canvas = [0u8; 4];
        let rect = Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        blit(&mut canvas, 4, (1, 1), rect, &xrgb, false);
        assert_eq!(canvas, [10, 20, 30, 255]);
        // XBGR memory order is R, G, B: reading it as BGRA swaps 0 and 2.
        let abgr = Picture {
            layout: Layout {
                alpha: false,
                swapped: true,
            },
            ..xrgb
        };
        blit(&mut canvas, 4, (1, 1), rect, &abgr, false);
        assert_eq!(canvas, [30, 20, 10, 255]);
    }

    #[test]
    fn blending_is_source_over_on_premultiplied_pixels() {
        let half = [0u8, 0, 128, 128]; // premultiplied half-transparent red
        let picture = Picture {
            bytes: &half,
            stride: 4,
            width: 1,
            height: 1,
            layout: ARGB,
        };
        let rect = Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        let mut canvas = [255u8, 255, 255, 255];
        blit(&mut canvas, 4, (1, 1), rect, &picture, true);
        assert_eq!(canvas, [127, 127, 255, 255]);
        // Over nothing, the premultiplied pixel is kept as it is.
        let mut canvas = [0u8; 4];
        blit(&mut canvas, 4, (1, 1), rect, &picture, true);
        assert_eq!(canvas, [0, 0, 128, 128]);
        // A fully transparent pixel leaves the canvas alone.
        let clear = Picture {
            bytes: &[0, 0, 0, 0],
            ..picture
        };
        let mut canvas = [5u8, 6, 7, 8];
        blit(&mut canvas, 4, (1, 1), rect, &clear, true);
        assert_eq!(canvas, [5, 6, 7, 8]);
    }

    #[test]
    fn format_choice_prefers_argb() {
        assert_eq!(choose_format(&[1, 0]).map(|(code, _)| code), Some(0));
        assert_eq!(choose_format(&[0x3432_4258, 1]).map(|(c, _)| c), Some(1));
        assert!(choose_format(&[0x3030_3234]).is_none());
    }
}
