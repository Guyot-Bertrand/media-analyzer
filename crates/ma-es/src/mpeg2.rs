//! MPEG-2 vidéo (ISO/CEI 13818-2).
//!
//! - Nouvelle AU sur `B3` (en-tête de séquence), `B8` (en-tête de GOP) ou `00` (en-tête
//!   d'image) qui suit les tranches (`01` à `AF`) de l'image courante. Chaque image de champ
//!   forme donc sa propre AU.
//! - `picture_coding_type` donne le type ; une I précédée dans la même AU d'un en-tête de
//!   séquence ou de GOP ⇒ `Intra { closed_gop, broken_link }`.
//! - `picture_coding_extension` : `picture_structure` 1 ⇒ `Top`, 2 ⇒ `Bottom`, 3 ⇒ `Frame`.

use ma_bits::{BitError, BitReader};
use ma_core::PictureType;

use crate::framer::{NalKind, NalParser};
use crate::{AuMeta, Field, MAX_HEADER_BYTES, MAX_SLICE_HEADER_BYTES, ParamSet, RapClass};

const PICTURE: u8 = 0x00;
const SEQUENCE_HEADER: u8 = 0xB3;
const EXTENSION: u8 = 0xB5;
const GROUP: u8 = 0xB8;

fn is_slice(code: u8) -> bool {
    (0x01..=0xAF).contains(&code)
}

/// `(closed_gop, broken_link)` d'un en-tête de GOP.
fn parse_gop(unit: &[u8]) -> Result<(bool, bool), BitError> {
    let mut r = BitReader::new(&unit[1..]);
    r.skip(25)?; // time_code
    Ok((r.flag()?, r.flag()?))
}

fn parse_picture(unit: &[u8]) -> Result<Option<PictureType>, BitError> {
    let mut r = BitReader::new(&unit[1..]);
    r.skip(10)?; // temporal_reference
    Ok(match r.u(3)? {
        1 | 4 => Some(PictureType::I),
        2 => Some(PictureType::P),
        3 => Some(PictureType::B),
        _ => None,
    })
}

/// Identifiant d'extension et, pour `picture_coding_extension` (8), la structure d'image.
fn parse_extension(unit: &[u8]) -> Result<(u64, Option<Field>), BitError> {
    let mut r = BitReader::new(&unit[1..]);
    let id = r.u(4)?;
    if id != 8 {
        return Ok((id, None));
    }
    r.skip(16 + 2)?; // f_code[2][2], intra_dc_precision
    let field = match r.u(2)? {
        1 => Some(Field::Top),
        2 => Some(Field::Bottom),
        3 => Some(Field::Frame),
        _ => None,
    };
    Ok((id, field))
}

/// État de l'AU courante.
#[derive(Default)]
struct Au {
    meta: AuMeta,
    picture_seen: bool,
    /// En-tête de séquence ou de GOP vu avant l'en-tête d'image.
    entry: bool,
    gop: Option<(bool, bool)>,
}

#[derive(Default)]
pub(crate) struct Mpeg2 {
    au: Au,
}

impl NalParser for Mpeg2 {
    fn header_cap(&self, first: u8) -> usize {
        if is_slice(first) {
            MAX_SLICE_HEADER_BYTES
        } else {
            MAX_HEADER_BYTES
        }
    }

    fn kind(&self, unit: &[u8]) -> NalKind {
        match unit[0] {
            PICTURE | SEQUENCE_HEADER | GROUP => NalKind::Opener,
            c if is_slice(c) => NalKind::Slice,
            _ => NalKind::Other,
        }
    }

    fn add(&mut self, unit: &[u8]) {
        let au = &mut self.au;
        match unit[0] {
            SEQUENCE_HEADER => {
                au.meta.params.insert(ParamSet::SEQ_HEADER);
                au.entry |= !au.picture_seen;
            }
            GROUP => {
                au.meta.params.insert(ParamSet::GOP_HEADER);
                if !au.picture_seen {
                    au.entry = true;
                    au.gop = parse_gop(unit).ok();
                }
            }
            EXTENSION => match parse_extension(unit) {
                Ok((1, _)) => au.meta.params.insert(ParamSet::SEQ_EXT),
                Ok((8, Some(field))) if au.picture_seen => au.meta.field = field,
                _ => {}
            },
            PICTURE if !au.picture_seen => {
                au.picture_seen = true;
                if let Ok(Some(p)) = parse_picture(unit) {
                    au.meta.picture = p;
                }
                au.meta.reference = au.meta.picture != PictureType::B;
            }
            _ => {}
        }
    }

    fn take(&mut self) -> AuMeta {
        let au = std::mem::take(&mut self.au);
        let mut meta = au.meta;
        if au.picture_seen && au.entry && meta.picture == PictureType::I {
            let (closed_gop, broken_link) = au.gop.unwrap_or((false, false));
            meta.rap = RapClass::Intra {
                closed_gop,
                broken_link,
            };
        }
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_units_are_errors() {
        assert!(parse_gop(&[GROUP, 0, 0]).is_err());
        assert!(parse_picture(&[PICTURE]).is_err());
        assert!(parse_extension(&[EXTENSION]).is_err());
        assert_eq!(parse_extension(&[EXTENSION, 0x80, 0, 0, 0]), Ok((8, None)));
        let mut p = Mpeg2::default();
        assert_eq!(p.kind(&[0xB2]), NalKind::Other);
        assert_eq!(p.kind(&[0x2A]), NalKind::Slice);
        // Extension d'image avant tout en-tête d'image : ignorée.
        p.add(&[EXTENSION, 0x8F, 0xFF, 0xF2]);
        assert_eq!(p.take().field, Field::Frame);
    }
}
