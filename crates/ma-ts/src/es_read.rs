//! Relecture d'une plage d'ES par rejeu des paquets.

use ma_core::{Message, MessageSink};
use ma_io::{IoError, ReadAt};

use crate::packet::PacketFormat;
use crate::pes::{EsChunk, EsPos, MAX_AU_BYTES, PesAssembler};
use crate::scan::PacketScanner;

/// Paquets **du PID** examinés au plus après le paquet de fin pour terminer un en-tête PES en
/// cours (un en-tête compte 3 paquets au plus).
const HEADER_TAIL_PACKETS: usize = 8;
/// Octets lus au plus pour cette recherche, tous PID confondus.
const HEADER_TAIL_BYTES: u64 = 4 << 20;
const HEADER_UNFINISHED: IoError = IoError::InvalidData("en-tete pes incomplet");
const TOO_LARGE: IoError = IoError::InvalidData("au trop grande");

/// Puits qui jette les messages d'une relecture (déjà émis lors de l'indexation).
struct Discard;

impl MessageSink for Discard {
    fn push(&mut self, _: Message) {}
}

/// État d'une relecture : octets collectés et bornes.
struct Replay<'b> {
    start: EsPos,
    end: EsPos,
    buf: &'b mut Vec<u8>,
    asm: Option<PesAssembler>,
    done: bool,
}

impl Replay<'_> {
    /// Retient la part de `chunk` comprise dans `[start, end)`.
    fn take(&mut self, chunk: &EsChunk<'_>) -> Result<(), IoError> {
        let (anchor, from) = (chunk.pos.packet_offset, usize::from(chunk.pos.skip));
        if anchor > self.end.packet_offset {
            self.done = true;
            return Ok(());
        }
        let lo = if anchor == self.start.packet_offset {
            usize::from(self.start.skip).saturating_sub(from)
        } else {
            0
        };
        let hi = if anchor == self.end.packet_offset {
            self.done = true;
            usize::from(self.end.skip).saturating_sub(from)
        } else {
            usize::MAX
        };
        let hi = hi.min(chunk.data.len());
        if lo < hi {
            if self.buf.len() + (hi - lo) > MAX_AU_BYTES {
                return Err(TOO_LARGE);
            }
            self.buf.extend_from_slice(&chunk.data[lo..hi]);
        }
        Ok(())
    }

    /// Rejoue les paquets de `[from, limit)` ; s'arrête dès que la fin de la plage est atteinte.
    fn run(
        &mut self,
        src: &dyn ReadAt,
        format: PacketFormat,
        pid: u16,
        (from, limit): (u64, u64),
        pid_budget: Option<usize>,
    ) -> Result<(), IoError> {
        let mut seen = 0;
        let mut scanner = PacketScanner::new(src, format, from, Some(limit));
        let mut sink = Discard;
        while !self.done {
            let Some(p) = scanner.next(&mut sink)? else {
                break;
            };
            if p.packet.header.pid != pid {
                continue;
            }
            seen += 1;
            if pid_budget.is_some_and(|b| seen > b) {
                return Err(HEADER_UNFINISHED);
            }
            // Le premier paquet du PID décide de l'état initial : début de PES, ou ES en cours
            // (sans historique de compteur, donc ni trou ni message à ce moment).
            let asm = self.asm.get_or_insert_with(|| {
                if p.packet.header.pusi {
                    PesAssembler::new(pid)
                } else {
                    PesAssembler::resuming(pid)
                }
            });
            if let Some(chunk) = asm.push(&p, &mut sink) {
                self.take(&chunk)?;
            }
        }
        Ok(())
    }
}

