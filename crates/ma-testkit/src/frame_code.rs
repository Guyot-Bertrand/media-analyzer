//! Lecture du numéro d'image incrusté dans le corpus (voir l'en-tête de `corpus/manifest.toml`).
//!
//! Géométrie : 16 blocs de `W/16` de large, bande de `H/24` de haut en haut de l'image, bit 0 à
//! gauche, blanc = 1. On échantillonne le centre de chaque bloc, sur la luma, avec un seuil à
//! mi-plage de la profondeur réelle.

use ma_decode::{PixelLayout, Planes, Rect, Size};

const BITS: u32 = 16;

/// Luma de l'échantillon (`x`, `y`) de l'image, ramenée à 8 ou 10 bits (selon le layout), et
/// seuil de mi-plage correspondant. `None` si l'échantillon est hors du tampon.
fn luma(img: &Planes, x: u32, y: u32) -> Option<(u32, u32)> {
    let plane = img.planes.first()?;
    let stride = usize::try_from(*img.strides.first()?).ok()?;
    let (x, y) = (usize::try_from(x).ok()?, usize::try_from(y).ok()?);
    let row = y.checked_mul(stride)?;
    match img.layout {
        PixelLayout::Planar8 | PixelLayout::SemiPlanar8 => {
            Some((u32::from(*plane.get(row.checked_add(x)?)?), 128))
        }
        PixelLayout::Planar10 | PixelLayout::SemiPlanar10 => {
            let at = row.checked_add(x.checked_mul(2)?)?;
            let raw = u16::from_le_bytes([*plane.get(at)?, *plane.get(at + 1)?]);
            let v = if img.layout == PixelLayout::SemiPlanar10 {
                raw >> 6 // P010 : bits de poids fort
            } else {
                raw // bits de poids faible
            };
            Some((u32::from(v), 512))
        }
    }
}

