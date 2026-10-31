//! Copie d'une région d'une image libavcodec (en RAM) vers des [`Planes`] compactés, au format
//! natif : yuv420p ⇒ `Planar8`, yuv420p10le ⇒ `Planar10`, nv12 ⇒ `SemiPlanar8`,
//! p010le ⇒ `SemiPlanar10`. Les `strides` sont en octets.

use ma_decode::{DecodeError, DecodeErrorKind, PixelLayout, Planes, Rect, Size};
use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;

/// Layout et octets par échantillon d'un format de pixel `FFmpeg` pris en charge.
pub(crate) fn layout_of(format: i32) -> Option<(PixelLayout, usize)> {
    match format {
        ffi::AV_PIX_FMT_YUV420P => Some((PixelLayout::Planar8, 1)),
        ffi::AV_PIX_FMT_YUV420P10LE => Some((PixelLayout::Planar10, 2)),
        ffi::AV_PIX_FMT_NV12 => Some((PixelLayout::SemiPlanar8, 1)),
        ffi::AV_PIX_FMT_P010LE => Some((PixelLayout::SemiPlanar10, 2)),
        _ => None,
    }
}

fn half(v: usize) -> usize {
    v.div_ceil(2)
}

/// (octets utiles par ligne, nombre de lignes) de chaque plan d'une image de taille `w` × `h`
/// (chroma 4:2:0, arrondie vers le haut pour les tailles impaires).
fn plane_dims(layout: PixelLayout, bps: usize, w: usize, h: usize) -> Vec<(usize, usize)> {
    match layout {
        PixelLayout::Planar8 | PixelLayout::Planar10 => {
            vec![
                (w * bps, h),
                (half(w) * bps, half(h)),
                (half(w) * bps, half(h)),
            ]
        }
        PixelLayout::SemiPlanar8 | PixelLayout::SemiPlanar10 => {
            vec![(w * bps, h), (half(w) * 2 * bps, half(h))]
        }
    }
}

/// (décalage en octets dans la ligne, première ligne) du point (`x`, `y`) de l'image, `x` et `y`
/// pairs, dans le plan `plane`.
fn plane_origin(
    layout: PixelLayout,
    bps: usize,
    plane: usize,
    x: usize,
    y: usize,
) -> (usize, usize) {
    match (plane, layout) {
        (0, _) => (x * bps, y),
        (_, PixelLayout::Planar8 | PixelLayout::Planar10) => (x / 2 * bps, y / 2),
        // Chroma entrelacée U, V : un couple par paire de colonnes de luma.
        (_, PixelLayout::SemiPlanar8 | PixelLayout::SemiPlanar10) => (x * bps, y / 2),
    }
}

/// Copie compacte de `rows` lignes de `row` octets, depuis (`x`, `y`) dans `src` (lignes de
/// `stride` octets).
///
/// # Panics
/// Si la zone sort de `src` (exclu par les appelants).
fn copy_plane(
    src: &[u8],
    stride: usize,
    (x, y): (usize, usize),
    (row, rows): (usize, usize),
) -> Vec<u8> {
    let mut out = Vec::with_capacity(row * rows);
    for r in y..y + rows {
        let start = r * stride + x;
        out.extend_from_slice(&src[start..start + row]);
    }
    out
}

fn internal(detail: String) -> DecodeError {
    DecodeError {
        kind: DecodeErrorKind::Internal,
        detail,
    }
}

fn to_usize(v: i32, what: &str) -> Result<usize, DecodeError> {
    usize::try_from(v).map_err(|_| internal(format!("{what} négatif : {v}")))
}

fn to_u32(v: usize) -> u32 {
    u32::try_from(v).expect("dimension d'image bornée par i32")
}

