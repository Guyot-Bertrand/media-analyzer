//! Indexeur progressif : un seul passage sur le fichier, à mémoire bornée.
//!
//! Pour chaque PID vidéo analysé de la structure courante, les PES sont assemblés
//! ([`PesAssembler`]), découpés en AU par `ma-es`, puis les AU deviennent des entrées d'index
//! ([`FrameEntry`] et [`AuInfo`]). Un préfixe stable de ces entrées est publié dans un
//! [`TsIndex`] partagé, lisible pendant l'indexation.
//!
//! # Correspondance entre l'ES et le fichier
//!
//! `ma-es` compte ses frontières en octets d'ES depuis le premier `push`. Chaque PID garde une
//! table bornée des chunks récents (offset d'ES, [`EsPos`], longueur, PES, trou) : l'octet `n`
//! d'un chunk est à `chunk.pos + n`, ce que [`crate::read_es`] sait rejouer. Une fin d'AU est
//! exprimée sur le chunk de son dernier octet (`skip` de ce chunk + longueur prise), jamais sur
//! le chunk suivant. La table est purgée dès que les AU qui la référencent sont émises ; elle
//! couvre au plus `2 × MAX_AU_BYTES` d'ES.
//!
//! # Règles
//!
//! - **PTS** : celui d'un PES s'applique à la 1re AU qui commence dans ce PES, sauf si un trou
//!   sépare le début du PES de celui de l'AU (le PTS peut alors être celui d'une AU perdue).
//!   Une AU sans PTS reçoit celui de l'AU précédente (ordre de décodage) plus la médiane des 16
//!   derniers écarts positifs entre AU consécutives à PTS réels (3600 par défaut), augmenté de 1
//!   tant qu'il existe parmi les PTS récents (`ts-au-no-pts`). Le DTS est déroulé avec le PTS
//!   (`DTS = PTS` quand le PES n'en porte pas).
//! - **Champs** : deux AU consécutives et contiguës `Top`/`Bottom` complémentaires (H.264 :
//!   même `frame_num`) forment une entrée (`fields = 2`).
//! - **RAP** et **images de tête** : voir `EntryBuilder` (même module).
//! - **`damaged`** : l'AU contient un trou (`gap_before` d'un chunk strictement à l'intérieur).
//! - **Préfixe publié** : L est le plus grand index de décodage d'un RAP r tel qu'un RAP
//!   postérieur à r soit vu, que `max(PTS de [0, r)) < min(PTS de [r, vus))` et que les
//!   `Recovery` de [0, r) soient résolus **dans [0, r)** (leur R aussi : `pres(R)` est alors
//!   définitif). En fin de fichier, L couvre tout. Une entrée publiée ne change plus.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ma_core::{
    Codec, Family, FrameEntry, Leading, Message, MessageSink, Param, PictureType, RapKind,
    Severity, TsUnwrapper,
};
use ma_es::{AuBoundary, AuFramer, AuMeta, Field, NalLeading, ParamSet, RapClass};
use ma_io::{IoError, ReadAt};

use crate::messages::{AU_NO_PTS, AU_TOO_LARGE};
use crate::packet::PacketFormat;
use crate::pes::{EsPos, MAX_AU_BYTES, PesAssembler};
use crate::scan::{PacketScanner, ScannedPacket};
use crate::structure::{StructureBuilder, TsStructure};
use crate::tables::StreamKind;

const SUB_FAMILY: &str = "index";
/// Écart de PTS par défaut entre deux AU (25 images/s).
const DEFAULT_PTS_STEP: i64 = 3600;
/// Écarts de PTS retenus pour l'estimation.
const PTS_DIFFS: usize = 16;
/// PTS récents gardés pour rendre un PTS estimé unique.
const RECENT_PTS: usize = 256;
/// Octets d'ES couverts au plus par la table des chunks d'un PID.
const CHUNK_WINDOW: u64 = 2 * MAX_AU_BYTES as u64;
const PID_COUNT: usize = 8192;
const MASK_33: u64 = (1 << 33) - 1;
const PANICKED: IoError = IoError::InvalidData("ma-index-panic");

/// Description d'une AU indexée (une entrée : deux AU pour une paire de champs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuInfo {
    /// Premier octet (voir [`EsPos`]).
    pub start: EsPos,
    /// Fin exclusive, exprimée sur le paquet du dernier octet (`skip` > 0).
    pub end: EsPos,
    /// Octets d'ES de l'entrée.
    pub size: u32,
    /// PTS déroulé (90 kHz).
    pub pts: i64,
    /// DTS déroulé ; égal au PTS quand le PES n'en porte pas ; `None` pour un PTS estimé.
    pub dts: Option<i64>,
    pub pts_estimated: bool,
    /// Des octets de l'entrée ont été perdus (rupture de continuité, TEI, brouillage).
    pub damaged: bool,
    pub params: ParamSet,
    /// Offset du paquet qui porte le début du PES où l'entrée commence.
    pub pes_packet: u64,
    /// Numéro du paquet `start.packet_offset` (depuis le début de l'indexation).
    pub packet_index: u64,
    /// Type de NAL de la 1re tranche (0 pour MPEG-2).
    pub nal_type: u8,
    /// 1, ou 2 pour une paire de champs.
    pub fields: u8,
}

/// Échantillon de PCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcrSample {
    pub packet_index: u64,
    /// Offset de l'octet 0x47 du paquet.
    pub offset: u64,
    /// Ticks de 27 MHz, déroulés.
    pub pcr: i64,
    /// `discontinuity_indicator` du paquet.
    pub discontinuity: bool,
}

/// Avancement de l'indexation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Fin du dernier paquet traité (offset dans le fichier).
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// L'indexation est allée jusqu'à la fin du fichier.
    pub complete: bool,
}

/// Point d'accès aléatoire stocké sur un octet (`Recovery` garde son `pres_span` à part).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RapTag {
    None,
    Idr,
    Cra,
    Bla,
    Recovery,
    Intra,
    IntraClosed,
}

const FLAG_ESTIMATED: u8 = 1;
const FLAG_DAMAGED: u8 = 2;
const FLAG_DTS: u8 = 4;

/// Entrée stockée, compacte (72 octets) ; [`AuInfo`] et [`FrameEntry`] en sont tirés.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    start_packet: u64,
    end_packet: u64,
    pts: i64,
    dts: i64,
    pes_packet: u64,
    packet_index: u64,
    size: u32,
    pres_idx: u32,
    pres_span: u32,
    start_skip: u16,
    end_skip: u16,
    flags: u8,
    params: ParamSet,
    nal_type: u8,
    fields: u8,
    picture: PictureType,
    rap: RapTag,
    leading: Leading,
}

impl Record {
    fn au(&self) -> AuInfo {
        AuInfo {
            start: EsPos {
                packet_offset: self.start_packet,
                skip: self.start_skip,
            },
            end: EsPos {
                packet_offset: self.end_packet,
                skip: self.end_skip,
            },
            size: self.size,
            pts: self.pts,
            dts: (self.flags & FLAG_DTS != 0).then_some(self.dts),
            pts_estimated: self.flags & FLAG_ESTIMATED != 0,
            damaged: self.flags & FLAG_DAMAGED != 0,
            params: self.params,
            pes_packet: self.pes_packet,
            packet_index: self.packet_index,
            nal_type: self.nal_type,
            fields: self.fields,
        }
    }

    fn rap_kind(&self) -> RapKind {
        match self.rap {
            RapTag::None => RapKind::None,
            RapTag::Idr => RapKind::Idr,
            RapTag::Cra => RapKind::Cra,
            RapTag::Bla => RapKind::Bla,
            RapTag::Recovery => RapKind::Recovery {
                pres_span: self.pres_span,
            },
            RapTag::Intra => RapKind::Intra { closed_gop: false },
            RapTag::IntraClosed => RapKind::Intra { closed_gop: true },
        }
    }

    fn entry(&self, decode_idx: u32) -> FrameEntry {
        FrameEntry {
            decode_idx,
            pres_idx: self.pres_idx,
            pts: self.pts,
            picture: self.picture,
            rap: self.rap_kind(),
            leading: self.leading,
        }
    }
}

/// Index d'un PID vidéo : entrées publiées, en ajout seul.
#[derive(Debug)]
pub struct VideoIndex {
    pid: u16,
    codec: Codec,
    records: Vec<Record>,
    /// `pres_idx` → `decode_idx`.
    by_pres: Vec<u32>,
}

impl VideoIndex {
    fn new(pid: u16, codec: Codec) -> Self {
        VideoIndex {
            pid,
            codec,
            records: Vec::new(),
            by_pres: Vec::new(),
        }
    }

    #[must_use]
    pub fn pid(&self) -> u16 {
        self.pid
    }

    #[must_use]
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Nombre d'entrées publiées : `[0, published())` est définitif.
    #[must_use]
    pub fn published(&self) -> u32 {
        u32::try_from(self.records.len()).unwrap_or(u32::MAX)
    }

