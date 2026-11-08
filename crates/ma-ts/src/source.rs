//! Source d'AU sur un fichier TS (`impl AuSource`) : l'index de [`crate::index`] et la
//! relecture des AU par [`read_es`] à travers un cache de blocs partagé.
//!
//! # Lectures
//!
//! - **Navigation** : `read_au` rejoue les paquets de `[start, end)` de l'AU (positions de l'index,
//!   seules valables) à travers le [`BlockCache`] partagé. Une AU `damaged` est lue telle quelle.
//! - **Indexation** : l'indexeur lit la source du cache **directement**, sans passer par ses
//!   blocs (même descripteur, même identité de fichier). Ses lectures sont séquentielles, par
//!   blocs de 4 Mio, et chaque octet n'est lu qu'une fois : les mettre en cache n'apporterait
//!   rien et évincerait (LRU) les blocs du navigateur toutes les 64 Mio. Les deux trafics sont
//!   comptés à part : [`TsAuSource::io_stats`] (cache) et [`TsAuSource::indexer_io_stats`].
//!
//! # Revalidation du fichier
//!
//! Le cache ne revalide jamais ses blocs. `FileSource::check_unchanged` est donc appelé à
//! l'ouverture, après toute lecture en échec, et toutes les [`REVALIDATE_EVERY`] lectures
//! d'AU (avant la lecture) : un contenu remplacé est détecté au plus tard à ce contrôle.
//!
//! - `Modified` (date, inode ou taille changés sans réduction, fichier remplacé ou supprimé) :
//!   définitif, toute lecture suivante échoue.
//! - `Truncated` (fichier raccourci : enregistrement coupé, NFS) : la taille constatée devient
//!   une borne ; une AU qui la dépasse est refusée sans lecture, les AU d'avant restent servies.
//!   Une troncature suivie d'une réécriture du début n'est pas distinguée d'une simple
//!   troncature tant que la taille reste sous la taille d'ouverture.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use ma_core::{AuSource, Codec, FrameEntry, Message, MessageSink};
use ma_io::{BlockCache, CacheConfig, FileSource, IoError, IoStats, ReadAt};

use crate::es_read::read_es;
use crate::index::{
    AuInfo, IndexError, IndexHandle, IndexOptions, TsIndex, index, spawn_index_then,
};
use crate::packet::PacketFormat;
use crate::scan::{OpenError, probe};
use crate::tables::StreamKind;

/// Lectures d'AU entre deux contrôles de l'identité du fichier (`check_unchanged` : `fstat` et
/// `stat`, quelques microsecondes en local ; servis par le cache d'attributs en NFS). Une
/// demande de navigation envoie de l'ordre d'un GOP d'AU : environ un contrôle par saut.
pub const REVALIDATE_EVERY: u32 = 32;
/// Attente entre deux consultations de l'index pendant `open_progressive`.
const POLL: Duration = Duration::from_millis(1);
const OUT_OF_INDEX: &str = "au-out-of-index";
const CANCELLED: IoError = IoError::InvalidData("index-cancelled");
/// Aucune troncature constatée.
const NO_LIMIT: u64 = u64::MAX;

/// Puits des messages d'indexation : aucun consommateur avant l'interface (M2b).
struct Discard;

impl MessageSink for Discard {
    fn push(&mut self, _: Message) {}
}

/// Lecture directe de la source du cache (voir la documentation du module), comptée.
struct Direct {
    io: Arc<BlockCache<FileSource>>,
    syscalls: AtomicU64,
    bytes: AtomicU64,
}

impl Direct {
    fn stats(&self) -> IoStats {
        IoStats {
            syscalls: self.syscalls.load(Ordering::Relaxed),
            bytes_read: self.bytes.load(Ordering::Relaxed),
            hits: 0,
        }
    }
}

