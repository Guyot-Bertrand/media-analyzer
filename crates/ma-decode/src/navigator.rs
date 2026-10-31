//! Navigateur d'images : décodage continu, fenêtres de conversion, cache LRU et tolérance aux AU
//! corrompues.
//!
//! Le `Navigator` sert une image (numéro de présentation, viewport) à la fois. Il poursuit le
//! décodage en cours quand c'est possible (image suivante, saut court en avant), sinon il repart du
//! point d'accès aléatoire donné par [`start_for`]. Pendant le décodage, seules les images d'une
//! fenêtre autour de la cible sont converties ; les autres sont jetées sans conversion.

use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    AuSource, DecodeError, DecodeErrorKind, DecodeEvent, DecodeMode, DecodedFrame, Decoder,
    DecoderFactory, DecoderStats, FrameEntry, ModePref, NotDecodable, RapKind, Received, Rect,
    Size, Viewport, ViewportError, start_for,
};

/// Viewport minimal utilisé pour apprendre la taille de l'image.
const PROBE_VIEWPORT: Viewport = Viewport {
    region: Rect {
        x: 0,
        y: 0,
        w: 2,
        h: 2,
    },
    out: Size { w: 2, h: 2 },
};

/// Réglages du navigateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavConfig {
    /// Capacité du cache, en images.
    pub cache_frames: usize,
    /// Marge de réordonnancement : on poursuit le décodage tant que la prochaine AU ne dépasse pas
    /// la cible de plus de `reorder_margin + latency`.
    pub reorder_margin: u32,
    /// Images converties après la cible.
    pub keep_ahead: u32,
    /// Images converties avant la cible, quand la demande est l'image précédente.
    pub keep_behind: u32,
}

impl Default for NavConfig {
    fn default() -> Self {
        NavConfig {
            cache_frames: 48,
            reorder_margin: 16,
            keep_ahead: 16,
            keep_behind: 24,
        }
    }
}

/// Échec d'une demande d'image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("image {pres_idx} hors du fichier ({len} images)")]
    OutOfRange { pres_idx: u32, len: u32 },
    #[error("image {pres_idx} indécodable : {reason}")]
    NotDecodable { pres_idx: u32, reason: NotDecodable },
    #[error("demande préemptée")]
    Preempted,
    #[error("lecture de la source : {0}")]
    Io(String),
    #[error(transparent)]
    Decoder(DecodeError),
    #[error("viewport invalide : {0}")]
    Viewport(ViewportError),
}

/// Compteurs du navigateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NavStats {
    /// AU envoyées au décodeur.
    pub aus_sent: u64,
    /// Demandes servies par le cache.
    pub cache_hits: u64,
    /// Compteurs du décodeur courant.
    pub decoder: DecoderStats,
}

type Key = (u32, Viewport);

/// Cache LRU des images converties, indexé par (image, viewport demandé).
struct Cache {
    cap: usize,
    tick: u64,
    map: HashMap<Key, (Arc<DecodedFrame>, u64)>,
}

impl Cache {
    fn get(&mut self, key: &Key) -> Option<Arc<DecodedFrame>> {
        self.tick += 1;
        let (frame, used) = self.map.get_mut(key)?;
        *used = self.tick;
        Some(Arc::clone(frame))
    }

    fn contains(&self, key: &Key) -> bool {
        self.map.contains_key(key)
    }

    fn insert(&mut self, key: Key, frame: Arc<DecodedFrame>) {
        if self.cap == 0 {
            return;
        }
        self.tick += 1;
        self.map.insert(key, (frame, self.tick));
        while self.map.len() > self.cap {
            let oldest = self.map.iter().min_by_key(|(_, (_, used))| *used);
            let Some(oldest) = oldest.map(|(k, _)| *k) else {
                break;
            };
            self.map.remove(&oldest);
        }
    }
}

