//! Service d'images : un fil de travail possède le [`Navigator`], sert les demandes explicites en
//! priorité et précharge les images voisines de la dernière demande.
//!
//! Les demandes explicites passent par un canal. Un compteur de demandes en attente est consulté
//! par le `cancel` du préchargement : une demande qui arrive pendant un préchargement le préempte
//! entre deux AU. Après chaque demande servie, le fil précharge, pour le même viewport et du plus
//! proche au plus lointain, les images du sens de la dernière navigation, puis celles de l'autre
//! sens. L'état observable (images prêtes, mode, compteurs, événements) est publié dans un état
//! partagé après chaque étape : le lire ne bloque jamais derrière un décodage en cours.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::{
    DecodeError, DecodeErrorKind, DecodeEvent, DecodeMode, DecodedFrame, Direction, FrameError,
    NavStats, Navigator, Size, Viewport,
};

/// Étendue du préchargement autour de la dernière demande explicite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefetchConfig {
    /// Images préchargées après la demande.
    pub ahead: u32,
    /// Images préchargées avant la demande.
    pub behind: u32,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        PrefetchConfig {
            ahead: 8,
            behind: 8,
        }
    }
}

/// Images prêtes autour de la dernière demande explicite servie, pour son viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readiness {
    /// Image de la dernière demande explicite servie.
    pub center: u32,
    /// Images consécutives en cache juste après `center` (au plus `ahead`).
    pub ahead_ready: u32,
    /// Images consécutives en cache juste avant `center` (au plus `behind`).
    pub behind_ready: u32,
}

type FrameResult = Result<Arc<DecodedFrame>, FrameError>;

enum Msg {
    Frame {
        pres_idx: u32,
        vp: Viewport,
        reply: Sender<FrameResult>,
    },
    Probe {
        reply: Sender<Result<Size, FrameError>>,
    },
}

/// État publié par le fil après chaque étape.
struct Published {
    readiness: Readiness,
    mode: DecodeMode,
    stats: NavStats,
    events: Vec<DecodeEvent>,
}

struct Shared {
    /// Demandes envoyées et pas encore reçues par le fil.
    pending: AtomicUsize,
    stop: AtomicBool,
    published: Mutex<Published>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Published> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn stopping(&self) -> bool {
        self.stop.load(SeqCst)
    }

    fn preempted(&self) -> bool {
        self.stopping() || self.pending.load(SeqCst) > 0
    }
}

/// Préchargement en cours : images voisines de `center`, du plus proche au plus lointain, d'abord
/// dans le sens `order[0]`. Chaque sens peut être abandonné (fin du fichier).
struct Plan {
    center: u32,
    vp: Viewport,
    order: [Direction; 2],
    /// Prochaine distance à précharger, par position dans `order`.
    next: [u32; 2],
    /// Distance maximale, par position dans `order`.
    limit: [u32; 2],
}

impl Plan {
    fn new(center: u32, vp: Viewport, first: Direction, cfg: PrefetchConfig) -> Plan {
        let limit_of = |d| match d {
            Direction::Forward => cfg.ahead,
            Direction::Backward => cfg.behind,
        };
        let order = match first {
            Direction::Forward => [Direction::Forward, Direction::Backward],
            Direction::Backward => [Direction::Backward, Direction::Forward],
        };
        Plan {
            center,
            vp,
            order,
            next: [1, 1],
            limit: [limit_of(order[0]), limit_of(order[1])],
        }
    }

    /// Prochaine image à précharger, sans avancer.
    fn peek(&self) -> Option<(u32, Direction)> {
        (0..2).find_map(|i| {
            if self.next[i] == 0 || self.next[i] > self.limit[i] {
                return None;
            }
            let pres = match self.order[i] {
                Direction::Forward => self.center.checked_add(self.next[i]),
                Direction::Backward => self.center.checked_sub(self.next[i]),
            }?;
            Some((pres, self.order[i]))
        })
    }

    /// Passe à l'image suivante du sens `dir`.
    fn advance(&mut self, dir: Direction) {
        let i = self.index(dir);
        // 0 marque un sens épuisé (la distance ne peut plus croître).
        self.next[i] = self.next[i].checked_add(1).unwrap_or(0);
    }

    /// Abandonne le sens `dir`.
    fn abandon(&mut self, dir: Direction) {
        let i = self.index(dir);
        self.next[i] = 0;
    }

    fn index(&self, dir: Direction) -> usize {
        usize::from(self.order[0] != dir)
    }
}

