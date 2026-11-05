//! H.264 (ISO/CEI 14496-10, Annex B).
//!
//! - Nouvelle AU sur un NAL 9, 7, 8, 6 ou 14 à 18 qui suit une tranche de l'AU courante, ou sur
//!   une tranche (1, 5) dont `first_mb_in_slice` vaut 0 (§7.4.1.2.3). Avec des plans de couleur
//!   séparés (4:4:4, `separate_colour_plane_flag = 1`), seule la tranche du plan 0
//!   (`colour_plane_id = 0`) ouvre une AU (§7.4.1.2.4).
//! - Le NAL 14 (préfixe SVC/MVC) n'ouvre une AU « après une tranche » que s'il suit la dernière
//!   tranche de l'image (§7.4.1.2.3) : un NAL 14 précède aussi chaque tranche d'une AU SVC/MVC.
//!   Ces flux sont hors des profils visés ; ils seraient coupés à tort entre deux tranches.
//! - SPS lu jusqu'à `frame_mbs_only_flag` (D3) ; PPS : `pps_id` et `sps_id` ; tranche : jusqu'à
//!   `bottom_field_flag`. SPS ou PPS inconnu ⇒ `Field::Frame`, `frame_num: None`.
//! - SEI « recovery point » (type 6) ⇒ `Recovery` sur une image I non IDR.

use ma_bits::{BitError, BitReader, rbsp};
use ma_core::PictureType;

use crate::framer::{NalKind, NalParser};
use crate::{AuMeta, Field, MAX_HEADER_BYTES, MAX_SLICE_HEADER_BYTES, ParamSet, RapClass};

/// Champs du SPS utiles à l'en-tête de tranche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sps {
    separate_colour_plane: bool,
    log2_max_frame_num: u32,
    frame_mbs_only: bool,
}