/// Décodage en cours depuis le dernier `reset`.
#[derive(Debug, Clone)]
struct Session {
    /// RAP de départ (`decode_idx`).
    start: u32,
    /// Prochaine AU à envoyer.
    next: u32,
    eof_sent: bool,
    drained: bool,
    /// `pts` de la dernière image sortie du décodeur.
    last_out: Option<i64>,
    /// AU corrompues envoyées (`decode_idx` → erreur).
    corrupt: HashMap<u32, DecodeError>,
}

impl Session {
    fn new(start: u32) -> Session {
        Session {
            start,
            next: start,
            eof_sent: false,
            drained: false,
            last_out: None,
            corrupt: HashMap::new(),
        }
    }
}

/// Issue d'un essai de décodage.
enum Outcome {
    Found(Arc<DecodedFrame>),
    /// La cible n'est pas sortie ; `corrupt` porte l'erreur de son AU si elle était corrompue.
    Missing {
        corrupt: Option<DecodeError>,
    },
}

/// Navigateur d'images sur une source d'AU.
pub struct Navigator {
    src: Arc<dyn AuSource>,
    #[expect(
        dead_code,
        reason = "réouverture du décodeur lors du repli logiciel (Tâche 5)"
    )]
    factory: Arc<dyn DecoderFactory>,
    #[expect(
        dead_code,
        reason = "réouverture du décodeur lors du repli logiciel (Tâche 5)"
    )]
    pref: ModePref,
    cfg: NavConfig,
    decoder: Box<dyn Decoder>,
    session: Option<Session>,
    cache: Cache,
    last_request: Option<u32>,
    size: Option<Size>,
    events: Vec<DecodeEvent>,
    aus_sent: u64,
    cache_hits: u64,
    buf: Vec<u8>,
}

impl Navigator {
    /// Ouvre un décodeur pour le codec de la source.
    pub fn new(
        src: Arc<dyn AuSource>,
        factory: Arc<dyn DecoderFactory>,
        pref: ModePref,
        cfg: NavConfig,
    ) -> Result<Self, DecodeError> {
        let decoder = factory.open(src.codec(), pref)?;
        Ok(Navigator {
            src,
            factory,
            pref,
            cfg,
            decoder,
            session: None,
            cache: Cache {
                cap: cfg.cache_frames,
                tick: 0,
                map: HashMap::new(),
            },
            last_request: None,
            size: None,
            events: Vec::new(),
            aus_sent: 0,
            cache_hits: 0,
            buf: Vec::new(),
        })
    }

    /// Image `pres_idx` (ordre de présentation) pour le viewport `vp`.
    ///
    /// Si la taille de l'image est inconnue, elle est d'abord apprise par [`Self::probe_size`].
    /// Le viewport est ensuite validé contre cette taille, avant tout appel au décodeur ; l'image
    /// rendue porte le viewport demandé. `cancel` est consulté entre deux AU : s'il renvoie vrai,
    /// la demande échoue en [`FrameError::Preempted`] et le décodage en cours reste réutilisable.
    pub fn frame(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        let len = self.src.len();
        if pres_idx >= len {
            return Err(FrameError::OutOfRange { pres_idx, len });
        }
        let size = self.probe_size(cancel).map_err(|e| match e {
            // Aucune image décodable dans le fichier : celle-ci non plus.
            FrameError::NotDecodable { reason, .. } => {
                FrameError::NotDecodable { pres_idx, reason }
            }
            e => e,
        })?;
        vp.normalized(size).map_err(FrameError::Viewport)?;
        let frame = self.fetch(pres_idx, vp, cancel)?;
        self.last_request = Some(pres_idx);
        Ok(frame)
    }

    /// Taille d'affichage des images ; si elle est inconnue, décode la première image décodable
    /// avec un viewport minimal.
    pub fn probe_size(&mut self, cancel: &dyn Fn() -> bool) -> Result<Size, FrameError> {
        if let Some(size) = self.size {
            return Ok(size);
        }
        let pres_idx = self.first_decodable()?;
        self.fetch(pres_idx, PROBE_VIEWPORT, cancel)?;
        self.size.ok_or_else(|| {
            FrameError::Decoder(DecodeError {
                kind: DecodeErrorKind::Internal,
                detail: "taille d'image inconnue après décodage".into(),
            })
        })
    }

