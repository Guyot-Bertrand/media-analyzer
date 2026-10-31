//! Source d'unités d'accès synthétique pour les tests (et, en Tâche 4, pour `FakeDecoder`).
//!
//! Les entrées sont en ordre de décodage. Les images B sont réordonnées : `pres_idx` suit l'ordre de
//! présentation, `pts = pres_idx × PTS_STEP`. Le contenu d'une AU est `decode_idx` sur 4 octets
//! petit-boutistes, de sorte qu'un décodeur factice sait quelle image il reçoit.
//!
//! Corruption : `with_corrupt` ne modifie pas les octets de l'AU (ils restent les 4 octets de
//! `decode_idx`). L'état est lisible par `is_corrupt(decode_idx)` et `corrupt_set()` ; la fabrique du
//! décodeur factice clone le jeu au moment de sa création et le consulte avec le `decode_idx` lu dans
//! l'AU.

use std::collections::HashSet;
use std::io;

use crate::{AuSource, Codec, FrameEntry, Leading, PictureType, RapKind};

/// Durée d'une image en unités de `pts`.
pub const PTS_STEP: i64 = 3600;

/// Image brute : (présentation, type, RAP, leading), en ordre de décodage.
type Raw = (u32, PictureType, RapKind, Leading);

#[derive(Debug, Clone)]
pub struct FakeSource {
    codec: Codec,
    entries: Vec<FrameEntry>,
    by_pres: Vec<u32>,
    corrupt: HashSet<u32>,
}

/// Images d'une suite `count` images après `start` (en présentation), `bframes` B par ancre P.
fn run(out: &mut Vec<Raw>, start: u32, count: u32, bframes: u32) {
    let (mut cur, mut left) = (start, count);
    while left > 0 {
        let group = (bframes + 1).min(left);
        let anchor = cur + group;
        out.push((anchor, PictureType::P, RapKind::None, Leading::None));
        for pres in cur + 1..anchor {
            out.push((pres, PictureType::B, RapKind::None, Leading::None));
        }
        cur = anchor;
        left -= group;
    }
}

fn closed_raw(gops: u32, gop_len: u32, bframes: u32, offset: u32) -> Vec<Raw> {
    assert!(gop_len >= 1, "gop_len doit valoir au moins 1");
    let mut raw = Vec::new();
    for g in 0..gops {
        let base = offset + g * gop_len;
        raw.push((base, PictureType::I, RapKind::Idr, Leading::None));
        run(&mut raw, base, gop_len - 1, bframes);
    }
    raw
}

impl FakeSource {
    /// Construit une source à partir d'entrées en ordre de décodage.
    ///
    /// # Panics
    /// Si `decode_idx` ne vaut pas la position, ou si `pres_idx` n'est pas une permutation.
    #[must_use]
    pub fn from_entries(codec: Codec, entries: Vec<FrameEntry>) -> FakeSource {
        let mut by_pres = vec![u32::MAX; entries.len()];
        for (pos, e) in entries.iter().enumerate() {
            assert_eq!(e.decode_idx as usize, pos, "decode_idx != position");
            let slot = by_pres
                .get_mut(e.pres_idx as usize)
                .expect("pres_idx hors limites");
            assert_eq!(*slot, u32::MAX, "pres_idx en double");
            *slot = e.decode_idx;
        }
        FakeSource {
            codec,
            entries,
            by_pres,
            corrupt: HashSet::new(),
        }
    }

    fn from_raw(codec: Codec, raw: Vec<Raw>) -> FakeSource {
        let entries = raw
            .into_iter()
            .zip(0u32..)
            .map(|((pres, picture, rap, leading), decode_idx)| FrameEntry {
                decode_idx,
                pres_idx: pres,
                pts: i64::from(pres) * PTS_STEP,
                picture,
                rap,
                leading,
            })
            .collect();
        FakeSource::from_entries(codec, entries)
    }

    /// GOP fermés H.264 : chaque GOP commence par un IDR.
    #[must_use]
    pub fn closed_gops(gops: u32, gop_len: u32, bframes: u32) -> FakeSource {
        FakeSource::from_raw(Codec::H264, closed_raw(gops, gop_len, bframes, 0))
    }

    /// GOP ouverts HEVC : chaque GOP commence par un CRA suivi de ses `bframes` images RASL
    /// (précédant le CRA en présentation). Le premier CRA ouvre le flux.
    ///
    /// # Panics
    /// Si `gop_len <= bframes`.
    #[must_use]
    pub fn open_gops(gops: u32, gop_len: u32, bframes: u32) -> FakeSource {
        assert!(gop_len > bframes, "gop_len doit dépasser bframes");
        let mut raw = Vec::new();
        for g in 0..gops {
            let cra = g * gop_len + bframes;
            raw.push((cra, PictureType::I, RapKind::Cra, Leading::None));
            for pres in cra - bframes..cra {
                raw.push((pres, PictureType::B, RapKind::None, Leading::Skippable));
            }
            run(&mut raw, cra, gop_len - bframes - 1, bframes);
        }
        FakeSource::from_raw(Codec::Hevc, raw)
    }

    /// Transforme en RADL (décodable) l'image qui précède immédiatement chaque CRA en présentation.
    #[must_use]
    pub fn with_radl(mut self) -> FakeSource {
        let cra_pres: HashSet<u32> = self
            .entries
            .iter()
            .filter(|e| e.rap == RapKind::Cra)
            .map(|e| e.pres_idx)
            .collect();
        for e in &mut self.entries {
            if e.leading == Leading::Skippable && cra_pres.contains(&(e.pres_idx + 1)) {
                e.leading = Leading::Decodable;
            }
        }
        self
    }