/// Profils dont le SPS porte `chroma_format_idc` et la suite (§7.3.2.1.1).
const HIGH_PROFILES: [u64; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

fn skip_scaling_list(r: &mut BitReader<'_>, size: u32) -> Result<(), BitError> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            next = (last + i64::from(r.se()?)).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// `(sps_id, Sps)`, ou une erreur de lecture ; identifiant hors plage ⇒ `None`.
fn parse_sps(nal: &[u8]) -> Result<Option<(usize, Sps)>, BitError> {
    let data = rbsp(nal.get(1..).unwrap_or_default()).data;
    let mut r = BitReader::new(&data);
    let profile = r.u(8)?;
    r.skip(16)?; // constraint_set*_flag, reserved_zero_2bits, level_idc
    let id = r.ue()?;
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num_minus4 = r.ue()?;
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.skip(1)?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let cycle = r.ue()?;
            if cycle > 255 {
                return Ok(None);
            }
            for _ in 0..cycle {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    let frame_mbs_only = r.flag()?;
    if id > 31 || log2_max_frame_num_minus4 > 12 {
        return Ok(None);
    }
    Ok(Some((
        id as usize,
        Sps {
            separate_colour_plane,
            log2_max_frame_num: log2_max_frame_num_minus4 + 4,
            frame_mbs_only,
        },
    )))
}

/// `(pps_id, sps_id)` ; identifiants hors plage ⇒ `None`.
fn parse_pps(nal: &[u8]) -> Result<Option<(usize, u8)>, BitError> {
    let data = rbsp(nal.get(1..).unwrap_or_default()).data;
    let mut r = BitReader::new(&data);
    let pps = r.ue()?;
    let sps = r.ue()?;
    Ok(u8::try_from(sps)
        .ok()
        .filter(|&s| s <= 31 && pps <= 255)
        .map(|s| (pps as usize, s)))
}

/// `recovery_frame_cnt` du 1er SEI « recovery point » du NAL, s'il y en a un et qu'il est lisible.
fn recovery_frame_cnt(nal: &[u8]) -> Option<u32> {
    let data = rbsp(nal.get(1..)?).data;
    let mut i = 0usize;
    let field = |i: &mut usize| -> Option<usize> {
        let mut v = 0usize;
        loop {
            let b = *data.get(*i)?;
            *i += 1;
            v += usize::from(b);
            if b != 0xFF {
                return Some(v);
            }
        }
    };
    // Fin des messages : il ne reste que `rbsp_trailing_bits` (0x80).
    while data.len() > i + 1 {
        let kind = field(&mut i)?;
        let size = field(&mut i)?;
        if kind == 6 {
            let payload = data.get(i..i.checked_add(size)?)?;
            return BitReader::new(payload).ue().ok();
        }
        i = i.checked_add(size)?;
    }
    None
}

fn slice_start(r: &mut BitReader<'_>) -> Result<(u32, u32), BitError> {
    Ok((r.ue()?, r.ue()?))
}

fn picture_of(slice_type: u32) -> Option<PictureType> {
    match slice_type {
        0 | 3 | 5 | 8 => Some(PictureType::P),
        1 | 6 => Some(PictureType::B),
        2 | 4 | 7 | 9 => Some(PictureType::I),
        _ => None,
    }
}

/// État de l'AU courante.
#[derive(Default)]
struct Au {
    meta: AuMeta,
    has_slice: bool,
    recovery: Option<u32>,
}

pub(crate) struct H264 {
    sps: [Option<Sps>; 32],
    /// `sps_id` de chaque PPS.
    pps: [Option<u8>; 256],
    au: Au,
}

impl Default for H264 {
    fn default() -> Self {
        Self {
            sps: [None; 32],
            pps: [None; 256],
            au: Au::default(),
        }
    }
}

impl H264 {
    /// Lit l'en-tête de la 1re tranche de l'AU ; s'arrête à la première erreur, l'AU gardant
    /// les valeurs déjà lues.
    fn first_slice(&mut self, nal: &[u8]) {
        let data = rbsp(&nal[1..]).data;
        let mut r = BitReader::new(&data);
        let Ok((_first_mb, slice_type)) = slice_start(&mut r) else {
            return;
        };
        if let Some(p) = picture_of(slice_type) {
            self.au.meta.picture = p;
        }
        let _ = self.field_and_frame_num(&mut r);
    }

    /// `colour_plane_id` d'une tranche lue jusqu'à `first_mb_in_slice` ; 0 si le SPS n'a pas
    /// de plans séparés ou est inconnu. Erreur de lecture ⇒ `Err` (traité comme le plan 0).
    fn colour_plane(&self, r: &mut BitReader<'_>) -> Result<u64, BitError> {
        r.ue()?; // slice_type
        let pps_id = r.ue()?;
        let sps = self
            .pps
            .get(pps_id as usize)
            .copied()
            .flatten()
            .and_then(|s| self.sps[usize::from(s)]);
        if sps.is_some_and(|s| s.separate_colour_plane) {
            r.u(2)
        } else {
            Ok(0)
        }
    }

    fn field_and_frame_num(&mut self, r: &mut BitReader<'_>) -> Result<(), BitError> {
        let pps_id = r.ue()?;
        let Some(sps) = self
            .pps
            .get(pps_id as usize)
            .copied()
            .flatten()
            .and_then(|s| self.sps[usize::from(s)])
        else {
            return Ok(());
        };
        if sps.separate_colour_plane {
            r.skip(2)?; // colour_plane_id
        }
        let frame_num = r.u(sps.log2_max_frame_num)?;
        let mut field = Field::Frame;
        if !sps.frame_mbs_only && r.flag()? {
            field = if r.flag()? { Field::Bottom } else { Field::Top };
        }
        self.au.meta.frame_num = u32::try_from(frame_num).ok();
        self.au.meta.field = field;
        Ok(())
    }
}

impl NalParser for H264 {
    fn header_cap(&self, first: u8) -> usize {
        if matches!(first & 0x1F, 1..=5) {
            MAX_SLICE_HEADER_BYTES
        } else {
            MAX_HEADER_BYTES
        }
    }

    fn kind(&self, nal: &[u8]) -> NalKind {
        match nal[0] & 0x1F {
            6..=9 | 14..=18 => NalKind::Opener,
            1 | 5 => {
                let data = rbsp(&nal[1..]).data;
                let mut r = BitReader::new(&data);
                if r.ue() != Ok(0) {
                    NalKind::Slice
                } else if self.colour_plane(&mut r).unwrap_or(0) == 0 {
                    NalKind::FirstSlice
                } else {
                    // Plans de couleur séparés : les tranches Cb et Cr (plans 1 et 2) ont aussi
                    // `first_mb_in_slice = 0`, mais appartiennent à l'image du plan 0.
                    NalKind::Slice
                }
            }
            2..=4 => NalKind::Slice,
            _ => NalKind::Other,
        }
    }

    fn add(&mut self, nal: &[u8]) {
        let t = nal[0] & 0x1F;
        match t {
            6 => {
                self.au.meta.params.insert(ParamSet::SEI);
                if self.au.recovery.is_none() {
                    self.au.recovery = recovery_frame_cnt(nal);
                }
            }
            7 => {
                self.au.meta.params.insert(ParamSet::SPS);
                if let Ok(Some((id, sps))) = parse_sps(nal) {
                    self.sps[id] = Some(sps);
                }
            }
            8 => {
                self.au.meta.params.insert(ParamSet::PPS);
                if let Ok(Some((id, sps))) = parse_pps(nal) {
                    self.pps[id] = Some(sps);
                }
            }
            9 => self.au.meta.params.insert(ParamSet::AUD),
            1..=5 if !self.au.has_slice => {
                self.au.has_slice = true;
                self.au.meta.first_vcl_type = t;
                self.au.meta.reference = nal[0] & 0x60 != 0;
                if t == 5 {
                    self.au.meta.rap = RapClass::Idr;
                }
                if matches!(t, 1 | 5) {
                    self.first_slice(nal);
                }
            }
            _ => {}
        }
    }

    fn take(&mut self) -> AuMeta {
        let au = std::mem::take(&mut self.au);
        let mut meta = au.meta;
        if let Some(frame_cnt) = au.recovery
            && meta.rap == RapClass::None
            && au.has_slice
            && meta.picture == PictureType::I
        {
            meta.rap = RapClass::Recovery { frame_cnt };
        }
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_kinds() {
        let p = H264::default();
        // first_mb = 0 (1) ; first_mb = 1 (010) ; ue illisible.
        assert_eq!(p.kind(&[0x65, 0b1000_1000]), NalKind::FirstSlice);
        assert_eq!(p.kind(&[0x41, 0b0100_1000]), NalKind::Slice);
        assert_eq!(p.kind(&[0x41]), NalKind::Slice);
        assert_eq!(picture_of(10), None);
    }

    #[test]
    fn sps_errors_do_not_panic() {
        assert!(parse_sps(&[0x67]).is_err());
        assert!(parse_sps(&[0x67, 100, 0, 40, 0x80]).is_err());
        assert!(parse_pps(&[0x68]).is_err());
        assert_eq!(recovery_frame_cnt(&[0x06]), None);
        // num_ref_frames_in_pic_order_cnt_cycle > 255 : SPS ignoré.
        let mut w = ma_bits::BitWriter::new();
        w.u(24, 0x4D_0028);
        w.ue(0);
        w.ue(0);
        w.ue(1);
        w.u(1, 0);
        w.se(0);
        w.se(0);
        w.ue(300);
        let mut nal = vec![0x67];
        nal.extend(w.finish());
        assert_eq!(parse_sps(&nal), Ok(None));
        let mut p = H264::default();
        assert_eq!(p.take(), AuMeta::default());
        assert_eq!(p.kind(&[0x0C]), NalKind::Other);
        assert_eq!(p.kind(&[0x01]), NalKind::Slice);
    }
}