/// Copie de la région `region` (normalisée : origine paire, incluse dans l'image) de `frame`.
pub(crate) fn extract(frame: &AVFrame, region: Rect) -> Result<Planes, DecodeError> {
    let (layout, bps) = layout_of(frame.format).ok_or_else(|| DecodeError {
        kind: DecodeErrorKind::Unsupported,
        detail: format!("format de pixel non géré : {}", frame.format),
    })?;
    let (w, h) = (
        to_usize(frame.width, "largeur")?,
        to_usize(frame.height, "hauteur")?,
    );
    let (x, y) = (region.x as usize, region.y as usize);
    let (rw, rh) = (region.w as usize, region.h as usize);
    if x % 2 != 0 || y % 2 != 0 || rw == 0 || rh == 0 || x + rw > w || y + rh > h {
        return Err(internal(format!(
            "région {region:?} hors de l'image {w}x{h}"
        )));
    }
    let full = plane_dims(layout, bps, w, h);
    let wanted = plane_dims(layout, bps, rw, rh);
    let mut planes = Vec::with_capacity(full.len());
    for (i, (&(full_row, full_rows), &dims)) in full.iter().zip(&wanted).enumerate() {
        let stride = to_usize(frame.linesize[i], "pas de ligne")?;
        let data = frame.data[i];
        if data.is_null() || stride < full_row {
            return Err(internal(format!(
                "plan {i} invalide (pas de ligne {stride})"
            )));
        }
        let len = stride * (full_rows - 1) + full_row;
        // SAFETY: pour une image décodée (ou transférée) dans un format pris en charge, `data[i]`
        // (non nul, vérifié) pointe sur `full_rows` lignes espacées de `linesize[i]` octets
        // (positif et au moins égal à la largeur utile du plan, vérifié), qui restent valides et
        // non modifiées tant que `frame` est emprunté.
        let src = unsafe { std::slice::from_raw_parts(data, len) };
        planes.push(copy_plane(
            src,
            stride,
            plane_origin(layout, bps, i, x, y),
            dims,
        ));
    }
    Ok(Planes {
        layout,
        size: Size {
            w: region.w,
            h: region.h,
        },
        planes,
        strides: wanted.iter().map(|&(row, _)| to_u32(row)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image allouée par libavutil, chaque octet valant (plan, ligne, octet de la ligne) mélangés.
    fn frame(format: i32, w: i32, h: i32) -> AVFrame {
        let mut f = AVFrame::new();
        f.set_format(format);
        f.set_width(w);
        f.set_height(h);
        f.alloc_buffer().unwrap();
        let (layout, bps) = layout_of(format).unwrap();
        for (i, (row, rows)) in plane_dims(
            layout,
            bps,
            usize::try_from(w).unwrap(),
            usize::try_from(h).unwrap(),
        )
        .into_iter()
        .enumerate()
        {
            let stride = usize::try_from(f.linesize[i]).unwrap();
            // SAFETY: plan alloué par `alloc_buffer` : `rows` lignes de `stride` octets.
            let plane = unsafe { std::slice::from_raw_parts_mut(f.data[i], stride * rows) };
            for r in 0..rows {
                for b in 0..row {
                    plane[r * stride + b] = value(i, r, b);
                }
            }
        }
        f
    }

    fn value(plane: usize, row: usize, byte: usize) -> u8 {
        u8::try_from((plane * 97 + row * 31 + byte * 7) % 251).unwrap()
    }

    #[test]
    fn layouts_follow_the_native_format() {
        assert_eq!(
            layout_of(ffi::AV_PIX_FMT_YUV420P),
            Some((PixelLayout::Planar8, 1))
        );
        assert_eq!(
            layout_of(ffi::AV_PIX_FMT_YUV420P10LE),
            Some((PixelLayout::Planar10, 2))
        );
        assert_eq!(
            layout_of(ffi::AV_PIX_FMT_NV12),
            Some((PixelLayout::SemiPlanar8, 1))
        );
        assert_eq!(
            layout_of(ffi::AV_PIX_FMT_P010LE),
            Some((PixelLayout::SemiPlanar10, 2))
        );
        assert_eq!(layout_of(ffi::AV_PIX_FMT_RGB24), None);
    }

    #[test]
    fn region_is_copied_byte_for_byte_in_every_layout() {
        let region = Rect {
            x: 4,
            y: 2,
            w: 7,
            h: 5,
        };
        for format in [
            ffi::AV_PIX_FMT_YUV420P,
            ffi::AV_PIX_FMT_YUV420P10LE,
            ffi::AV_PIX_FMT_NV12,
            ffi::AV_PIX_FMT_P010LE,
        ] {
            let f = frame(format, 11, 7);
            let p = extract(&f, region).unwrap();
            let (layout, bps) = layout_of(format).unwrap();
            assert_eq!((p.layout, p.size), (layout, Size { w: 7, h: 5 }));
            let dims = plane_dims(layout, bps, 7, 5);
            assert_eq!(p.planes.len(), dims.len());
            for (i, (&(row, rows), plane)) in dims.iter().zip(&p.planes).enumerate() {
                assert_eq!(p.strides[i] as usize, row);
                assert_eq!(plane.len(), row * rows);
                let (ox, oy) = plane_origin(layout, bps, i, 4, 2);
                for r in 0..rows {
                    for b in 0..row {
                        assert_eq!(
                            plane[r * row + b],
                            value(i, oy + r, ox + b),
                            "{format} plan {i}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn region_reaching_the_odd_edge() {
        // Image 11×7 entière : chroma de 6×4.
        let f = frame(ffi::AV_PIX_FMT_NV12, 11, 7);
        let p = extract(
            &f,
            Rect {
                x: 0,
                y: 0,
                w: 11,
                h: 7,
            },
        )
        .unwrap();
        assert_eq!(p.strides, [11, 12]);
        assert_eq!(p.planes[1].len(), 12 * 4);
    }

    #[test]
    fn invalid_regions_and_formats_are_rejected() {
        let f = frame(ffi::AV_PIX_FMT_YUV420P, 8, 8);
        for r in [
            Rect {
                x: 1,
                y: 0,
                w: 2,
                h: 2,
            },
            Rect {
                x: 0,
                y: 1,
                w: 2,
                h: 2,
            },
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 2,
            },
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 0,
            },
            Rect {
                x: 6,
                y: 0,
                w: 4,
                h: 2,
            },
            Rect {
                x: 0,
                y: 6,
                w: 2,
                h: 4,
            },
        ] {
            assert_eq!(
                extract(&f, r).unwrap_err().kind,
                DecodeErrorKind::Internal,
                "{r:?}"
            );
        }
        let mut rgb = AVFrame::new();
        rgb.set_format(ffi::AV_PIX_FMT_RGB24);
        let err = extract(
            &rgb,
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Unsupported);
        // Image sans tampon, ou de taille négative.
        let mut empty = AVFrame::new();
        empty.set_format(ffi::AV_PIX_FMT_YUV420P);
        empty.set_width(8);
        empty.set_height(8);
        assert_eq!(
            extract(
                &empty,
                Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2
                }
            )
            .unwrap_err()
            .kind,
            DecodeErrorKind::Internal
        );
        empty.set_height(-1);
        assert!(
            extract(
                &empty,
                Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2
                }
            )
            .unwrap_err()
            .detail
            .contains("négatif")
        );
    }
}
