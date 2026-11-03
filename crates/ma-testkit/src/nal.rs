//! Classification d'une AU d'après ses NAL (ou ses codes de début MPEG-2) : type d'image, point
//! d'accès aléatoire et image de tête (HEVC).
//!
//! Seul le premier NAL de tranche de l'AU est lu ; les AU sont supposées complètes (une image par
//! AU). Hypothèse HEVC : `num_extra_slice_header_bits` vaut 0 (cas de x265) pour lire `slice_type`.

use ma_decode::{Codec, PictureType, RapKind};

/// Statut d'image de tête lisible dans le type NAL (HEVC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalLeading {
    /// RASL (types 8 et 9).
    Skippable,
    /// RADL (types 6 et 7).
    Decodable,
}

/// Position du début de chaque NAL (premier octet après le code de début `00 00 01`).
fn nal_starts(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                i += 3;
                return Some(i);
            }
            i += 1;
        }
        None
    })
}

/// Retire les octets d'échappement (`00 00 03` devient `00 00`).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    /// Exp-Golomb non signé, ue(v).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..zeros {
            v = (v << 1) | self.bit()?;
        }
        Some((1u32 << zeros) - 1 + v)
    }
}

fn slice_type_picture(slice_type: u32) -> PictureType {
    match slice_type % 5 {
        1 => PictureType::B,
        2 | 4 => PictureType::I,
        _ => PictureType::P,
    }
}

/// `recovery_frame_cnt` du SEI « recovery point » (type 6) d'un NAL SEI H.264.
fn h264_recovery_cnt(sei: &[u8]) -> Option<u32> {
    let rbsp = unescape(sei.get(1..)?);
    let mut i = 0;
    let field = |i: &mut usize| -> Option<usize> {
        let mut v = 0usize;
        loop {
            let b = *rbsp.get(*i)?;
            *i += 1;
            v += usize::from(b);
            if b != 0xFF {
                return Some(v);
            }
        }
    };
    while i < rbsp.len() {
        let kind = field(&mut i)?;
        let size = field(&mut i)?;
        if kind == 6 {
            let payload = rbsp.get(i..i + size)?;
            return Bits {
                data: payload,
                pos: 0,
            }
            .ue();
        }
        i += size;
    }
    None
}

fn classify_h264(au: &[u8]) -> (PictureType, RapKind, Option<NalLeading>) {
    let mut recovery = None;
    for s in nal_starts(au) {
        let nal = &au[s..];
        match nal[0] & 0x1F {
            6 => {
                recovery = recovery.or_else(|| h264_recovery_cnt(nal));
            }
            t @ (1 | 5) => {
                let rbsp = unescape(&nal[1..nal.len().min(17)]);
                let mut bits = Bits {
                    data: &rbsp,
                    pos: 0,
                };
                let picture = bits
                    .ue()
                    .and_then(|_first_mb| bits.ue())
                    .map_or(PictureType::P, slice_type_picture);
                let rap = match (t, recovery) {
                    (5, _) => RapKind::Idr,
                    // Valeur brute de `recovery_frame_cnt` : le vrai calcul en ordre de présentation
                    // viendra en Tâche 10.
                    (_, Some(frame_cnt)) => RapKind::Recovery {
                        pres_span: frame_cnt,
                    },
                    _ => RapKind::None,
                };
                return (picture, rap, None);
            }
            _ => {}
        }
    }
    (PictureType::P, RapKind::None, None)
}

fn classify_hevc(au: &[u8]) -> (PictureType, RapKind, Option<NalLeading>) {
    for s in nal_starts(au) {
        let nal = &au[s..];
        let t = (nal[0] >> 1) & 0x3F;
        if t >= 32 {
            continue;
        }
        let rap = match t {
            16..=18 => RapKind::Bla,
            19 | 20 => RapKind::Idr,
            21 => RapKind::Cra,
            _ => RapKind::None,
        };
        let leading = match t {
            6 | 7 => Some(NalLeading::Decodable),
            8 | 9 => Some(NalLeading::Skippable),
            _ => None,
        };
        let picture = if rap == RapKind::None {
            hevc_slice_picture(nal)
        } else {
            PictureType::I
        };
        return (picture, rap, leading);
    }
    (PictureType::P, RapKind::None, None)
}