    /// Vrai si l'image est en cache pour ce viewport (sans la marquer comme utilisée).
    #[must_use]
    pub fn is_cached(&self, pres_idx: u32, vp: &Viewport) -> bool {
        self.cache.contains(&(pres_idx, *vp))
    }

    /// Mode effectif du décodeur courant.
    #[must_use]
    pub fn mode(&self) -> DecodeMode {
        self.decoder.mode()
    }

    /// Taille d'affichage, connue après la première image reçue.
    #[must_use]
    pub fn frame_size(&self) -> Option<Size> {
        self.size
    }

    /// Événements émis depuis le dernier appel.
    pub fn take_events(&mut self) -> Vec<DecodeEvent> {
        std::mem::take(&mut self.events)
    }

    #[must_use]
    pub fn stats(&self) -> NavStats {
        NavStats {
            aus_sent: self.aus_sent,
            cache_hits: self.cache_hits,
            decoder: self.decoder.stats(),
        }
    }

    fn entry_of(&self, pres_idx: u32) -> Option<FrameEntry> {
        self.src
            .decode_idx_of(pres_idx)
            .and_then(|d| self.src.entry(d))
    }

    /// Image de présentation de la première AU décodable (en ordre de décodage).
    fn first_decodable(&self) -> Result<u32, FrameError> {
        let len = self.src.len();
        let not_decodable = |reason| FrameError::NotDecodable {
            pres_idx: 0,
            reason,
        };
        if len == 0 {
            return Err(FrameError::OutOfRange { pres_idx: 0, len });
        }
        let first_rap = (0..len)
            .find(|&d| self.src.entry(d).is_some_and(|e| e.rap != RapKind::None))
            .ok_or(not_decodable(NotDecodable::BeforeFirstRap))?;
        (first_rap..len)
            .find_map(|d| {
                start_for(&*self.src, d).ok()?;
                self.src.entry(d).map(|e| e.pres_idx)
            })
            .ok_or(not_decodable(NotDecodable::LeadingOfFirstRap))
    }

    /// `pts` → image des images à convertir : fenêtre autour de la cible, hors images déjà en
    /// cache (la cible y est toujours).
    fn window(&self, target: u32, vp: Viewport) -> HashMap<i64, u32> {
        let last = self.src.len().saturating_sub(1);
        let backward = self
            .last_request
            .is_some_and(|last| target.checked_add(1) == Some(last));
        let (lo, hi) = if backward {
            (target.saturating_sub(self.cfg.keep_behind), target)
        } else {
            (target, target.saturating_add(self.cfg.keep_ahead).min(last))
        };
        (lo..=hi)
            .filter(|&p| p == target || !self.cache.contains(&(p, vp)))
            .filter_map(|p| self.entry_of(p).map(|e| (e.pts, p)))
            .collect()
    }

    /// Image demandée, depuis le cache ou le décodeur, sans validation du viewport.
    fn fetch(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        if let Some(frame) = self.cache.get(&(pres_idx, vp)) {
            self.cache_hits += 1;
            return Ok(frame);
        }
        let len = self.src.len();
        let target = self
            .entry_of(pres_idx)
            .ok_or(FrameError::OutOfRange { pres_idx, len })?;
        let start = start_for(&*self.src, target.decode_idx)
            .map_err(|reason| FrameError::NotDecodable { pres_idx, reason })?;
        let wanted = self.window(pres_idx, vp);
        for retry in [false, true] {
            match self.attempt(&target, start, vp, &wanted, retry, cancel)? {
                Outcome::Found(frame) => return Ok(frame),
                Outcome::Missing { corrupt: Some(e) } => return Err(FrameError::Decoder(e)),
                Outcome::Missing { corrupt: None } => {}
            }
        }
        Err(FrameError::Decoder(DecodeError {
            kind: DecodeErrorKind::InvalidData,
            detail: format!("image {pres_idx} absente de la sortie du décodeur"),
        }))
    }