    #[must_use]
    pub fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
        self.record(decode_idx).map(|r| r.entry(decode_idx))
    }

    #[must_use]
    pub fn au(&self, decode_idx: u32) -> Option<AuInfo> {
        self.record(decode_idx).map(Record::au)
    }

    #[must_use]
    pub fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
        self.by_pres.get(usize::try_from(pres_idx).ok()?).copied()
    }

    fn record(&self, decode_idx: u32) -> Option<&Record> {
        self.records.get(usize::try_from(decode_idx).ok()?)
    }
}

/// État partagé, sous le verrou de [`TsIndex`].
#[derive(Debug)]
struct Shared {
    structure: TsStructure,
    videos: Vec<VideoIndex>,
    pcrs: BTreeMap<u16, Vec<PcrSample>>,
    progress: Progress,
}

/// Index d'un fichier, lisible pendant son indexation (verrou lecteurs/écrivain interne,
/// tenu brièvement par l'indexeur à chaque publication).
#[derive(Debug)]
pub struct TsIndex {
    format: PacketFormat,
    inner: RwLock<Shared>,
}

impl TsIndex {
    #[must_use]
    pub fn new(format: PacketFormat, bytes_total: u64) -> TsIndex {
        TsIndex {
            format,
            inner: RwLock::new(Shared {
                structure: TsStructure {
                    format,
                    ts_id: None,
                    has_cat: false,
                    programs: Vec::new(),
                },
                videos: Vec::new(),
                pcrs: BTreeMap::new(),
                progress: Progress {
                    bytes_done: 0,
                    bytes_total,
                    complete: false,
                },
            }),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Shared> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Shared> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Dernière structure vue (PAT, PMT, SDT).
    #[must_use]
    pub fn structure(&self) -> TsStructure {
        self.read().structure.clone()
    }

    /// PID vidéo indexés, dans l'ordre de leur apparition (y compris ceux retirés depuis).
    #[must_use]
    pub fn videos(&self) -> Vec<(u16, Codec)> {
        self.read()
            .videos
            .iter()
            .map(|v| (v.pid, v.codec))
            .collect()
    }

    /// Applique `f` à l'index du PID `pid`, sous le verrou de lecture (sans copie).
    pub fn with_video<R>(&self, pid: u16, f: impl FnOnce(&VideoIndex) -> R) -> Option<R> {
        self.read().videos.iter().find(|v| v.pid == pid).map(f)
    }

    /// PCR publiés du PID `pcr_pid`.
    #[must_use]
    pub fn pcrs(&self, pcr_pid: u16) -> Vec<PcrSample> {
        self.read().pcrs.get(&pcr_pid).cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn progress(&self) -> Progress {
        self.read().progress
    }

    #[must_use]
    pub fn format(&self) -> PacketFormat {
        self.format
    }
}

/// Réglages de l'indexation.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Taille des lectures, et intervalle (en octets) entre deux consultations de l'annulation
    /// et de l'horloge de publication. Les lectures sont bornées à [16 Kio, 4 Mio].
    pub chunk_size: usize,
    /// Intervalle entre deux publications (zéro : à chaque bloc).
    pub publish_every: Duration,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            chunk_size: 4 << 20,
            publish_every: Duration::from_millis(100),
        }
    }
}

/// Fin anticipée de l'indexation ; l'index partiel publié reste valable.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error(transparent)]
    Io(#[from] IoError),
    #[error("indexation annulée")]
    Cancelled,
}

// ------------------------------------------------------------------ entrées d'un PID

/// AU résolue : positions dans le fichier, horodatages bruts et métadonnées.
#[derive(Debug, Clone, Copy)]
struct ResolvedAu {
    es_start: u64,
    es_end: u64,
    start: EsPos,
    end: EsPos,
    packet_index: u64,
    pes_packet: u64,
    pts: Option<u64>,
    dts: Option<u64>,
    damaged: bool,
    /// Un trou précède immédiatement l'AU.
    gap_before: bool,
    meta: AuMeta,
}

/// Point de reprise en attente de son image R.
#[derive(Debug, Clone, Copy)]
struct Recovery {
    frame_cnt: u32,
    /// AU de référence vues dans [RAP, AU courante).
    refs: u32,
    /// Index de décodage de R.
    r: Option<u32>,
}

impl Recovery {
    /// Prend en compte l'AU `decode_idx` (de référence ou non).
    fn see(&mut self, decode_idx: u32, reference: bool) {
        if self.r.is_some() {
            return;
        }
        if self.refs == self.frame_cnt {
            self.r = Some(decode_idx);
        } else {
            self.refs += u32::from(reference);
        }
    }
}

/// Entrée vue, pas encore publiée.
#[derive(Debug, Clone, Copy)]
struct PendingEntry {
    rec: Record,
    recovery: Option<Recovery>,
}

/// Entrées à ajouter à un [`VideoIndex`].
#[derive(Debug, Default)]
struct Batch {
    records: Vec<Record>,
    by_pres: Vec<u32>,
}

/// PTS récents (comptés : un PTS réel peut se répéter).
#[derive(Debug, Default)]
struct RecentPts {
    order: VecDeque<i64>,
    count: BTreeMap<i64, u32>,
}

impl RecentPts {
    fn contains(&self, pts: i64) -> bool {
        self.count.contains_key(&pts)
    }

    fn insert(&mut self, pts: i64) {
        *self.count.entry(pts).or_insert(0) += 1;
        self.order.push_back(pts);
        if self.order.len() > RECENT_PTS
            && let Some(old) = self.order.pop_front()
            && let Some(n) = self.count.get_mut(&old)
        {
            *n -= 1;
            if *n == 0 {
                self.count.remove(&old);
            }
        }
    }
}

/// Signé de `a - b` modulo 2^33, ramené dans [-2^32, 2^32).
fn wrap_diff(a: u64, b: u64) -> i64 {
    let d = i64::try_from(a.wrapping_sub(b) & MASK_33).unwrap_or(0);
    if d >= 1 << 32 { d - (1 << 33) } else { d }
}

/// Construit les entrées d'un PID à partir des AU résolues, en ordre de décodage, et décide du
/// préfixe publiable.
///
/// - `RapClass` devient `RapKind` ; `Intra { broken_link: true }` donne `Bla`.
/// - `Recovery { frame_cnt }` : R est la 1re AU, à partir du RAP, telle que [RAP, R) compte
///   `frame_cnt` AU de référence ; `pres_span = pres(R) − pres(RAP)`, borné à 0. Sans R en fin
///   de flux, `pres_span` couvre toutes les images présentées à partir du RAP.
/// - Images de tête : HEVC d'après le type de NAL ; H.264 et MPEG-2, une AU non RAP présentée
///   avant le dernier RAP (ordre de décodage) est `Skippable`, `Decodable` si ce RAP est une
///   `Idr` ou une `Intra` de GOP fermé. Avant le 1er RAP du flux : `None` (le Navigator rend
///   alors `BeforeFirstRap` ; les RASL d'une 1re CRA rendent `LeadingOfFirstRap`).
struct EntryBuilder {
    pid: u16,
    codec: Codec,
    held: Option<ResolvedAu>,
    pts_unwrap: TsUnwrapper,
    /// PTS de l'AU précédente et s'il était réel.
    prev_pts: Option<(i64, bool)>,
    diffs: VecDeque<i64>,
    recent: RecentPts,
    /// PTS du dernier RAP et images de tête décodables.
    last_rap: Option<(i64, bool)>,
    pending: Vec<PendingEntry>,
    /// Index de décodage des points de reprise non résolus.
    unresolved: Vec<u32>,
    /// Entrées déjà publiées.
    base: u32,
    published_max: Option<i64>,
}

impl EntryBuilder {
    fn new(pid: u16, codec: Codec, base: u32) -> Self {
        EntryBuilder {
            pid,
            codec,
            held: None,
            pts_unwrap: TsUnwrapper::new(33),
            prev_pts: None,
            diffs: VecDeque::with_capacity(PTS_DIFFS),
            recent: RecentPts::default(),
            last_rap: None,
            pending: Vec::new(),
            unresolved: Vec::new(),
            base,
            published_max: None,
        }
    }

    fn push(&mut self, au: ResolvedAu, sink: &mut dyn MessageSink) {
        if let Some(first) = self.held.take() {
            if self.pairs(&first, &au) {
                self.add(first, Some(au), sink);
                return;
            }
            self.add(first, None, sink);
        }
        if au.meta.field == Field::Frame {
            self.add(au, None, sink);
        } else {
            self.held = Some(au);
        }
    }

    /// Fin du flux : un champ seul devient une entrée.
    fn flush(&mut self, sink: &mut dyn MessageSink) {
        if let Some(first) = self.held.take() {
            self.add(first, None, sink);
        }
    }

    /// `b` est le champ complémentaire de `a`, qu'il suit immédiatement.
    fn pairs(&self, a: &ResolvedAu, b: &ResolvedAu) -> bool {
        let opposite = matches!(
            (a.meta.field, b.meta.field),
            (Field::Top, Field::Bottom) | (Field::Bottom, Field::Top)
        );
        let same_frame = self.codec != Codec::H264
            || (a.meta.frame_num.is_some() && a.meta.frame_num == b.meta.frame_num);
        opposite && same_frame && a.es_end == b.es_start
    }