/// Type de la première tranche d'un NAL non IRAP. P par défaut si la tranche n'est pas la
/// première de l'image.
fn hevc_slice_picture(nal: &[u8]) -> PictureType {
    let rbsp = unescape(&nal[2.min(nal.len())..nal.len().min(18)]);
    let mut bits = Bits {
        data: &rbsp,
        pos: 0,
    };
    if bits.bit() != Some(1) {
        return PictureType::P;
    }
    // slice_pic_parameter_set_id puis slice_type (aucun bit d'en-tête supplémentaire).
    match bits.ue().and_then(|_pps| bits.ue()) {
        Some(0) => PictureType::B,
        Some(2) => PictureType::I,
        _ => PictureType::P,
    }
}

fn classify_mpeg2(au: &[u8]) -> (PictureType, RapKind, Option<NalLeading>) {
    let mut closed_gop = None;
    for s in nal_starts(au) {
        let nal = &au[s..];
        match nal[0] {
            0xB8 => closed_gop = nal.get(4).map(|b| b & 0x40 != 0),
            0x00 => {
                let picture = match nal.get(2).map(|b| (b >> 3) & 7) {
                    Some(1 | 4) => PictureType::I,
                    Some(3) => PictureType::B,
                    _ => PictureType::P,
                };
                let rap = match (picture, closed_gop) {
                    (PictureType::I, Some(closed_gop)) => RapKind::Intra { closed_gop },
                    _ => RapKind::None,
                };
                return (picture, rap, None);
            }
            _ => {}
        }
    }
    (PictureType::P, RapKind::None, None)
}

/// Type d'image, point d'accès aléatoire et statut d'image de tête (HEVC seulement) d'une AU.
///
/// HEVC : NAL 16–18 → `Bla`, 19–20 → `Idr`, 21 → `Cra` ; RADL (6–7) → `Decodable`, RASL (8–9) →
/// `Skippable`. H.264 : NAL 5 → `Idr`, sinon SEI « recovery point » → `Recovery`. MPEG-2 : image I
/// précédée d'un en-tête de GOP → `Intra { closed_gop }`. Le statut `Leading` des autres cas se
/// déduit de l'ordre de présentation (voir `TsAuSource`).
///
/// Une AU sans tranche reconnue donne `(P, None, None)`.
#[must_use]
pub fn classify(codec: Codec, au: &[u8]) -> (PictureType, RapKind, Option<NalLeading>) {
    match codec {
        Codec::H264 => classify_h264(au),
        Codec::Hevc => classify_hevc(au),
        Codec::Mpeg2 => classify_mpeg2(au),
    }
}