    /// Un essai : poursuit le décodage en cours si possible (jamais si `restart`), sinon repart de
    /// `start`. Une erreur du décodeur autre que `InvalidData` abandonne le décodage en cours.
    fn attempt(
        &mut self,
        target: &FrameEntry,
        start: u32,
        vp: Viewport,
        wanted: &HashMap<i64, u32>,
        restart: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Outcome, FrameError> {
        let limit = target
            .decode_idx
            .saturating_add(self.cfg.reorder_margin)
            .saturating_add(self.decoder.latency());
        let reusable = |s: &Session| {
            s.start <= start
                && (start..=limit).contains(&s.next)
                && !s.drained
                && s.last_out.is_none_or(|pts| pts < target.pts)
        };
        let mut session = match self.session.take() {
            Some(s) if !restart && reusable(&s) => s,
            _ => {
                self.decoder.reset();
                Session::new(start)
            }
        };
        let r = self.run(&mut session, target, limit, vp, wanted, cancel);
        if !matches!(r, Err(FrameError::Decoder(_))) {
            self.session = Some(session);
        }
        r
    }

    fn run(
        &mut self,
        s: &mut Session,
        target: &FrameEntry,
        limit: u32,
        vp: Viewport,
        wanted: &HashMap<i64, u32>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Outcome, FrameError> {
        let want = |pts: i64| wanted.contains_key(&pts).then_some(vp);
        let len = self.src.len();
        let missing = |s: &Session| Outcome::Missing {
            corrupt: s.corrupt.get(&target.decode_idx).cloned(),
        };
        loop {
            loop {
                let pts = match self.decoder.receive(&want).map_err(FrameError::Decoder)? {
                    Received::NeedInput => break,
                    Received::Drained => {
                        s.drained = true;
                        break;
                    }
                    Received::Discarded { pts } => pts,
                    Received::Frame {
                        pts,
                        image,
                        corrupt,
                        full,
                    } => {
                        self.size.get_or_insert(full);
                        if let Some(&pres_idx) = wanted.get(&pts) {
                            let frame = Arc::new(DecodedFrame {
                                pres_idx,
                                viewport: vp,
                                image,
                                corrupt,
                                full,
                                blocks: None,
                            });
                            self.cache.insert((pres_idx, vp), Arc::clone(&frame));
                            if pres_idx == target.pres_idx {
                                s.last_out = Some(pts);
                                return Ok(Outcome::Found(frame));
                            }
                        }
                        pts
                    }
                };
                s.last_out = Some(pts);
                if pts > target.pts {
                    // Sortie par pts croissant : la cible a été sautée.
                    return Ok(missing(s));
                }
            }
            if s.drained {
                return Ok(missing(s));
            }
            if s.next <= limit && s.next < len {
                if cancel() {
                    return Err(FrameError::Preempted);
                }
                let d = s.next;
                let entry = self
                    .src
                    .entry(d)
                    .ok_or_else(|| FrameError::Io(format!("entrée {d} absente de l'index")))?;
                self.src
                    .read_au(d, &mut self.buf)
                    .map_err(|e| FrameError::Io(e.to_string()))?;
                self.aus_sent += 1;
                s.next += 1;
                match self.decoder.send(entry.pts, &self.buf) {
                    Ok(()) => {}
                    Err(e) if e.kind == DecodeErrorKind::InvalidData => {
                        self.events.push(DecodeEvent::CorruptAu {
                            decode_idx: d,
                            detail: e.detail.clone(),
                        });
                        s.corrupt.insert(d, e);
                    }
                    Err(e) => return Err(FrameError::Decoder(e)),
                }
            } else if !s.eof_sent {
                self.decoder.send_eof().map_err(FrameError::Decoder)?;
                s.eof_sent = true;
            } else {
                // Plus rien à envoyer et le décodeur réclame encore des données.
                s.drained = true;
                return Ok(missing(s));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::io;

    use super::*;
    use crate::fake::{FAKE_SIZE, FakeFactory, FakeSource, oracle};
    use crate::{DecodeErrorKind, FrameEntry, Rect};

    /// Petit viewport réduit : images légères, oracle lisible.
    fn small() -> Viewport {
        Viewport::fit(FAKE_SIZE, Size { w: 64, h: 36 })
    }

    fn nav_with(factory: FakeFactory) -> Navigator {
        let src = Arc::new(factory.source.clone());
        Navigator::new(src, Arc::new(factory), ModePref::Auto, NavConfig::default()).unwrap()
    }

    fn nav(src: &FakeSource) -> Navigator {
        nav_with(FakeFactory::new(src))
    }

    fn never() -> bool {
        false
    }

    fn get(nav: &mut Navigator, pres: u32) -> Arc<DecodedFrame> {
        let f = nav.frame(pres, small(), &never).unwrap();
        assert_eq!(f.pres_idx, pres);
        assert_eq!(oracle(&f.image), Some(pres), "oracle de l'image {pres}");
        f
    }

    /// Générateur pseudo-aléatoire déterministe (LCG).
    struct Lcg(u64);
    impl Lcg {
        fn below(&mut self, n: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            u32::try_from((self.0 >> 33) % u64::from(n)).unwrap()
        }
    }

    #[ma_trace::covers("NAV.ORACLE.01")]
    #[test]
    fn every_requested_frame_is_the_one_returned() {
        for src in [
            FakeSource::closed_gops(20, 24, 2),
            FakeSource::open_gops(20, 24, 2),
        ] {
            let mut nav = nav(&src);
            let mut rng = Lcg(0x5eed);
            // Les deux premières images d'un flux ouvert (RASL du premier CRA) sont indécodables.
            let first = 2;
            let mut at = first;
            for _ in 0..50 {
                at = first + rng.below(src.len() - first);
                assert!(!get(&mut nav, at).corrupt);
            }
            let at = at.clamp(first + 20, src.len() - 21);
            for p in at..=at + 20 {
                get(&mut nav, p);
            }
            for p in (at..at + 20).rev() {
                get(&mut nav, p);
            }
            assert_eq!(nav.take_events(), []);
        }
    }

    #[test]
    fn sequential_step_continues_decoding() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let mut nav = nav(&src);
        get(&mut nav, 30);
        let before = nav.stats().aus_sent;
        for p in 31..=40 {
            get(&mut nav, p);
        }
        let sent = nav.stats().aus_sent - before;
        assert!(
            sent <= 10 + 4,
            "{sent} AU envoyées pour 10 images suivantes"
        );
    }

    #[test]
    fn step_across_gop_does_not_restart() {
        // GOP fermé (IDR en 24) et GOP ouvert (CRA en 26, RASL 24 et 25 dépendant du GOP 0).
        for src in [
            FakeSource::closed_gops(4, 24, 2),
            FakeSource::open_gops(4, 24, 2),
        ] {
            let mut nav = nav(&src);
            get(&mut nav, 18);
            let before = nav.stats().aus_sent;
            for p in 19..=32 {
                get(&mut nav, p);
            }
            let sent = nav.stats().aus_sent - before;
            assert!(
                sent <= 14 + 4,
                "{sent} AU envoyées pour 14 images suivantes"
            );
        }
    }

    #[test]
    fn backward_step_converts_window_behind() {
        let src = FakeSource::closed_gops(3, 60, 2);
        let mut nav = nav(&src);
        get(&mut nav, 100);
        get(&mut nav, 99);
        let cfg = NavConfig::default();
        for p in 99 - cfg.keep_behind..=99 {
            assert!(nav.is_cached(p, &small()), "image {p} absente du cache");
        }
        let (sent, hits) = (nav.stats().aus_sent, nav.stats().cache_hits);
        for p in (99 - cfg.keep_behind..99).rev() {
            get(&mut nav, p);
        }
        assert_eq!(nav.stats().aus_sent, sent);
        assert_eq!(nav.stats().cache_hits, hits + u64::from(cfg.keep_behind));
    }

    #[test]
    fn discarded_frames_are_not_converted() {
        let src = FakeSource::closed_gops(4, 60, 2);
        let mut nav = nav(&src);
        nav.probe_size(&never).unwrap();
        let before = nav.stats().decoder.converted;
        get(&mut nav, 150);
        let converted = nav.stats().decoder.converted - before;
        let window = u64::from(NavConfig::default().keep_ahead) + 1;
        assert!(converted <= window + 1, "{converted} images converties");
        assert!(!nav.is_cached(149, &small()));
        assert!(!nav.is_cached(120, &small()));
    }

    #[test]
    fn cache_is_keyed_by_viewport() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let mut nav = nav(&src);
        let crop = Viewport::region_1to1(
            FAKE_SIZE,
            Rect {
                x: 100,
                y: 50,
                w: 32,
                h: 16,
            },
        );
        let a = get(&mut nav, 10);
        assert!(!nav.is_cached(10, &crop));
        let b = nav.frame(10, crop, &never).unwrap();
        assert_eq!(oracle(&b.image), Some(10));
        assert_eq!(b.viewport, crop);
        assert_eq!(b.image.size, Size { w: 32, h: 16 });
        assert_ne!(a.image.size, b.image.size);
        assert!(nav.is_cached(10, &small()) && nav.is_cached(10, &crop));
        let hits = nav.stats().cache_hits;
        let again = get(&mut nav, 10);
        assert!(Arc::ptr_eq(&a, &again));
        assert_eq!(nav.stats().cache_hits, hits + 1);
    }

    #[test]
    fn last_frame_is_served_after_drain() {
        for src in [
            FakeSource::closed_gops(2, 24, 2),
            FakeSource::open_gops(2, 24, 2),
        ] {
            let last = src.len() - 1;
            // Saut direct sur la dernière image.
            let mut nav = nav(&src);
            get(&mut nav, last);
            // Pas à pas jusqu'à la dernière image.
            let mut nav = self::nav(&src);
            for p in last - 6..=last {
                get(&mut nav, p);
            }
            // Retour en arrière depuis la fin.
            for p in (last - 6..last).rev() {
                get(&mut nav, p);
            }
        }
    }

    #[test]
    fn corrupt_au_does_not_fail_whole_gop() {
        // GOP 1 = images 24 à 47 (sans B : décodage = présentation) ; AU 30 corrompue.
        let src = FakeSource::closed_gops(3, 24, 0).with_corrupt(HashSet::from([30]));
        let mut nav = nav(&src);
        for p in 24..30 {
            assert!(!get(&mut nav, p).corrupt, "image {p}");
        }
        let err = nav.frame(30, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        for p in 31..48 {
            assert!(get(&mut nav, p).corrupt, "image {p}");
        }
        assert!(!get(&mut nav, 48).corrupt);
        let events = nav.take_events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            events[0],
            DecodeEvent::CorruptAu { decode_idx: 30, .. }
        ));
        assert_eq!(nav.take_events(), []);
    }

    #[test]
    fn cancel_returns_preempted_and_resumes() {
        let src = FakeSource::closed_gops(4, 24, 2);
        let mut reference = nav(&src);
        reference.probe_size(&never).unwrap();
        get(&mut reference, 60);
        let expected = reference.stats().aus_sent;

        let mut nav = nav(&src);
        nav.probe_size(&never).unwrap();
        let calls = Cell::new(0);
        let after_three = || {
            calls.set(calls.get() + 1);
            calls.get() > 3
        };
        let before = nav.stats().aus_sent;
        assert_eq!(
            nav.frame(60, small(), &after_three),
            Err(FrameError::Preempted)
        );
        assert_eq!(nav.stats().aus_sent, before + 3);
        get(&mut nav, 60);
        assert_eq!(nav.stats().aus_sent, expected);
    }

    #[test]
    fn frame_size_is_learnt_and_probed() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let mut nav = nav_with(FakeFactory {
            size: Size { w: 640, h: 360 },
            ..FakeFactory::new(&src)
        });
        assert_eq!(nav.frame_size(), None);
        assert_eq!(nav.mode(), DecodeMode::Unknown);
        assert_eq!(nav.probe_size(&never), Ok(Size { w: 640, h: 360 }));
        assert_eq!(nav.frame_size(), Some(Size { w: 640, h: 360 }));
        assert_eq!(nav.mode(), DecodeMode::Hardware);
        let sent = nav.stats().aus_sent;
        assert_eq!(nav.probe_size(&never), Ok(Size { w: 640, h: 360 }));
        assert_eq!(nav.stats().aus_sent, sent);
        let f = nav.frame(5, small(), &never).unwrap();
        assert_eq!(f.full, Size { w: 640, h: 360 });
        assert_eq!(f.viewport, small());

        // Sans sondage préalable, la première demande apprend la taille.
        let mut nav = self::nav(&src);
        get(&mut nav, 30);
        assert_eq!(nav.frame_size(), Some(FAKE_SIZE));
    }

    #[test]
    fn probe_skips_undecodable_head() {
        let src = FakeSource::starting_mid_gop(2, 24, 2, 5);
        let mut nav = nav(&src);
        assert_eq!(nav.probe_size(&never), Ok(FAKE_SIZE));
        let empty = FakeSource::closed_gops(0, 24, 2);
        assert_eq!(
            self::nav(&empty).probe_size(&never),
            Err(FrameError::OutOfRange {
                pres_idx: 0,
                len: 0
            })
        );
        let no_rap = FakeSource::starting_mid_gop(0, 24, 2, 5);
        assert!(matches!(
            self::nav(&no_rap).probe_size(&never),
            Err(FrameError::NotDecodable {
                reason: NotDecodable::BeforeFirstRap,
                ..
            })
        ));
        let entry = |rap| FrameEntry {
            decode_idx: 0,
            pres_idx: 0,
            pts: 0,
            picture: crate::PictureType::I,
            rap,
            leading: crate::Leading::None,
        };
        let recovery_only = FakeSource::from_entries(
            crate::Codec::H264,
            vec![entry(crate::RapKind::Recovery { frame_cnt: 4 })],
        );
        assert!(matches!(
            self::nav(&recovery_only).probe_size(&never),
            Err(FrameError::NotDecodable {
                reason: NotDecodable::LeadingOfFirstRap,
                ..
            })
        ));
    }

    #[test]
    fn invalid_requests_are_errors_not_panics() {
        let src = FakeSource::open_gops(2, 24, 2);
        let mut nav = nav(&src);
        assert_eq!(
            nav.frame(48, small(), &never),
            Err(FrameError::OutOfRange {
                pres_idx: 48,
                len: 48
            })
        );
        assert_eq!(
            nav.frame(u32::MAX, small(), &never),
            Err(FrameError::OutOfRange {
                pres_idx: u32::MAX,
                len: 48
            })
        );
        assert_eq!(
            nav.frame(1, small(), &never),
            Err(FrameError::NotDecodable {
                pres_idx: 1,
                reason: NotDecodable::LeadingOfFirstRap
            })
        );
        nav.probe_size(&never).unwrap();
        let sent = nav.stats().aus_sent;
        let outside = Viewport {
            region: Rect {
                x: 5000,
                y: 0,
                w: 10,
                h: 10,
            },
            out: Size { w: 10, h: 10 },
        };
        assert_eq!(
            nav.frame(10, outside, &never),
            Err(FrameError::Viewport(ViewportError::Empty))
        );
        let empty_out = Viewport {
            out: Size { w: 0, h: 0 },
            ..small()
        };
        assert_eq!(
            nav.frame(10, empty_out, &never),
            Err(FrameError::Viewport(ViewportError::Empty))
        );
        assert_eq!(nav.stats().aus_sent, sent);
        // Après une erreur, la navigation continue.
        get(&mut nav, 10);
    }

    #[test]
    fn huge_config_values_do_not_panic() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let cfg = NavConfig {
            cache_frames: 0,
            reorder_margin: u32::MAX,
            keep_ahead: u32::MAX,
            keep_behind: u32::MAX,
        };
        let f = FakeFactory::new(&src);
        let mut nav = Navigator::new(Arc::new(src), Arc::new(f), ModePref::Auto, cfg).unwrap();
        get(&mut nav, 40);
        get(&mut nav, 39);
        assert!(!nav.is_cached(39, &small()));
    }

