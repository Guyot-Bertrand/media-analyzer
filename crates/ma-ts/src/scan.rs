//! Lecture séquentielle des paquets (par blocs de 4 Mio), resynchronisation et ouverture d'un
//! fichier (`probe`).

use std::path::Path;

use ma_core::{Family, Message, MessageSink, Param, Severity};
use ma_io::{FileSource, IoError, ReadAt};

use crate::messages::{ADAPTATION_LENGTH, RESYNC, TRUNCATED_PACKET};
use crate::packet::{
    Packet, PacketFormat, SYNC_BYTE, SYNC_RUN, TS_PACKET_SIZE, detect_format, parse_packet,
};

/// Taille des blocs lus par le scanner.
const BLOCK: usize = 4 << 20;
/// Zone lue par `probe` pour détecter le format.
const PROBE_BYTES: usize = 1 << 20;
/// Invariant interne rompu (ne doit pas arriver) : erreur explicite, jamais de panique.
const INCONSISTENT: IoError = IoError::InvalidData("ts-scanner-inconsistent");
const SUB_FAMILY: &str = "sync";

/// Paquet lu, avec sa position dans le fichier.
#[derive(Debug)]
pub struct ScannedPacket<'a> {
    /// Offset de l'octet 0x47 (pour `Tts192`, le préfixe horodaté le précède de 4 octets).
    pub offset: u64,
    /// Numéro du paquet depuis le début de la plage (les octets parasites n'en consomment pas ;
    /// un paquet ignoré pour champ d'adaptation invalide garde son numéro).
    pub index: u64,
    pub packet: Packet<'a>,
}

/// Lecteur séquentiel de paquets. `next` emprunte `self` (itérateur « prêteur ») : le paquet
/// pointe dans le tampon interne, sans copie.
pub struct PacketScanner<'s> {
    src: &'s dyn ReadAt,
    format: PacketFormat,
    /// Octet 0x47 attendu du prochain paquet.
    pos: u64,
    /// Fin exclusive des données exploitables.
    limit: u64,
    index: u64,
    buf: Vec<u8>,
    buf_start: u64,
    /// La dernière lecture a rendu moins que demandé : rien de plus à attendre dans ce bloc.
    exhausted: bool,
}

impl<'s> PacketScanner<'s> {
    /// `start` est l'offset de l'octet 0x47 du premier paquet (voir `detect_format`/`probe`) ;
    /// `end` borne la lecture (exclusif), `None` pour aller jusqu'à la fin de la source.
    #[must_use]
    pub fn new(src: &'s dyn ReadAt, format: PacketFormat, start: u64, end: Option<u64>) -> Self {
        let limit = end.map_or(src.len(), |e| e.min(src.len()));
        PacketScanner {
            src,
            format,
            pos: start,
            limit,
            index: 0,
            buf: Vec::new(),
            buf_start: 0,
            exhausted: false,
        }
    }

    fn buf_end(&self) -> u64 {
        self.buf_start + self.buf.len() as u64
    }

    /// Charge un bloc à partir de `at`.
    fn load(&mut self, at: u64) -> Result<(), IoError> {
        let want = usize::try_from(self.limit.saturating_sub(at)).map_or(BLOCK, |n| n.min(BLOCK));
        self.buf.clear();
        self.buf.resize(want, 0);
        self.buf_start = at;
        let got = self.src.read_at(at, &mut self.buf)?;
        self.buf.truncate(got);
        self.exhausted = got < want || at + want as u64 >= self.limit;
        Ok(())
    }

    /// Garantit que `[at, at + need)` est en mémoire, sauf si la source s'arrête avant.
    fn ensure(&mut self, at: u64, need: usize) -> Result<(), IoError> {
        let covered = at >= self.buf_start && at + need as u64 <= self.buf_end();
        if covered || (self.exhausted && at >= self.buf_start) {
            return Ok(());
        }
        self.load(at)
    }

    /// Octets disponibles à partir de `at` (après `ensure`).
    fn available(&self, at: u64) -> usize {
        usize::try_from(self.buf_end().saturating_sub(at)).unwrap_or(usize::MAX)
    }

    fn byte(&self, at: u64) -> Option<u8> {
        let i = usize::try_from(at.checked_sub(self.buf_start)?).ok()?;
        self.buf.get(i).copied()
    }

