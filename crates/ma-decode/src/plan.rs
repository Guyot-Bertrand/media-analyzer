//! Plan d'accès aléatoire : choix de l'image de départ du décodage pour atteindre une image cible.

use crate::{AuSource, FrameEntry, Leading, RapKind};

/// Raison pour laquelle une image ne peut pas être décodée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NotDecodable {
    #[error("l'image précède le premier point d'accès aléatoire")]
    BeforeFirstRap,
    #[error("l'image dépend d'images absentes avant le premier point d'accès aléatoire")]
    LeadingOfFirstRap,
    #[error("numéro d'image hors du fichier")]
    OutOfRange,
}

/// Dernier RAP d'index de décodage au plus égal à `from` (recherche arrière linéaire, bornée par
/// le GOP : elle s'arrête au premier RAP rencontré).
fn rap_at_or_before(src: &dyn AuSource, from: u32) -> Option<FrameEntry> {
    (0..=from)
        .rev()
        .filter_map(|i| src.entry(i))
        .find(|e| e.rap != RapKind::None)
}

/// `decode_idx` à partir duquel décoder pour obtenir l'image `decode_idx`.
///
/// On part du dernier RAP au plus égal à la cible. Si la cible est une image `Skippable` de ce RAP
/// (RASL, B précédant un I de GOP ouvert), ou si elle précède la fin de l'intervalle de reprise
/// d'un `Recovery { frame_cnt }`, on part du RAP précédent.
pub fn start_for(src: &dyn AuSource, decode_idx: u32) -> Result<u32, NotDecodable> {
    let target = src.entry(decode_idx).ok_or(NotDecodable::OutOfRange)?;
    let mut from = decode_idx;
    let mut nearest = true;
    loop {
        let Some(rap) = rap_at_or_before(src, from) else {
            return Err(if nearest {
                NotDecodable::BeforeFirstRap
            } else {
                NotDecodable::LeadingOfFirstRap
            });
        };
        let skipped = nearest && target.leading == Leading::Skippable;
        let recovering = matches!(
            rap.rap,
            RapKind::Recovery { frame_cnt }
                if target.pres_idx < rap.pres_idx.saturating_add(frame_cnt)
        );
        if !skipped && !recovering {
            return Ok(rap.decode_idx);
        }
        from = rap
            .decode_idx
            .checked_sub(1)
            .ok_or(NotDecodable::LeadingOfFirstRap)?;
        nearest = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSource;
    use crate::{Codec, PictureType};

    #[ma_trace::covers("NAV.ORACLE.02")]
    #[test]
    fn rasl_starts_from_previous_rap() {
        // GOP de 8 : CRA, 2 RASL, 5 images suivantes. CRA du GOP 1 en décodage 8.
        let s = FakeSource::open_gops(3, 8, 2);
        assert_eq!(s.entry(9).unwrap().leading, Leading::Skippable);
        assert_eq!(start_for(&s, 9), Ok(0));
        assert_eq!(start_for(&s, 10), Ok(0));
        assert_eq!(start_for(&s, 17), Ok(8));
        // Une image qui suit le CRA sans lui précéder part du CRA.
        assert_eq!(start_for(&s, 12), Ok(8));
        assert_eq!(start_for(&s, 8), Ok(8));
    }

    #[test]
    fn radl_starts_from_its_cra() {
        let s = FakeSource::open_gops(3, 8, 2).with_radl();
        assert_eq!(s.entry(10).unwrap().leading, Leading::Decodable);
        assert_eq!(start_for(&s, 10), Ok(8));
        // Le RASL du même CRA part toujours du RAP précédent.
        assert_eq!(start_for(&s, 9), Ok(0));
    }

    #[ma_trace::covers("NAV.ORACLE.02")]
    #[test]
    fn only_first_cra_rasl_not_decodable() {
        let s = FakeSource::open_gops(3, 8, 2);
        assert_eq!(start_for(&s, 0), Ok(0));
        assert_eq!(start_for(&s, 1), Err(NotDecodable::LeadingOfFirstRap));
        assert_eq!(start_for(&s, 2), Err(NotDecodable::LeadingOfFirstRap));
        assert_eq!(start_for(&s, 3), Ok(0));
    }

    #[ma_trace::covers("NAV.ORACLE.03")]
    #[test]
    fn recovery_point_bounds_decode_to_one_interval() {
        let (gop_len, frame_cnt) = (10, 3);
        let s = FakeSource::recovery(4, gop_len, frame_cnt);
        // Cible au milieu : recovery point le plus proche, jamais l'image 0.
        assert_eq!(start_for(&s, 25), Ok(20));
        assert_eq!(start_for(&s, 33), Ok(30));
        // Cible dans l'intervalle de reprise : RAP précédent.
        assert_eq!(start_for(&s, 21), Ok(10));
        assert_eq!(start_for(&s, 20), Ok(10));
        for t in 0..s.len() {
            let start = start_for(&s, t).unwrap();
            assert!(t - start < gop_len + frame_cnt, "{t} part de {start}");
            if t >= 2 * gop_len + frame_cnt {
                assert_ne!(start, 0, "{t} ne doit pas repartir de l'image 0");
            }
        }
    }

    #[test]
    fn recovery_interval_longer_than_gop_walks_back() {
        let s = FakeSource::recovery(4, 10, 15);
        // 25 < 20 + 15 et 25 >= 10 + 15 : part du RAP 10.
        assert_eq!(start_for(&s, 25), Ok(10));
        // 15 < 10 + 15 et 15 < 0 + 0 faux : part du RAP 0 (IDR).
        assert_eq!(start_for(&s, 15), Ok(0));
        // Premier RAP lui-même de type recovery : rien avant.
        let first = FakeSource::from_entries(
            Codec::H264,
            vec![crate::FrameEntry {
                decode_idx: 0,
                pres_idx: 0,
                pts: 0,
                picture: PictureType::I,
                rap: RapKind::Recovery { frame_cnt: 2 },
                leading: Leading::None,
            }],
        );
        assert_eq!(start_for(&first, 0), Err(NotDecodable::LeadingOfFirstRap));
    }

    #[ma_trace::covers("NAV.ORACLE.04")]
    #[test]
    fn frames_before_first_rap_not_decodable() {
        let s = FakeSource::starting_mid_gop(2, 6, 2, 3);
        for t in 0..3 {
            assert_eq!(start_for(&s, t), Err(NotDecodable::BeforeFirstRap));
        }
        assert_eq!(start_for(&s, 3), Ok(3));
        assert_eq!(start_for(&s, 5), Ok(3));
        assert_eq!(start_for(&s, 9), Ok(9));
    }

    #[test]
    fn idr_target_starts_at_itself() {
        let s = FakeSource::closed_gops(3, 8, 2);
        assert_eq!(start_for(&s, 0), Ok(0));
        assert_eq!(start_for(&s, 8), Ok(8));
        assert_eq!(start_for(&s, 16), Ok(16));
        // Une B réordonnée du GOP 1 part de son IDR.
        assert_eq!(start_for(&s, 10), Ok(8));
    }

    #[test]
    fn mpeg2_open_gop_leading_b_skippable() {
        use crate::FrameEntry;
        let fe = |decode_idx, pres_idx, picture, rap, leading| FrameEntry {
            decode_idx,
            pres_idx,
            pts: 0,
            picture,
            rap,
            leading,
        };
        let intra = RapKind::Intra { closed_gop: false };
        let s = FakeSource::from_entries(
            Codec::Mpeg2,
            vec![
                fe(0, 2, PictureType::I, intra, Leading::None),
                fe(1, 0, PictureType::B, RapKind::None, Leading::Skippable),
                fe(2, 1, PictureType::B, RapKind::None, Leading::Skippable),
                fe(3, 5, PictureType::P, RapKind::None, Leading::None),
                fe(4, 3, PictureType::B, RapKind::None, Leading::None),
                fe(5, 4, PictureType::B, RapKind::None, Leading::None),
                fe(6, 8, PictureType::I, intra, Leading::None),
                fe(7, 6, PictureType::B, RapKind::None, Leading::Skippable),
                fe(8, 7, PictureType::B, RapKind::None, Leading::Skippable),
            ],
        );
        assert_eq!(start_for(&s, 1), Err(NotDecodable::LeadingOfFirstRap));
        assert_eq!(start_for(&s, 7), Ok(0));
        assert_eq!(start_for(&s, 4), Ok(0));
        assert_eq!(start_for(&s, 6), Ok(6));
    }

    #[test]
    fn out_of_range() {
        let s = FakeSource::closed_gops(1, 4, 0);
        assert_eq!(start_for(&s, 4), Err(NotDecodable::OutOfRange));
        let empty = FakeSource::closed_gops(0, 4, 0);
        assert_eq!(start_for(&empty, 0), Err(NotDecodable::OutOfRange));
    }
}