    fn estimate_pts(&self) -> i64 {
        let step = if self.diffs.is_empty() {
            DEFAULT_PTS_STEP
        } else {
            let mut d: Vec<i64> = self.diffs.iter().copied().collect();
            d.sort_unstable();
            d[d.len() / 2]
        };
        let mut pts = self.prev_pts.map_or(0, |(p, _)| p.saturating_add(step));
        while self.recent.contains(pts) {
            pts += 1;
        }
        pts
    }

    /// PTS (déroulé, ou estimé avec `ts-au-no-pts`) et DTS d'une entrée.
    fn timestamps(
        &mut self,
        first: &ResolvedAu,
        second: Option<&ResolvedAu>,
        decode_idx: u32,
        sink: &mut dyn MessageSink,
    ) -> (i64, Option<i64>) {
        let timed = if first.pts.is_some() {
            Some(first)
        } else {
            second.filter(|s| s.pts.is_some())
        };
        let raw = timed.and_then(|t| t.pts.map(|p| (p, t.dts)));
        let (pts, dts) = if let Some((p, d)) = raw {
            let pts = self.pts_unwrap.unwrap(p);
            (pts, Some(pts - d.map_or(0, |d| wrap_diff(p, d))))
        } else {
            sink.push(
                Message::new(&AU_NO_PTS, Severity::Warning, Family::Video, SUB_FAMILY)
                    .param("pid", Param::Pid(self.pid))
                    .at(first.start.packet_offset)
                    .pid(self.pid)
                    .frame(decode_idx),
            );
            (self.estimate_pts(), None)
        };
        let real = raw.is_some();
        if let Some((prev, true)) = self.prev_pts
            && real
            && pts > prev
        {
            if self.diffs.len() == PTS_DIFFS {
                self.diffs.pop_front();
            }
            self.diffs.push_back(pts - prev);
        }
        self.prev_pts = Some((pts, real));
        self.recent.insert(pts);
        (pts, dts)
    }

    /// Point d'accès et image de tête d'une entrée de PTS `pts`.
    fn classify(&mut self, meta: &AuMeta, pts: i64) -> (RapTag, Leading) {
        let (rap, decodable_leading) = match meta.rap {
            RapClass::None => (RapTag::None, false),
            RapClass::Idr => (RapTag::Idr, true),
            RapClass::Cra => (RapTag::Cra, false),
            RapClass::Bla => (RapTag::Bla, false),
            RapClass::Recovery { .. } => (RapTag::Recovery, false),
            RapClass::Intra {
                closed_gop,
                broken_link,
            } => (
                match (broken_link, closed_gop) {
                    (true, _) => RapTag::Bla,
                    (false, true) => RapTag::IntraClosed,
                    (false, false) => RapTag::Intra,
                },
                closed_gop,
            ),
        };
        let leading = if self.codec == Codec::Hevc {
            match meta.leading {
                Some(NalLeading::Skippable) => Leading::Skippable,
                Some(NalLeading::Decodable) => Leading::Decodable,
                None => Leading::None,
            }
        } else {
            match self.last_rap {
                Some((rap_pts, true)) if rap == RapTag::None && pts < rap_pts => Leading::Decodable,
                Some((rap_pts, false)) if rap == RapTag::None && pts < rap_pts => {
                    Leading::Skippable
                }
                _ => Leading::None,
            }
        };
        if rap != RapTag::None {
            self.last_rap = Some((pts, decodable_leading));
        }
        (rap, leading)
    }

    /// Fait avancer les points de reprise en attente ; rend celui de l'entrée si c'en est un.
    fn recoveries(&mut self, meta: &AuMeta, decode_idx: u32, reference: bool) -> Option<Recovery> {
        for &d in &self.unresolved {
            if let Some(rc) = self
                .pending
                .get_mut((d - self.base) as usize)
                .and_then(|e| e.recovery.as_mut())
            {
                rc.see(decode_idx, reference);
            }
        }
        let RapClass::Recovery { frame_cnt } = meta.rap else {
            return None;
        };
        let mut rc = Recovery {
            frame_cnt,
            refs: 0,
            r: None,
        };
        rc.see(decode_idx, reference);
        self.unresolved.push(decode_idx);
        Some(rc)
    }

    /// Ajoute une entrée (une AU, ou une paire de champs).
    fn add(&mut self, first: ResolvedAu, second: Option<ResolvedAu>, sink: &mut dyn MessageSink) {
        let decode_idx = self.base + u32::try_from(self.pending.len()).unwrap_or(u32::MAX);
        let last = second.unwrap_or(first);
        let (pts, dts) = self.timestamps(&first, second.as_ref(), decode_idx, sink);
        let meta = first.meta;
        let (rap, leading) = self.classify(&meta, pts);
        let reference = meta.reference || second.is_some_and(|s| s.meta.reference);
        let recovery = self.recoveries(&meta, decode_idx, reference);
        let mut params = meta.params;
        let mut damaged = first.damaged;
        if let Some(s) = second {
            params.insert(s.meta.params);
            damaged |= s.damaged || s.gap_before;
        }
        let flags = if dts.is_none() {
            FLAG_ESTIMATED
        } else {
            FLAG_DTS
        } | if damaged { FLAG_DAMAGED } else { 0 };
        let rec = Record {
            start_packet: first.start.packet_offset,
            end_packet: last.end.packet_offset,
            pts,
            dts: dts.unwrap_or(0),
            pes_packet: first.pes_packet,
            packet_index: first.packet_index,
            size: u32::try_from(last.es_end - first.es_start).unwrap_or(u32::MAX),
            pres_idx: 0,
            pres_span: 0,
            start_skip: first.start.skip,
            end_skip: last.end.skip,
            flags,
            params,
            nal_type: meta.first_vcl_type,
            fields: 1 + u8::from(second.is_some()),
            picture: meta.picture,
            rap,
            leading,
        };
        self.pending.push(PendingEntry { rec, recovery });
        self.unresolved.retain(|&d| {
            self.pending
                .get((d - self.base) as usize)
                .and_then(|e| e.recovery)
                .is_some_and(|rc| rc.r.is_none())
        });
    }

    /// Nombre d'entrées en attente publiables (règle du préfixe L, voir le module).
    fn cut(&self) -> Option<usize> {
        let p = &self.pending;
        let last_rap = p.iter().rposition(|e| e.rec.rap != RapTag::None)?;
        let mut suffix_min = vec![i64::MAX; p.len() + 1];
        for i in (0..p.len()).rev() {
            suffix_min[i] = suffix_min[i + 1].min(p[i].rec.pts);
        }
        let mut prefix_max = self.published_max.unwrap_or(i64::MIN);
        // [0, k) doit contenir R de chaque point de reprise de [0, k).
        let mut need = 0;
        let mut best = None;
        for k in 0..last_rap {
            if k > 0 && need <= k && p[k].rec.rap != RapTag::None && prefix_max < suffix_min[k] {
                best = Some(k);
            }
            prefix_max = prefix_max.max(p[k].rec.pts);
            if let Some(rc) = p[k].recovery {
                match rc.r {
                    Some(r) => need = need.max((r - self.base) as usize + 1),
                    None => break,
                }
            }
        }
        best
    }

    /// Entrées à publier : le préfixe L, ou tout (`all`, fin du flux).
    fn publish(&mut self, all: bool) -> Option<Batch> {
        let k = if all { self.pending.len() } else { self.cut()? };
        if k == 0 {
            return None;
        }
        let mut batch: Vec<PendingEntry> = self.pending.drain(..k).collect();
        let mut order: Vec<usize> = (0..k).collect();
        order.sort_by_key(|&i| (batch[i].rec.pts, i));
        let base = self.base;
        let to_u32 = |i: usize| base + u32::try_from(i).unwrap_or(u32::MAX);
        let mut by_pres = Vec::with_capacity(k);
        for (rank, &i) in order.iter().enumerate() {
            batch[i].rec.pres_idx = to_u32(rank);
            by_pres.push(to_u32(i));
        }
        let end = to_u32(k);
        for i in 0..k {
            if let Some(rc) = batch[i].recovery {
                let pres = batch[i].rec.pres_idx;
                let until =
                    rc.r.and_then(|r| batch.get((r - base) as usize))
                        .map_or(end, |e| e.rec.pres_idx);
                batch[i].rec.pres_span = until.saturating_sub(pres);
            }
        }
        let max = batch.iter().map(|e| e.rec.pts).max();
        self.published_max = self.published_max.max(max);
        self.base = end;
        self.unresolved.retain(|&d| d >= end);
        Some(Batch {
            records: batch.into_iter().map(|e| e.rec).collect(),
            by_pres,
        })
    }
}

// ------------------------------------------------------------------ PES et AU d'un PID

/// Octets d'ES d'un paquet (table de correspondance).
#[derive(Debug, Clone, Copy)]
struct ChunkRec {
    es_start: u64,
    len: u64,
    pos: EsPos,
    packet_index: u64,
    /// Numéro du PES qui porte le chunk.
    pes: u64,
}