/// Numéro incrusté lu dans `img`, qui représente la région `region_of_full` de l'image entière
/// de taille `full` (éventuellement réduite : zoom « Ajusté »).
///
/// Renvoie `None` si les centres des 16 blocs ne sont pas tous dans la région, ou si le tampon est
/// trop petit. La valeur est `n mod code_modulo` (voir le manifeste).
#[must_use]
pub fn read_frame_code(img: &Planes, region_of_full: Rect, full: Size) -> Option<u32> {
    let (bw, bh) = (full.w / BITS, full.h / 24);
    let r = region_of_full;
    if bw == 0 || bh == 0 || r.w == 0 || r.h == 0 || img.size.w == 0 || img.size.h == 0 {
        return None;
    }
    let (x0, y0) = (u64::from(r.x), u64::from(r.y));
    let (x1, y1) = (x0 + u64::from(r.w), y0 + u64::from(r.h));
    let cy = u64::from(bh / 2);
    if cy < y0 || cy >= y1 {
        return None;
    }
    // Coordonnée de l'image pour une coordonnée de l'image entière.
    let map = |f: u64, origin: u64, region: u32, size: u32| {
        let v = (f - origin) * u64::from(size) / u64::from(region);
        u32::try_from(v).unwrap_or(u32::MAX).min(size - 1)
    };
    let iy = map(cy, y0, r.h, img.size.h);
    let mut code = 0;
    for i in 0..BITS {
        let cx = u64::from(i * bw + bw / 2);
        if cx < x0 || cx >= x1 {
            return None;
        }
        let (v, mid) = luma(img, map(cx, x0, r.w, img.size.w), iy)?;
        code |= u32::from(v >= mid) << i;
    }
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image synthétique de la région `r` de `full`, à la taille `size`, portant `code`.
    pub(crate) fn synth(layout: PixelLayout, size: Size, r: Rect, full: Size, code: u32) -> Planes {
        let (bw, bh) = (full.w / 16, full.h / 24);
        let bytes = match layout {
            PixelLayout::Planar8 | PixelLayout::SemiPlanar8 => 1,
            _ => 2,
        };
        let stride = size.w as usize * bytes + 4; // rembourrage de fin de ligne
        let mut luma = vec![0u8; stride * size.h as usize];
        for y in 0..size.h {
            for x in 0..size.w {
                let fx = r.x + x * r.w / size.w;
                let fy = r.y + y * r.h / size.h;
                let white = fy < bh && fx < bw * 16 && (code >> (fx / bw)) & 1 == 1;
                let at = y as usize * stride + x as usize * bytes;
                match layout {
                    PixelLayout::Planar8 | PixelLayout::SemiPlanar8 => {
                        luma[at] = if white { 235 } else { 16 };
                    }
                    PixelLayout::Planar10 => {
                        let v: u16 = if white { 940 } else { 64 };
                        luma[at..at + 2].copy_from_slice(&v.to_le_bytes());
                    }
                    PixelLayout::SemiPlanar10 => {
                        let v: u16 = if white { 940 << 6 } else { 64 << 6 };
                        luma[at..at + 2].copy_from_slice(&v.to_le_bytes());
                    }
                }
            }
        }
        Planes {
            layout,
            size,
            planes: vec![luma, vec![128; 16]],
            strides: vec![u32::try_from(stride).unwrap(), 16],
        }
    }

    const FULL: Size = Size { w: 1280, h: 720 };
    const WHOLE: Rect = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 720,
    };
    const LAYOUTS: [PixelLayout; 4] = [
        PixelLayout::Planar8,
        PixelLayout::Planar10,
        PixelLayout::SemiPlanar8,
        PixelLayout::SemiPlanar10,
    ];

    #[test]
    fn frame_code_roundtrip() {
        for layout in LAYOUTS {
            for code in [0xA5C3, 0, 0xFFFF, 1, 0x8000] {
                for size in [FULL, Size { w: 640, h: 360 }, Size { w: 320, h: 180 }] {
                    let img = synth(layout, size, WHOLE, FULL, code);
                    assert_eq!(
                        read_frame_code(&img, WHOLE, FULL),
                        Some(code),
                        "{layout:?} {size:?} {code:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn frame_code_in_a_one_to_one_region() {
        // Région qui contient la bande et les centres des 16 blocs.
        let r = Rect {
            x: 20,
            y: 0,
            w: 1260,
            h: 300,
        };
        for layout in LAYOUTS {
            let img = synth(layout, Size { w: 1260, h: 300 }, r, FULL, 0xA5C3);
            assert_eq!(read_frame_code(&img, r, FULL), Some(0xA5C3), "{layout:?}");
        }
    }

    #[test]
    fn frame_code_needs_the_whole_band_in_the_region() {
        let img = synth(PixelLayout::Planar8, FULL, WHOLE, FULL, 0xA5C3);
        // Région sans la bande (sous elle), ou ne couvrant que la moitié gauche.
        let below = Rect {
            x: 0,
            y: 100,
            w: 1280,
            h: 200,
        };
        assert_eq!(read_frame_code(&img, below, FULL), None);
        let left = Rect {
            x: 0,
            y: 0,
            w: 640,
            h: 720,
        };
        assert_eq!(read_frame_code(&img, left, FULL), None);
        let right = Rect {
            x: 640,
            y: 0,
            w: 640,
            h: 720,
        };
        assert_eq!(read_frame_code(&img, right, FULL), None);
    }

    #[test]
    fn frame_code_rejects_degenerate_input() {
        let img = synth(PixelLayout::Planar8, FULL, WHOLE, FULL, 3);
        assert_eq!(read_frame_code(&img, WHOLE, Size { w: 8, h: 8 }), None);
        let empty = Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 720,
        };
        assert_eq!(read_frame_code(&img, empty, FULL), None);
        let mut small = img.clone();
        small.size = Size { w: 0, h: 0 };
        assert_eq!(read_frame_code(&small, WHOLE, FULL), None);
        // Tampon trop court pour la taille annoncée.
        let mut short = img.clone();
        short.planes[0].truncate(10);
        assert_eq!(read_frame_code(&short, WHOLE, FULL), None);
        let mut no_plane = img.clone();
        no_plane.planes.clear();
        assert_eq!(read_frame_code(&no_plane, WHOLE, FULL), None);
        let mut no_stride = img;
        no_stride.strides.clear();
        assert_eq!(read_frame_code(&no_stride, WHOLE, FULL), None);
        // Échantillon 16 bits coupé en deux.
        let img10 = Planes {
            layout: PixelLayout::Planar10,
            size: FULL,
            planes: vec![vec![0; 1]],
            strides: vec![2560],
        };
        assert_eq!(read_frame_code(&img10, WHOLE, FULL), None);
    }

    #[test]
    fn threshold_sits_at_mid_range_of_the_real_depth() {
        // Planar10 : un blanc 8 bits (235) lu comme 10 bits est noir ; P010 : seuil à 0x8000.
        let mk = |layout, v: u16| Planes {
            layout,
            size: Size { w: 1, h: 1 },
            planes: vec![v.to_le_bytes().to_vec()],
            strides: vec![2],
        };
        assert_eq!(
            luma(&mk(PixelLayout::Planar10, 511), 0, 0),
            Some((511, 512))
        );
        assert_eq!(
            luma(&mk(PixelLayout::Planar10, 940), 0, 0),
            Some((940, 512))
        );
        assert_eq!(
            luma(&mk(PixelLayout::SemiPlanar10, 0x7FFF), 0, 0),
            Some((511, 512))
        );
        assert_eq!(
            luma(&mk(PixelLayout::SemiPlanar10, 0x8000), 0, 0),
            Some((512, 512))
        );
    }
}
