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

/// Côté minimal de la sortie du viewport de sondage : le traitement vidéo VA-API (VPP) refuse les
/// sorties de moins de 16 pixels (« Hardware does not support scaling to size 2x2 »), ce qui,
/// avec 2×2, faisait repasser en logiciel dès le sondage. Le décodeur borne le viewport à la
/// taille de l'image si elle est plus petite.
const PROBE_SIDE: u32 = 16;

/// Viewport minimal utilisé pour apprendre la taille de l'image.
const PROBE_VIEWPORT: Viewport = Viewport {
    region: Rect {
        x: 0,
        y: 0,
        w: PROBE_SIDE,
        h: PROBE_SIDE,
    },
    out: Size {
        w: PROBE_SIDE,
        h: PROBE_SIDE,
    },
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
    ///
    /// Ne s'applique qu'aux images qui sortent du décodeur pendant l'appel (vidage compris) : le
    /// navigateur s'arrête dès la cible obtenue, pour un pas aussi court que possible. Le
    /// préchargement des images suivantes relève de `FrameService` (Tâche 6).
    pub keep_ahead: u32,
    /// Images converties avant la cible, pour une demande en arrière (image précédente de la
    /// dernière demande de premier plan, ou préchargement [`Direction::Backward`]).
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

/// Sens de navigation : choisit la fenêtre d'images converties autour de la cible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Fenêtre `[cible, cible + keep_ahead]`.
    Forward,
    /// Fenêtre `[cible - keep_behind, cible]`.
    Backward,
}

/// Plus grand nombre de RAP essayés par [`Navigator::probe_size`].
const PROBE_ATTEMPTS: usize = 8;