impl ChunkRec {
    /// Position de l'octet d'ES `off` (dans le chunk, ou juste après son dernier octet).
    fn at(&self, off: u64) -> Option<EsPos> {
        let n = u16::try_from(off.checked_sub(self.es_start)?).ok()?;
        Some(EsPos {
            packet_offset: self.pos.packet_offset,
            skip: self.pos.skip.checked_add(n)?,
        })
    }
}

/// Début de PES.
#[derive(Debug, Clone, Copy)]
struct PesRec {
    id: u64,
    es_start: u64,
    pts: Option<u64>,
    dts: Option<u64>,
    packet_offset: u64,
    /// Une AU commence déjà dans ce PES.
    taken: bool,
}

/// Indexation d'un PID vidéo.
struct Track {
    pid: u16,
    codec: Codec,
    asm: PesAssembler,
    framer: Box<dyn AuFramer>,
    es_total: u64,
    pes_seq: u64,
    chunks: VecDeque<ChunkRec>,
    pes: VecDeque<PesRec>,
    /// Offsets d'ES où des octets ont été perdus.
    gaps: VecDeque<u64>,
    out: Vec<AuBoundary>,
    /// Fin de la dernière AU émise.
    last_end: u64,
    builder: EntryBuilder,
}

impl Track {
    fn new(pid: u16, codec: Codec, base: u32) -> Self {
        Track {
            pid,
            codec,
            asm: PesAssembler::new(pid),
            framer: ma_es::framer(codec),
            es_total: 0,
            pes_seq: 0,
            chunks: VecDeque::new(),
            pes: VecDeque::new(),
            gaps: VecDeque::new(),
            out: Vec::new(),
            last_end: 0,
            builder: EntryBuilder::new(pid, codec, base),
        }
    }

    fn push(&mut self, pkt: &ScannedPacket<'_>, sink: &mut dyn MessageSink) {
        let Some(c) = self.asm.push(pkt, sink) else {
            return;
        };
        if c.gap_before {
            self.framer.gap();
            self.gaps.push_back(self.es_total);
        }
        if let Some(s) = c.pes_start {
            self.pes_seq += 1;
            self.pes.push_back(PesRec {
                id: self.pes_seq,
                es_start: self.es_total,
                pts: s.pts,
                dts: s.dts,
                packet_offset: s.packet_offset,
                taken: false,
            });
        }
        if c.data.is_empty() {
            return;
        }
        // Ancrage au paquet du PUSI quand l'en-tête PES couvrait plusieurs paquets.
        let packet_index = match c.pes_start {
            Some(s) if c.pos.packet_offset != pkt.offset => s.packet_index,
            _ => pkt.index,
        };
        let len = c.data.len() as u64;
        self.chunks.push_back(ChunkRec {
            es_start: self.es_total,
            len,
            pos: c.pos,
            packet_index,
            pes: self.pes_seq,
        });
        self.framer.push(c.data, &mut self.out);
        self.es_total += len;
        self.drain(sink);
    }

    fn finish(&mut self, sink: &mut dyn MessageSink) {
        self.framer.finish(&mut self.out);
        self.drain(sink);
        self.builder.flush(sink);
    }

    fn drain(&mut self, sink: &mut dyn MessageSink) {
        if self.out.is_empty() {
            return;
        }
        let out = std::mem::take(&mut self.out);
        for b in &out {
            if let Some(au) = self.resolve(b, sink) {
                self.builder.push(au, sink);
            }
            self.last_end = self.last_end.max(b.end);
        }
        self.out = out;
        self.out.clear();
        self.prune();
    }

    fn chunk_at(&self, off: u64) -> Option<&ChunkRec> {
        let i = self.chunks.partition_point(|c| c.es_start + c.len <= off);
        self.chunks.get(i).filter(|c| c.es_start <= off)
    }

    /// Positions, PTS et dégâts d'une frontière d'AU ; `None` (avec message) si l'AU dépasse
    /// `MAX_AU_BYTES`.
    fn resolve(&mut self, b: &AuBoundary, sink: &mut dyn MessageSink) -> Option<ResolvedAu> {
        let size = b.end.checked_sub(b.start).filter(|&s| s > 0)?;
        let first = self.chunk_at(b.start).copied();
        let last = self.chunk_at(b.end - 1).copied();
        let fits = size <= MAX_AU_BYTES as u64;
        let (Some(first), Some(last), true) = (first, last, fits) else {
            let mut m = Message::new(&AU_TOO_LARGE, Severity::Warning, Family::Video, SUB_FAMILY)
                .param("pid", Param::Pid(self.pid))
                .param("size", Param::UInt(size))
                .pid(self.pid);
            if let Some(c) = first {
                m = m.at(c.pos.packet_offset);
            }
            sink.push(m);
            return None;
        };
        let start = first.at(b.start)?;
        let end = last.at(b.end)?;
        let damaged = self.gaps.iter().any(|&g| g > b.start && g < b.end);
        let gap_before = self.gaps.contains(&b.start);
        // Le PES de l'AU est celui de la fin de son code de début (`b.start + 2`) : l'octet nul
        // qui précède `00 00 01` (`zero_byte`, ou zéro de fin de l'image précédente en MPEG-2)
        // peut être le dernier octet du PES précédent.
        let mark = (b.start + 2).min(b.end - 1);
        let owner = self.chunk_at(mark).map_or(first.pes, |c| c.pes);
        let front = self.pes.front().map_or(0, |p| p.id);
        let i = usize::try_from(owner.checked_sub(front)?).ok()?;
        let pes = self.pes.get_mut(i)?;
        let mut timing = (None, None);
        if !pes.taken {
            pes.taken = true;
            let (from, to) = (pes.es_start, mark);
            if !self.gaps.iter().any(|&g| g > from && g <= to) {
                timing = (pes.pts, pes.dts);
            }
        }
        Some(ResolvedAu {
            es_start: b.start,
            es_end: b.end,
            start,
            end,
            packet_index: first.packet_index,
            pes_packet: pes.packet_offset,
            pts: timing.0,
            dts: timing.1,
            damaged,
            gap_before,
            meta: b.meta,
        })
    }

    /// Oublie les chunks, PES et trous qu'aucune AU à venir ne peut référencer.
    fn prune(&mut self) {
        while let Some(c) = self.chunks.front() {
            let used = c.es_start + c.len <= self.last_end;
            let too_old = self.es_total - c.es_start > CHUNK_WINDOW;
            if !used && !too_old {
                break;
            }
            self.chunks.pop_front();
        }
        let keep_pes = self.chunks.front().map_or(self.pes_seq, |c| c.pes);
        while self.pes.front().is_some_and(|p| p.id < keep_pes) {
            self.pes.pop_front();
        }
        let floor = self
            .pes
            .front()
            .map_or(self.es_total, |p| p.es_start)
            .min(self.chunks.front().map_or(self.es_total, |c| c.es_start));
        while self.gaps.front().is_some_and(|&g| g < floor) {
            self.gaps.pop_front();
        }
    }
}

// ------------------------------------------------------------------ indexeur

/// PCR d'un PID de PCR, en attente de publication.
struct PcrTrack {
    unwrap: TsUnwrapper,
    buf: Vec<PcrSample>,
}

struct Indexer<'o> {
    out: &'o TsIndex,
    structure: StructureBuilder,
    generation: u32,
    /// PID → 1 + position dans `tracks` ; 0 : non indexé.
    slot: Vec<u16>,
    /// Même ordre que `Shared::videos` ; `None` : PID retiré (index figé).
    tracks: Vec<Option<Track>>,
    pcr_slot: Vec<bool>,
    pcr: BTreeMap<u16, PcrTrack>,
}

impl<'o> Indexer<'o> {
    fn new(out: &'o TsIndex) -> Self {
        Indexer {
            out,
            structure: StructureBuilder::new(out.format()),
            generation: 0,
            slot: vec![0; PID_COUNT],
            tracks: Vec::new(),
            pcr_slot: vec![false; PID_COUNT],
            pcr: BTreeMap::new(),
        }
    }

    fn packet(&mut self, p: &ScannedPacket<'_>, sink: &mut dyn MessageSink) {
        self.structure.push(p, sink);
        if self.structure.generation() != self.generation {
            self.generation = self.structure.generation();
            self.follow_structure(sink);
        }
        let pid = usize::from(p.packet.header.pid);
        if self.pcr_slot.get(pid) == Some(&true)
            && let Some(pcr) = p.packet.adaptation.and_then(|a| a.pcr.map(|v| (v, a)))
            && let Some(t) = self.pcr.get_mut(&p.packet.header.pid)
        {
            let (v, af) = pcr;
            let base = t.unwrap.unwrap(v.base);
            t.buf.push(PcrSample {
                packet_index: p.index,
                offset: p.offset,
                pcr: base.saturating_mul(300) + i64::from(v.ext),
                discontinuity: af.discontinuity,
            });
        }
        let slot = usize::from(self.slot.get(pid).copied().unwrap_or(0));
        if let Some(t) = slot
            .checked_sub(1)
            .and_then(|i| self.tracks.get_mut(i))
            .and_then(Option::as_mut)
        {
            t.push(p, sink);
        }
    }