    #[test]
    fn decoder_error_is_reported_and_navigation_restarts() {
        let src = FakeSource::closed_gops(4, 24, 2);
        let mut nav = nav_with(FakeFactory {
            hw_fail_after_receives: Some(5),
            ..FakeFactory::new(&src)
        });
        let err = nav.frame(50, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::Gpu),
            "{err:?}"
        );
        assert!(matches!(
            nav.frame(50, small(), &never),
            Err(FrameError::Decoder(_))
        ));
        assert_eq!(err.to_string(), format!("{err}"));
    }

    #[test]
    fn navigator_can_move_to_a_worker_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Navigator>();
    }

    #[test]
    fn no_decodable_frame_reports_the_requested_one() {
        let no_rap = FakeSource::starting_mid_gop(0, 24, 2, 5);
        assert_eq!(
            nav(&no_rap).frame(3, small(), &never),
            Err(FrameError::NotDecodable {
                pres_idx: 3,
                reason: NotDecodable::BeforeFirstRap
            })
        );
    }

    #[test]
    fn unsupported_codec_fails_at_open() {
        let src = FakeSource::closed_gops(1, 4, 0);
        let other = FakeSource::open_gops(1, 4, 0);
        let r = Navigator::new(
            Arc::new(src),
            Arc::new(FakeFactory::new(&other)),
            ModePref::SoftwareOnly,
            NavConfig::default(),
        );
        assert!(matches!(r, Err(e) if e.kind == DecodeErrorKind::Unsupported));
    }

    /// Source qui ment : une AU illisible, ou une image présentée à tort comme un IDR.
    struct Patched {
        inner: FakeSource,
        fail_read: Option<u32>,
        fake_idr: Option<u32>,
    }

    impl AuSource for Patched {
        fn codec(&self) -> crate::Codec {
            self.inner.codec()
        }
        fn len(&self) -> u32 {
            self.inner.len()
        }
        fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
            let mut e = self.inner.entry(decode_idx)?;
            if self.fake_idr == Some(decode_idx) {
                e.rap = crate::RapKind::Idr;
            }
            Some(e)
        }
        fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
            self.inner.decode_idx_of(pres_idx)
        }
        fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
            if self.fail_read == Some(decode_idx) {
                return Err(io::Error::other("lecture impossible"));
            }
            self.inner.read_au(decode_idx, buf)
        }
    }

    fn patched(src: &FakeSource, fail_read: Option<u32>, fake_idr: Option<u32>) -> Navigator {
        let inner = src.clone();
        let patched = Patched {
            inner,
            fail_read,
            fake_idr,
        };
        Navigator::new(
            Arc::new(patched),
            Arc::new(FakeFactory::new(src)),
            ModePref::Auto,
            NavConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn io_error_is_reported() {
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = patched(&src, Some(3), None);
        assert_eq!(
            nav.frame(10, small(), &never),
            Err(FrameError::Io("lecture impossible".into()))
        );
        get(&mut nav, 30);
    }

    #[test]
    fn missing_target_is_retried_once_then_reported() {
        // L'index annonce un IDR en 5 ; pour le décodeur c'est une P sans référence, jamais émise.
        let src = FakeSource::closed_gops(1, 8, 0);
        let mut nav = patched(&src, None, Some(5));
        nav.probe_size(&never).unwrap();
        let sent = nav.stats().aus_sent;
        let err = nav.frame(5, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        // Deux essais depuis l'image 5 : AU 5, 6 et 7, deux fois.
        assert_eq!(nav.stats().aus_sent - sent, 6);
        get(&mut nav, 4);
    }
}