    /// H.264 sans réordonnancement : un IDR au début, puis un I non IDR avec SEI recovery point
    /// (`frame_cnt`) tous les `gop_len` images.
    #[must_use]
    pub fn recovery(gops: u32, gop_len: u32, frame_cnt: u32) -> FakeSource {
        let raw = (0..gops * gop_len)
            .map(|i| {
                let (picture, rap) = if i == 0 {
                    (PictureType::I, RapKind::Idr)
                } else if i % gop_len == 0 {
                    (PictureType::I, RapKind::Recovery { frame_cnt })
                } else {
                    (PictureType::P, RapKind::None)
                };
                (i, picture, rap, Leading::None)
            })
            .collect();
        FakeSource::from_raw(Codec::H264, raw)
    }

    /// Fichier coupé au milieu d'un GOP : `lead_in` images P sans RAP, puis des GOP fermés.
    #[must_use]
    pub fn starting_mid_gop(gops: u32, gop_len: u32, bframes: u32, lead_in: u32) -> FakeSource {
        let mut raw: Vec<Raw> = (0..lead_in)
            .map(|p| (p, PictureType::P, RapKind::None, Leading::None))
            .collect();
        raw.extend(closed_raw(gops, gop_len, bframes, lead_in));
        FakeSource::from_raw(Codec::H264, raw)
    }

    /// Marque ces AU (par `decode_idx`) comme corrompues.
    #[must_use]
    pub fn with_corrupt(mut self, set: HashSet<u32>) -> FakeSource {
        self.corrupt = set;
        self
    }

    #[must_use]
    pub fn is_corrupt(&self, decode_idx: u32) -> bool {
        self.corrupt.contains(&decode_idx)
    }

    #[must_use]
    pub fn corrupt_set(&self) -> &HashSet<u32> {
        &self.corrupt
    }
}

impl AuSource for FakeSource {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).expect("source trop longue")
    }

    fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
        self.entries.get(decode_idx as usize).copied()
    }

    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
        self.by_pres.get(pres_idx as usize).copied()
    }

    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        if decode_idx >= self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AU hors limites",
            ));
        }
        buf.clear();
        buf.extend_from_slice(&decode_idx.to_le_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_gops_reorders_b_frames() {
        let s = FakeSource::closed_gops(2, 6, 2);
        assert_eq!(s.len(), 12);
        assert_eq!(s.codec(), Codec::H264);
        // GOP 0 en ordre de décodage : I0 P3 B1 B2 P5 B4.
        let pres: Vec<u32> = (0..6).map(|d| s.entry(d).unwrap().pres_idx).collect();
        assert_eq!(pres, [0, 3, 1, 2, 5, 4]);
        assert_eq!(s.entry(6).unwrap().rap, RapKind::Idr);
        for d in 0..12 {
            let e = s.entry(d).unwrap();
            assert_eq!(e.pts, i64::from(e.pres_idx) * PTS_STEP);
            assert_eq!(s.decode_idx_of(e.pres_idx), Some(d));
        }
        assert_eq!(s.decode_idx_of(12), None);
        assert!(s.entry(12).is_none());
    }

    #[test]
    fn open_gops_has_leading_pictures() {
        let s = FakeSource::open_gops(2, 8, 2);
        assert_eq!(s.len(), 16);
        assert_eq!(s.codec(), Codec::Hevc);
        let e = s.entry(0).unwrap();
        assert_eq!((e.rap, e.pres_idx), (RapKind::Cra, 2));
        assert_eq!(s.entry(1).unwrap().leading, Leading::Skippable);
        assert_eq!(s.entry(2).unwrap().leading, Leading::Skippable);
        let radl = FakeSource::open_gops(2, 8, 2).with_radl();
        assert_eq!(radl.entry(1).unwrap().leading, Leading::Skippable);
        assert_eq!(radl.entry(2).unwrap().leading, Leading::Decodable);
    }

    #[test]
    fn au_content_is_decode_idx_le() {
        let s = FakeSource::closed_gops(1, 4, 0);
        let mut buf = vec![9; 10];
        s.read_au(3, &mut buf).unwrap();
        assert_eq!(buf, 3u32.to_le_bytes());
        assert!(s.read_au(4, &mut buf).is_err());
    }

    #[test]
    fn corrupt_marking() {
        let s = FakeSource::closed_gops(1, 4, 0).with_corrupt(HashSet::from([2]));
        assert!(s.is_corrupt(2));
        assert!(!s.is_corrupt(1));
        assert_eq!(s.corrupt_set().len(), 1);
    }

    #[test]
    fn recovery_and_mid_gop_shapes() {
        let r = FakeSource::recovery(3, 5, 2);
        assert_eq!(r.entry(0).unwrap().rap, RapKind::Idr);
        assert_eq!(r.entry(5).unwrap().rap, RapKind::Recovery { frame_cnt: 2 });
        let m = FakeSource::starting_mid_gop(2, 4, 0, 3);
        assert_eq!(m.entry(2).unwrap().rap, RapKind::None);
        assert_eq!(m.entry(3).unwrap().rap, RapKind::Idr);
        assert_eq!(m.len(), 11);
    }
}
