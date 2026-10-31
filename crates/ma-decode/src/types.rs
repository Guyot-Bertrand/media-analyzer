//! Types de base : codec, entrées d'index, géométrie, viewport et plans d'image.

use crate::blocks::BlockTree;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Mpeg2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PictureType {
    I,
    P,
    B,
}

/// Point d'accès aléatoire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RapKind {
    None,
    Idr,
    Cra,
    Bla,
    Recovery { frame_cnt: u32 },
    Intra { closed_gop: bool },
}

/// Image précédant son RAP en ordre de présentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leading {
    None,
    /// RASL ou équivalent : dépend du RAP précédent.
    Skippable,
    /// RADL : décodable seule.
    Decodable,
}

/// Entrée d'index d'une image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameEntry {
    pub decode_idx: u32,
    pub pres_idx: u32,
    pub pts: i64,
    pub picture: PictureType,
    pub rap: RapKind,
    pub leading: Leading,
}

/// Source d'unités d'accès (une AU par image), indexée en ordre de décodage.
pub trait AuSource: Send + Sync {
    fn codec(&self) -> Codec;
    /// Nombre d'images (ordre de décodage).
    fn len(&self) -> u32;
    fn entry(&self, decode_idx: u32) -> Option<FrameEntry>;
    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32>;
    /// Remplace le contenu de `buf` par l'AU demandée.
    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> std::io::Result<()>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: u32,
    pub h: u32,
}

/// Viewport invalide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ViewportError {
    #[error("viewport vide")]
    Empty,
}

/// Région de l'image source et taille de sortie. Sert de clé de cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Viewport {
    pub region: Rect,
    pub out: Size,
}

/// Arrondit au pair inférieur, avec un minimum de 2.
fn even_floor(v: u64) -> u32 {
    u32::try_from(v & !1).unwrap_or(!1).max(2)
}

impl Viewport {
    /// Image entière, réduite pour tenir dans `area` (ratio conservé, dimensions paires), sans
    /// agrandissement.
    #[must_use]
    pub fn fit(frame: Size, area: Size) -> Viewport {
        let region = Rect {
            x: 0,
            y: 0,
            w: frame.w,
            h: frame.h,
        };
        let (fw, fh) = (u64::from(frame.w), u64::from(frame.h));
        let (aw, ah) = (u64::from(area.w), u64::from(area.h));
        let out = if fw <= aw && fh <= ah {
            frame
        } else if fw * ah >= fh * aw {
            // Limité par la largeur.
            Size {
                w: even_floor(aw),
                h: even_floor(fh * aw / fw),
            }
        } else {
            Size {
                w: even_floor(fw * ah / fh),
                h: even_floor(ah),
            }
        };
        Viewport { region, out }
    }

    /// Région affichée à l'échelle 1:1 (zoom ≥ 100 %).
    #[must_use]
    pub fn region_1to1(_frame: Size, region: Rect) -> Viewport {
        Viewport {
            region,
            out: Size {
                w: region.w,
                h: region.h,
            },
        }
    }

    /// Borne la région à l'image, l'aligne sur des coordonnées paires (origine arrondie vers le
    /// bas, fin vers le haut, dans la limite de l'image) et borne la sortie à la région.
    pub fn normalized(self, frame: Size) -> Result<Viewport, ViewportError> {
        let r = self.region;
        let x0 = r.x.min(frame.w);
        let y0 = r.y.min(frame.h);
        let x1 = r.x.saturating_add(r.w).min(frame.w);
        let y1 = r.y.saturating_add(r.h).min(frame.h);
        if x1 <= x0 || y1 <= y0 || self.out.w == 0 || self.out.h == 0 {
            return Err(ViewportError::Empty);
        }
        let x0 = x0 & !1;
        let y0 = y0 & !1;
        let x1 = (x1 + (x1 & 1)).min(frame.w);
        let y1 = (y1 + (y1 & 1)).min(frame.h);
        let region = Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        };
        let out = Size {
            w: self.out.w.min(region.w),
            h: self.out.h.min(region.h),
        };
        Ok(Viewport { region, out })
    }

    /// Vrai si la sortie est plus petite que la région (image approchée, en 8 bits).
    #[must_use]
    pub fn is_reduced(&self) -> bool {
        self.out.w < self.region.w || self.out.h < self.region.h
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelLayout {
    Planar8,
    /// `u16` petit-boutiste, bits de poids faible.
    Planar10,
    /// NV12.
    SemiPlanar8,
    /// P010, bits de poids fort.
    SemiPlanar10,
}

/// Image en plans compactés. `strides` est en **octets** pour tous les layouts, 16 bits compris
/// (au moins `2 × largeur` pour `Planar10` et `SemiPlanar10`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planes {
    pub layout: PixelLayout,
    pub size: Size,
    pub planes: Vec<Vec<u8>>,
    pub strides: Vec<u32>,
}