impl ReadAt for Direct {
    fn len(&self) -> u64 {
        self.io.source().len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        self.syscalls.fetch_add(1, Ordering::Relaxed);
        let n = self.io.source().read_at(offset, buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// État du fichier vu par une source (voir « Revalidation » dans la documentation du module).
#[derive(Default)]
struct Health {
    reads: AtomicU32,
    modified: AtomicBool,
    /// Taille après troncature, `NO_LIMIT` sinon.
    limit: AtomicU64,
}

/// Source d'AU d'un PID vidéo d'un fichier TS (voir la documentation du module).
pub struct TsAuSource {
    index: Arc<TsIndex>,
    pid: u16,
    codec: Codec,
    format: PacketFormat,
    io: Arc<BlockCache<FileSource>>,
    indexer: Option<Arc<Direct>>,
    health: Health,
}

/// Fichier ouvert : cache, lecteur direct de l'indexeur, index vide et offset de départ.
struct Opened {
    io: Arc<BlockCache<FileSource>>,
    direct: Arc<Direct>,
    index: Arc<TsIndex>,
    start: u64,
}

fn open_file(path: &Path) -> Result<Opened, OpenError> {
    let (file, format, start) = probe(path)?;
    let cfg = CacheConfig::for_locality(file.locality());
    let len = file.len();
    let io = Arc::new(BlockCache::new(file, cfg));
    let direct = Arc::new(Direct {
        io: Arc::clone(&io),
        syscalls: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    });
    Ok(Opened {
        io,
        direct,
        index: Arc::new(TsIndex::new(format, len)),
        start,
    })
}

/// PID vidéo indexés : ceux de la structure (programmes, puis ordre de la PMT), puis les autres
/// dans l'ordre de leur apparition.
fn candidates(index: &TsIndex) -> Vec<u16> {
    let videos = index.videos();
    let known = |pid: &u16| videos.iter().any(|v| v.0 == *pid);
    let mut out: Vec<u16> = index
        .structure()
        .programs
        .iter()
        .flat_map(|p| &p.streams)
        .filter(|s| s.analysed && matches!(s.kind, StreamKind::Video(Some(_))))
        .map(|s| s.pid)
        .filter(known)
        .collect();
    for (pid, _) in videos {
        if !out.contains(&pid) {
            out.push(pid);
        }
    }
    out
}

fn published(index: &TsIndex, pid: u16) -> u32 {
    index
        .with_video(pid, crate::VideoIndex::published)
        .unwrap_or(0)
}

/// STRUCT.TREE.06 : 1re vidéo (ordre de [`candidates`]) qui a des images, sinon la 1re tout court.
fn select(index: &TsIndex) -> Option<u16> {
    let all = candidates(index);
    all.iter()
        .copied()
        .find(|&pid| published(index, pid) > 0)
        .or_else(|| all.first().copied())
}

fn index_error(e: IndexError) -> OpenError {
    match e {
        IndexError::Io(e) => OpenError::Io(e),
        IndexError::Cancelled => OpenError::Io(CANCELLED),
    }
}

impl TsAuSource {
    /// Ouvre `path`, l'indexe entièrement et choisit la 1re vidéo analysée, dans l'ordre des
    /// programmes puis de la PMT (STRUCT.TREE.06).
    ///
    /// # Errors
    /// Celles de [`probe`] ; `Io` si la lecture échoue ou si le fichier change pendant
    /// l'indexation ; `NoVideo` sans flux vidéo indexé.
    pub fn open(path: &Path) -> Result<TsAuSource, OpenError> {
        let o = open_file(path)?;
        index(
            &*o.direct,
            o.start,
            &o.index,
            &IndexOptions::default(),
            &mut Discard,
            &AtomicBool::new(false),
        )
        .map_err(index_error)?;
        o.io.source().check_unchanged()?;
        let pid = select(&o.index).ok_or(OpenError::NoVideo)?;
        Self::build(o.index, o.io, pid, Some(o.direct)).ok_or(OpenError::NoVideo)
    }

    /// Ouvre `path` et l'indexe en tâche de fond. Rend la main dès que la 1re vidéo de la PMT a
    /// une image publiée, ou à la fin de l'indexation ; `len()` croît ensuite au fil des
    /// publications. L'abandon du handle annule l'indexation (l'index partiel reste valable).
    ///
    /// L'indexeur ne revalide pas le fichier pendant l'indexation (une lecture courte donne
    /// `Truncated`) ; comme `open`, il contrôle son identité à la fin, avant de publier l'index
    /// complet. Un fichier modifié pendant l'indexation donne `Io(Modified)` : ici si
    /// l'indexation est déjà finie, sinon au `join` du handle ; l'index publié reste lisible mais
    /// n'est jamais `complete`.
    ///
    /// # Errors
    /// Celles de [`probe`] ; `Io` si l'indexation a déjà échoué, ou échoue avant toute image ;
    /// `NoVideo` si elle se termine sans flux vidéo.
    pub fn open_progressive(path: &Path) -> Result<(TsAuSource, IndexHandle), OpenError> {
        let o = open_file(path)?;
        // Comme `open` : identité du fichier contrôlée en fin d'indexation (erreur de `join`).
        let io = Arc::clone(&o.io);
        let handle = spawn_index_then(
            Arc::clone(&o.direct) as Arc<dyn ReadAt>,
            o.start,
            Arc::clone(&o.index),
            IndexOptions::default(),
            Box::new(Discard),
            Box::new(move || io.source().check_unchanged()),
        );
        let pid = loop {
            // Fil fini sans index complet : l'indexation (ou le contrôle de fin) a échoué. Lu
            // dans cet ordre : un fil fini a déjà publié son état final.
            let finished = handle.is_finished();
            let complete = o.index.progress().complete;
            if finished && !complete {
                return Err(match handle.join() {
                    Err(e) => index_error(e),
                    Ok(()) => OpenError::NoVideo,
                });
            }
            let first = candidates(&o.index).first().copied();
            if let Some(pid) = first.filter(|&p| published(&o.index, p) > 0) {
                break pid;
            }
            if complete || finished {
                if let Some(pid) = select(&o.index) {
                    break pid;
                }
                return Err(match handle.join() {
                    Err(e) => index_error(e),
                    Ok(()) => OpenError::NoVideo,
                });
            }
            std::thread::sleep(POLL);
        };
        let src = Self::build(o.index, o.io, pid, Some(o.direct)).ok_or(OpenError::NoVideo)?;
        Ok((src, handle))
    }

    /// Source du PID vidéo `pid` d'un index déjà construit (autre vidéo du même fichier), lue
    /// par `io` ; `None` si `pid` n'est pas une vidéo de l'index.
    #[must_use]
    pub fn for_video(
        index: Arc<TsIndex>,
        io: Arc<BlockCache<FileSource>>,
        pid: u16,
    ) -> Option<TsAuSource> {
        Self::build(index, io, pid, None)
    }

    fn build(
        index: Arc<TsIndex>,
        io: Arc<BlockCache<FileSource>>,
        pid: u16,
        indexer: Option<Arc<Direct>>,
    ) -> Option<TsAuSource> {
        let codec = index.with_video(pid, crate::VideoIndex::codec)?;
        Some(TsAuSource {
            format: index.format(),
            index,
            pid,
            codec,
            io,
            indexer,
            health: Health {
                limit: AtomicU64::new(NO_LIMIT),
                ..Health::default()
            },
        })
    }

    #[must_use]
    pub fn index(&self) -> &Arc<TsIndex> {
        &self.index
    }

    #[must_use]
    pub fn pid(&self) -> u16 {
        self.pid
    }

    /// Positions, horodatages et drapeaux de l'entrée `decode_idx` (publiée).
    #[must_use]
    pub fn au_info(&self, decode_idx: u32) -> Option<AuInfo> {
        self.index
            .with_video(self.pid, |v| v.au(decode_idx))
            .flatten()
    }

    /// Lectures de la navigation (cache de blocs partagé).
    #[must_use]
    pub fn io_stats(&self) -> IoStats {
        self.io.stats()
    }

    /// Lectures directes de l'indexeur lancé par `open` ou `open_progressive` (zéro pour
    /// `for_video`).
    #[must_use]
    pub fn indexer_io_stats(&self) -> IoStats {
        self.indexer
            .as_ref()
            .map_or_else(IoStats::default, |d| d.stats())
    }

    /// Contrôle l'identité du fichier et retient le résultat (`Modified` définitif, taille
    /// tronquée en borne). Une erreur du système (métadonnées illisibles) est rendue telle quelle.
    fn revalidate(&self) -> Result<(), IoError> {
        match self.io.source().check_unchanged() {
            Err(IoError::Modified) => {
                self.health.modified.store(true, Ordering::Relaxed);
                Err(IoError::Modified)
            }
            Err(IoError::Truncated { actual_len, .. }) => {
                self.health.limit.fetch_min(actual_len, Ordering::Relaxed);
                Ok(())
            }
            other => other,
        }
    }

    /// Refuse sans lecture une AU d'un fichier modifié, ou qui dépasse la troncature connue.
    fn gate(&self, au: &AuInfo) -> Result<(), IoError> {
        if self.health.modified.load(Ordering::Relaxed) {
            return Err(IoError::Modified);
        }
        let limit = self.health.limit.load(Ordering::Relaxed);
        let tail = if au.end.skip == 0 {
            0
        } else {
            self.format.tail() as u64
        };
        if au.end.packet_offset + tail > limit {
            return Err(IoError::Truncated {
                expected_len: self.io.len(),
                actual_len: limit,
            });
        }
        Ok(())
    }

    fn read(&self, decode_idx: u32, buf: &mut Vec<u8>) -> Result<(), IoError> {
        let au = self
            .au_info(decode_idx)
            .ok_or(IoError::InvalidData(OUT_OF_INDEX))?;
        if self.health.reads.fetch_add(1, Ordering::Relaxed) % REVALIDATE_EVERY
            == REVALIDATE_EVERY - 1
        {
            self.revalidate()?;
        }
        self.gate(&au)?;
        read_es(&*self.io, self.format, self.pid, au.start, au.end, buf).or_else(|e| {
            buf.clear();
            self.revalidate()?;
            self.gate(&au)?;
            Err(e)
        })
    }
}

impl AuSource for TsAuSource {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn len(&self) -> u32 {
        published(&self.index, self.pid)
    }

    fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
        self.index
            .with_video(self.pid, |v| v.entry(decode_idx))
            .flatten()
    }

    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
        self.index
            .with_video(self.pid, |v| v.decode_idx_of(pres_idx))
            .flatten()
    }

    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        self.read(decode_idx, buf).map_err(|e| match e {
            IoError::InvalidData(OUT_OF_INDEX) => io::Error::new(io::ErrorKind::InvalidInput, e),
            other => other.into(),
        })
    }
}