/// Position du premier octet après l'en-tête de la première tranche de l'AU (NAL H.264/HEVC, code
/// de tranche MPEG-2), ou `None` s'il n'y a pas de tranche.
#[must_use]
pub fn first_slice_payload_offset(codec: Codec, au: &[u8]) -> Option<usize> {
    nal_starts(au).find_map(|s| {
        let b = au[s];
        match codec {
            Codec::H264 => matches!(b & 0x1F, 1 | 5).then_some(s + 1),
            Codec::Hevc => (((b >> 1) & 0x3F) < 32).then_some(s + 2),
            Codec::Mpeg2 => (0x01..=0xAF).contains(&b).then_some(s + 1),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: [u8; 4] = [0, 0, 0, 1];

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&SC);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn exp_golomb() {
        // 1 -> 0 ; 010 -> 1 ; 011 -> 2 ; 00100 -> 3
        let mut b = Bits {
            data: &[0b1010_0110, 0b0100_0000],
            pos: 0,
        };
        assert_eq!(b.ue(), Some(0));
        assert_eq!(b.ue(), Some(1));
        assert_eq!(b.ue(), Some(2));
        assert_eq!(b.ue(), Some(3));
        let mut b = Bits {
            data: &[0, 0, 0, 0, 0],
            pos: 0,
        };
        assert_eq!(b.ue(), None);
        let mut b = Bits { data: &[0], pos: 0 };
        assert_eq!(b.ue(), None);
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape(&[1, 0, 0, 3, 1, 0, 0, 3]), [1, 0, 0, 1, 0, 0]);
        assert_eq!(unescape(&[0, 3, 0]), [0, 3, 0]);
    }

    #[test]
    fn h264_idr_and_slice_types() {
        // first_mb=0 (1), slice_type=7 (0001000) : I ; 5 -> P ; 6 -> B
        let idr = au(&[&[0x67, 1, 2], &[0x65, 0b1000_1000]]);
        assert_eq!(
            classify(Codec::H264, &idr),
            (PictureType::I, RapKind::Idr, None)
        );
        // slice_type 5 = 00110 -> "1" + "00110"
        let p = au(&[&[0x41, 0b1001_1000]]);
        assert_eq!(
            classify(Codec::H264, &p),
            (PictureType::P, RapKind::None, None)
        );
        // slice_type 6 = 00111
        let b = au(&[&[0x01, 0b1001_1100]]);
        assert_eq!(
            classify(Codec::H264, &b),
            (PictureType::B, RapKind::None, None)
        );
        // slice_type 2 = 011
        let i = au(&[&[0x41, 0b1011_0000]]);
        assert_eq!(
            classify(Codec::H264, &i),
            (PictureType::I, RapKind::None, None)
        );
    }

    #[test]
    fn h264_recovery_point_sei() {
        // SEI : user data (5, taille 2), puis recovery point (6, taille 1) frame_cnt=ue(2)=011
        let sei = [0x06, 5, 2, 0xAA, 0xBB, 6, 1, 0b0111_1000, 0x80];
        let a = au(&[&sei, &[0x41, 0b1011_0000]]);
        assert_eq!(
            classify(Codec::H264, &a),
            (PictureType::I, RapKind::Recovery { pres_span: 2 }, None)
        );
        // Un IDR l'emporte sur un SEI.
        let a = au(&[&sei, &[0x65, 0b1011_0000]]);
        assert_eq!(classify(Codec::H264, &a).1, RapKind::Idr);
        // Taille codée sur plusieurs octets (0xFF), SEI sans recovery point, SEI tronqué.
        let long = [0x06, 5, 0xFF, 1, 0xCC];
        assert_eq!(h264_recovery_cnt(&long), None);
        assert_eq!(h264_recovery_cnt(&[0x06]), None);
        assert_eq!(h264_recovery_cnt(&[0x06, 6]), None);
        let mut with_big_type = vec![0x06, 0xFF, 1, 1, 0xAA];
        with_big_type.extend_from_slice(&[6, 1, 0b1000_0000, 0x80]);
        assert_eq!(h264_recovery_cnt(&with_big_type), Some(0));
        let a = au(&[&[0x06, 6, 1, 0b1000_0000, 0x80], &[0x01, 0b1001_1000]]);
        assert_eq!(
            classify(Codec::H264, &a).1,
            RapKind::Recovery { pres_span: 0 }
        );
    }

    #[test]
    fn hevc_nal_types() {
        let nal = |t: u8, rest: &[u8]| {
            let mut v = vec![t << 1, 1];
            v.extend_from_slice(rest);
            v
        };
        let rap = |t| classify(Codec::Hevc, &au(&[&nal(32, &[0]), &nal(t, &[0x80])]));
        assert_eq!(rap(19), (PictureType::I, RapKind::Idr, None));
        assert_eq!(rap(20).1, RapKind::Idr);
        assert_eq!(rap(21).1, RapKind::Cra);
        assert_eq!(rap(16).1, RapKind::Bla);
        assert_eq!(rap(18).1, RapKind::Bla);
        assert_eq!(rap(22).1, RapKind::None);
        // RASL / RADL : first_slice=1, pps=0 (1), slice_type 0 (B) = 1 -> bits 1 1 1
        let lead = |t| classify(Codec::Hevc, &au(&[&nal(t, &[0b1110_0000])]));
        assert_eq!(
            lead(8),
            (PictureType::B, RapKind::None, Some(NalLeading::Skippable))
        );
        assert_eq!(lead(9).2, Some(NalLeading::Skippable));
        assert_eq!(lead(6).2, Some(NalLeading::Decodable));
        assert_eq!(lead(7).2, Some(NalLeading::Decodable));
        // TRAIL_R : slice_type 1 (P) = 010 ; slice_type 2 (I) = 011
        let trail = |bits| classify(Codec::Hevc, &au(&[&nal(1, &[bits])]));
        assert_eq!(trail(0b1101_0000), (PictureType::P, RapKind::None, None));
        assert_eq!(trail(0b1101_1000).0, PictureType::I);
        // Tranche qui n'est pas la première de l'image.
        assert_eq!(trail(0b0101_1000).0, PictureType::P);
        // Aucune tranche : seulement des paramètres.
        assert_eq!(
            classify(Codec::Hevc, &au(&[&nal(33, &[1])])),
            (PictureType::P, RapKind::None, None)
        );
    }

    #[test]
    fn mpeg2_gop_and_picture() {
        let seq = [0xB3, 0x2D, 0x02, 0x40];
        let gop = |closed: bool| [0xB8, 0, 8, 0, if closed { 0x40 } else { 0 }];
        // temporal_reference=0, picture_coding_type=1 (I) -> 0b001_000_00 << ...
        let pic = |pct: u8| [0x00, 0, pct << 3, 0xFF];
        let slice = [0x01, 0x11];
        let a = au(&[&seq, &gop(true), &pic(1), &slice]);
        assert_eq!(
            classify(Codec::Mpeg2, &a),
            (PictureType::I, RapKind::Intra { closed_gop: true }, None)
        );
        let a = au(&[&seq, &gop(false), &pic(1), &slice]);
        assert_eq!(
            classify(Codec::Mpeg2, &a).1,
            RapKind::Intra { closed_gop: false }
        );
        assert_eq!(
            classify(Codec::Mpeg2, &au(&[&pic(2), &slice])),
            (PictureType::P, RapKind::None, None)
        );
        assert_eq!(
            classify(Codec::Mpeg2, &au(&[&pic(3), &slice])).0,
            PictureType::B
        );
        // I sans en-tête de GOP : pas un point d'accès.
        assert_eq!(
            classify(Codec::Mpeg2, &au(&[&pic(1), &slice])).1,
            RapKind::None
        );
        // En-tête de GOP sans image.
        assert_eq!(
            classify(Codec::Mpeg2, &au(&[&[0xB8, 0, 0, 0, 0x40]])),
            (PictureType::P, RapKind::None, None)
        );
        assert_eq!(
            classify(Codec::Mpeg2, &[]),
            (PictureType::P, RapKind::None, None)
        );
    }

    #[test]
    fn slice_payload_offsets() {
        let h264 = au(&[&[0x67, 1], &[0x65, 0x88, 9]]);
        assert_eq!(
            first_slice_payload_offset(Codec::H264, &h264),
            Some(4 + 2 + 4 + 1)
        );
        let hevc = au(&[&[0x40, 1, 9], &[19 << 1, 1, 9]]);
        assert_eq!(
            first_slice_payload_offset(Codec::Hevc, &hevc),
            Some(4 + 3 + 4 + 2)
        );
        let mpeg2 = au(&[&[0xB3, 1], &[0x00, 0, 8, 0], &[0x01, 5]]);
        assert_eq!(
            first_slice_payload_offset(Codec::Mpeg2, &mpeg2),
            Some(4 + 2 + 4 + 4 + 4 + 1)
        );
        assert_eq!(
            first_slice_payload_offset(Codec::H264, &au(&[&[0x67, 1]])),
            None
        );
        assert_eq!(
            first_slice_payload_offset(Codec::Hevc, &au(&[&[0x40, 1]])),
            None
        );
        assert_eq!(first_slice_payload_offset(Codec::Mpeg2, &[1, 2, 3]), None);
    }
}