/// Relit les octets d'ES de `[start, end)` dans `buf` (vidé d'abord).
///
/// Rejoue `PacketScanner` et `PesAssembler` à partir de `start.packet_offset` (voir la
/// documentation de [`EsPos`] pour la sémantique des positions) et renvoie exactement les octets
/// de l'assemblage en direct, y compris autour d'un trou de continuité. Les messages du rejeu
/// sont jetés. Le paquet `end.packet_offset` n'est pas lu quand `end.skip == 0` ; sinon rien
/// n'est lu au-delà, sauf pour finir l'en-tête PES en cours (8 paquets du PID au plus, 4 Mio au
/// plus) ; si l'en-tête n'est pas fini alors, c'est `InvalidData("en-tete pes incomplet")`.
///
/// # Errors
/// Erreur d'E/S de la source, ou `InvalidData("au trop grande")` si la plage dépasse
/// `MAX_AU_BYTES`, ou en-tête PES de la borne `end` introuvable (`InvalidData`).
pub fn read_es(
    src: &dyn ReadAt,
    format: PacketFormat,
    pid: u16,
    start: EsPos,
    end: EsPos,
    buf: &mut Vec<u8>,
) -> Result<(), IoError> {
    buf.clear();
    if start >= end {
        return Ok(());
    }
    let mut replay = Replay {
        start,
        end,
        buf,
        asm: None,
        done: false,
    };
    // Le paquet de fin est exclu à `skip == 0`, lu en entier sinon.
    let packet_end = end.packet_offset
        + if end.skip == 0 {
            0
        } else {
            format.tail() as u64
        };
    replay.run(src, format, pid, (start.packet_offset, packet_end), None)?;
    let pending = replay.asm.as_ref().is_some_and(PesAssembler::in_header);
    if !replay.done && end.skip > 0 && pending {
        let next = end.packet_offset + format.size() as u64;
        let limit = next + HEADER_TAIL_BYTES;
        replay.run(src, format, pid, (next, limit), Some(HEADER_TAIL_PACKETS))?;
        if !replay.done {
            // Jamais une plage silencieusement courte : l'assemblage en direct aurait des octets.
            return Err(HEADER_UNFINISHED);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::packet::PacketFormat;
    use crate::pes::{MAX_AU_BYTES, PesAssembler};
    use crate::scan::PacketScanner;
    use crate::testutil::{packet, pes_bytes, pes_packets, repacketize};
    use ma_core::VecSink;
    use ma_io::testing::MemSource;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PID: u16 = 0x100;

    fn es(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
            .collect()
    }

    /// Flux de test : 4 PES, dont un à en-tête sur 2 paquets, des paquets d'un autre PID, un
    /// paquet perdu, un doublon, un paquet TEI et un brouillé.
    fn stream() -> Vec<u8> {
        let mut cc = 0u8;
        let mut packets: Vec<[u8; 188]> = Vec::new();
        let mut add = |packets: &mut Vec<[u8; 188]>, pes: &[u8], first: usize| {
            let p = pes_packets(PID, cc, pes, first);
            cc = cc.wrapping_add(p.len() as u8);
            packets.extend(p);
        };
        add(
            &mut packets,
            &pes_bytes(Some(100), Some(90), &es(900, 1)),
            184,
        );
        packets.push(packet(0x200, true, 0, None, &[5; 100]));
        add(&mut packets, &pes_bytes(Some(200), None, &es(700, 2)), 12);
        packets.push(packet(0x200, false, 1, None, &[6; 100]));
        let third = packets.len();
        add(&mut packets, &pes_bytes(Some(300), None, &es(1200, 3)), 184);
        let fourth = packets.len();
        add(&mut packets, &pes_bytes(Some(400), None, &es(1000, 4)), 184);
        // Dégâts : paquet retiré et doublon (PES 3), TEI et brouillé (PES 4).
        packets.remove(third + 2);
        packets.insert(third + 6, packets[third + 5]);
        packets[fourth + 3][1] |= 0x80;
        packets[fourth + 4][3] |= 0x80;
        packets.into_iter().flatten().collect()
    }

    /// Chunks de l'assemblage en direct : (`pos`, octets, `gap_before`).
    fn live(data: &[u8], format: PacketFormat) -> Vec<(EsPos, Vec<u8>, bool)> {
        let src = MemSource::new(data.to_vec());
        let start = format.sync_offset() as u64;
        let mut sc = PacketScanner::new(&src, format, start, None);
        let mut asm = PesAssembler::new(PID);
        let mut sink = VecSink::default();
        let mut out = Vec::new();
        while let Some(p) = sc.next(&mut sink).unwrap() {
            if let Some(c) = asm.push(&p, &mut sink) {
                out.push((c.pos, c.data.to_vec(), c.gap_before));
            }
        }
        out
    }

    /// Toutes les positions d'octet du flux d'ES en direct, avec leur rang dans l'ES concaténé.
    fn positions(chunks: &[(EsPos, Vec<u8>, bool)]) -> (Vec<u8>, Vec<(EsPos, usize)>) {
        let mut es = Vec::new();
        let mut pos = Vec::new();
        for (p, d, _) in chunks {
            for n in 0..=d.len() {
                pos.push((
                    EsPos {
                        packet_offset: p.packet_offset,
                        skip: p.skip + n as u16,
                    },
                    es.len() + n,
                ));
            }
            es.extend(d);
        }
        assert!(
            pos.windows(2).all(|w| w[0].0 < w[1].0),
            "positions strictement croissantes"
        );
        (es, pos)
    }

    #[test]
    fn read_es_equals_live_assembly_for_every_range_and_format() {
        let ts = stream();
        for format in [
            PacketFormat::Ts188,
            PacketFormat::Tts192,
            PacketFormat::Rs204,
            PacketFormat::Ts208,
        ] {
            let data = repacketize(&ts, format);
            let chunks = live(&data, format);
            assert!(chunks.iter().any(|c| c.2), "le flux contient des trous");
            let (all, pos) = positions(&chunks);
            let src = MemSource::new(data);
            let mut buf = Vec::new();
            // Début et fin pris à chaque 7e position (début, milieu et fin de paquets).
            for (i, &(s, si)) in pos.iter().enumerate().step_by(7) {
                for &(e, ei) in pos.iter().skip(i).step_by(11) {
                    // Deux positions ne désignant que des fins de chunks différents du même octet
                    // (skip = longueur du chunk / skip = 0 du chunk suivant) sont équivalentes.
                    read_es(&src, format, PID, s, e, &mut buf).unwrap();
                    assert_eq!(buf, all[si..ei], "{format:?} {s:?}..{e:?}");
                }
            }
        }
    }

    #[test]
    fn read_es_whole_stream_and_empty_ranges() {
        let ts = stream();
        let chunks = live(&ts, PacketFormat::Ts188);
        let (all, _) = positions(&chunks);
        let src = MemSource::new(ts);
        let mut buf = vec![1, 2, 3];
        let first = chunks[0].0;
        let far = EsPos {
            packet_offset: 1 << 40,
            skip: 0,
        };
        read_es(&src, PacketFormat::Ts188, PID, first, far, &mut buf).unwrap();
        assert_eq!(buf, all);
        // Plage vide ou inversée : tampon vidé, pas d'erreur.
        buf.push(9);
        read_es(&src, PacketFormat::Ts188, PID, first, first, &mut buf).unwrap();
        assert!(buf.is_empty());
        buf.push(9);
        read_es(&src, PacketFormat::Ts188, PID, far, first, &mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn read_es_from_a_pes_whose_header_spans_two_packets() {
        let ts = stream();
        let chunks = live(&ts, PacketFormat::Ts188);
        // Le 2e PES a son en-tête sur 2 paquets : son 1er chunk est ancré au paquet du PUSI.
        let spanning = chunks
            .iter()
            .find(|c| c.0.skip == 0 && c.1.starts_with(&es(10, 2)))
            .unwrap();
        let sp = packet_offset_of_second_pes();
        assert_eq!(spanning.0.packet_offset, sp);
        let src = MemSource::new(ts);
        let mut buf = Vec::new();
        let end = EsPos {
            packet_offset: sp,
            skip: 20,
        };
        read_es(&src, PacketFormat::Ts188, PID, spanning.0, end, &mut buf).unwrap();
        assert_eq!(buf, es(20, 2));
        // Fin à skip = 0 : le paquet de fin est exclu.
        let end = EsPos {
            packet_offset: sp,
            skip: 0,
        };
        read_es(&src, PacketFormat::Ts188, PID, spanning.0, end, &mut buf).unwrap();
        assert!(buf.is_empty());
    }

    /// Offset du 1er paquet du 2e PES de `stream` (900 octets + 19 d'en-tête = 5 paquets, puis
    /// un paquet d'un autre PID).
    fn packet_offset_of_second_pes() -> u64 {
        6 * 188
    }

    /// Source qui mémorise l'offset de fin maximal lu.
    struct Recording {
        inner: MemSource,
        max_end: AtomicU64,
    }

    impl ReadAt for Recording {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
            self.max_end
                .fetch_max(offset + buf.len() as u64, Ordering::SeqCst);
            self.inner.read_at(offset, buf)
        }
    }

    #[test]
    fn read_es_never_reads_past_the_end_packet() {
        let ts = stream();
        let chunks = live(&ts, PacketFormat::Ts188);
        let rec = Recording {
            inner: MemSource::new(ts),
            max_end: AtomicU64::new(0),
        };
        let mut buf = Vec::new();
        // Fin à skip = 0 : le paquet de fin n'est pas lu du tout.
        let end = chunks[6].0;
        assert_eq!(end.skip, 0);
        read_es(&rec, PacketFormat::Ts188, PID, chunks[0].0, end, &mut buf).unwrap();
        assert!(rec.max_end.load(Ordering::SeqCst) <= end.packet_offset);
        // Fin au milieu d'un paquet : ce paquet est lu en entier, pas le suivant.
        let end = EsPos {
            packet_offset: chunks[6].0.packet_offset,
            skip: 5,
        };
        rec.max_end.store(0, Ordering::SeqCst);
        read_es(&rec, PacketFormat::Ts188, PID, chunks[0].0, end, &mut buf).unwrap();
        assert!(rec.max_end.load(Ordering::SeqCst) <= end.packet_offset + 188);
    }

    #[test]
    fn read_es_rejects_oversized_au() {
        // Un PES de 33 Mio d'ES.
        let big = pes_bytes(Some(1), None, &vec![0xAB; MAX_AU_BYTES + 1000]);
        let data: Vec<u8> = pes_packets(PID, 0, &big, 184)
            .into_iter()
            .flatten()
            .collect();
        let src = MemSource::new(data);
        let first = EsPos {
            packet_offset: 0,
            skip: 0,
        };
        let far = EsPos {
            packet_offset: 1 << 40,
            skip: 0,
        };
        let mut buf = Vec::new();
        let err = read_es(&src, PacketFormat::Ts188, PID, first, far, &mut buf).unwrap_err();
        assert!(
            matches!(err, IoError::InvalidData("au trop grande")),
            "{err:?}"
        );
        assert!(buf.len() <= MAX_AU_BYTES);
    }

    #[test]
    fn read_es_finishes_a_pes_header_despite_interleaved_pids() {
        // PES à en-tête sur 2 paquets, 5 paquets d'un autre PID entre P1 et P2.
        let pes = pes_bytes(Some(10), Some(5), &es(300, 8));
        let p = pes_packets(PID, 0, &pes, 10);
        let mut packets = vec![p[0]];
        packets.extend((0..5).map(|k| packet(0x200, false, k, None, &[k; 50])));
        packets.extend(&p[1..]);
        let data: Vec<u8> = packets.into_iter().flatten().collect();
        let chunks = live(&data, PacketFormat::Ts188);
        assert_eq!(
            chunks[0].0,
            EsPos {
                packet_offset: 0,
                skip: 0
            }
        );
        let (all, _) = positions(&chunks);
        assert_eq!(all, es(300, 8));
        let src = MemSource::new(data);
        let mut buf = Vec::new();
        let end = EsPos {
            packet_offset: 0,
            skip: 20,
        };
        read_es(&src, PacketFormat::Ts188, PID, chunks[0].0, end, &mut buf).unwrap();
        assert_eq!(buf, all[..20]);
        // En-tête qui ne se termine jamais dans la fenêtre : erreur, pas une plage courte.
        let mut lone = vec![pes_packets(PID, 0, &pes, 10)[0]];
        lone.extend((0..20).map(|k| packet(PID + 1, false, k, None, &[0; 50])));
        let data: Vec<u8> = lone.into_iter().flatten().collect();
        let src = MemSource::new(data);
        let start = EsPos {
            packet_offset: 0,
            skip: 0,
        };
        let err = read_es(&src, PacketFormat::Ts188, PID, start, end, &mut buf).unwrap_err();
        assert!(
            matches!(err, IoError::InvalidData("en-tete pes incomplet")),
            "{err:?}"
        );
    }
}
