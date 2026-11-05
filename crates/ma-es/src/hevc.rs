//! HEVC (UIT-T H.265, Annex B).
//!
//! - Nouvelle AU sur une tranche dont `first_slice_segment_in_pic_flag` vaut 1, ou sur un NAL
//!   32 à 35, 39, 41 à 44 ou 48 à 55 qui suit une tranche de l'AU courante (§7.4.2.4.4).
//! - 19 et 20 ⇒ `Idr` ; 21 ⇒ `Cra` ; 16 à 18 ⇒ `Bla` ; 8 et 9 ⇒ RASL ; 6 et 7 ⇒ RADL.
//! - `slice_type` lu avec le `num_extra_slice_header_bits` du PPS ; PPS inconnu ⇒ I pour une
//!   IRAP, sinon P.

use ma_bits::{BitError, BitReader, rbsp};
use ma_core::PictureType;

use crate::framer::{NalKind, NalParser};
use crate::{AuMeta, MAX_HEADER_BYTES, MAX_SLICE_HEADER_BYTES, NalLeading, ParamSet, RapClass};

fn nal_type(nal: &[u8]) -> u8 {
    (nal[0] >> 1) & 0x3F
}

fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}

/// `(pps_id, num_extra_slice_header_bits)` ; identifiants hors plage ⇒ `None`.
fn parse_pps(nal: &[u8]) -> Result<Option<(usize, u8)>, BitError> {
    let data = rbsp(nal.get(2..).unwrap_or_default()).data;
    let mut r = BitReader::new(&data);
    let pps = r.ue()?;
    let sps = r.ue()?;
    r.skip(2)?; // dependent_slice_segments_enabled_flag, output_flag_present_flag
    let extra = r.u(3)?;
    Ok((pps < 64 && sps < 16).then(|| (pps as usize, u8::try_from(extra).unwrap_or(0))))
}

/// État de l'AU courante.
#[derive(Default)]
struct Au {
    meta: AuMeta,
    has_slice: bool,
}

pub(crate) struct Hevc {
    /// `num_extra_slice_header_bits` de chaque PPS.
    pps: [Option<u8>; 64],
    au: Au,
}

impl Default for Hevc {
    fn default() -> Self {
        Self {
            pps: [None; 64],
            au: Au::default(),
        }
    }
}

impl Hevc {
    /// `slice_type` de la 1re tranche d'une image, si son PPS est connu et l'en-tête lisible.
    fn slice_type(&self, t: u8, nal: &[u8]) -> Result<Option<PictureType>, BitError> {
        let data = rbsp(nal.get(2..).unwrap_or_default()).data;
        let mut r = BitReader::new(&data);
        if !r.flag()? {
            return Ok(None);
        }
        if is_irap(t) {
            r.skip(1)?; // no_output_of_prior_pics_flag
        }
        let pps = r.ue()?;
        let Some(extra) = self.pps.get(pps as usize).copied().flatten() else {
            return Ok(None);
        };
        r.skip(u64::from(extra))?;
        Ok(match r.ue()? {
            0 => Some(PictureType::B),
            1 => Some(PictureType::P),
            2 => Some(PictureType::I),
            _ => None,
        })
    }
}

impl NalParser for Hevc {
    fn header_cap(&self, first: u8) -> usize {
        if (first >> 1) & 0x3F < 32 {
            MAX_SLICE_HEADER_BYTES
        } else {
            MAX_HEADER_BYTES
        }
    }

    fn kind(&self, nal: &[u8]) -> NalKind {
        match nal_type(nal) {
            0..=31 if nal.get(2).is_some_and(|b| b & 0x80 != 0) => NalKind::FirstSlice,
            0..=31 => NalKind::Slice,
            32..=35 | 39 | 41..=44 | 48..=55 => NalKind::Opener,
            _ => NalKind::Other,
        }
    }

    fn add(&mut self, nal: &[u8]) {
        let t = nal_type(nal);
        let params = &mut self.au.meta.params;
        match t {
            32 => params.insert(ParamSet::VPS),
            33 => params.insert(ParamSet::SPS),
            34 => {
                params.insert(ParamSet::PPS);
                if let Ok(Some((id, extra))) = parse_pps(nal) {
                    self.pps[id] = Some(extra);
                }
            }
            35 => params.insert(ParamSet::AUD),
            39 | 40 => params.insert(ParamSet::SEI),
            0..=31 if !self.au.has_slice => {
                self.au.has_slice = true;
                let meta = &mut self.au.meta;
                meta.first_vcl_type = t;
                meta.reference = !(t <= 14 && t.is_multiple_of(2));
                meta.rap = match t {
                    16..=18 => RapClass::Bla,
                    19 | 20 => RapClass::Idr,
                    21 => RapClass::Cra,
                    _ => RapClass::None,
                };
                meta.leading = match t {
                    6 | 7 => Some(NalLeading::Decodable),
                    8 | 9 => Some(NalLeading::Skippable),
                    _ => None,
                };
                let default = if is_irap(t) {
                    PictureType::I
                } else {
                    PictureType::P
                };
                let picture = self.slice_type(t, nal).ok().flatten().unwrap_or(default);
                self.au.meta.picture = picture;
            }
            _ => {}
        }
    }

    fn take(&mut self) -> AuMeta {
        std::mem::take(&mut self.au).meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pps_and_slice_errors_do_not_panic() {
        assert!(parse_pps(&[0x44, 0x01]).is_err());
        assert!(parse_pps(&[0x44]).is_err());
        let p = Hevc::default();
        // Tranche qui n'est pas la 1re de l'image, en-têtes tronqués.
        assert_eq!(p.slice_type(1, &[0x02, 0x01, 0x40]), Ok(None));
        assert!(p.slice_type(1, &[0x02, 0x01]).is_err());
        assert!(p.slice_type(19, &[0x26, 0x01, 0x80]).is_err());
        assert_eq!(p.kind(&[0x02]), NalKind::Slice);
        assert_eq!(p.kind(&[40 << 1, 1]), NalKind::Other);
        assert_eq!(p.header_cap(0x26), MAX_SLICE_HEADER_BYTES);
        assert_eq!(p.header_cap(0x40), MAX_HEADER_BYTES);
    }

    #[test]
    fn unknown_slice_type_keeps_default() {
        let mut p = Hevc::default();
        // PPS 0 sans bits supplémentaires, puis tranche TRAIL_R de slice_type 3 (invalide).
        p.add(&[34 << 1, 1, 0b1100_0000]);
        // first = 1, pps = 0 (1), slice_type = 3 (00100)
        p.add(&[1 << 1, 1, 0b1100_1000]);
        assert_eq!(p.take().picture, PictureType::P);
    }
}