impl Planes {
    /// Taille totale en octets de tous les plans.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        self.planes.iter().map(Vec::len).sum()
    }
}

/// Image décodée pour un viewport donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub pres_idx: u32,
    pub viewport: Viewport,
    pub image: Planes,
    pub corrupt: bool,
    /// Taille d'affichage réelle de l'image décodée.
    pub full: Size,
    pub blocks: Option<BlockTree>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const UHD: Size = Size { w: 3840, h: 2160 };

    #[test]
    fn viewport_fit_keeps_aspect_ratio() {
        let v = Viewport::fit(UHD, Size { w: 1920, h: 1080 });
        assert_eq!(
            v.region,
            Rect {
                x: 0,
                y: 0,
                w: 3840,
                h: 2160
            }
        );
        assert_eq!(v.out, Size { w: 1920, h: 1080 });
        let v = Viewport::fit(UHD, Size { w: 1000, h: 1000 });
        assert_eq!(v.out, Size { w: 1000, h: 562 });
        let v = Viewport::fit(Size { w: 1080, h: 1920 }, Size { w: 1000, h: 1000 });
        assert_eq!(v.out, Size { w: 562, h: 1000 });
    }

    #[test]
    fn viewport_fit_never_enlarges() {
        let small = Size { w: 640, h: 360 };
        let v = Viewport::fit(small, Size { w: 1920, h: 1080 });
        assert_eq!(v.out, small);
        assert!(!v.is_reduced());
    }

    #[test]
    fn viewport_normalization() {
        let frame = Size { w: 1280, h: 720 };
        let vp = |x, y, w, h, ow, oh| Viewport {
            region: Rect { x, y, w, h },
            out: Size { w: ow, h: oh },
        };
        // Alignement sur le pair.
        let n = vp(1, 1, 9, 9, 9, 9).normalized(frame).unwrap();
        assert_eq!(
            n.region,
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10
            }
        );
        assert_eq!(n.out, Size { w: 9, h: 9 });
        // Région débordante bornée.
        let n = vp(1200, 700, 500, 500, 80, 20).normalized(frame).unwrap();
        assert_eq!(
            n.region,
            Rect {
                x: 1200,
                y: 700,
                w: 80,
                h: 20
            }
        );
        // Région vide, ou entièrement hors image, ou sortie vide.
        assert_eq!(
            vp(0, 0, 0, 10, 1, 1).normalized(frame),
            Err(ViewportError::Empty)
        );
        assert_eq!(
            vp(1280, 0, 10, 10, 10, 10).normalized(frame),
            Err(ViewportError::Empty)
        );
        assert_eq!(
            vp(0, 0, 10, 10, 0, 10).normalized(frame),
            Err(ViewportError::Empty)
        );
        // Sortie supérieure à la région bornée.
        let n = vp(0, 0, 100, 100, 500, 50).normalized(frame).unwrap();
        assert_eq!(n.out, Size { w: 100, h: 50 });
        // Image de taille impaire : la fin reste dans l'image.
        let n = vp(0, 0, 100, 100, 100, 100)
            .normalized(Size { w: 99, h: 99 })
            .unwrap();
        assert_eq!(
            n.region,
            Rect {
                x: 0,
                y: 0,
                w: 99,
                h: 99
            }
        );
        // Idempotence.
        assert_eq!(n.normalized(Size { w: 99, h: 99 }), Ok(n));
    }

    #[test]
    fn is_reduced() {
        let frame = Size { w: 1920, h: 1080 };
        assert!(Viewport::fit(frame, Size { w: 960, h: 540 }).is_reduced());
        let region = Rect {
            x: 10,
            y: 10,
            w: 200,
            h: 100,
        };
        let v = Viewport::region_1to1(frame, region);
        assert_eq!(v.out, Size { w: 200, h: 100 });
        assert!(!v.is_reduced());
    }

    #[test]
    fn planes_byte_len() {
        let planes = Planes {
            layout: PixelLayout::SemiPlanar8,
            size: Size { w: 1920, h: 1080 },
            planes: vec![vec![0; 1920 * 1080], vec![0; 1920 * 540]],
            strides: vec![1920, 1920],
        };
        assert_eq!(planes.byte_len(), 3_110_400);
        assert!(planes.byte_len() < 4 * 1024 * 1024);
    }

    #[test]
    fn decoded_frame_without_blocks() {
        let frame = Size { w: 2, h: 2 };
        let d = DecodedFrame {
            pres_idx: 0,
            viewport: Viewport::fit(frame, frame),
            image: Planes {
                layout: PixelLayout::Planar8,
                size: frame,
                planes: vec![vec![0; 4]],
                strides: vec![2],
            },
            corrupt: false,
            full: frame,
            blocks: None,
        };
        assert_eq!(d.clone(), d);
    }
}