    /// Suit une nouvelle structure : PID vidéo retirés figés (fin de flux), nouveaux PID indexés,
    /// PID de PCR mis à jour.
    fn follow_structure(&mut self, sink: &mut dyn MessageSink) {
        let s = self.structure.structure();
        let mut wanted: Vec<(u16, Codec)> = Vec::new();
        for st in s.programs.iter().flat_map(|p| &p.streams) {
            if let (true, StreamKind::Video(Some(codec))) = (st.analysed, st.kind)
                && wanted.iter().all(|w| w.0 != st.pid)
            {
                wanted.push((st.pid, codec));
            }
        }
        for i in 0..self.tracks.len() {
            let keep = self.tracks[i]
                .as_ref()
                .is_none_or(|t| wanted.contains(&(t.pid, t.codec)));
            if keep {
                continue;
            }
            if let Some(mut t) = self.tracks[i].take() {
                self.slot[usize::from(t.pid)] = 0;
                t.finish(sink);
                if let Some(b) = t.builder.publish(true) {
                    Self::apply(&mut self.out.write(), i, b);
                }
            }
        }
        let mut sh = self.out.write();
        for (pid, codec) in wanted {
            let p = usize::from(pid);
            if self.slot[p] != 0 {
                continue;
            }
            match sh.videos.iter().position(|v| v.pid == pid) {
                // PID revenu : son index (figé) reprend, en ajout.
                Some(i) if sh.videos[i].codec == codec && self.tracks[i].is_none() => {
                    self.tracks[i] = Some(Track::new(pid, codec, sh.videos[i].published()));
                    self.slot[p] = u16::try_from(i + 1).unwrap_or(0);
                }
                // Même PID, autre codec : non indexé (un PID n'a qu'un index).
                Some(_) => {}
                None => {
                    sh.videos.push(VideoIndex::new(pid, codec));
                    self.tracks.push(Some(Track::new(pid, codec, 0)));
                    self.slot[p] = u16::try_from(self.tracks.len()).unwrap_or(0);
                }
            }
        }
        self.pcr_slot.fill(false);
        for pcr in s.programs.iter().filter_map(|p| p.pcr_pid) {
            if let Some(f) = self.pcr_slot.get_mut(usize::from(pcr)) {
                *f = true;
            }
            self.pcr.entry(pcr).or_insert_with(|| PcrTrack {
                unwrap: TsUnwrapper::new(33),
                buf: Vec::new(),
            });
        }
        sh.structure = s;
    }

    fn apply(sh: &mut Shared, i: usize, b: Batch) {
        if let Some(v) = sh.videos.get_mut(i) {
            v.records.extend(b.records);
            v.by_pres.extend(b.by_pres);
        }
    }

    /// Publie le préfixe stable (ou tout, `all`) et l'avancement.
    fn publish(&mut self, all: bool, done: u64) {
        let batches: Vec<(usize, Batch)> = self
            .tracks
            .iter_mut()
            .enumerate()
            .filter_map(|(i, t)| Some((i, t.as_mut()?.builder.publish(all)?)))
            .collect();
        let mut sh = self.out.write();
        for (i, b) in batches {
            Self::apply(&mut sh, i, b);
        }
        for (&pid, t) in &mut self.pcr {
            if !t.buf.is_empty() {
                sh.pcrs.entry(pid).or_default().append(&mut t.buf);
            }
        }
        sh.progress.bytes_done = done;
        sh.progress.complete = all;
    }

    fn finish(&mut self, sink: &mut dyn MessageSink) {
        for t in self.tracks.iter_mut().flatten() {
            t.finish(sink);
        }
    }
}

/// Source vue par l'indexeur : une lecture courte avant la taille d'ouverture est une
/// troncature. Les octets lus avant sont rendus (le scanner les traite, puis s'arrête) et
/// l'erreur est retenue : celle de la source à la relecture, sinon `Truncated`.
struct Guarded<'s> {
    inner: &'s dyn ReadAt,
    failure: Mutex<Option<IoError>>,
}

impl Guarded<'_> {
    fn fail(&self, e: IoError) {
        let mut f = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        f.get_or_insert(e);
    }

    fn take_failure(&self) -> Option<IoError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl ReadAt for Guarded<'_> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let avail = self.len().saturating_sub(offset);
        let want = usize::try_from(avail).map_or(buf.len(), |a| a.min(buf.len()));
        let mut done = 0;
        while done < want {
            let at = offset + done as u64;
            match self.inner.read_at(at, &mut buf[done..want]) {
                Ok(0) => {
                    self.fail(IoError::Truncated {
                        expected_len: self.len(),
                        actual_len: at,
                    });
                    break;
                }
                Ok(n) => done += n,
                Err(e) if done == 0 => return Err(e),
                Err(e) => {
                    self.fail(e);
                    break;
                }
            }
        }
        Ok(done)
    }
}

/// Indexe `src` à partir de `start` (octet 0x47 du 1er paquet) dans `out`, en un seul passage.
///
/// Toutes les `publish_every`, le préfixe stable est publié ; `cancel` est consulté à chaque
/// bloc de `chunk_size` octets. En fin de fichier, tout est publié et `progress().complete`
/// devient vrai.
///
/// # Errors
/// `Cancelled` si `cancel` passe à vrai, `Io` si la source échoue ou est tronquée (une lecture
/// courte avant sa taille d'ouverture). Dans les deux cas, le préfixe stable déjà vu est publié
/// et reste valable.
pub fn index(
    src: &dyn ReadAt,
    start: u64,
    out: &TsIndex,
    opts: &IndexOptions,
    sink: &mut dyn MessageSink,
    cancel: &AtomicBool,
) -> Result<(), IndexError> {
    let guarded = Guarded {
        inner: src,
        failure: Mutex::new(None),
    };
    let format = out.format();
    let mut scanner = PacketScanner::new(&guarded, format, start, None).with_block(opts.chunk_size);
    let mut ix = Indexer::new(out);
    let step = opts.chunk_size.max(1) as u64;
    let mut next_check = start;
    let mut last_publish = Instant::now();
    let mut done = start;
    loop {
        if done >= next_check {
            next_check = done + step;
            if cancel.load(Ordering::Relaxed) {
                ix.publish(false, done);
                return Err(IndexError::Cancelled);
            }
            if last_publish.elapsed() >= opts.publish_every {
                ix.publish(false, done);
                last_publish = Instant::now();
            }
        }
        match scanner.next(sink) {
            Ok(Some(p)) => {
                done = p.offset + format.tail() as u64;
                ix.packet(&p, sink);
            }
            Ok(None) => break,
            Err(e) => {
                ix.publish(false, done);
                return Err(IndexError::Io(e));
            }
        }
    }
    if let Some(e) = guarded.take_failure() {
        ix.publish(false, done);
        return Err(IndexError::Io(e));
    }
    ix.finish(sink);
    ix.publish(true, src.len());
    Ok(())
}

/// Indexation en tâche de fond (fil `ma-index`). L'abandon du handle annule puis attend.
pub struct IndexHandle {
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), IndexError>>>,
    spawn_error: Option<IoError>,
}

/// Lance [`index`] sur un fil `ma-index`.
#[must_use]
pub fn spawn_index(
    src: Arc<dyn ReadAt>,
    start: u64,
    out: Arc<TsIndex>,
    opts: IndexOptions,
    mut sink: Box<dyn MessageSink>,
) -> IndexHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name("ma-index".into())
        .spawn(move || index(&*src, start, &out, &opts, &mut *sink, &flag));
    let (thread, spawn_error) = match spawned {
        Ok(t) => (Some(t), None),
        Err(e) => (None, Some(IoError::Os(e))),
    };
    IndexHandle {
        cancel,
        thread,
        spawn_error,
    }
}

impl IndexHandle {
    /// Demande l'arrêt ; l'indexation s'arrête au bloc suivant.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Attend la fin de l'indexation.
    ///
    /// # Errors
    /// Celle de [`index`] ; un fil qui n'a pas pu démarrer donne `Io(Os)`, un fil qui a paniqué
    /// `Io(InvalidData("ma-index-panic"))`.
    pub fn join(mut self) -> Result<(), IndexError> {
        self.wait()
    }

    fn wait(&mut self) -> Result<(), IndexError> {
        if let Some(e) = self.spawn_error.take() {
            return Err(IndexError::Io(e));
        }
        match self.thread.take() {
            Some(t) => t.join().unwrap_or(Err(IndexError::Io(PANICKED))),
            None => Ok(()),
        }
    }
}

impl Drop for IndexHandle {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.wait();
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
mod tests {
    use super::*;
    use crate::pes::MAX_AU_BYTES;
    use crate::read_es;
    use crate::testutil::{AfSpec, packet, pat, pes_bytes, pes_packets, pmt, psi_packets};
    use ma_bits::BitWriter;
    use ma_core::{Pcr, PictureType, RapKind, VecSink};
    use ma_io::testing::MemSource;
    use std::collections::BTreeMap;

    const SH: [u8; 9] = [0xB3, 0x2D, 0x02, 0x40, 0x13, 0xFF, 0xFF, 0xE0, 0x18];
    const SEQ_EXT: [u8; 7] = [0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00];
    const SLICE: [u8; 4] = [0x01, 0x2B, 0x7E, 0x7F];
    const PMT_PID: u16 = 0x1000;