impl Iterator for Plan {
    type Item = (u32, Direction);

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.peek();
        if let Some((_, dir)) = item {
            self.advance(dir);
        }
        item
    }
}

/// Service d'images : un fil nommé `ma-decode` possède le [`Navigator`].
///
/// À la destruction, le fil est arrêté (le décodage en cours est interrompu entre deux AU) et
/// attendu.
pub struct FrameService {
    tx: Option<Sender<Msg>>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl FrameService {
    /// Lance le fil de travail sur `nav`.
    ///
    /// # Panics
    ///
    /// Si le système refuse de créer le fil.
    #[must_use]
    pub fn start(nav: Navigator, cfg: PrefetchConfig) -> FrameService {
        let (tx, rx) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared {
            pending: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            published: Mutex::new(Published {
                readiness: Readiness::default(),
                mode: nav.mode(),
                stats: nav.stats(),
                events: Vec::new(),
            }),
        });
        let worker = Worker {
            nav,
            cfg,
            rx,
            shared: Arc::clone(&shared),
            queue: VecDeque::new(),
            plan: None,
            last: [None, None],
        };
        let handle = std::thread::Builder::new()
            .name("ma-decode".into())
            .spawn(move || worker.run())
            .expect("création du fil ma-decode");
        FrameService {
            tx: Some(tx),
            shared,
            worker: Some(handle),
        }
    }

    /// Demande explicite, bloquante : image `pres_idx` pour le viewport `vp`.
    pub fn get(&self, pres_idx: u32, vp: Viewport) -> Result<Arc<DecodedFrame>, FrameError> {
        self.request(pres_idx, vp)
            .recv()
            .unwrap_or_else(|_| Err(stopped()))
    }

    /// Demande explicite, non bloquante : la réponse arrive sur le canal rendu. Une demande
    /// identique (même image, même viewport) arrivée pendant le traitement de celle-ci n'est pas
    /// décodée à nouveau : toutes reçoivent la même réponse.
    #[must_use]
    pub fn request(&self, pres_idx: u32, vp: Viewport) -> Receiver<FrameResult> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.send(Msg::Frame {
            pres_idx,
            vp,
            reply,
        });
        rx
    }

    /// Taille d'affichage des images, apprise par le fil ([`Navigator::probe_size`]). Bloquante.
    pub fn probe_size(&self) -> Result<Size, FrameError> {
        let (reply, rx) = crossbeam_channel::bounded(1);
        self.send(Msg::Probe { reply });
        rx.recv().unwrap_or_else(|_| Err(stopped()))
    }

    /// Images prêtes autour de la dernière demande explicite servie.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.shared.lock().readiness
    }

    /// Mode effectif du décodeur.
    #[must_use]
    pub fn mode(&self) -> DecodeMode {
        self.shared.lock().mode
    }

    /// Événements émis depuis le dernier appel.
    #[must_use]
    pub fn take_events(&self) -> Vec<DecodeEvent> {
        std::mem::take(&mut self.shared.lock().events)
    }

    /// Compteurs du navigateur, à la dernière étape terminée.
    #[must_use]
    pub fn stats(&self) -> NavStats {
        self.shared.lock().stats
    }

    fn send(&self, msg: Msg) {
        self.shared.pending.fetch_add(1, SeqCst);
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(msg).is_ok());
        if !sent {
            // Le fil est arrêté : la réponse ne viendra pas (canal de réponse fermé).
            self.shared.pending.fetch_sub(1, SeqCst);
        }
    }
}

impl Drop for FrameService {
    fn drop(&mut self) {
        self.shared.stop.store(true, SeqCst);
        self.tx = None;
        if let Some(handle) = self.worker.take() {
            // Une panique du fil a déjà été signalée ; la propager ici ferait avorter le processus.
            let _ = handle.join();
        }
    }
}

fn stopped() -> FrameError {
    FrameError::Decoder(DecodeError {
        kind: DecodeErrorKind::Internal,
        detail: "service d'images arrêté".into(),
    })
}

/// Fil de travail.
struct Worker {
    nav: Navigator,
    cfg: PrefetchConfig,
    rx: Receiver<Msg>,
    shared: Arc<Shared>,
    /// Demandes reçues, pas encore servies.
    queue: VecDeque<Msg>,
    plan: Option<Plan>,
    /// Deux dernières demandes explicites servies : `[avant-dernière, dernière]`.
    last: [Option<u32>; 2],
}

