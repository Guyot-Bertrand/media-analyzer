//! Simulateur pour les tests : source d'unités d'accès (`FakeSource`), décodeur (`FakeDecoder`) et
//! fabrique (`FakeFactory`). Compilé pour les tests de la crate, ou avec la feature `fake` pour les
//! tests des autres crates.
//!
//! Les entrées sont en ordre de décodage. Les images B sont réordonnées : `pres_idx` suit l'ordre de
//! présentation, `pts = pres_idx × PTS_STEP`. Le contenu d'une AU est `decode_idx` sur 4 octets
//! petit-boutistes, de sorte qu'un décodeur factice sait quelle image il reçoit.
//!
//! Corruption : `with_corrupt` ne modifie pas les octets de l'AU (ils restent les 4 octets de
//! `decode_idx`). L'état est lisible par `is_corrupt(decode_idx)` et `corrupt_set()` ; la fabrique du
//! décodeur factice clone le jeu au moment de sa création et le consulte avec le `decode_idx` lu dans
//! l'AU.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::time::Duration;

use crate::{
    AuSource, Codec, DecodeError, DecodeErrorKind, DecodeMode, Decoder, DecoderFactory,
    DecoderStats, FrameEntry, Leading, ModePref, PictureType, PixelLayout, Planes, RapKind,
    Received, Rect, Size, Viewport,
};

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

    /// Comme [`Self::open_gops`], mais tous les RAP après le premier sont des `Bla` : leurs images
    /// en tête (`Skippable`) dépendent d'images remplacées et ne sortent pas du décodeur.
    ///
    /// # Panics
    /// Si `gop_len <= bframes`.
    #[must_use]
    pub fn broken_link_gops(gops: u32, gop_len: u32, bframes: u32) -> FakeSource {
        let mut s = FakeSource::open_gops(gops, gop_len, bframes);
        let first = s.entries.iter().position(|e| e.rap == RapKind::Cra);
        for (i, e) in s.entries.iter_mut().enumerate() {
            if e.rap == RapKind::Cra && Some(i) != first {
                e.rap = RapKind::Bla;
            }
        }
        s
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
    /// (`pres_span`) tous les `gop_len` images.
    #[must_use]
    pub fn recovery(gops: u32, gop_len: u32, pres_span: u32) -> FakeSource {
        let raw = (0..gops * gop_len)
            .map(|i| {
                let (picture, rap) = if i == 0 {
                    (PictureType::I, RapKind::Idr)
                } else if i % gop_len == 0 {
                    (PictureType::I, RapKind::Recovery { pres_span })
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

/// Taille d'image par défaut du décodeur factice.
pub const FAKE_SIZE: Size = Size { w: 1280, h: 720 };

/// Oracle du simulateur : `pres_idx` lu dans les 4 premiers octets du plan de luminance, si l'image
/// est assez grande pour le porter.
#[must_use]
pub fn oracle(image: &Planes) -> Option<u32> {
    let head = image.planes.first()?.get(..4)?;
    Some(u32::from_le_bytes(head.try_into().ok()?))
}

/// Fabrique de [`FakeDecoder`] sur une [`FakeSource`].
///
/// Le jeu d'AU corrompues est celui de la source au moment de la création de la fabrique.
#[derive(Debug, Clone)]
pub struct FakeFactory {
    pub source: FakeSource,
    /// Décodage matériel disponible : `ModePref::Auto` ouvre alors un décodeur `Hardware`.
    pub hw_available: bool,
    /// Un décodeur matériel échoue (erreur `Gpu`, définitivement) au-delà de ce nombre d'appels à
    /// `receive`.
    pub hw_fail_after_receives: Option<u32>,
    /// Taille d'affichage des images décodées.
    pub size: Size,
    /// Retard du pipeline, en images, au-delà du réordonnancement.
    pub latency: u32,
    /// Délai simulé par AU envoyée.
    pub au_delay: Duration,
    /// L'erreur `InvalidData` d'une AU corrompue est renvoyée par le `receive` suivant au lieu de
    /// `send` (erreur différée, comme avec les threads d'images de libavcodec).
    pub deferred_corruption: bool,
}

impl FakeFactory {
    /// Matériel disponible, sans échec, 1280×720, une image de latence, sans délai.
    #[must_use]
    pub fn new(source: &FakeSource) -> FakeFactory {
        FakeFactory {
            source: source.clone(),
            hw_available: true,
            hw_fail_after_receives: None,
            size: FAKE_SIZE,
            latency: 1,
            au_delay: Duration::ZERO,
            deferred_corruption: false,
        }
    }
}

impl FakeFactory {
    /// Nombre d'AU qu'il faut envoyer, à partir du point de départ du plan de décodage
    /// ([`crate::start_for`]), pour que le décodeur factice sorte l'image `pres_idx` (retard de
    /// `reorder + latency` images). Calculé depuis la source seule, sans passer par le Navigator.
    ///
    /// # Panics
    /// Si l'image est hors de la source ou non décodable.
    #[must_use]
    pub fn aus_until_output(&self, pres_idx: u32) -> u64 {
        let src = &self.source;
        let target = src.entry(src.decode_idx_of(pres_idx).expect("image hors source"));
        let target = target.expect("entrée absente");
        let start = crate::start_for(src, target.decode_idx).expect("image non décodable");
        let delay = (reorder_depth(src) + self.latency) as usize;
        let mut buf = BTreeSet::new();
        for (sent, d) in (1u64..).zip(start..src.len()) {
            buf.insert(src.entry(d).expect("entrée absente").pts);
            while buf.len() > delay {
                if buf.pop_first() == Some(target.pts) {
                    return sent;
                }
            }
        }
        panic!("l'image n'est jamais sortie");
    }
}

impl DecoderFactory for FakeFactory {
    fn open(&self, codec: Codec, pref: ModePref) -> Result<Box<dyn Decoder>, DecodeError> {
        if codec != self.source.codec {
            return Err(DecodeError {
                kind: DecodeErrorKind::Unsupported,
                detail: format!("codec {codec:?} différent de celui de la source"),
            });
        }
        let hw = pref == ModePref::Auto && self.hw_available;
        Ok(Box::new(FakeDecoder::new(self, hw)))
    }
}

/// Nombre minimal d'images à garder en tampon pour sortir la source par `pts` croissant.
fn reorder_depth(src: &FakeSource) -> u32 {
    let in_order = |depth: usize| {
        let mut buf = BTreeSet::new();
        let mut last = i64::MIN;
        for e in &src.entries {
            buf.insert(e.pts);
            if buf.len() > depth {
                let pts = buf.pop_first().unwrap_or(i64::MAX);
                if pts < last {
                    return false;
                }
                last = pts;
            }
        }
        buf.first().is_none_or(|&pts| pts > last)
    };
    let depth = (0..src.entries.len()).find(|&d| in_order(d)).unwrap_or(0);
    u32::try_from(depth).unwrap_or(u32::MAX)
}

/// GOP en cours de décodage : son RAP et l'état de corruption de ses références.
#[derive(Debug, Clone, Copy)]
struct Gop {
    rap: FrameEntry,
    corrupt: bool,
}

/// Image décodée en attente de sortie.
#[derive(Debug, Clone, Copy)]
struct Pending {
    pres_idx: u32,
    corrupt: bool,
}

/// Décodeur simulé.
///
/// Il sort les images par `pts` croissant, avec un retard de `reorder + latency` images. Une image
/// sans référence décodée depuis le dernier `reset` n'est pas émise : image qui précède le premier
/// RAP reçu, `Skippable` (ou image de l'intervalle de reprise d'un recovery point) dont le RAP
/// précédent n'a pas été reçu. Une AU corrompue renvoie `InvalidData` (depuis `send`, ou depuis le
/// `receive` suivant avec `deferred_corruption`) ; les images suivantes du même GOP (et les
/// `Skippable` du GOP suivant) sortent `corrupt`.
#[allow(clippy::struct_excessive_bools)] // simulateur : un drapeau par comportement injecté
pub struct FakeDecoder {
    src: FakeSource,
    hw: bool,
    fail_after: Option<u32>,
    size: Size,
    latency: u32,
    delay: Duration,
    reorder: u32,
    receives: u32,
    failed: bool,
    deferred: bool,
    /// Erreur différée, rendue par le prochain `receive`.
    pending_error: Option<DecodeError>,
    /// Mode effectif, connu à la première image sortie.
    mode: DecodeMode,
    eof: bool,
    buffer: BTreeMap<i64, Pending>,
    gop: Option<Gop>,
    prev_gop: Option<Gop>,
    stats: DecoderStats,
}

impl FakeDecoder {
    fn new(f: &FakeFactory, hw: bool) -> FakeDecoder {
        FakeDecoder {
            src: f.source.clone(),
            hw,
            fail_after: f.hw_fail_after_receives,
            size: f.size,
            latency: f.latency,
            delay: f.au_delay,
            reorder: reorder_depth(&f.source),
            receives: 0,
            failed: false,
            deferred: f.deferred_corruption,
            pending_error: None,
            mode: DecodeMode::Unknown,
            eof: false,
            buffer: BTreeMap::new(),
            gop: None,
            prev_gop: None,
            stats: DecoderStats::default(),
        }
    }

    fn check_failed(&self) -> Result<(), DecodeError> {
        if self.failed {
            return Err(error(DecodeErrorKind::Gpu, "périphérique perdu"));
        }
        Ok(())
    }

    /// Image `e` reçue intacte : la garde en tampon si ses références sont décodées.
    fn accept(&mut self, e: FrameEntry) {
        if e.rap != RapKind::None {
            self.prev_gop = self.gop.take();
            self.gop = Some(Gop {
                rap: e,
                corrupt: false,
            });
        }
        let Some(gop) = self.gop else { return };
        // Lien rompu : les images en tête d'un BLA ne sont jamais produites.
        if e.leading == Leading::Skippable && gop.rap.rap == RapKind::Bla {
            return;
        }
        let needs_prev = e.leading == Leading::Skippable
            || matches!(gop.rap.rap, RapKind::Recovery { pres_span }
                if e.rap == RapKind::None
                    && e.pres_idx < gop.rap.pres_idx.saturating_add(pres_span));
        let corrupt = match (needs_prev, self.prev_gop) {
            (false, _) => gop.corrupt,
            (true, Some(prev)) => gop.corrupt || prev.corrupt,
            (true, None) => return,
        };
        self.buffer.insert(
            e.pts,
            Pending {
                pres_idx: e.pres_idx,
                corrupt,
            },
        );
    }

    /// Image NV12 à la taille de sortie, `pres_idx` dans les 4 premiers octets.
    fn render(pres_idx: u32, out: Size) -> Planes {
        let (w, h) = (out.w as usize, out.h as usize);
        let uv_stride = w.div_ceil(2) * 2;
        let mut y = vec![0x80; w * h];
        for (dst, src) in y.iter_mut().zip(pres_idx.to_le_bytes()) {
            *dst = src;
        }
        Planes {
            layout: PixelLayout::SemiPlanar8,
            size: out,
            planes: vec![y, vec![0x80; uv_stride * h.div_ceil(2)]],
            strides: vec![out.w, u32::try_from(uv_stride).unwrap_or(u32::MAX)],
        }
    }
}

fn error(kind: DecodeErrorKind, detail: &str) -> DecodeError {
    DecodeError {
        kind,
        detail: detail.to_owned(),
    }
}

impl Decoder for FakeDecoder {
    fn mode(&self) -> DecodeMode {
        self.mode
    }

    fn latency(&self) -> u32 {
        self.latency
    }

    fn send(&mut self, pts: i64, au: &[u8]) -> Result<(), DecodeError> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.check_failed()?;
        if self.eof {
            return Err(error(
                DecodeErrorKind::Internal,
                "envoi après send_eof sans reset",
            ));
        }
        let entry = <[u8; 4]>::try_from(au)
            .ok()
            .map(u32::from_le_bytes)
            .and_then(|d| self.src.entry(d))
            .ok_or_else(|| error(DecodeErrorKind::InvalidData, "AU illisible"))?;
        if entry.pts != pts {
            return Err(error(
                DecodeErrorKind::Internal,
                "pts différent de celui de l'AU",
            ));
        }
        if self.src.is_corrupt(entry.decode_idx) {
            if entry.rap == RapKind::None {
                if let Some(gop) = &mut self.gop {
                    gop.corrupt = true;
                }
            } else {
                self.prev_gop = self.gop.take();
                self.gop = Some(Gop {
                    rap: entry,
                    corrupt: true,
                });
            }
            let e = error(DecodeErrorKind::InvalidData, "AU corrompue");
            if self.deferred {
                self.pending_error = Some(e);
                return Ok(());
            }
            return Err(e);
        }
        self.accept(entry);
        Ok(())
    }

    fn send_eof(&mut self) -> Result<(), DecodeError> {
        self.check_failed()?;
        self.eof = true;
        Ok(())
    }

    fn receive(&mut self, want: &dyn Fn(i64) -> Option<Viewport>) -> Result<Received, DecodeError> {
        self.check_failed()?;
        self.receives = self.receives.saturating_add(1);
        if self.hw && self.fail_after.is_some_and(|n| self.receives > n) {
            self.failed = true;
            self.check_failed()?;
        }
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        let delay = (self.reorder + self.latency) as usize;
        let ready = self.buffer.len() > delay || (self.eof && !self.buffer.is_empty());
        if !ready {
            return Ok(if self.eof {
                Received::Drained
            } else {
                Received::NeedInput
            });
        }
        let Some((pts, p)) = self.buffer.pop_first() else {
            return Ok(Received::Drained);
        };
        self.mode = if self.hw {
            DecodeMode::Hardware
        } else {
            DecodeMode::Software
        };
        let Some(vp) = want(pts) else {
            return Ok(Received::Discarded { pts });
        };
        let vp = vp
            .normalized(self.size)
            .map_err(|e| error(DecodeErrorKind::Unsupported, &format!("viewport : {e}")))?;
        self.stats.converted += 1;
        let full = Rect {
            x: 0,
            y: 0,
            w: self.size.w,
            h: self.size.h,
        };
        if self.hw && vp.region == full && !vp.is_reduced() {
            self.stats.full_res_transfers += 1;
        }
        Ok(Received::Frame {
            pts,
            image: FakeDecoder::render(p.pres_idx, vp.out),
            corrupt: p.corrupt,
            full: self.size,
        })
    }

    fn reset(&mut self) {
        self.eof = false;
        self.buffer.clear();
        self.gop = None;
        self.prev_gop = None;
        self.pending_error = None;
    }

    fn stats(&self) -> DecoderStats {
        self.stats
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
        assert_eq!(r.entry(5).unwrap().rap, RapKind::Recovery { pres_span: 2 });
        let m = FakeSource::starting_mid_gop(2, 4, 0, 3);
        assert_eq!(m.entry(2).unwrap().rap, RapKind::None);
        assert_eq!(m.entry(3).unwrap().rap, RapKind::Idr);
        assert_eq!(m.len(), 11);
    }

    /// Décode `from..to` (ordre de décodage) après un `reset`, vide, et rend les images sorties :
    /// (`pres_idx` lu par l'oracle, `corrupt`).
    fn decode_range(
        dec: &mut dyn Decoder,
        src: &FakeSource,
        from: u32,
        to: u32,
    ) -> Vec<(u32, bool)> {
        let vp = Viewport::fit(FAKE_SIZE, Size { w: 32, h: 18 });
        let mut out = Vec::new();
        let mut collect = |dec: &mut dyn Decoder| loop {
            match dec.receive(&|_| Some(vp)).unwrap() {
                Received::Frame {
                    image,
                    corrupt,
                    full,
                    ..
                } => {
                    assert_eq!(full, FAKE_SIZE);
                    out.push((oracle(&image).unwrap(), corrupt));
                }
                Received::NeedInput | Received::Drained => break,
                Received::Discarded { .. } => unreachable!(),
            }
        };
        dec.reset();
        let mut buf = Vec::new();
        for d in from..to {
            src.read_au(d, &mut buf).unwrap();
            let r = dec.send(src.entry(d).unwrap().pts, &buf);
            assert_eq!(r.is_err(), src.is_corrupt(d));
            collect(dec);
        }
        dec.send_eof().unwrap();
        collect(dec);
        out
    }

    fn open(src: &FakeSource, pref: ModePref) -> Box<dyn Decoder> {
        FakeFactory::new(src).open(src.codec(), pref).unwrap()
    }

    fn pres(out: &[(u32, bool)]) -> Vec<u32> {
        out.iter().map(|(p, _)| *p).collect()
    }

    #[test]
    fn decoder_outputs_in_pts_order() {
        let s = FakeSource::closed_gops(2, 8, 2);
        let mut dec = open(&s, ModePref::Auto);
        assert_eq!(dec.mode(), DecodeMode::Unknown);
        assert_eq!(dec.latency(), 1);
        let out = decode_range(&mut *dec, &s, 0, 16);
        assert_eq!(pres(&out), (0..16).collect::<Vec<_>>());
        assert_eq!(dec.mode(), DecodeMode::Hardware);
        assert_eq!(dec.stats().converted, 16);
        assert_eq!(dec.stats().full_res_transfers, 0);
        assert_eq!(reorder_depth(&s), 1);
        assert_eq!(reorder_depth(&FakeSource::recovery(2, 4, 1)), 0);
    }

    #[test]
    fn decoder_drops_pictures_without_references() {
        // RASL du CRA 8 (images 8 et 9) : émises seulement si le CRA 0 a été reçu.
        let s = FakeSource::open_gops(3, 8, 2);
        let mut dec = open(&s, ModePref::SoftwareOnly);
        assert_eq!(
            pres(&decode_range(&mut *dec, &s, 8, 16)),
            (10..16).collect::<Vec<_>>()
        );
        assert_eq!(
            pres(&decode_range(&mut *dec, &s, 0, 16)),
            (2..16).collect::<Vec<_>>()
        );
        assert_eq!(dec.mode(), DecodeMode::Software);
        // Images avant le premier RAP reçu.
        let m = FakeSource::starting_mid_gop(1, 4, 0, 3);
        let mut dec = open(&m, ModePref::Auto);
        assert_eq!(pres(&decode_range(&mut *dec, &m, 0, 7)), [3, 4, 5, 6]);
        // Intervalle de reprise d'un recovery point sans RAP précédent.
        let r = FakeSource::recovery(3, 10, 3);
        let mut dec = open(&r, ModePref::Auto);
        let out = pres(&decode_range(&mut *dec, &r, 10, 20));
        assert_eq!(out, [10, 13, 14, 15, 16, 17, 18, 19]);
        assert_eq!(
            pres(&decode_range(&mut *dec, &r, 0, 14)),
            (0..14).collect::<Vec<_>>()
        );
    }

    #[test]
    fn corruption_propagates_within_gop_and_to_leading_pictures() {
        // AU 5 corrompue dans le GOP 0 ; le RAP du GOP 2 (décodage 16) aussi.
        let s = FakeSource::open_gops(3, 8, 2).with_corrupt(HashSet::from([5, 16]));
        let mut dec = open(&s, ModePref::Auto);
        let out = decode_range(&mut *dec, &s, 0, 24);
        let corrupt: Vec<u32> = out.iter().filter(|(_, c)| *c).map(|(p, _)| *p).collect();
        let pres_of = |d: u32| s.entry(d).unwrap().pres_idx;
        let gop0_after: Vec<u32> = (6..8).map(pres_of).collect();
        // Images du GOP 0 après l'AU 5, RASL du GOP 1 (8 et 9), images du GOP 2 après son RAP.
        let mut expected: Vec<u32> = gop0_after;
        expected.extend([8, 9]);
        expected.extend((17..24).map(pres_of));
        expected.sort_unstable();
        assert_eq!(corrupt, expected);
        assert!(!pres(&out).contains(&pres_of(5)));
        assert!(!pres(&out).contains(&pres_of(16)));
    }

    #[test]
    fn decoder_rejects_misuse() {
        let s = FakeSource::closed_gops(1, 4, 0);
        let mut dec = open(&s, ModePref::Auto);
        let err = dec.send(0, &[0, 0, 0]).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::InvalidData);
        assert_eq!(
            dec.send(0, &9u32.to_le_bytes()).unwrap_err().kind,
            DecodeErrorKind::InvalidData
        );
        assert_eq!(
            dec.send(1, &0u32.to_le_bytes()).unwrap_err().kind,
            DecodeErrorKind::Internal
        );
        dec.send(0, &0u32.to_le_bytes()).unwrap();
        dec.send_eof().unwrap();
        assert_eq!(
            dec.send(PTS_STEP, &1u32.to_le_bytes()).unwrap_err().kind,
            DecodeErrorKind::Internal
        );
        // Viewport impossible à normaliser : `Unsupported`, qui ne déclenche jamais le repli.
        let empty = Viewport {
            region: Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            out: Size { w: 2, h: 2 },
        };
        assert_eq!(
            dec.receive(&|_| Some(empty)).unwrap_err().kind,
            DecodeErrorKind::Unsupported
        );
        assert_eq!(dec.receive(&|_| None).unwrap(), Received::Drained);
        dec.reset();
        dec.send(0, &0u32.to_le_bytes()).unwrap();
        assert_eq!(dec.receive(&|_| None).unwrap(), Received::NeedInput);
    }

    #[test]
    fn hardware_failure_is_injected_after_receives() {
        let s = FakeSource::closed_gops(1, 8, 0);
        let f = FakeFactory {
            hw_fail_after_receives: Some(2),
            ..FakeFactory::new(&s)
        };
        let mut dec = f.open(Codec::H264, ModePref::Auto).unwrap();
        assert!(dec.receive(&|_| None).is_ok());
        assert!(dec.receive(&|_| None).is_ok());
        assert_eq!(
            dec.receive(&|_| None).unwrap_err().kind,
            DecodeErrorKind::Gpu
        );
        assert_eq!(
            dec.send(0, &0u32.to_le_bytes()).unwrap_err().kind,
            DecodeErrorKind::Gpu
        );
        assert_eq!(dec.send_eof().unwrap_err().kind, DecodeErrorKind::Gpu);
        dec.reset();
        assert_eq!(
            dec.receive(&|_| None).unwrap_err().kind,
            DecodeErrorKind::Gpu
        );
        // Le décodeur logiciel n'est pas concerné.
        let mut sw = f.open(Codec::H264, ModePref::SoftwareOnly).unwrap();
        for _ in 0..5 {
            assert!(sw.receive(&|_| None).is_ok());
        }
        // Sans matériel, `Auto` ouvre un décodeur logiciel.
        let no_hw = FakeFactory {
            hw_available: false,
            ..f
        };
        let mut dec = no_hw.open(Codec::H264, ModePref::Auto).unwrap();
        assert_eq!(decode_range(&mut *dec, &s, 0, 8).len(), 8);
        assert_eq!(dec.mode(), DecodeMode::Software);
    }

    #[test]
    fn full_resolution_transfers_are_counted_in_hardware() {
        let s = FakeSource::closed_gops(1, 2, 0);
        let f = FakeFactory {
            size: Size { w: 64, h: 32 },
            latency: 0,
            ..FakeFactory::new(&s)
        };
        let full = Viewport::fit(f.size, f.size);
        for (pref, transfers) in [(ModePref::Auto, 1), (ModePref::SoftwareOnly, 0)] {
            let mut dec = f.open(Codec::H264, pref).unwrap();
            dec.send(0, &0u32.to_le_bytes()).unwrap();
            let Received::Frame { image, .. } = dec.receive(&|_| Some(full)).unwrap() else {
                panic!("image attendue");
            };
            assert_eq!(image.size, f.size);
            assert_eq!(image.byte_len(), 64 * 32 * 3 / 2);
            assert_eq!(dec.stats().full_res_transfers, transfers);
        }
    }

    #[test]
    fn au_delay_is_simulated() {
        let s = FakeSource::closed_gops(1, 2, 0);
        let f = FakeFactory {
            au_delay: Duration::from_millis(5),
            ..FakeFactory::new(&s)
        };
        let mut dec = f.open(Codec::H264, ModePref::Auto).unwrap();
        let t = std::time::Instant::now();
        dec.send(0, &0u32.to_le_bytes()).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn oracle_needs_four_bytes() {
        let tiny = FakeDecoder::render(7, Size { w: 1, h: 1 });
        assert_eq!(tiny.planes[0].len(), 1);
        assert_eq!(tiny.planes[1].len(), 2);
        assert_eq!(oracle(&tiny), None);
        let odd = FakeDecoder::render(7, Size { w: 3, h: 3 });
        assert_eq!(oracle(&odd), Some(7));
        assert_eq!(odd.strides, [3, 4]);
        assert_eq!(odd.planes[1].len(), 8);
    }
}
