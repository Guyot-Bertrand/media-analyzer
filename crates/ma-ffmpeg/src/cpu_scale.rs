//! Réduction et recadrage en logiciel : libswscale (`SWS_AREA`, sortie yuv420p 8 bits) pour une
//! réduction, copie de la région au format natif en 1:1.

use ma_decode::{DecodeError, DecodeErrorKind, Planes, Rect, Viewport};
use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;

use crate::planes;

fn internal(detail: String) -> DecodeError {
    DecodeError {
        kind: DecodeErrorKind::Internal,
        detail,
    }
}

fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Réduction et recadrage en RAM ; garde le contexte libswscale d'un appel à l'autre.
#[derive(Default)]
pub(crate) struct CpuScaler {
    sws: Option<SwsContext>,
}

impl CpuScaler {
    /// Image logicielle `frame` rendue pour `vp` (normalisé) : réduction en `Planar8`, ou copie
    /// de la région 1:1 au format natif.
    pub(crate) fn scale(&mut self, frame: &AVFrame, vp: &Viewport) -> Result<Planes, DecodeError> {
        if !vp.is_reduced() {
            return planes::extract(frame, vp.region);
        }
        let (layout, bps) = planes::layout_of(frame.format).ok_or_else(|| DecodeError {
            kind: DecodeErrorKind::Unsupported,
            detail: format!("format de pixel non géré : {}", frame.format),
        })?;
        let r = vp.region;
        let inside = u32::try_from(frame.width).is_ok_and(|w| r.x + r.w <= w)
            && u32::try_from(frame.height).is_ok_and(|h| r.y + r.h <= h);
        if !r.x.is_multiple_of(2) || !r.y.is_multiple_of(2) || !inside {
            return Err(internal(format!(
                "région {r:?} hors de l'image {}x{}",
                frame.width, frame.height
            )));
        }
        let mut src: [*const u8; 4] = [std::ptr::null(); 4];
        let mut src_stride = [0; 4];
        for i in 0..planes::plane_dims(layout, bps, 2, 2).len() {
            if frame.data[i].is_null() {
                return Err(internal(format!("plan {i} absent")));
            }
            let (ox, oy) = planes::plane_origin(layout, bps, i, r.x as usize, r.y as usize);
            let stride = usize::try_from(frame.linesize[i])
                .map_err(|_| internal(format!("pas de ligne négatif : {}", frame.linesize[i])))?;
            // SAFETY: `data[i]` (non nul, vérifié) pointe sur un plan d'au moins `oy + 1` lignes de
            // `stride` octets, dont l'octet `ox` existe : la région est dans l'image (vérifié) et
            // son origine (`ox`, `oy`) dans le plan s'en déduit.
            src[i] = unsafe { frame.data[i].add(oy * stride + ox) };
            src_stride[i] = frame.linesize[i];
        }
        let sws = match self.sws.take() {
            Some(s) => s.get_cached_context(
                to_i32(r.w),
                to_i32(r.h),
                frame.format,
                to_i32(vp.out.w),
                to_i32(vp.out.h),
                ffi::AV_PIX_FMT_YUV420P,
                ffi::SWS_AREA,
                None,
                None,
                None,
            ),
            None => SwsContext::get_context(
                to_i32(r.w),
                to_i32(r.h),
                frame.format,
                to_i32(vp.out.w),
                to_i32(vp.out.h),
                ffi::AV_PIX_FMT_YUV420P,
                ffi::SWS_AREA,
                None,
                None,
                None,
            ),
        };
        let sws = self
            .sws
            .insert(sws.ok_or_else(|| internal("contexte libswscale refusé".into()))?);
        let mut out = AVFrame::new();
        out.set_format(ffi::AV_PIX_FMT_YUV420P);
        out.set_width(to_i32(vp.out.w));
        out.set_height(to_i32(vp.out.h));
        out.alloc_buffer()
            .map_err(|e| internal(format!("image réduite : {e}")))?;
        // SAFETY: `src` et `src_stride` décrivent la région (`r.w` × `r.h`) dans les plans vivants
        // de `frame` (voir ci-dessus) ; `out` est alloué au format et à la taille de sortie du
        // contexte.
        unsafe {
            sws.scale(
                src.as_ptr(),
                src_stride.as_ptr(),
                0,
                to_i32(r.h),
                out.data.as_ptr(),
                out.linesize.as_ptr(),
            )
        }
        .map_err(|e| internal(format!("libswscale : {e}")))?;
        planes::extract(
            &out,
            Rect {
                x: 0,
                y: 0,
                w: vp.out.w,
                h: vp.out.h,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ma_decode::{PixelLayout, Size};

    fn frame(format: i32, w: i32, h: i32, luma: u8) -> AVFrame {
        let mut f = AVFrame::new();
        f.set_format(format);
        f.set_width(w);
        f.set_height(h);
        f.alloc_buffer().unwrap();
        let (layout, bps) = planes::layout_of(format).unwrap();
        for (i, (row, rows)) in planes::plane_dims(
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
                    // Luma à gauche de x = 32 noire, à droite `luma` ; chroma neutre (8 bits).
                    plane[r * stride + b] = match (i, bps) {
                        (0, 1) => {
                            if b < 32 {
                                16
                            } else {
                                luma
                            }
                        }
                        (0, _) => 0,
                        _ => 128,
                    };
                }
            }
        }
        f
    }

    fn vp(x: u32, y: u32, w: u32, h: u32, ow: u32, oh: u32) -> Viewport {
        Viewport {
            region: Rect { x, y, w, h },
            out: Size { w: ow, h: oh },
        }
    }

    #[test]
    fn reduction_is_8_bit_planar_and_follows_the_region() {
        let f = frame(ffi::AV_PIX_FMT_YUV420P, 128, 64, 200);
        let mut s = CpuScaler::default();
        // Moitié droite (x ≥ 64) réduite de moitié : uniformément claire.
        let p = s.scale(&f, &vp(64, 0, 64, 64, 32, 32)).unwrap();
        assert_eq!(
            (p.layout, p.size),
            (PixelLayout::Planar8, Size { w: 32, h: 32 })
        );
        assert_eq!(p.strides, [32, 16, 16]);
        assert!(
            p.planes[0].iter().all(|&v| v == 200),
            "{:?}",
            &p.planes[0][..8]
        );
        assert!(p.planes[1].iter().all(|&v| v == 128));
        // Même contexte réutilisé pour une autre région de même taille ; puis autre sortie.
        let p = s.scale(&f, &vp(0, 0, 64, 64, 32, 32)).unwrap();
        assert_eq!(p.planes[0][0], 16);
        assert_eq!(p.planes[0][31], 200);
        let p = s.scale(&f, &vp(0, 0, 128, 64, 30, 14)).unwrap();
        assert_eq!(p.size, Size { w: 30, h: 14 });
        assert_eq!(p.byte_len(), 30 * 14 + 2 * 15 * 7);
    }

    #[test]
    fn ten_bit_and_semi_planar_sources_are_reduced_to_8_bit() {
        let mut s = CpuScaler::default();
        for format in [
            ffi::AV_PIX_FMT_YUV420P10LE,
            ffi::AV_PIX_FMT_NV12,
            ffi::AV_PIX_FMT_P010LE,
        ] {
            let f = frame(format, 64, 32, 200);
            let p = s.scale(&f, &vp(2, 2, 60, 28, 30, 14)).unwrap();
            assert_eq!(p.layout, PixelLayout::Planar8, "{format}");
            assert_eq!(p.size, Size { w: 30, h: 14 });
        }
    }

    #[test]
    fn one_to_one_is_a_native_copy() {
        let f = frame(ffi::AV_PIX_FMT_YUV420P10LE, 64, 32, 0);
        let p = CpuScaler::default()
            .scale(&f, &vp(4, 2, 10, 6, 10, 6))
            .unwrap();
        assert_eq!((p.layout, p.strides[0]), (PixelLayout::Planar10, 20));
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        let mut s = CpuScaler::default();
        let f = frame(ffi::AV_PIX_FMT_YUV420P, 64, 32, 0);
        for bad in [
            vp(1, 0, 32, 32, 8, 8),
            vp(0, 1, 32, 30, 8, 8),
            vp(40, 0, 32, 32, 8, 8),
        ] {
            assert_eq!(
                s.scale(&f, &bad).unwrap_err().kind,
                DecodeErrorKind::Internal
            );
        }
        let mut rgb = AVFrame::new();
        rgb.set_format(ffi::AV_PIX_FMT_RGB24);
        assert_eq!(
            s.scale(&rgb, &vp(0, 0, 4, 4, 2, 2)).unwrap_err().kind,
            DecodeErrorKind::Unsupported
        );
        let mut empty = AVFrame::new();
        empty.set_format(ffi::AV_PIX_FMT_YUV420P);
        empty.set_width(8);
        empty.set_height(8);
        let err = s.scale(&empty, &vp(0, 0, 8, 8, 4, 4)).unwrap_err();
        assert!(err.detail.contains("absent"), "{err}");
    }
}