/// Erreurs `InvalidData` consécutives tolérées de `receive` : au-delà, le décodeur est jugé
/// bloqué et l'erreur remonte (garde contre une boucle sans fin).
const MAX_RECEIVE_ERRORS: u32 = 64;

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
    factory: Arc<dyn DecoderFactory>,
    pref: ModePref,
    cfg: NavConfig,
    decoder: Box<dyn Decoder>,
    /// Vrai une fois le repli logiciel effectué : il n'a lieu qu'une fois par session.
    fell_back: bool,
    session: Option<Session>,
    cache: Cache,
    /// Dernière demande de premier plan ([`Navigator::frame`]).
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
            fell_back: false,
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

    /// Demande de premier plan : image `pres_idx` (ordre de présentation) pour le viewport `vp`.
    ///
    /// Le sens se déduit de la demande de premier plan précédente : `pres_idx` juste avant elle est
    /// un pas en arrière. Si la taille de l'image est connue, le viewport est validé contre elle
    /// avant tout appel au décodeur ; sinon il est transmis tel quel (le décodeur le normalise) et
    /// la taille est apprise sur l'image reçue. L'image rendue porte le viewport demandé.
    /// `cancel` est consulté entre deux AU : s'il renvoie vrai, la demande échoue en
    /// [`FrameError::Preempted`] et le décodage en cours reste réutilisable.
    pub fn frame(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        let backward = self
            .last_request
            .is_some_and(|last| pres_idx.checked_add(1) == Some(last));
        let dir = if backward {
            Direction::Backward
        } else {
            Direction::Forward
        };
        let frame = self.request(pres_idx, vp, dir, cancel)?;
        self.last_request = Some(pres_idx);
        Ok(frame)
    }

    /// Demande de préchargement : comme [`Self::frame`], avec la fenêtre du sens `dir`, sans
    /// modifier la dernière demande de premier plan.
    pub fn prefetch_frame(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        dir: Direction,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        self.request(pres_idx, vp, dir, cancel)
    }

    /// Taille d'affichage des images ; si elle est inconnue, décode avec un viewport minimal la
    /// première image décodable, puis, en cas d'échec (hors préemption et erreur de lecture), les
    /// RAP suivants, dans la limite de 8 essais.
    pub fn probe_size(&mut self, cancel: &dyn Fn() -> bool) -> Result<Size, FrameError> {
        if let Some(size) = self.size {
            return Ok(size);
        }
        let mut last_err = None;
        for pres_idx in self.probe_candidates()? {
            match self.fetch(pres_idx, PROBE_VIEWPORT, Direction::Forward, cancel) {
                Ok(_) => {}
                Err(e @ (FrameError::Preempted | FrameError::Io(_))) => return Err(e),
                Err(e) => last_err = Some(e),
            }
            // La taille a pu être apprise sur une autre image, même en cas d'échec.
            if let Some(size) = self.size {
                return Ok(size);
            }
        }
        Err(last_err.unwrap_or_else(|| {
            FrameError::Decoder(DecodeError {
                kind: DecodeErrorKind::Internal,
                detail: "taille d'image inconnue après décodage".into(),
            })
        }))
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

    /// Images à essayer pour apprendre la taille : la première AU décodable (en ordre de
    /// décodage), puis les RAP décodables suivants, 8 au plus en tout.
    fn probe_candidates(&self) -> Result<Vec<u32>, FrameError> {
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
        let decodable = |d: u32| {
            start_for(&*self.src, d).ok()?;
            self.src.entry(d)
        };
        let first = (first_rap..len)
            .find_map(decodable)
            .ok_or(not_decodable(NotDecodable::LeadingOfFirstRap))?;
        let next_raps = (first.decode_idx + 1..len)
            .filter_map(decodable)
            .filter(|e| e.rap != RapKind::None);
        Ok(std::iter::once(first)
            .chain(next_raps)
            .take(PROBE_ATTEMPTS)
            .map(|e| e.pres_idx)
            .collect())
    }

    /// Demande validée (numéro d'image, viewport), servie par [`Self::fetch`].
    fn request(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        dir: Direction,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        let len = self.src.len();
        if pres_idx >= len {
            return Err(FrameError::OutOfRange { pres_idx, len });
        }
        // Taille inconnue : seuls les viewports vides sont détectables ici.
        let bound = self.size.unwrap_or(Size {
            w: u32::MAX,
            h: u32::MAX,
        });
        vp.normalized(bound).map_err(FrameError::Viewport)?;
        self.fetch(pres_idx, vp, dir, cancel)
    }

    /// `pts` → image des images à convertir : fenêtre autour de la cible, hors images déjà en
    /// cache (la cible y est toujours).
    fn window(&self, target: u32, vp: Viewport, dir: Direction) -> HashMap<i64, u32> {
        let last = self.src.len().saturating_sub(1);
        let (lo, hi) = match dir {
            Direction::Backward => (target.saturating_sub(self.cfg.keep_behind), target),
            Direction::Forward => (target, target.saturating_add(self.cfg.keep_ahead).min(last)),
        };
        (lo..=hi)
            .filter(|&p| p == target || !self.cache.contains(&(p, vp)))
            .filter_map(|p| self.entry_of(p).map(|e| (e.pts, p)))
            .collect()
    }

    /// Image demandée, depuis le cache ou le décodeur, sans validation du viewport.
    ///
    /// Repli logiciel : une erreur `Gpu`, `Device` ou `Internal` d'un décodeur matériel déclenche,
    /// une seule fois par navigateur, la réouverture d'un décodeur logiciel puis un nouvel essai
    /// de la demande depuis son point d'accès aléatoire. `InvalidData` et `Unsupported` ne
    /// déclenchent jamais le repli. Une erreur du décodeur logiciel remonte telle quelle.
    fn fetch(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        dir: Direction,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Arc<DecodedFrame>, FrameError> {
        match self.fetch_once(pres_idx, vp, dir, cancel) {
            Err(FrameError::Decoder(e)) if self.falls_back_on(&e) => {
                if !self.fall_back(pres_idx, &e) {
                    return Err(FrameError::Decoder(e));
                }
                self.fetch_once(pres_idx, vp, dir, cancel)
            }
            r => r,
        }
    }

    /// Vrai si `e` doit faire basculer en logiciel.
    ///
    /// Le décodeur est considéré matériel s'il l'est (`Hardware`) ou si son mode n'est pas encore
    /// connu (`Unknown`, aucune image sortie) alors que le matériel a été demandé
    /// ([`ModePref::Auto`]). Dans ce dernier cas, un décodeur en réalité logiciel (matériel
    /// indisponible) est rouvert en logiciel pour rien : le repli est alors sans effet et l'erreur
    /// du second décodeur remonte.
    fn falls_back_on(&self, e: &DecodeError) -> bool {
        let hardware = match self.decoder.mode() {
            DecodeMode::Hardware => true,
            DecodeMode::Unknown => self.pref == ModePref::Auto,
            DecodeMode::Software => false,
        };
        !self.fell_back
            && hardware
            && matches!(
                e.kind,
                DecodeErrorKind::Gpu | DecodeErrorKind::Device | DecodeErrorKind::Internal
            )
    }

    /// Remplace le décodeur par un décodeur logiciel ; vide le cache et la session. Rend faux si
    /// le décodeur logiciel ne peut pas être ouvert (le décodeur courant est alors conservé).
    fn fall_back(&mut self, pres_idx: u32, cause: &DecodeError) -> bool {
        let Ok(decoder) = self.factory.open(self.src.codec(), ModePref::SoftwareOnly) else {
            return false;
        };
        self.fell_back = true;
        self.decoder = decoder;
        self.session = None;
        self.cache.map.clear();
        self.events.push(DecodeEvent::FellBackToSoftware {
            reason: cause.to_string(),
            at_pres: Some(pres_idx),
        });
        true
    }

    fn fetch_once(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        dir: Direction,
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
        let wanted = self.window(pres_idx, vp, dir);
        // Un seul nouvel essai, depuis le RAP, et seulement si le premier poursuivait un décodage.
        for restart in [false, true] {
            match self.attempt(&target, start, vp, &wanted, restart, cancel)? {
                (Outcome::Found(frame), _) => return Ok(frame),
                (Outcome::Missing { corrupt: Some(e) }, _) => return Err(FrameError::Decoder(e)),
                (Outcome::Missing { corrupt: None }, true) => {}
                (Outcome::Missing { corrupt: None }, false) => break,
            }
        }
        Err(FrameError::Decoder(DecodeError {
            kind: DecodeErrorKind::InvalidData,
            detail: format!("image {pres_idx} absente de la sortie du décodeur"),
        }))
    }

    /// Un essai : poursuit le décodage en cours si possible (jamais si `restart`), sinon repart de
    /// `start`. Rend aussi vrai si le décodage en cours a été poursuivi. Une erreur du décodeur
    /// autre que `InvalidData` abandonne le décodage en cours.
    fn attempt(
        &mut self,
        target: &FrameEntry,
        start: u32,
        vp: Viewport,
        wanted: &HashMap<i64, u32>,
        restart: bool,
        cancel: &dyn Fn() -> bool,
    ) -> Result<(Outcome, bool), FrameError> {
        let len = self.src.len();
        let limit = target
            .decode_idx
            .saturating_add(self.cfg.reorder_margin)
            .saturating_add(self.decoder.latency());
        let reusable = |s: &Session| {
            s.start <= start
                && (start..=limit).contains(&s.next)
                && !s.drained
                // Après `send_eof`, aucune AU ne peut plus être envoyée.
                && (!s.eof_sent || s.next >= len)
                && s.last_out.is_none_or(|pts| pts < target.pts)
        };
        let (mut session, reused) = match self.session.take() {
            Some(s) if !restart && reusable(&s) => (s, true),
            _ => {
                self.decoder.reset();
                (Session::new(start), false)
            }
        };
        let r = self.run(&mut session, target, limit, vp, wanted, cancel);
        if !matches!(r, Err(FrameError::Decoder(_))) {
            self.session = Some(session);
        }
        r.map(|outcome| (outcome, reused))
    }

    /// AU `decode_idx` corrompue : événement [`DecodeEvent::CorruptAu`], puis le décodage
    /// continue.
    ///
    /// Une erreur rendue par `receive` est différée : elle porte sur une AU déjà envoyée, au plus
    /// `latency` AU plus tôt avec les threads d'images, sans que le décodeur dise laquelle. On
    /// l'impute à la dernière AU envoyée (la plus plausible : c'est celle dont l'envoi a déclenché
    /// le décodage en échec quand la latence est nulle), ou au RAP de départ si rien n'a été envoyé.
    fn record_corrupt(&mut self, s: &mut Session, decode_idx: u32, e: DecodeError) {
        self.events.push(DecodeEvent::CorruptAu {
            decode_idx,
            detail: e.detail.clone(),
        });
        s.corrupt.insert(decode_idx, e);
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
        let mut receive_errors = 0;
        loop {
            loop {
                let received = match self.decoder.receive(&want) {
                    Ok(r) => {
                        receive_errors = 0;
                        r
                    }
                    // Erreur différée (threads d'images) : tolérée comme celle de `send`.
                    Err(e)
                        if e.kind == DecodeErrorKind::InvalidData
                            && receive_errors < MAX_RECEIVE_ERRORS =>
                    {
                        receive_errors += 1;
                        self.record_corrupt(s, s.next.saturating_sub(1).max(s.start), e);
                        continue;
                    }
                    Err(e) => return Err(FrameError::Decoder(e)),
                };
                let pts = match received {
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
            if !s.eof_sent && s.next <= limit && s.next < len {
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
                        self.record_corrupt(s, d, e);
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
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;

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

    /// Demande `pres` ; une fois sur trois, d'abord avec un `cancel` vrai après k AU (k < 5),
    /// puis à nouveau sans préemption.
    fn get_maybe_preempted(nav: &mut Navigator, rng: &mut Lcg, pres: u32) -> Arc<DecodedFrame> {
        if rng.below(3) == 0 {
            let k = rng.below(5);
            let calls = Cell::new(0);
            let cancel = || {
                calls.set(calls.get() + 1);
                calls.get() > k
            };
            match nav.frame(pres, small(), &cancel) {
                Ok(f) => {
                    assert_eq!((f.pres_idx, oracle(&f.image)), (pres, Some(pres)));
                    return f;
                }
                Err(e) => assert_eq!(e, FrameError::Preempted),
            }
        }
        get(nav, pres)
    }

    #[ma_trace::covers("NAV.ORACLE.01")]
    #[test]
    fn every_requested_frame_is_the_one_returned() {
        for latency in [0, 1, 4] {
            for src in [
                FakeSource::closed_gops(20, 24, 2),
                FakeSource::open_gops(20, 24, 2),
            ] {
                let mut nav = nav_with(FakeFactory {
                    latency,
                    ..FakeFactory::new(&src)
                });
                let mut rng = Lcg(0x5eed);
                // Les deux premières images d'un flux ouvert (RASL du premier CRA) sont
                // indécodables.
                let first = 2;
                let mut at = first;
                for _ in 0..50 {
                    at = first + rng.below(src.len() - first);
                    assert!(!get_maybe_preempted(&mut nav, &mut rng, at).corrupt);
                }
                let at = at.clamp(first + 20, src.len() - 21);
                for p in at..=at + 20 {
                    get_maybe_preempted(&mut nav, &mut rng, p);
                }
                for p in (at..at + 20).rev() {
                    get_maybe_preempted(&mut nav, &mut rng, p);
                }
                assert_eq!(nav.take_events(), []);
            }
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
    fn deferred_invalid_data_from_receive_is_tolerated() {
        // Même scénario, mais l'erreur de l'AU 30 remonte au `receive` qui suit son envoi
        // (threads d'images de libavcodec) : la session continue comme pour `send`.
        let src = FakeSource::closed_gops(3, 24, 0).with_corrupt(HashSet::from([30]));
        let mut nav = nav_with(FakeFactory {
            deferred_corruption: true,
            ..FakeFactory::new(&src)
        });
        for p in 24..30 {
            assert!(!get(&mut nav, p).corrupt, "image {p}");
        }
        let sent = nav.stats().aus_sent;
        let err = nav.frame(30, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        for p in 31..48 {
            assert!(get(&mut nav, p).corrupt, "image {p}");
        }
        assert!(!get(&mut nav, 48).corrupt);
        // Décodage poursuivi sans repartir du RAP : une AU par image, plus la latence.
        assert!(nav.stats().aus_sent - sent <= 19 + 2, "{:?}", nav.stats());
        let events = nav.take_events();
        assert!(
            matches!(events[..], [DecodeEvent::CorruptAu { decode_idx: 30, .. }]),
            "{events:?}"
        );
    }

    #[test]
    fn every_au_corrupt_with_deferred_errors() {
        // Toutes les AU corrompues, erreurs différées : rien ne sort, la cible est signalée
        // corrompue, une fois chaque AU envoyée.
        let src = FakeSource::closed_gops(1, 24, 0).with_corrupt((0..24).collect());
        let mut nav = nav_with(FakeFactory {
            deferred_corruption: true,
            ..FakeFactory::new(&src)
        });
        let err = nav.frame(5, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        let events = nav.take_events();
        assert!(events.len() >= 6, "{events:?}");
        assert!(
            events
                .iter()
                .all(|e| matches!(e, DecodeEvent::CorruptAu { .. }))
        );
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
    fn probe_viewport_is_accepted_by_hardware_scalers() {
        // Le VPP VA-API refuse les sorties de moins de 16 pixels.
        const { assert!(PROBE_SIDE >= 16) };
        assert_eq!(PROBE_VIEWPORT.out.w, PROBE_VIEWPORT.region.w);
        assert_eq!(PROBE_VIEWPORT.out.h, PROBE_VIEWPORT.region.h);
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
    fn empty_viewport_is_rejected_before_size_is_known() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let mut nav = nav(&src);
        let empty_out = Viewport {
            out: Size { w: 0, h: 2 },
            ..small()
        };
        assert_eq!(
            nav.frame(10, empty_out, &never),
            Err(FrameError::Viewport(ViewportError::Empty))
        );
        assert_eq!(nav.stats().aus_sent, 0);
        assert_eq!(nav.frame_size(), None);
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

    /// Navigation continue de 60 images ; le matériel tombe après 30 appels à `receive`.
    #[ma_trace::covers("PERF.HW.01")]
    #[test]
    fn gpu_error_mid_navigation_falls_back_without_losing_frame() {
        let src = FakeSource::closed_gops(4, 24, 2);
        let mut nav = nav_with(FakeFactory {
            hw_fail_after_receives: Some(30),
            ..FakeFactory::new(&src)
        });
        for p in 2..62 {
            get(&mut nav, p);
        }
        assert_eq!(nav.mode(), DecodeMode::Software);
        let falls: Vec<_> = nav
            .take_events()
            .into_iter()
            .filter(|e| matches!(e, DecodeEvent::FellBackToSoftware { .. }))
            .collect();
        assert_eq!(falls.len(), 1, "{falls:?}");
        assert!(matches!(
            &falls[0],
            DecodeEvent::FellBackToSoftware { reason, at_pres: Some(_) } if reason.contains("Gpu")
        ));
        // Le repli ne se reproduit pas.
        get(&mut nav, 5);
        assert_eq!(nav.take_events(), []);
    }

    #[test]
    fn fallback_clears_cache_and_restarts_from_rap() {
        let src = FakeSource::closed_gops(4, 24, 0);
        let mut nav = nav_with(FakeFactory {
            hw_fail_after_receives: Some(14),
            ..FakeFactory::new(&src)
        });
        get(&mut nav, 3);
        assert!(nav.is_cached(3, &small()));
        // Le matériel tombe pendant cette demande ; l'image 3 était en cache avant le repli.
        get(&mut nav, 50);
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert!(!nav.is_cached(3, &small()), "cache vidé par le repli");
        assert_eq!(nav.take_events().len(), 1);
    }

    #[ma_trace::covers("PERF.HW.01")]
    #[test]
    fn unavailable_hw_opens_software() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let mut nav = nav_with(FakeFactory {
            hw_available: false,
            ..FakeFactory::new(&src)
        });
        get(&mut nav, 2);
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert_eq!(nav.take_events(), []);
    }

    #[test]
    fn invalid_data_never_triggers_fallback() {
        // Viewport hors image (taille inconnue du navigateur) : `Unsupported` du décodeur matériel.
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = nav(&src);
        get(&mut nav, 2);
        let mut nav = Navigator::new(
            Arc::new(src.clone()),
            Arc::new(FakeFactory::new(&src)),
            ModePref::Auto,
            NavConfig::default(),
        )
        .unwrap();
        let outside = Viewport {
            region: Rect {
                x: 5000,
                y: 0,
                w: 10,
                h: 10,
            },
            out: Size { w: 10, h: 10 },
        };
        let err = nav.frame(2, outside, &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::Unsupported),
            "{err:?}"
        );
        assert_eq!(nav.take_events(), []);
        // AU corrompue : `InvalidData`, toléré sans repli.
        let src = FakeSource::closed_gops(2, 24, 0).with_corrupt(HashSet::from([5]));
        let mut nav = nav_with(FakeFactory::new(&src));
        let _ = nav.frame(5, small(), &never);
        let _ = nav.frame(10, small(), &never);
        assert_eq!(nav.mode(), DecodeMode::Hardware);
        assert!(
            nav.take_events()
                .iter()
                .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. }))
        );
    }

    #[test]
    fn software_error_is_reported() {
        // Un décodeur logiciel qui échoue (`Internal`) n'a nulle part où se replier.
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = patched_with(
            &src,
            Patched {
                wrong_au_once: Some(30),
                ..Patched::default()
            },
            ModePref::SoftwareOnly,
        );
        let err = nav.frame(35, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::Internal),
            "{err:?}"
        );
        assert_eq!(nav.take_events(), []);
        assert_eq!(nav.mode(), DecodeMode::Software);
    }

    #[test]
    fn fallback_happens_once_and_second_error_is_reported() {
        // Matériel qui tombe dès le départ : le repli a lieu ; l'erreur du logiciel remonte.
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = patched(
            &src,
            Patched {
                wrong_au_once: Some(30),
                ..Patched::default()
            },
        );
        get(&mut nav, 5); // mode matériel connu
        // `Internal` sur matériel : repli, puis la nouvelle tentative réussit (l'AU n'est
        // remplacée qu'une fois).
        get(&mut nav, 35);
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert_eq!(nav.take_events().len(), 1);
    }

    #[test]
    fn unknown_mode_error_with_auto_falls_back_but_not_software_only() {
        // Aucune image sortie : mode `Unknown`. `Auto` suppose le matériel ; `SoftwareOnly` jamais.
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = patched(
            &src,
            Patched {
                wrong_au_once: Some(0),
                ..Patched::default()
            },
        );
        assert_eq!(nav.mode(), DecodeMode::Unknown);
        get(&mut nav, 3);
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert_eq!(nav.take_events().len(), 1);

        let mut nav = patched_with(
            &src,
            Patched {
                wrong_au_once: Some(0),
                ..Patched::default()
            },
            ModePref::SoftwareOnly,
        );
        assert!(nav.frame(3, small(), &never).is_err());
        assert_eq!(nav.take_events(), []);
    }

    #[test]
    fn probe_size_falls_back_too() {
        let src = FakeSource::closed_gops(4, 24, 2);
        let mut nav = nav_with(FakeFactory {
            hw_fail_after_receives: Some(0),
            ..FakeFactory::new(&src)
        });
        assert_eq!(nav.probe_size(&never), Ok(FAKE_SIZE));
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert_eq!(nav.take_events().len(), 1);
    }

    #[test]
    fn prefetch_falls_back_too() {
        let src = FakeSource::closed_gops(4, 24, 2);
        let mut nav = nav_with(FakeFactory {
            hw_fail_after_receives: Some(0),
            ..FakeFactory::new(&src)
        });
        let f = nav
            .prefetch_frame(10, small(), Direction::Forward, &never)
            .unwrap();
        assert_eq!(oracle(&f.image), Some(10));
        assert_eq!(nav.mode(), DecodeMode::Software);
        assert_eq!(nav.take_events().len(), 1);
    }

    #[test]
    fn decoder_error_abandons_session_and_restarts_from_rap() {
        // AU 30 remplacée une seule fois : le décodeur logiciel répond `Internal` pendant la demande 35
        // (en matériel, ce serait un repli).
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut reference = nav(&src);
        get(&mut reference, 35);
        let fresh = reference.stats().aus_sent;

        let mut nav = patched_with(
            &src,
            Patched {
                wrong_au_once: Some(30),
                ..Patched::default()
            },
            ModePref::SoftwareOnly,
        );
        let err = nav.frame(35, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::Internal),
            "{err:?}"
        );
        assert_eq!(nav.stats().aus_sent, 7, "AU 24 à 30");
        // Le décodage en cours est abandonné : la demande suivante repart de l'IDR 24 et renvoie
        // l'AU 30, au lieu de continuer à l'AU 31.
        let before = nav.stats().aus_sent;
        get(&mut nav, 35);
        assert_eq!(nav.stats().aus_sent - before, fresh);
    }

    #[test]
    fn probe_and_first_frame_survive_a_corrupt_first_rap() {
        let src = FakeSource::closed_gops(3, 60, 0).with_corrupt(HashSet::from([0]));
        let factory = FakeFactory {
            latency: 0,
            ..FakeFactory::new(&src)
        };
        let cfg = NavConfig {
            reorder_margin: 2,
            keep_ahead: 0,
            ..NavConfig::default()
        };
        let open = || {
            Navigator::new(
                Arc::new(src.clone()),
                Arc::new(factory.clone()),
                ModePref::Auto,
                cfg,
            )
            .unwrap()
        };
        // Sans sondage : la taille est apprise sur l'image demandée.
        let mut nav = open();
        get(&mut nav, 100);
        assert_eq!(nav.frame_size(), Some(FAKE_SIZE));
        assert_eq!(nav.probe_size(&never), Ok(FAKE_SIZE));
        // Le sondage passe au RAP suivant quand le premier est corrompu.
        let mut nav = open();
        assert_eq!(nav.probe_size(&never), Ok(FAKE_SIZE));
        assert!(matches!(
            nav.take_events()[..],
            [DecodeEvent::CorruptAu { decode_idx: 0, .. }]
        ));
        // Préemption : remontée telle quelle, sans essayer d'autre RAP.
        let mut nav = open();
        let always = || true;
        assert_eq!(nav.probe_size(&always), Err(FrameError::Preempted));
        assert_eq!(nav.stats().aus_sent, 0);
    }

    #[test]
    fn probe_gives_up_after_eight_raps() {
        let corrupt: HashSet<u32> = (0..10).map(|g| g * 4).collect();
        let src = FakeSource::closed_gops(10, 4, 0).with_corrupt(corrupt);
        let factory = FakeFactory {
            latency: 0,
            ..FakeFactory::new(&src)
        };
        // Sans fenêtre avant, les images corrompues qui suivent chaque RAP ne sont pas converties.
        let cfg = NavConfig {
            keep_ahead: 0,
            ..NavConfig::default()
        };
        let mut nav = Navigator::new(
            Arc::new(src.clone()),
            Arc::new(factory),
            ModePref::Auto,
            cfg,
        )
        .unwrap();
        let err = nav.probe_size(&never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        assert_eq!(nav.frame_size(), None);
        let raps: Vec<u32> = nav
            .take_events()
            .iter()
            .filter_map(|e| match e {
                DecodeEvent::CorruptAu { decode_idx, .. } => Some(*decode_idx),
                DecodeEvent::FellBackToSoftware { .. } => None,
            })
            .collect();
        assert_eq!(raps, (0..8).map(|g| g * 4).collect::<Vec<_>>());
    }

    #[test]
    fn prefetch_does_not_change_foreground_direction() {
        let src = FakeSource::closed_gops(3, 60, 2);
        let mut nav = nav(&src);
        get(&mut nav, 100);
        for p in 101..=108 {
            let f = nav
                .prefetch_frame(p, small(), Direction::Forward, &never)
                .unwrap();
            assert_eq!(oracle(&f.image), Some(p));
        }
        // Pas arrière de premier plan : le GOP 60..119 est décodé une fois, jusqu'à 99.
        let before = nav.stats().aus_sent;
        get(&mut nav, 99);
        let sent = nav.stats().aus_sent - before;
        assert!(sent <= 40 + 4, "{sent} AU pour décoder 60..=99");
        let (sent, hits) = (nav.stats().aus_sent, nav.stats().cache_hits);
        get(&mut nav, 98);
        assert_eq!(nav.stats().aus_sent, sent);
        assert_eq!(nav.stats().cache_hits, hits + 1);
        // Préchargement en arrière : fenêtre arrière, sans dernière demande de premier plan.
        let mut nav = self::nav(&src);
        nav.prefetch_frame(150, small(), Direction::Backward, &never)
            .unwrap();
        assert!(nav.is_cached(150 - NavConfig::default().keep_behind, &small()));
        assert_eq!(
            nav.prefetch_frame(500, small(), Direction::Forward, &never),
            Err(FrameError::OutOfRange {
                pres_idx: 500,
                len: 180
            })
        );
    }

    /// GOP fermés hiérarchiques de 9 images (réordonnancement de profondeur 3), sans fichier
    /// incrusté : présentation dans l'ordre de décodage I0 P8 B4 B2 B1 B3 B6 B5 B7.
    fn hierarchical(gops: u32) -> FakeSource {
        let pattern = [0, 8, 4, 2, 1, 3, 6, 5, 7];
        let entries = (0..gops)
            .flat_map(|g| pattern.iter().map(move |&p| (g, g * 9 + p)))
            .zip(0..)
            .map(|((g, pres_idx), decode_idx)| FrameEntry {
                decode_idx,
                pres_idx,
                pts: i64::from(pres_idx) * crate::fake::PTS_STEP,
                picture: crate::PictureType::B,
                rap: if decode_idx == g * 9 {
                    crate::RapKind::Idr
                } else {
                    crate::RapKind::None
                },
                leading: crate::Leading::None,
            })
            .collect();
        FakeSource::from_entries(crate::Codec::H264, entries)
    }

    #[test]
    fn no_au_is_sent_after_eof() {
        // Marge nulle, latence nulle : l'image 7 (AU 8) ne sort qu'au vidage, après `send_eof`,
        // et l'image 8 reste dans le décodeur. L'image 12 exige l'AU 9, jamais envoyée : la session
        // vidée n'est pas réutilisable, la demande repart de l'IDR 9.
        let src = hierarchical(3);
        let factory = FakeFactory {
            latency: 0,
            ..FakeFactory::new(&src)
        };
        let cfg = NavConfig {
            reorder_margin: 0,
            ..NavConfig::default()
        };
        let mut nav = Navigator::new(
            Arc::new(src.clone()),
            Arc::new(factory),
            ModePref::Auto,
            cfg,
        )
        .unwrap();
        get(&mut nav, 7);
        get(&mut nav, 12);
        for p in 13..src.len() {
            get(&mut nav, p);
        }
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

    /// Source qui ment : une AU illisible, une image présentée à tort comme un IDR, ou une AU
    /// remplacée une fois par la suivante (le décodeur y voit un pts incohérent : `Internal`).
    #[derive(Default)]
    struct Patched {
        inner: Option<FakeSource>,
        fail_read: Option<u32>,
        fake_idr: Option<u32>,
        wrong_au_once: Option<u32>,
        wrong_au_done: AtomicBool,
    }

    impl Patched {
        fn inner(&self) -> &FakeSource {
            self.inner.as_ref().unwrap()
        }
    }

    impl AuSource for Patched {
        fn codec(&self) -> crate::Codec {
            self.inner().codec()
        }
        fn len(&self) -> u32 {
            self.inner().len()
        }
        fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
            let mut e = self.inner().entry(decode_idx)?;
            if self.fake_idr == Some(decode_idx) {
                e.rap = crate::RapKind::Idr;
            }
            Some(e)
        }
        fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
            self.inner().decode_idx_of(pres_idx)
        }
        fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
            if self.fail_read == Some(decode_idx) {
                return Err(io::Error::other("lecture impossible"));
            }
            if self.wrong_au_once == Some(decode_idx) && !self.wrong_au_done.swap(true, SeqCst) {
                return self.inner().read_au(decode_idx + 1, buf);
            }
            self.inner().read_au(decode_idx, buf)
        }
    }

    fn patched(src: &FakeSource, patch: Patched) -> Navigator {
        patched_with(src, patch, ModePref::Auto)
    }

    fn patched_with(src: &FakeSource, patch: Patched, pref: ModePref) -> Navigator {
        let patched = Patched {
            inner: Some(src.clone()),
            ..patch
        };
        Navigator::new(
            Arc::new(patched),
            Arc::new(FakeFactory::new(src)),
            pref,
            NavConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn io_error_is_reported() {
        let src = FakeSource::closed_gops(2, 24, 0);
        let mut nav = patched(
            &src,
            Patched {
                fail_read: Some(3),
                ..Patched::default()
            },
        );
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
        let mut nav = patched(
            &src,
            Patched {
                fake_idr: Some(5),
                ..Patched::default()
            },
        );
        nav.probe_size(&never).unwrap();
        let sent = nav.stats().aus_sent;
        let err = nav.frame(5, small(), &never).unwrap_err();
        assert!(
            matches!(&err, FrameError::Decoder(e) if e.kind == DecodeErrorKind::InvalidData),
            "{err:?}"
        );
        // Un seul essai : il partait déjà du RAP (supposé) 5, avec un décodeur remis à zéro.
        assert_eq!(nav.stats().aus_sent - sent, 3);
        get(&mut nav, 4);
    }
}