    fn unit(bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1];
        v.extend_from_slice(bytes);
        v
    }

    fn gop() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(25, 0x0800);
        w.u(1, 1);
        w.u(1, 0);
        w.u(5, 0);
        let mut v = vec![0xB8];
        v.extend(w.finish());
        v
    }

    fn picture(pct: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(10, 5);
        w.u(3, pct);
        w.u(16, 0xFFFF);
        if pct >= 2 {
            w.u(4, 7);
        }
        if pct == 3 {
            w.u(4, 7);
        }
        let mut v = vec![0x00];
        v.extend(w.finish());
        v
    }

    fn pce(structure: u64) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(4, 8);
        w.u(16, 0xFFFF);
        w.u(2, 0);
        w.u(2, structure);
        w.u(8, 0x80);
        let mut v = vec![0xB5];
        v.extend(w.finish());
        v
    }

    /// AU MPEG-2 : I (1) précédée de SH, extension de séquence et GOP fermé, ou P (2) ;
    /// `structure` : 1 champ haut, 2 champ bas, 3 image entière.
    fn au(pct: u64, structure: u64) -> Vec<u8> {
        let mut v = Vec::new();
        if pct == 1 {
            v.extend(unit(&SH));
            v.extend(unit(&SEQ_EXT));
            v.extend(unit(&gop()));
        }
        v.extend(unit(&picture(pct)));
        v.extend(unit(&pce(structure)));
        v.extend(unit(&SLICE));
        v.extend([0x55; 300]);
        v
    }

    /// Flux TS en construction (compteurs de continuité par PID).
    #[derive(Default)]
    struct Ts {
        packets: Vec<[u8; 188]>,
        cc: BTreeMap<u16, u8>,
    }

    impl Ts {
        fn next_cc(&mut self, pid: u16, n: usize) -> u8 {
            let cc = self.cc.entry(pid).or_insert(0);
            let first = *cc;
            *cc = cc.wrapping_add(n as u8) & 0x0F;
            first
        }

        fn psi(&mut self, pid: u16, section: &[u8]) {
            let cc = self.next_cc(pid, 0);
            let p = psi_packets(pid, section, cc);
            self.next_cc(pid, p.len());
            self.packets.extend(p);
        }

        /// PAT (programme 1 → `PMT_PID`) et PMT `version` : `pcr`, flux MPEG-2 `video`.
        fn tables(&mut self, version: u8, pcr: u16, video: u16) {
            self.psi(0, &pat(1, 0, &[(1, PMT_PID)]));
            self.psi(PMT_PID, &pmt(1, version, pcr, &[], &[(0x02, video, &[])]));
        }

        fn pes(&mut self, pid: u16, pts: u64, es: &[u8], first_len: usize) {
            let cc = self.next_cc(pid, 0);
            let p = pes_packets(pid, cc, &pes_bytes(Some(pts), None, es), first_len);
            self.next_cc(pid, p.len());
            self.packets.extend(p);
        }

        fn bytes(&self) -> Vec<u8> {
            self.packets.iter().flatten().copied().collect()
        }
    }

    fn run(data: Vec<u8>) -> (TsIndex, VecSink, Result<(), IndexError>, MemSource) {
        let src = MemSource::new(data);
        let out = TsIndex::new(PacketFormat::Ts188, src.len());
        let mut sink = VecSink::default();
        let r = index(
            &src,
            0,
            &out,
            &IndexOptions::default(),
            &mut sink,
            &AtomicBool::new(false),
        );
        (out, sink, r, src)
    }

    fn all(idx: &TsIndex, pid: u16) -> Vec<(FrameEntry, AuInfo)> {
        idx.with_video(pid, |v| {
            (0..v.published())
                .map(|d| (v.entry(d).unwrap(), v.au(d).unwrap()))
                .collect()
        })
        .unwrap_or_default()
    }

    #[test]
    fn au_info_is_compact() {
        assert!(size_of::<AuInfo>() <= 96, "{}", size_of::<AuInfo>());
        // Entrée stockée (AuInfo + FrameEntry compactés) : sous 96 octets aussi.
        assert!(size_of::<Record>() <= 96, "{}", size_of::<Record>());
    }

    #[test]
    fn field_pairs_make_one_entry() {
        const PID: u16 = 0x100;
        // (type, structure, PTS) : paire I, image P, paire P, champ haut seul, image P, paire I.
        let aus = [
            (1, 1, 3600),
            (2, 2, 5400),
            (2, 3, 7200),
            (2, 1, 10800),
            (2, 2, 12600),
            (2, 1, 14400),
            (2, 3, 18000),
            (1, 1, 21600),
            (2, 2, 23400),
        ];
        let mut ts = Ts::default();
        ts.tables(0, PID, PID);
        let bytes: Vec<Vec<u8>> = aus.iter().map(|&(t, s, _)| au(t, s)).collect();
        for (a, b) in aus.iter().zip(&bytes) {
            ts.pes(PID, a.2, b, 184);
        }
        let (idx, _, r, src) = run(ts.bytes());
        r.unwrap();
        let e = all(&idx, PID);
        let fields: Vec<u8> = e.iter().map(|(_, a)| a.fields).collect();
        assert_eq!(fields, [2, 1, 2, 1, 1, 2]);
        let pts: Vec<i64> = e.iter().map(|(f, _)| f.pts).collect();
        assert_eq!(pts, [3600, 7200, 10800, 14400, 18000, 21600]);
        let closed = RapKind::Intra { closed_gop: true };
        assert_eq!(e[0].0.rap, closed);
        assert_eq!(e[5].0.rap, closed);
        assert_eq!(e[0].0.picture, PictureType::I);
        // L'entrée couvre les deux champs : relecture = concaténation.
        let groups: [&[usize]; 6] = [&[0, 1], &[2], &[3, 4], &[5], &[6], &[7, 8]];
        let mut buf = Vec::new();
        for ((_, a), g) in e.iter().zip(groups) {
            let want: Vec<u8> = g.iter().flat_map(|&i| bytes[i].clone()).collect();
            read_es(&src, PacketFormat::Ts188, PID, a.start, a.end, &mut buf).unwrap();
            assert_eq!(buf, want);
            assert_eq!(a.size as usize, want.len());
        }
    }

    #[test]
    fn pes_header_across_packets_rereads_exactly() {
        const PID: u16 = 0x100;
        let mut ts = Ts::default();
        ts.tables(0, PID, PID);
        let bytes: Vec<Vec<u8>> = (0..6)
            .map(|i| au(if i % 3 == 0 { 1 } else { 2 }, 3))
            .collect();
        for (i, b) in bytes.iter().enumerate() {
            // En-tête PES (14 octets) réparti sur deux paquets pour une AU sur deux.
            ts.pes(
                PID,
                3600 * (i as u64 + 1),
                b,
                if i % 2 == 0 { 5 } else { 184 },
            );
        }
        let (idx, _, r, src) = run(ts.bytes());
        r.unwrap();
        let e = all(&idx, PID);
        assert_eq!(e.len(), 6);
        let mut buf = Vec::new();
        for ((_, a), want) in e.iter().zip(&bytes) {
            read_es(&src, PacketFormat::Ts188, PID, a.start, a.end, &mut buf).unwrap();
            assert_eq!(&buf, want);
        }
    }

    #[test]
    fn pmt_change_starts_new_video_index() {
        let mut ts = Ts::default();
        ts.tables(0, 0x100, 0x100);
        for (i, t) in [1, 2, 2, 1, 2].into_iter().enumerate() {
            ts.pes(0x100, 3600 * (i as u64 + 1), &au(t, 3), 184);
        }
        ts.tables(1, 0x101, 0x101);
        for (i, t) in [1, 2, 2, 1, 2].into_iter().enumerate() {
            ts.pes(0x101, 900_000 + 3600 * i as u64, &au(t, 3), 184);
        }
        // Paquets de l'ancien PID après le changement : ignorés.
        ts.pes(0x100, 7_000_000, &au(1, 3), 184);
        let (idx, sink, r, _) = run(ts.bytes());
        r.unwrap();
        assert_eq!(idx.videos(), [(0x100, Codec::Mpeg2), (0x101, Codec::Mpeg2)]);
        let old: Vec<i64> = all(&idx, 0x100).iter().map(|(f, _)| f.pts).collect();
        let new: Vec<i64> = all(&idx, 0x101).iter().map(|(f, _)| f.pts).collect();
        assert_eq!(old, [3600, 7200, 10800, 14400, 18000]);
        assert_eq!(new, [900_000, 903_600, 907_200, 910_800, 914_400]);
        let s = idx.structure();
        assert_eq!(s.programs[0].streams[0].pid, 0x101);
        assert!(sink.messages.iter().any(|m| m.id == "ts-pmt-changed"));
    }

    #[test]
    fn pcr_samples_follow_pcr_pids() {
        const PCR_PID: u16 = 0x1FF0;
        let mut ts = Ts::default();
        ts.tables(0, PCR_PID, 0x100);
        let top = (1u64 << 33) - 10;
        let first = ts.packets.len() as u64;
        for (i, (base, disc)) in [(top, false), (5, false), (100, true)]
            .into_iter()
            .enumerate()
        {
            let af = AfSpec {
                discontinuity: disc,
                random_access: false,
                pcr: Some(Pcr { base, ext: 7 }),
            };
            ts.packets
                .push(packet(PCR_PID, false, i as u8, Some(af), &[]));
        }
        ts.pes(0x100, 3600, &au(1, 3), 184);
        let (idx, _, r, _) = run(ts.bytes());
        r.unwrap();
        let p = idx.pcrs(PCR_PID);
        let wrap = 1i64 << 33;
        let ticks = |b: i64| b * 300 + 7;
        assert_eq!(
            p.iter().map(|s| s.pcr).collect::<Vec<_>>(),
            [ticks(wrap - 10), ticks(wrap + 5), ticks(wrap + 100)]
        );
        assert_eq!(
            p.iter().map(|s| s.discontinuity).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(p[0].packet_index, first);
        assert_eq!(p[0].offset, p[0].packet_index * 188);
        assert_eq!(idx.pcrs(0x100), []);
    }

    #[test]
    fn oversized_au_is_skipped_with_a_message() {
        const PID: u16 = 0x100;
        let mut ts = Ts::default();
        ts.tables(0, PID, PID);
        ts.pes(PID, 3600, &au(1, 3), 184);
        let mut big = au(2, 3);
        big.resize(MAX_AU_BYTES + 1000, 0x55);
        ts.pes(PID, 7200, &big, 184);
        ts.pes(PID, 10800, &au(2, 3), 184);
        let (idx, sink, r, _) = run(ts.bytes());
        r.unwrap();
        let pts: Vec<i64> = all(&idx, PID).iter().map(|(f, _)| f.pts).collect();
        assert_eq!(pts, [3600, 10800]);
        let m: Vec<_> = sink
            .messages
            .iter()
            .filter(|m| m.id == "ts-au-too-large")
            .collect();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].pid, Some(PID));
    }

    // ------------------------------------------------------------ constructeur d'entrées

    fn meta(rap: RapClass, reference: bool, picture: PictureType) -> AuMeta {
        AuMeta {
            rap,
            reference,
            picture,
            ..AuMeta::default()
        }
    }

    /// AU résolue n° `i` (100 octets d'ES, un paquet chacune).
    fn rau(i: u64, pts: Option<u64>, meta: AuMeta) -> ResolvedAu {
        ResolvedAu {
            es_start: i * 100,
            es_end: (i + 1) * 100,
            start: EsPos {
                packet_offset: i * 188,
                skip: 0,
            },
            end: EsPos {
                packet_offset: i * 188,
                skip: 100,
            },
            packet_index: i,
            pes_packet: i * 188,
            pts,
            dts: None,
            damaged: false,
            gap_before: false,
            meta,
        }
    }

    /// Entrées (décodage) d'un lot.
    fn kinds(b: &Batch) -> Vec<(u32, RapKind, Leading)> {
        b.records
            .iter()
            .map(|r| (r.pres_idx, r.rap_kind(), r.leading))
            .collect()
    }

    #[test]
    fn recovery_waits_for_r_and_spans_presentation() {
        use PictureType::{B, I, P};
        let rec = RapClass::Recovery { frame_cnt: 2 };
        // (PTS, classe, référence, type) en ordre de décodage.
        let aus = [
            (0, RapClass::Idr, true, I),
            (3, RapClass::None, true, P),
            (9, rec, true, I),
            (6, RapClass::None, false, B),
            (15, RapClass::None, true, P),
            (12, RapClass::None, false, B), // R : [2, 5) compte 2 AU de référence
            (21, RapClass::None, true, P),
            (24, RapClass::Idr, true, I),
            (27, RapClass::None, true, P),
            (30, RapClass::Idr, true, I),
        ];
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        let mut sink = VecSink::default();
        let mut feed = |b: &mut EntryBuilder, range: std::ops::Range<usize>| {
            for i in range {
                let (pts, rap, r, t) = aus[i];
                b.push(rau(i as u64, Some(pts), meta(rap, r, t)), &mut sink);
            }
        };
        feed(&mut b, 0..5);
        // Le RAP n° 2 est le dernier vu : rien n'est publiable.
        assert!(b.publish(false).is_none());
        feed(&mut b, 5..8);
        // L = 2 : le point de reprise (R = 5) n'est pas encore dans un préfixe publiable.
        let first = b.publish(false).unwrap();
        assert_eq!(first.records.len(), 2);
        assert_eq!(first.by_pres, [0, 1]);
        assert!(b.publish(false).is_none());
        feed(&mut b, 8..10);
        let second = b.publish(false).unwrap();
        assert_eq!(
            kinds(&second),
            [
                (3, RapKind::Recovery { pres_span: 1 }, Leading::None),
                (2, RapKind::None, Leading::Skippable),
                (5, RapKind::None, Leading::None),
                (4, RapKind::None, Leading::None),
                (6, RapKind::None, Leading::None),
            ]
        );
        assert_eq!(second.by_pres, [3, 2, 5, 4, 6]);
        let last = b.publish(true).unwrap();
        assert_eq!(last.records.len(), 3);
        assert!(b.publish(true).is_none());
    }

    #[test]
    fn unresolved_recovery_spans_to_the_end() {
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        let mut sink = VecSink::default();
        let rec = RapClass::Recovery { frame_cnt: 100 };
        b.push(rau(0, Some(0), meta(rec, true, PictureType::I)), &mut sink);
        for i in 1..4 {
            b.push(
                rau(i, Some(i * 10), meta(RapClass::None, true, PictureType::P)),
                &mut sink,
            );
        }
        b.push(
            rau(4, Some(40), meta(RapClass::Idr, true, PictureType::I)),
            &mut sink,
        );
        b.push(
            rau(5, Some(50), meta(RapClass::Idr, true, PictureType::I)),
            &mut sink,
        );
        // Le point de reprise bloque la publication (non résolu).
        assert!(b.publish(false).is_none());
        let all = b.publish(true).unwrap();
        assert_eq!(
            all.records[0].rap_kind(),
            RapKind::Recovery { pres_span: 6 }
        );
    }

    #[test]
    fn leading_pictures_follow_the_last_rap() {
        use PictureType::{B, I};
        let cases = [
            (Codec::H264, RapClass::Idr, RapKind::Idr, Leading::Decodable),
            (
                Codec::Mpeg2,
                RapClass::Intra {
                    closed_gop: true,
                    broken_link: false,
                },
                RapKind::Intra { closed_gop: true },
                Leading::Decodable,
            ),
            (
                Codec::Mpeg2,
                RapClass::Intra {
                    closed_gop: false,
                    broken_link: false,
                },
                RapKind::Intra { closed_gop: false },
                Leading::Skippable,
            ),
            (
                Codec::Mpeg2,
                RapClass::Intra {
                    closed_gop: false,
                    broken_link: true,
                },
                RapKind::Bla,
                Leading::Skippable,
            ),
            (Codec::Hevc, RapClass::Bla, RapKind::Bla, Leading::None),
        ];
        for (codec, class, kind, leading) in cases {
            let mut b = EntryBuilder::new(0x100, codec, 0);
            let mut sink = VecSink::default();
            b.push(rau(0, Some(100), meta(class, true, I)), &mut sink);
            b.push(rau(1, Some(50), meta(RapClass::None, false, B)), &mut sink);
            b.push(rau(2, Some(150), meta(RapClass::None, false, B)), &mut sink);
            let all = b.publish(true).unwrap();
            assert_eq!(
                kinds(&all),
                [
                    (1, kind, Leading::None),
                    (0, RapKind::None, leading),
                    (2, RapKind::None, Leading::None)
                ],
                "{codec:?} {class:?}"
            );
        }
        // HEVC : d'après le type de NAL.
        let mut b = EntryBuilder::new(0x100, Codec::Hevc, 0);
        let mut sink = VecSink::default();
        b.push(rau(0, Some(100), meta(RapClass::Cra, true, I)), &mut sink);
        for (i, l) in [NalLeading::Skippable, NalLeading::Decodable]
            .into_iter()
            .enumerate()
        {
            let mut m = meta(RapClass::None, false, B);
            m.leading = Some(l);
            b.push(rau(i as u64 + 1, Some(60 + i as u64), m), &mut sink);
        }
        let all = b.publish(true).unwrap();
        let leading: Vec<Leading> = all.records.iter().map(|r| r.leading).collect();
        assert_eq!(
            leading,
            [Leading::None, Leading::Skippable, Leading::Decodable]
        );
    }

    #[test]
    fn estimated_pts_use_the_median_step_and_stay_unique() {
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        let mut sink = VecSink::default();
        let p = |rap| meta(rap, true, PictureType::P);
        // Écarts positifs entre PTS réels consécutifs : 3000, 3000, 6000 ⇒ médiane 3000.
        for (i, pts) in [0, 3000, 6000, 12000, 9000].into_iter().enumerate() {
            let rap = if i == 0 {
                RapClass::Idr
            } else {
                RapClass::None
            };
            b.push(rau(i as u64, Some(pts), p(rap)), &mut sink);
        }
        // 9000 + 3000 = 12000 existe déjà ⇒ 12001, puis 15001.
        b.push(rau(5, None, p(RapClass::None)), &mut sink);
        b.push(rau(6, None, p(RapClass::None)), &mut sink);
        let all = b.publish(true).unwrap();
        let pts: Vec<(i64, Option<i64>)> =
            all.records.iter().map(|r| (r.pts, r.au().dts)).collect();
        assert_eq!(&pts[5..], [(12001, None), (15001, None)]);
        assert!(all.records[5].au().pts_estimated && !all.records[4].au().pts_estimated);
        assert_eq!(all.records[4].au().dts, Some(9000));
        assert_eq!(sink.messages.len(), 2);
        assert_eq!(sink.messages[0].decode_idx, Some(5));
        // Sans AU précédente : 0.
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        b.push(rau(0, None, p(RapClass::None)), &mut sink);
        assert_eq!(b.publish(true).unwrap().records[0].pts, 0);
    }

    #[test]
    fn dts_follows_pts_across_the_wrap() {
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        let mut sink = VecSink::default();
        let top = (1u64 << 33) - 1000;
        let mut a = rau(0, Some(top), meta(RapClass::Idr, true, PictureType::I));
        a.dts = Some(top - 3600);
        b.push(a, &mut sink);
        let mut a = rau(1, Some(500), meta(RapClass::None, true, PictureType::P));
        a.dts = Some(top - 1000);
        b.push(a, &mut sink);
        let all = b.publish(true).unwrap();
        let wrap = 1i64 << 33;
        let got: Vec<(i64, Option<i64>)> =
            all.records.iter().map(|r| (r.pts, r.au().dts)).collect();
        assert_eq!(
            got,
            [
                (wrap - 1000, Some(wrap - 4600)),
                (wrap + 500, Some(wrap - 2000))
            ]
        );
    }

    #[test]
    fn h264_fields_pair_on_the_same_frame_num() {
        let mut b = EntryBuilder::new(0x100, Codec::H264, 0);
        let mut sink = VecSink::default();
        let field = |f, n, rap| AuMeta {
            field: f,
            frame_num: Some(n),
            ..meta(rap, true, PictureType::I)
        };
        b.push(
            rau(0, Some(0), field(Field::Top, 1, RapClass::Idr)),
            &mut sink,
        );
        b.push(
            rau(1, None, field(Field::Bottom, 1, RapClass::Idr)),
            &mut sink,
        );
        b.push(
            rau(2, Some(3600), field(Field::Top, 2, RapClass::None)),
            &mut sink,
        );
        let mut damaged = rau(3, Some(5400), field(Field::Bottom, 3, RapClass::None));
        damaged.gap_before = true;
        b.push(damaged, &mut sink);
        let mut pair = rau(4, Some(7200), field(Field::Bottom, 4, RapClass::None));
        pair.meta.params = ParamSet::SPS;
        b.push(pair, &mut sink);
        let mut second = rau(5, None, field(Field::Top, 4, RapClass::None));
        second.gap_before = true;
        second.meta.params = ParamSet::SEI;
        b.push(second, &mut sink);
        b.push(
            rau(6, Some(9000), field(Field::Top, 5, RapClass::None)),
            &mut sink,
        );
        b.flush(&mut sink);
        let all = b.publish(true).unwrap();
        let got: Vec<(u8, i64, u32, bool)> = all
            .records
            .iter()
            .map(|r| (r.fields, r.pts, r.size, r.au().damaged))
            .collect();
        assert_eq!(
            got,
            [
                (2, 0, 200, false),
                (1, 3600, 100, false),
                (1, 5400, 100, false),
                (2, 7200, 200, true),
                (1, 9000, 100, false),
            ]
        );
        assert!(all.records[3].params.contains(ParamSet::SPS));
        assert!(all.records[3].params.contains(ParamSet::SEI));
        assert_eq!(all.records[0].end_packet, 188);
        // Aucun message : la 2e AU d'une paire n'a pas besoin de PTS.
        assert!(sink.messages.iter().all(|m| m.id != "ts-au-no-pts"));
    }

    #[test]
    fn recent_pts_window_is_bounded() {
        let mut r = RecentPts::default();
        r.insert(5);
        r.insert(5);
        for v in 0..(RECENT_PTS as i64 + 10) {
            r.insert(1000 + v);
        }
        assert!(!r.contains(5));
        assert!(r.contains(1000 + RECENT_PTS as i64));
        assert_eq!(r.order.len(), RECENT_PTS);
        assert_eq!(r.count.len(), RECENT_PTS);
        assert_eq!(wrap_diff(5, (1 << 33) - 5), 10);
        assert_eq!(wrap_diff((1 << 33) - 5, 5), -10);
    }

    #[test]
    fn returning_pid_resumes_its_index_and_codec_change_is_ignored() {
        let mut ts = Ts::default();
        ts.tables(0, 0x100, 0x100);
        ts.pes(0x100, 3600, &au(1, 3), 184);
        ts.pes(0x100, 7200, &au(2, 3), 184);
        ts.tables(1, 0x101, 0x101);
        ts.pes(0x101, 90_000, &au(1, 3), 184);
        ts.tables(2, 0x100, 0x100);
        ts.pes(0x100, 10_800, &au(1, 3), 184);
        ts.pes(0x100, 14_400, &au(2, 3), 184);
        // 0x101 revient en H.264 : un PID n'a qu'un index, ce flux n'est pas indexé.
        ts.psi(PMT_PID, &pmt(1, 3, 0x101, &[], &[(0x1B, 0x101, &[])]));
        ts.pes(0x101, 99_000, &[0, 0, 0, 1, 0x09, 0xF0], 184);
        let (idx, _, r, _) = run(ts.bytes());
        r.unwrap();
        assert_eq!(idx.videos(), [(0x100, Codec::Mpeg2), (0x101, Codec::Mpeg2)]);
        let pts: Vec<i64> = all(&idx, 0x100).iter().map(|(f, _)| f.pts).collect();
        assert_eq!(pts, [3600, 7200, 10_800, 14_400]);
        let e = all(&idx, 0x100);
        assert_eq!(
            e.iter().map(|(f, _)| f.pres_idx).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(all(&idx, 0x101).len(), 1);
        idx.with_video(0x100, |v| {
            assert_eq!((v.pid(), v.codec()), (0x100, Codec::Mpeg2));
            assert_eq!(v.entry(4), None);
            assert_eq!(v.au(4), None);
            assert_eq!(v.decode_idx_of(4), None);
        });
        assert!(idx.with_video(0x200, |_| ()).is_none());
    }

    /// Source qui annonce plus d'octets qu'elle n'en a (lecture courte sans erreur).
    struct Longer(MemSource);

    impl ReadAt for Longer {
        fn len(&self) -> u64 {
            self.0.len() + 10_000
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            self.0.read_at(offset, buf)
        }
    }

    /// Source qui panique.
    struct Panicking;

    impl ReadAt for Panicking {
        fn len(&self) -> u64 {
            1000
        }

        fn read_at(&self, _: u64, _: &mut [u8]) -> Result<usize, IoError> {
            panic!("source de test")
        }
    }

    #[test]
    fn short_reads_and_failures_are_errors() {
        let mut ts = Ts::default();
        ts.tables(0, 0x100, 0x100);
        for i in 0..6 {
            ts.pes(
                0x100,
                3600 * (i + 1),
                &au(if i % 2 == 0 { 1 } else { 2 }, 3),
                184,
            );
        }
        let data = ts.bytes();
        let len = data.len() as u64;
        let src = Longer(MemSource::new(data.clone()));
        let out = TsIndex::new(PacketFormat::Ts188, src.len());
        let mut sink = VecSink::default();
        let r = index(
            &src,
            0,
            &out,
            &IndexOptions::default(),
            &mut sink,
            &AtomicBool::new(false),
        );
        assert!(
            matches!(r, Err(IndexError::Io(IoError::Truncated { expected_len, actual_len }))
                if expected_len == len + 10_000 && actual_len == len),
            "{r:?}"
        );
        // AU 0 à 4 vues (I P I P I) : préfixe publié jusqu'à l'I n° 2.
        assert_eq!(all(&out, 0x100).len(), 2);
        // Erreur dès la 1re lecture.
        let src = ma_io::testing::TruncatingSource::new(MemSource::new(data), 0);
        let out = TsIndex::new(PacketFormat::Ts188, src.len());
        let r = index(
            &src,
            0,
            &out,
            &IndexOptions::default(),
            &mut sink,
            &AtomicBool::new(false),
        );
        assert!(matches!(r, Err(IndexError::Io(IoError::Truncated { .. }))));
        assert_eq!(out.progress().bytes_done, 0);
        // Fil qui panique : erreur, pas de plantage.
        let out = Arc::new(TsIndex::new(PacketFormat::Ts188, 1000));
        let h = spawn_index(
            Arc::new(Panicking),
            0,
            out,
            IndexOptions::default(),
            Box::new(VecSink::default()),
        );
        assert!(matches!(
            h.join(),
            Err(IndexError::Io(IoError::InvalidData("ma-index-panic")))
        ));
        assert!(IndexError::Cancelled.to_string().contains("annul"));
    }
}