    /// Premier offset `>= from` où `SYNC_RUN` synchronisations consécutives tombent à la période
    /// (moins si les données s'arrêtent avant).
    fn find_sync(&mut self, from: u64) -> Result<Option<u64>, IoError> {
        let period = self.format.size() as u64;
        let span = (SYNC_RUN as u64 - 1) * period + 1;
        let mut q = from;
        while q < self.limit {
            self.ensure(q, usize::try_from(span).unwrap_or(usize::MAX).min(BLOCK))?;
            // Dernier candidat dont toute la fenêtre est en mémoire (ou jusqu'à la fin des données).
            let end = if self.exhausted {
                self.buf_end()
            } else {
                (self.buf_end() + 1).saturating_sub(span).max(q + 1)
            };
            while q < end {
                if self.byte(q) == Some(SYNC_BYTE)
                    && (1..SYNC_RUN as u64).all(|k| {
                        let at = q + k * period;
                        at >= self.limit || self.byte(at) == Some(SYNC_BYTE)
                    })
                {
                    return Ok(Some(q));
                }
                q += 1;
            }
            if self.exhausted {
                break;
            }
        }
        Ok(None)
    }

    /// Fenêtre de 188 octets à la position `i` du tampon.
    fn raw(&self, i: usize) -> Result<&[u8; TS_PACKET_SIZE], IoError> {
        self.buf
            .get(i..i + TS_PACKET_SIZE)
            .and_then(|w| <&[u8; TS_PACKET_SIZE]>::try_from(w).ok())
            .ok_or(INCONSISTENT)
    }

    /// Avance jusqu'au prochain paquet valide et renvoie (offset, index, position dans le tampon).
    fn advance(
        &mut self,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<(u64, u64, usize)>, IoError> {
        let tail = self.format.tail();
        loop {
            if self.pos >= self.limit {
                return Ok(None);
            }
            let period = self.format.size();
            let follows = self.pos + (period as u64) < self.limit;
            self.ensure(self.pos, if follows { period + 1 } else { tail })?;
            let avail = self
                .available(self.pos)
                .min(usize::try_from(self.limit - self.pos).unwrap_or(usize::MAX));
            if avail == 0 {
                self.pos = self.limit;
                return Ok(None);
            }
            let mut resumed = None;
            if self.byte(self.pos) != Some(SYNC_BYTE) {
                resumed = Some(self.find_sync(self.pos + 1)?.unwrap_or(self.limit));
            } else if follows && self.byte(self.pos + period as u64) != Some(SYNC_BYTE) {
                // Candidat suspect : le paquet suivant n'est pas à la période. Une vraie suite de
                // synchros qui commence avant lui prouve qu'il est faux (0x47 parasite) ; sinon
                // c'est le dernier paquet avant une zone parasite.
                let lost = self.pos;
                resumed = self
                    .find_sync(lost + 1)?
                    .filter(|&q| q < lost + period as u64);
                self.ensure(lost, tail)?;
            }
            if let Some(resumed) = resumed {
                let lost = self.pos;
                sink.push(
                    Message::new(&RESYNC, Severity::Warning, Family::Transport, SUB_FAMILY)
                        .param("skipped", Param::UInt(resumed - lost))
                        .at(lost),
                );
                self.pos = resumed;
                continue;
            }
            if avail < tail {
                sink.push(
                    Message::new(
                        &TRUNCATED_PACKET,
                        Severity::Warning,
                        Family::Transport,
                        SUB_FAMILY,
                    )
                    .param("bytes", Param::UInt(avail as u64))
                    .at(self.pos),
                );
                self.pos = self.limit;
                return Ok(None);
            }
            let offset = self.pos;
            let index = self.index;
            self.pos += self.format.size() as u64;
            self.index += 1;
            let i = usize::try_from(offset - self.buf_start).map_err(|_| INCONSISTENT)?;
            let raw = self.raw(i)?;
            if parse_packet(raw).is_ok() {
                return Ok(Some((offset, index, i)));
            }
            sink.push(
                Message::new(
                    &ADAPTATION_LENGTH,
                    Severity::Warning,
                    Family::Transport,
                    SUB_FAMILY,
                )
                .param("length", Param::UInt(u64::from(raw[4])))
                .at(offset)
                .pid(u16::from(raw[1] & 0x1F) << 8 | u16::from(raw[2])),
            );
        }
    }

    /// Paquet suivant, `None` à la fin des données. Les octets parasites produisent `ts-resync`,
    /// un dernier paquet incomplet `ts-truncated-packet`, un champ d'adaptation invalide
    /// `ts-adaptation-length` (le paquet est alors ignoré). Une source tronquée ou modifiée
    /// renvoie l'erreur d'E/S.
    pub fn next(
        &mut self,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<ScannedPacket<'_>>, IoError> {
        let Some((offset, index, i)) = self.advance(sink)? else {
            return Ok(None);
        };
        // Déjà validé par `advance` : un échec ici serait un défaut interne, signalé et non masqué.
        let packet = parse_packet(self.raw(i)?).map_err(|_| INCONSISTENT)?;
        Ok(Some(ScannedPacket {
            offset,
            index,
            packet,
        }))
    }
}

/// Échec d'ouverture d'un fichier.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("fichier vide")]
    Empty,
    #[error("accès au fichier refusé")]
    AccessDenied,
    #[error("format non reconnu : ce n'est pas un flux de transport MPEG-2")]
    NotTs,
    /// Déclaré pour l'ouverture complète : n'est produit qu'à partir de la source d'AU (Tâche 11),
    /// quand le fichier ne contient aucun flux vidéo.
    #[error("aucun flux vidéo")]
    NoVideo,
    #[error(transparent)]
    Io(#[from] IoError),
}

/// Ouvre `path` et détecte le format dans le premier Mio. Renvoie la source, le format et
/// l'offset du premier octet 0x47.
pub fn probe(path: &Path) -> Result<(FileSource, PacketFormat, u64), OpenError> {
    let src = FileSource::open(path).map_err(|e| match e {
        IoError::Os(ref io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            OpenError::AccessDenied
        }
        other => OpenError::Io(other),
    })?;
    if src.is_empty() {
        return Err(OpenError::Empty);
    }
    let want = usize::try_from(src.len()).map_or(PROBE_BYTES, |n| n.min(PROBE_BYTES));
    let mut head = vec![0u8; want];
    let got = src.read_at(0, &mut head)?;
    head.truncate(got);
    let (format, offset) = detect_format(&head).ok_or(OpenError::NotTs)?;
    Ok((src, format, offset as u64))
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::messages::{ADAPTATION_LENGTH, RESYNC, TRUNCATED_PACKET};
    use crate::testutil::{packet, repacketize};
    use ma_core::{Param, VecSink};
    use ma_io::testing::MemSource;
    use proptest::prelude::*;

    fn stream(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| packet(0x100, i == 0, (i % 16) as u8, None, &[i as u8; 10]))
            .collect()
    }