impl Worker {
    fn run(mut self) {
        while !self.shared.stopping() {
            if let Some(msg) = self.queue.pop_front() {
                self.handle(msg);
                continue;
            }
            let prefetching = self.plan.as_ref().is_some_and(|p| p.peek().is_some());
            let msg = if prefetching {
                match self.rx.try_recv() {
                    Ok(msg) => msg,
                    Err(TryRecvError::Empty) => {
                        self.prefetch_step();
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            } else {
                match self.rx.recv() {
                    Ok(msg) => msg,
                    Err(_) => break,
                }
            };
            self.shared.pending.fetch_sub(1, SeqCst);
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: Msg) {
        let shared = Arc::clone(&self.shared);
        let cancel = || shared.stopping();
        match msg {
            Msg::Probe { reply } => {
                let result = self.nav.probe_size(&cancel);
                self.publish();
                let _ = reply.send(result);
            }
            Msg::Frame {
                pres_idx,
                vp,
                reply,
            } => {
                let result = self.nav.frame(pres_idx, vp, &cancel);
                let mut replies = vec![reply];
                self.take_duplicates(pres_idx, vp, &mut replies);
                if result.is_ok() {
                    self.last = [self.last[1], Some(pres_idx)];
                    let dir = match self.last {
                        [Some(prev), Some(cur)] if cur < prev => Direction::Backward,
                        _ => Direction::Forward,
                    };
                    self.plan = Some(Plan::new(pres_idx, vp, dir, self.cfg));
                }
                // Publier avant de répondre : l'appelant qui a sa réponse voit l'état à jour.
                self.publish();
                for reply in replies {
                    let _ = reply.send(result.clone());
                }
            }
        }
    }

    /// Retire, en tête des demandes reçues, celles identiques à (`pres_idx`, `vp`) et garde leur
    /// canal de réponse. Seules les demandes consécutives sont fusionnées, pour garder l'ordre.
    fn take_duplicates(
        &mut self,
        pres_idx: u32,
        vp: Viewport,
        replies: &mut Vec<Sender<FrameResult>>,
    ) {
        while let Ok(msg) = self.rx.try_recv() {
            self.shared.pending.fetch_sub(1, SeqCst);
            self.queue.push_back(msg);
        }
        while let Some(Msg::Frame {
            pres_idx: p, vp: v, ..
        }) = self.queue.front()
        {
            if (*p, *v) != (pres_idx, vp) {
                break;
            }
            if let Some(Msg::Frame { reply, .. }) = self.queue.pop_front() {
                replies.push(reply);
            }
        }
    }

    /// Précharge la prochaine image du plan, sauf si elle est déjà en cache.
    fn prefetch_step(&mut self) {
        let Some(plan) = &mut self.plan else {
            return;
        };
        let Some((pres_idx, dir)) = plan.peek() else {
            return;
        };
        let vp = plan.vp;
        if self.nav.is_cached(pres_idx, &vp) {
            plan.advance(dir);
            return;
        }
        let shared = Arc::clone(&self.shared);
        let result = self
            .nav
            .prefetch_frame(pres_idx, vp, dir, &|| shared.preempted());
        match result {
            // Préempté : l'image sera reprise si le plan reste le même (par exemple après un
            // sondage de taille).
            Err(FrameError::Preempted) => {}
            Err(FrameError::OutOfRange { .. }) => plan.abandon(dir),
            // Une image indécodable ou en erreur n'empêche pas de précharger les suivantes.
            Ok(_) | Err(_) => plan.advance(dir),
        }
        self.publish();
    }

    fn readiness(&self) -> Readiness {
        let Some(plan) = &self.plan else {
            return Readiness::default();
        };
        let count = |limit: u32, step: fn(u32, u32) -> Option<u32>| {
            let mut n = 0;
            while n < limit
                && step(plan.center, n + 1).is_some_and(|p| self.nav.is_cached(p, &plan.vp))
            {
                n += 1;
            }
            n
        };
        Readiness {
            center: plan.center,
            ahead_ready: count(self.cfg.ahead, u32::checked_add),
            behind_ready: count(self.cfg.behind, u32::checked_sub),
        }
    }

    fn publish(&mut self) {
        let readiness = self.readiness();
        let events = self.nav.take_events();
        let mut p = self.shared.lock();
        p.readiness = readiness;
        p.mode = self.nav.mode();
        p.stats = self.nav.stats();
        p.events.extend(events);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::fake::{FAKE_SIZE, FakeFactory, FakeSource, oracle};
    use crate::{AuSource, Direction, ModePref, NavConfig};

    fn small() -> Viewport {
        Viewport::fit(FAKE_SIZE, Size { w: 64, h: 36 })
    }

    fn larger() -> Viewport {
        Viewport::fit(FAKE_SIZE, Size { w: 128, h: 72 })
    }

    fn service_on(src: Arc<dyn AuSource>, factory: FakeFactory) -> FrameService {
        let nav =
            Navigator::new(src, Arc::new(factory), ModePref::Auto, NavConfig::default()).unwrap();
        FrameService::start(nav, PrefetchConfig::default())
    }

    fn service(factory: FakeFactory) -> FrameService {
        service_on(Arc::new(factory.source.clone()), factory)
    }

    fn delayed(src: &FakeSource, ms: u64) -> FakeFactory {
        FakeFactory {
            au_delay: Duration::from_millis(ms),
            ..FakeFactory::new(src)
        }
    }

    fn get(svc: &FrameService, pres: u32, vp: Viewport) -> Arc<DecodedFrame> {
        let f = svc.get(pres, vp).unwrap();
        assert_eq!((f.pres_idx, f.viewport), (pres, vp));
        assert_eq!(oracle(&f.image), Some(pres), "oracle de l'image {pres}");
        f
    }

    /// Sonde `cond` toutes les 10 ms pendant au plus `limit`.
    fn wait_until(limit: Duration, cond: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn all_ready(svc: &FrameService, center: u32) -> bool {
        svc.readiness()
            == Readiness {
                center,
                ahead_ready: 8,
                behind_ready: 8,
            }
    }

    #[ma_trace::covers("PERF.PREFETCH.01")]
    #[test]
    fn neighbours_ready_after_one_second() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let svc = service(delayed(&src, 2));
        assert_eq!(svc.readiness(), Readiness::default());
        get(&svc, 100, small());
        assert!(
            wait_until(Duration::from_secs(1), || all_ready(&svc, 100)),
            "{:?}",
            svc.readiness()
        );
        // Les voisines sont servies par le cache, sans décodage.
        let before = svc.stats();
        for p in (92..100).chain(101..109) {
            get(&svc, p, small());
        }
        let after = svc.stats();
        assert_eq!(after.cache_hits - before.cache_hits, 16);
    }

    #[ma_trace::covers("PERF.PREFETCH.01")]
    #[test]
    fn explicit_request_preempts_prefetch() {
        let src = FakeSource::closed_gops(220, 24, 2);
        // Référence : durée et AU d'un préchargement complet autour de 100.
        let reference = service(delayed(&src, 20));
        get(&reference, 100, small());
        let aus_before = reference.stats().aus_sent;
        let start = Instant::now();
        assert!(wait_until(Duration::from_secs(10), || all_ready(
            &reference, 100
        )));
        let full_time = start.elapsed();
        let full_aus = reference.stats().aus_sent - aus_before;
        drop(reference);

        let svc = service(delayed(&src, 20));
        get(&svc, 100, small());
        let aus_before = svc.stats().aus_sent;
        let start = Instant::now();
        get(&svc, 5000, small());
        let served = start.elapsed();
        let aus = svc.stats().aus_sent - aus_before;
        assert!(served < full_time, "{served:?} >= {full_time:?}");
        assert!(aus < full_aus, "{aus} AU >= {full_aus} AU");
        // Le préchargement repart autour de la nouvelle demande.
        assert!(wait_until(Duration::from_secs(10), || all_ready(
            &svc, 5000
        )));
    }

    /// Une demande explicite interrompt une étape de préchargement longue entre deux AU, sans
    /// attendre sa fin.
    #[ma_trace::covers("PERF.PREFETCH.01")]
    #[test]
    fn explicit_request_interrupts_long_prefetch_step() {
        let src = FakeSource::closed_gops(220, 24, 2);
        let alone = service(delayed(&src, 20));
        get(&alone, 5000, small());
        let aus_alone = alone.stats().aus_sent;
        drop(alone);

        let svc = service(delayed(&src, 20));
        get(&svc, 97, small());
        // Pas en arrière : la première étape (image 95) repart du RAP 72, soit environ 24 AU.
        get(&svc, 96, small());
        std::thread::sleep(Duration::from_millis(60));
        let aus_before = svc.stats().aus_sent;
        get(&svc, 5000, small());
        let aus = svc.stats().aus_sent - aus_before;
        assert!(
            aus < aus_alone + 15,
            "{aus} AU (seule : {aus_alone}) : l'étape longue n'a pas été interrompue"
        );
    }

    #[test]
    fn zoom_change_restarts_prefetch_on_new_viewport() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let svc = service(delayed(&src, 2));
        get(&svc, 100, small());
        assert!(wait_until(Duration::from_secs(1), || all_ready(&svc, 100)));
        get(&svc, 100, larger());
        assert!(
            wait_until(Duration::from_secs(1), || all_ready(&svc, 100)),
            "{:?}",
            svc.readiness()
        );
        // Les voisines sont prêtes au nouveau zoom.
        for p in [101, 92] {
            let hits = svc.stats().cache_hits;
            get(&svc, p, larger());
            assert_eq!(svc.stats().cache_hits, hits + 1, "image {p}");
        }
    }

    #[test]
    fn gpu_failure_during_prefetch_falls_back_once() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let svc = service(FakeFactory {
            hw_fail_after_receives: Some(12),
            ..delayed(&src, 2)
        });
        get(&svc, 100, small());
        // L'erreur n'a pas encore eu lieu : elle survient pendant le préchargement.
        assert_eq!(svc.mode(), DecodeMode::Hardware);
        assert!(wait_until(Duration::from_secs(1), || svc.mode()
            == DecodeMode::Software));
        get(&svc, 50, small());
        assert!(wait_until(Duration::from_secs(1), || all_ready(&svc, 50)));
        let events = svc.take_events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            DecodeEvent::FellBackToSoftware { reason, .. } if reason.contains("Gpu")
        ));
        assert_eq!(svc.take_events(), []);
    }

    #[test]
    fn drop_stops_worker() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let factory = delayed(&src, 20);
        let shared_src: Arc<dyn AuSource> = Arc::new(src);
        let weak = Arc::downgrade(&shared_src);
        let svc = service_on(shared_src, factory);
        get(&svc, 100, small());
        // Le préchargement est en cours : l'arrêt doit l'interrompre.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(svc);
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(1))
            .expect("arrêt du fil en moins de 1 s");
        // Le fil a rendu le navigateur, donc la source.
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn duplicate_request_is_decoded_once() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let svc = service(delayed(&src, 20));
        let first = svc.request(100, small());
        let second = svc.request(100, small());
        let (a, b) = (
            first.recv().unwrap().unwrap(),
            second.recv().unwrap().unwrap(),
        );
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(oracle(&a.image), Some(100));
        assert_eq!(svc.stats().cache_hits, 0);
    }

    #[test]
    fn probe_size_and_errors_are_served() {
        let src = FakeSource::closed_gops(2, 24, 2);
        let svc = service(FakeFactory::new(&src));
        assert_eq!(svc.probe_size(), Ok(FAKE_SIZE));
        assert_eq!(
            svc.get(48, small()),
            Err(FrameError::OutOfRange {
                pres_idx: 48,
                len: 48
            })
        );
        // Une erreur ne déplace pas le préchargement.
        get(&svc, 46, small());
        assert!(wait_until(Duration::from_secs(1), || {
            let r = svc.readiness();
            r.center == 46 && r.ahead_ready == 1 && r.behind_ready == 8
        }));
    }

    #[test]
    fn plan_goes_nearest_first_in_navigation_direction() {
        let cfg = PrefetchConfig {
            ahead: 3,
            behind: 2,
        };
        let fwd: Vec<_> = Plan::new(10, small(), Direction::Forward, cfg).collect();
        assert_eq!(
            fwd,
            [
                (11, Direction::Forward),
                (12, Direction::Forward),
                (13, Direction::Forward),
                (9, Direction::Backward),
                (8, Direction::Backward),
            ]
        );
        let back: Vec<_> = Plan::new(1, small(), Direction::Backward, cfg)
            .map(|(p, _)| p)
            .collect();
        assert_eq!(back, [0, 2, 3, 4]);
        let end: Vec<_> = Plan::new(u32::MAX - 1, small(), Direction::Forward, cfg)
            .map(|(p, _)| p)
            .collect();
        assert_eq!(end, [u32::MAX, u32::MAX - 2, u32::MAX - 3]);
    }

    #[test]
    fn backward_navigation_prefetches_behind_first() {
        let src = FakeSource::closed_gops(10, 24, 2);
        let svc = service(delayed(&src, 2));
        get(&svc, 101, small());
        get(&svc, 100, small());
        assert!(wait_until(Duration::from_secs(1), || all_ready(&svc, 100)));
    }
}