    /// Parcourt tout et renvoie (offset, index, pid, cc) de chaque paquet.
    fn scan_all(
        src: &dyn ReadAt,
        f: PacketFormat,
        start: u64,
        end: Option<u64>,
        sink: &mut VecSink,
    ) -> Vec<(u64, u64, u16, u8)> {
        let mut sc = PacketScanner::new(src, f, start, end);
        let mut out = Vec::new();
        while let Some(p) = sc.next(sink).unwrap() {
            out.push((p.offset, p.index, p.packet.header.pid, p.packet.header.cc));
        }
        out
    }

    #[test]
    fn scans_every_format_with_exact_offsets() {
        let ts = stream(30);
        for f in [
            PacketFormat::Ts188,
            PacketFormat::Tts192,
            PacketFormat::Rs204,
            PacketFormat::Ts208,
        ] {
            let data = repacketize(&ts, f);
            let (_, off) = detect_format(&data).unwrap();
            let mut sink = VecSink::default();
            let got = scan_all(&MemSource::new(data), f, off as u64, None, &mut sink);
            assert_eq!(got.len(), 30, "{f:?}");
            for (i, g) in got.iter().enumerate() {
                assert_eq!(g.0, (f.sync_offset() + i * f.size()) as u64);
                assert_eq!(g.1, i as u64);
            }
            assert!(sink.messages.is_empty());
        }
    }

    #[test]
    fn range_start_and_end_are_honoured() {
        let src = MemSource::new(stream(30));
        let mut sink = VecSink::default();
        let got = scan_all(&src, PacketFormat::Ts188, 188 * 5, Some(188 * 9), &mut sink);
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            vec![940, 1128, 1316, 1504]
        );
        assert_eq!(got[0].1, 0, "l'index compte depuis `start`");
        // Plage vide ou hors fichier.
        assert!(scan_all(&src, PacketFormat::Ts188, 10_000_000, None, &mut sink).is_empty());
        assert!(scan_all(&src, PacketFormat::Ts188, 400, Some(100), &mut sink).is_empty());
        assert!(sink.messages.is_empty());
    }

    #[test]
    fn resync_after_garbage_keeps_offsets() {
        let mut data = vec![0xAAu8; 1000];
        data.extend(stream(10));
        data.extend([0x11u8; 7]);
        data.extend(stream(10));
        let (f, off) = detect_format(&data).unwrap();
        assert_eq!((f, off), (PacketFormat::Ts188, 1000));
        let mut sink = VecSink::default();
        let got = scan_all(&MemSource::new(data), f, off as u64, None, &mut sink);
        assert_eq!(got.len(), 20);
        for (i, g) in got.iter().enumerate() {
            let extra = if i < 10 { 0 } else { 7 };
            assert_eq!(g.0, 1000 + extra + i as u64 * 188, "paquet {i}");
            assert_eq!(g.1, i as u64);
        }
        let resyncs: Vec<_> = sink.messages.iter().filter(|m| m.id == RESYNC.id).collect();
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(resyncs[0].params, vec![("skipped", Param::UInt(7))]);
        assert_eq!(resyncs[0].offset, Some(1000 + 10 * 188));
    }

    #[test]
    fn false_sync_byte_in_garbage_is_not_trusted() {
        let mut data = stream(6);
        data.extend([0x00, 0x47, 0x47, 0x47]); // 0x47 isolés
        data.extend(stream(6));
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), 12);
        assert_eq!(got[6].0, 6 * 188 + 4);
        assert_eq!(sink.messages.len(), 1);
    }

    #[test]
    fn false_sync_byte_at_range_start_is_not_trusted() {
        let mut data = vec![0x47, 0x00, 0x47];
        data.extend(stream(8));
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), 8);
        assert_eq!(got[0].0, 3);
        assert_eq!(sink.messages[0].params, vec![("skipped", Param::UInt(3))]);
    }

    #[test]
    fn garbage_without_any_packet_after_it_ends_the_scan() {
        let mut data = stream(3);
        data.extend([0x00u8; 500]);
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), 3);
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(sink.messages[0].params, vec![("skipped", Param::UInt(500))]);
    }

    #[test]
    fn resync_across_block_boundary() {
        // Plus de 4 Mio : le bruit se trouve à cheval sur la frontière de bloc.
        let per_block = BLOCK / 188;
        let mut data = stream(per_block - 1);
        data.extend(vec![0u8; 100]);
        data.extend(stream(40));
        let total = per_block - 1 + 40;
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), total);
        assert_eq!(got[total - 1].0, ((total - 1) * 188 + 100) as u64);
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(sink.messages[0].params, vec![("skipped", Param::UInt(100))]);
    }

    #[test]
    fn last_partial_packet_is_reported() {
        let data = stream(5);
        let src = MemSource::new(data[..188 * 4 + 60].to_vec());
        let mut sink = VecSink::default();
        let got = scan_all(&src, PacketFormat::Ts188, 0, None, &mut sink);
        assert_eq!(got.len(), 4);
        assert_eq!(sink.messages.len(), 1);
        let m = &sink.messages[0];
        assert_eq!(m.id, TRUNCATED_PACKET.id);
        assert_eq!(m.params, vec![("bytes", Param::UInt(60))]);
        assert_eq!(m.offset, Some(188 * 4));
        // Un 204 dont le trailer est coupé est incomplet aussi.
        let d = repacketize(&data, PacketFormat::Rs204);
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(d[..204 * 4 + 190].to_vec()),
            PacketFormat::Rs204,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), 4);
        assert_eq!(sink.messages[0].params, vec![("bytes", Param::UInt(190))]);
    }

    #[test]
    fn truncated_source_ends_the_scan_without_panic() {
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Comme `FileSource` : une lecture au-delà de la taille actuelle est `Truncated`.
        struct Shrinking {
            inner: MemSource,
            cut: AtomicU64,
        }
        impl ReadAt for Shrinking {
            fn len(&self) -> u64 {
                self.inner.len()
            }
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
                let cut = self.cut.load(Ordering::SeqCst);
                if offset + buf.len() as u64 > cut {
                    return Err(IoError::Truncated {
                        expected_len: self.inner.len(),
                        actual_len: cut,
                    });
                }
                self.inner.read_at(offset, buf)
            }
        }

        let packets = 2 * BLOCK / 188;
        let src = Shrinking {
            inner: MemSource::new(stream(packets)),
            cut: AtomicU64::new(u64::MAX),
        };
        let mut sink = VecSink::default();
        let mut sc = PacketScanner::new(&src, PacketFormat::Ts188, 0, None);
        // Le 1er bloc contient BLOCK / 188 paquets entiers ; le fichier rétrécit ensuite.
        let in_first_block = BLOCK / 188;
        for i in 0..in_first_block {
            assert_eq!(sc.next(&mut sink).unwrap().unwrap().index, i as u64);
            if i == 0 {
                src.cut.store(1000, Ordering::SeqCst);
            }
        }
        let err = sc.next(&mut sink).err();
        assert!(
            matches!(
                err,
                Some(IoError::Truncated {
                    actual_len: 1000,
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(sink.messages.is_empty());
    }

    #[test]
    fn stray_sync_byte_after_a_packet_is_not_a_packet() {
        let mut data = stream(10);
        data.extend([0x47, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        data.extend(stream(10));
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.len(), 20);
        for (i, g) in got.iter().enumerate() {
            let extra = if i < 10 { 0 } else { 7 };
            assert_eq!(g.0, extra + i as u64 * 188, "paquet {i}");
            assert_eq!(g.1, i as u64);
        }
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(sink.messages[0].id, RESYNC.id);
        assert_eq!(sink.messages[0].params, vec![("skipped", Param::UInt(7))]);
    }

    #[test]
    fn bad_adaptation_length_is_reported_and_packet_skipped() {
        let mut data = stream(3);
        data[188 + 3] = 0x30;
        data[188 + 4] = 200;
        let mut sink = VecSink::default();
        let got = scan_all(
            &MemSource::new(data),
            PacketFormat::Ts188,
            0,
            None,
            &mut sink,
        );
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(sink.messages.len(), 1);
        let m = &sink.messages[0];
        assert_eq!((m.id, m.offset), (ADAPTATION_LENGTH.id, Some(188)));
        assert_eq!(m.params, vec![("length", Param::UInt(200))]);
        assert_eq!(m.pid, Some(0x100));
    }

    fn temp(data: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(data).unwrap();
        f.flush().unwrap();
        f
    }

    #[ma_trace::covers("STRUCT.OPEN.04")]
    #[test]
    fn empty_text_and_unreadable_files_are_reported() {
        let empty = temp(&[]);
        assert!(matches!(probe(empty.path()), Err(OpenError::Empty)));

        let text = temp(
            "Ceci n'est pas un flux de transport.\n"
                .repeat(500)
                .as_bytes(),
        );
        assert!(matches!(probe(text.path()), Err(OpenError::NotTs)));

        let missing = probe(Path::new("/nonexistent/ma-ts/x.ts"));
        assert!(matches!(missing, Err(OpenError::Io(_))));

        let ok = temp(&repacketize(&stream(20), PacketFormat::Rs204));
        let (src, f, off) = probe(ok.path()).unwrap();
        assert_eq!((f, off, src.len()), (PacketFormat::Rs204, 0, 20 * 204));

        // SAFETY-free : id utilisateur lu via /proc (pas de `unsafe`).
        let root = std::fs::read_to_string("/proc/self/status").is_ok_and(|s| {
            s.lines()
                .any(|l| l.starts_with("Uid:") && l.split_whitespace().nth(1) == Some("0"))
        });
        if root {
            eprintln!("exécuté en root : cas accès refusé non vérifiable");
        } else {
            use std::os::unix::fs::PermissionsExt;
            let locked = temp(&stream(10));
            std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            assert!(matches!(probe(locked.path()), Err(OpenError::AccessDenied)));
        }
    }

    #[test]
    fn open_error_messages_are_not_empty() {
        for e in [
            OpenError::Empty,
            OpenError::AccessDenied,
            OpenError::NotTs,
            OpenError::NoVideo,
        ] {
            assert!(!e.to_string().is_empty());
        }
        assert!(!OpenError::Io(IoError::Modified).to_string().is_empty());
    }

    proptest! {
        #[ma_trace::covers("SYN.ROBUST.01")]
        #[test]
        fn scanner_never_panics(
            data in prop::collection::vec(prop_oneof![Just(0x47u8), any::<u8>()], 0..4000),
            fmt in 0usize..4,
            start in 0u64..5000,
            end in prop::option::of(0u64..5000),
        ) {
            let f = [PacketFormat::Ts188, PacketFormat::Tts192, PacketFormat::Rs204, PacketFormat::Ts208][fmt];
            let _ = detect_format(&data);
            let src = MemSource::new(data);
            let mut sink = VecSink::default();
            let mut sc = PacketScanner::new(&src, f, start, end);
            let mut last = None;
            while let Ok(Some(p)) = sc.next(&mut sink) {
                // Les offsets progressent strictement.
                if let Some(l) = last { prop_assert!(p.offset > l); }
                last = Some(p.offset);
            }
        }
    }
}
