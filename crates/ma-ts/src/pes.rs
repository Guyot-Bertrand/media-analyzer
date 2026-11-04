//! Assemblage des PES d'un PID : en-tête PES (PTS/DTS), octets d'ES, positions dans l'ES et
//! continuité des compteurs.
//!
//! # Positions dans l'ES (`EsPos`)
//!
//! `EsPos { packet_offset, skip }` désigne un octet d'ES : `packet_offset` est l'offset de l'octet
//! 0x47 d'un paquet TS du PID, `skip` le nombre d'octets d'ES qui le précèdent **dans la charge
//! de ce paquet** (l'en-tête PES n'est pas de l'ES et ne compte pas). Un début de PES dont
//! l'en-tête occupe tout le début de la charge a donc `skip = 0` sur son premier octet d'ES.
//!
//! - Le `pos` d'un [`EsChunk`] est la position de son premier octet ; l'octet `n` du chunk est
//!   à `EsPos { packet_offset, skip: pos.skip + n }`.
//! - Une position de fin à `skip == 0` désigne le début du paquet : ses octets sont exclus.
//! - Cas particulier : quand l'en-tête PES est réparti sur plusieurs paquets, les octets d'ES
//!   du dernier de ces paquets sont ancrés au **premier** paquet du PES (celui qui porte le
//!   `PUSI`) avec `skip` compté à partir de 0 : rejouer depuis un paquet intermédiaire est
//!   impossible, car l'en-tête y est en cours.
//!
//! [`crate::read_es`] rejoue les paquets depuis `start.packet_offset` et renvoie les mêmes octets
//! que l'assemblage en direct.

use ma_core::{Family, Message, MessageSink, Param, Severity};

use crate::messages::{CC_GAP, PES_INVALID, SCRAMBLED};
use crate::scan::ScannedPacket;

const SUB_FAMILY: &str = "continuity";
/// Taille maximale d'un en-tête PES : 9 octets fixes et 255 de données facultatives.
const MAX_PES_HEADER: usize = 9 + 255;

/// Taille maximale d'une AU (32 Mio), comme en M1.
pub const MAX_AU_BYTES: usize = 32 * 1024 * 1024;

/// Position d'un octet d'ES (voir la documentation du module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EsPos {
    /// Offset de l'octet 0x47 du paquet TS d'ancrage.
    pub packet_offset: u64,
    /// Octets d'ES qui précèdent, comptés depuis le début de la charge de ce paquet.
    pub skip: u16,
}

/// En-tête PES décodé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PesHeader {
    pub stream_id: u8,
    /// `PES_packet_length` déclaré (0 : non borné, fréquent pour la vidéo).
    pub packet_length: u16,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// Taille de l'en-tête (début de l'ES) en octets.
    pub header_len: usize,
}

/// Échec de décodage d'un en-tête PES.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PesError {
    #[error("préfixe 00 00 01 absent")]
    NoStartCode,
    #[error("en-tête PES tronqué")]
    Truncated,
}

/// Flux dont le PES porte l'en-tête facultatif (13818-1 tableau 2-21).
fn has_optional_header(stream_id: u8) -> bool {
    !matches!(
        stream_id,
        0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF
    )
}

/// Horodatage de 33 bits sur 5 octets (marqueurs non vérifiés).
fn timestamp(b: &[u8]) -> Option<u64> {
    let b: &[u8; 5] = b.get(..5)?.try_into().ok()?;
    Some(
        u64::from(b[0] >> 1 & 7) << 30
            | u64::from(b[1]) << 22
            | u64::from(b[2] >> 1) << 15
            | u64::from(b[3]) << 7
            | u64::from(b[4] >> 1),
    )
}

/// Décode l'en-tête PES au début de `buf`.
///
/// # Errors
/// `NoStartCode` si le préfixe n'est pas `00 00 01`, `Truncated` si `buf` ne contient pas tout
/// l'en-tête. Des drapeaux PTS/DTS sans les octets correspondants (champ trop court) donnent
/// simplement des horodatages absents.
pub fn parse_pes_header(buf: &[u8]) -> Result<PesHeader, PesError> {
    // Le préfixe est jugé sur les octets déjà disponibles : un faux départ n'attend pas la suite.
    if buf.iter().zip([0u8, 0, 1]).any(|(&b, want)| b != want) {
        return Err(PesError::NoStartCode);
    }
    let [_, _, _, stream_id, len_hi, len_lo, ..] = *buf else {
        return Err(PesError::Truncated);
    };
    let packet_length = u16::from(len_hi) << 8 | u16::from(len_lo);
    if !has_optional_header(stream_id) {
        return Ok(PesHeader {
            stream_id,
            packet_length,
            pts: None,
            dts: None,
            header_len: 6,
        });
    }
    let [_, flags, data_len, ..] = buf[6..] else {
        return Err(PesError::Truncated);
    };
    let header_len = 9 + usize::from(data_len);
    let data = buf.get(9..header_len).ok_or(PesError::Truncated)?;
    let (pts, dts) = match flags >> 6 {
        2 => (timestamp(data), None),
        3 => (timestamp(data), data.get(5..).and_then(timestamp)),
        _ => (None, None),
    };
    Ok(PesHeader {
        stream_id,
        packet_length,
        pts,
        dts,
        header_len,
    })
}

/// Début d'un PES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PesStart {
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// Offset du paquet TS qui porte le début du PES (`PUSI`).
    pub packet_offset: u64,
    pub packet_index: u64,
}

/// Octets d'ES d'un paquet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EsChunk<'a> {
    pub data: &'a [u8],
    /// Position du premier octet de `data`.
    pub pos: EsPos,
    /// Présent sur le premier chunk d'un PES (même si `data` est vide).
    pub pes_start: Option<PesStart>,
    /// Des octets ont été perdus (compteur, TEI, brouillage) entre le chunk précédent et celui-ci.
    pub gap_before: bool,
}

/// Phase d'un PES.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Avant le premier début de PES, ou PES abandonné : les paquets sont ignorés jusqu'au
    /// prochain `PUSI`.
    Skip,
    /// En-tête en cours de mise en tampon.
    Header,
    /// En-tête décodé : la charge est de l'ES.
    Data,
}

/// Assembleur de PES d'un PID : suit le compteur de continuité et sépare l'en-tête PES des
/// octets d'ES, paquet par paquet (le tampon d'en-tête reste sous `MAX_PES_HEADER` octets plus une charge de paquet : il est
/// décodé dès qu'il est complet, et `Truncated` signifie qu'il est plus court que son en-tête).
pub struct PesAssembler {
    pid: u16,
    phase: Phase,
    last_cc: Option<u8>,
    /// Le doublon admis après `last_cc` a déjà été vu.
    duplicate_seen: bool,
    /// Des octets ont été perdus depuis le dernier chunk renvoyé.
    gap: bool,
    scrambled_reported: bool,
    header: Vec<u8>,
    /// Paquet qui porte le début du PES en cours d'en-tête : (offset, index).
    header_packet: (u64, u64),
}

impl PesAssembler {
    #[must_use]
    pub fn new(pid: u16) -> Self {
        PesAssembler {
            pid,
            phase: Phase::Skip,
            last_cc: None,
            duplicate_seen: false,
            gap: false,
            scrambled_reported: false,
            header: Vec::with_capacity(MAX_PES_HEADER + 184),
            header_packet: (0, 0),
        }
    }

    /// Assembleur qui reprend au milieu d'un PES (relecture à partir d'un paquet de données) :
    /// le premier paquet est de l'ES, sans historique de compteur.
    pub(crate) fn resuming(pid: u16) -> Self {
        PesAssembler {
            phase: Phase::Data,
            ..PesAssembler::new(pid)
        }
    }

    /// Vrai tant qu'un en-tête PES attend la suite de ses octets.
    pub(crate) fn in_header(&self) -> bool {
        self.phase == Phase::Header
    }

    fn message(&self, pkt: &ScannedPacket<'_>, def: &'static ma_core::MsgDef) -> Message {
        Message::new(def, Severity::Warning, Family::Transport, SUB_FAMILY)
            .param("pid", Param::Pid(self.pid))
            .at(pkt.offset)
            .pid(self.pid)
    }

    /// Des octets sont perdus : le PES dont l'en-tête n'est pas fini est abandonné.
    fn lose(&mut self) {
        self.gap = true;
        if self.phase == Phase::Header {
            self.phase = Phase::Skip;
            self.header.clear();
        }
    }

    /// Traite un paquet ; renvoie ses octets d'ES s'il en porte. Les paquets d'un autre PID ou
    /// sans charge utile sont ignorés.
    ///
    /// - Compteur de continuité : un doublon unique est ignoré ; un saut est un trou
    ///   (`gap_before` sur le chunk suivant, message `ts-cc-gap`), sauf avec
    ///   `discontinuity_indicator`. Le premier paquet n'a pas d'historique.
    /// - `transport_error_indicator` : charge ignorée, trou, historique de compteur oublié.
    /// - Brouillage : charge ignorée, trou, message `ts-scrambled` une fois par PID.
    pub fn push<'a>(
        &mut self,
        pkt: &'a ScannedPacket<'a>,
        sink: &mut dyn MessageSink,
    ) -> Option<EsChunk<'a>> {
        let h = &pkt.packet.header;
        if h.pid != self.pid || h.afc & 1 == 0 {
            return None;
        }
        if h.tei {
            self.lose();
            self.last_cc = None;
            return None;
        }
        let discontinuity = pkt.packet.adaptation.is_some_and(|a| a.discontinuity);
        match self.last_cc {
            Some(last) if last == h.cc && !self.duplicate_seen => {
                self.duplicate_seen = true;
                return None;
            }
            Some(last) if !discontinuity && h.cc != (last + 1) & 0x0F => {
                sink.push(
                    self.message(pkt, &CC_GAP)
                        .param("expected", Param::UInt(u64::from((last + 1) & 0x0F)))
                        .param("got", Param::UInt(u64::from(h.cc))),
                );
                self.lose();
            }
            _ => {}
        }
        if self.last_cc != Some(h.cc) {
            self.duplicate_seen = false;
        }
        self.last_cc = Some(h.cc);
        if h.scrambling != 0 {
            if !self.scrambled_reported {
                self.scrambled_reported = true;
                sink.push(self.message(pkt, &SCRAMBLED));
            }
            self.lose();
            return None;
        }
        let payload = pkt.packet.payload;
        if h.pusi {
            if self.phase == Phase::Header {
                // En-tête jamais terminé : le PES précédent est perdu.
                sink.push(self.message(pkt, &PES_INVALID));
                self.gap = true;
            }
            self.header.clear();
            self.header.extend_from_slice(payload);
            self.header_packet = (pkt.offset, pkt.index);
            self.phase = Phase::Header;
            return self.decode_header(pkt, 0, sink);
        }
        match self.phase {
            Phase::Skip => None,
            Phase::Header => {
                let before = self.header.len();
                self.header.extend_from_slice(payload);
                self.decode_header(pkt, before, sink)
            }
            Phase::Data => (!payload.is_empty()).then(|| EsChunk {
                data: payload,
                pos: EsPos {
                    packet_offset: pkt.offset,
                    skip: 0,
                },
                pes_start: None,
                gap_before: std::mem::take(&mut self.gap),
            }),
        }
    }

    /// Tente de décoder le tampon d'en-tête (`before` octets étaient déjà là avant ce paquet).
    fn decode_header<'a>(
        &mut self,
        pkt: &'a ScannedPacket<'a>,
        before: usize,
        sink: &mut dyn MessageSink,
    ) -> Option<EsChunk<'a>> {
        let parsed = match parse_pes_header(&self.header) {
            Err(PesError::Truncated) => return None,
            Ok(h) if h.packet_length == 0 || usize::from(h.packet_length) + 6 >= h.header_len => h,
            Ok(_) | Err(PesError::NoStartCode) => {
                sink.push(self.message(pkt, &PES_INVALID));
                self.phase = Phase::Skip;
                self.header.clear();
                self.gap = true;
                return None;
            }
        };
        // Les octets d'ES sont la fin de la charge de ce paquet (l'en-tête finit ici : il
        // n'était pas complet avant).
        let es_len = self.header.len().saturating_sub(parsed.header_len);
        let payload = pkt.packet.payload;
        let data = payload.get(payload.len().checked_sub(es_len)?..)?;
        let (offset, index) = self.header_packet;
        self.phase = Phase::Data;
        self.header.clear();
        // Ancrage au paquet du PUSI si l'en-tête a commencé dans un paquet précédent.
        let packet_offset = if before == 0 { pkt.offset } else { offset };
        Some(EsChunk {
            data,
            pos: EsPos {
                packet_offset,
                skip: 0,
            },
            pes_start: Some(PesStart {
                pts: parsed.pts,
                dts: parsed.dts,
                packet_offset: offset,
                packet_index: index,
            }),
            gap_before: std::mem::take(&mut self.gap),
        })
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::messages::{CC_GAP, PES_INVALID, SCRAMBLED};
    use crate::packet::parse_packet;
    use crate::testutil::{AfSpec, packet, pes_bytes, pes_header, pes_packets};
    use ma_core::{Param, VecSink};
    use proptest::prelude::*;

    const PID: u16 = 0x100;

    /// Chunk possédé, pour comparer sans emprunt.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Got {
        data: Vec<u8>,
        pos: EsPos,
        start: Option<PesStart>,
        gap: bool,
    }

    /// Passe `packets` (consécutifs, 188 octets, offsets `i * 188`) à un assembleur neuf.
    pub(super) fn run(packets: &[[u8; 188]], sink: &mut VecSink) -> Vec<Got> {
        let mut asm = PesAssembler::new(PID);
        let mut out = Vec::new();
        for (i, raw) in packets.iter().enumerate() {
            let sp = ScannedPacket {
                offset: i as u64 * 188,
                index: i as u64,
                packet: parse_packet(raw).unwrap(),
            };
            if let Some(c) = asm.push(&sp, sink) {
                out.push(Got {
                    data: c.data.to_vec(),
                    pos: c.pos,
                    start: c.pes_start,
                    gap: c.gap_before,
                });
            }
        }
        out
    }

    fn es(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    }

    fn concat(chunks: &[Got]) -> Vec<u8> {
        chunks.iter().flat_map(|c| c.data.clone()).collect()
    }

    #[test]
    fn pes_header_pts_dts() {
        // PTS seul, valeur sur 33 bits.
        let pts = 0x1_2345_6789;
        let h = pes_header(0, Some(pts), None, 0);
        let got = parse_pes_header(&h).unwrap();
        assert_eq!(
            got,
            PesHeader {
                stream_id: 0xE0,
                packet_length: 0,
                pts: Some(pts),
                dts: None,
                header_len: 14,
            }
        );
        // PTS et DTS, bourrage dans l'en-tête, octets d'ES derrière.
        let mut h = pes_header(0x1234, Some(90_000), Some(86_400), 3);
        h.extend([9, 9, 9]);
        let got = parse_pes_header(&h).unwrap();
        assert_eq!(
            (got.packet_length, got.pts, got.dts, got.header_len),
            (0x1234, Some(90_000), Some(86_400), 9 + 10 + 3)
        );
        // Ni PTS ni DTS.
        let got = parse_pes_header(&pes_header(0, None, None, 0)).unwrap();
        assert_eq!((got.pts, got.dts, got.header_len), (None, None, 9));
        // Flux sans en-tête facultatif (bourrage 0xBE) : 6 octets.
        let got = parse_pes_header(&[0, 0, 1, 0xBE, 0, 3, 0xFF, 0xFF, 0xFF]).unwrap();
        assert_eq!((got.stream_id, got.header_len, got.pts), (0xBE, 6, None));
        // Marqueurs de PTS déclarés mais données absentes : pas de PTS, pas d'erreur.
        let mut bad = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 2, 0xAA, 0xBB];
        let got = parse_pes_header(&bad).unwrap();
        assert_eq!((got.pts, got.header_len), (None, 11));
        // Erreurs.
        assert_eq!(parse_pes_header(&[]), Err(PesError::Truncated));
        assert_eq!(
            parse_pes_header(&[0, 0, 1, 0xE0, 0]),
            Err(PesError::Truncated)
        );
        assert_eq!(parse_pes_header(&h[..13]), Err(PesError::Truncated));
        assert_eq!(parse_pes_header(&h[..8]), Err(PesError::Truncated));
        assert_eq!(
            parse_pes_header(&[0, 1, 1, 0xE0, 0, 0]),
            Err(PesError::NoStartCode)
        );
        assert_eq!(parse_pes_header(&[7]), Err(PesError::NoStartCode));
        bad[2] = 2;
        assert_eq!(parse_pes_header(&bad), Err(PesError::NoStartCode));
    }

    #[test]
    fn single_packet_pes_gives_positions_in_es_bytes() {
        let a = es(500, 1);
        let b = es(100, 2);
        let mut packets = pes_packets(PID, 0, &pes_bytes(Some(900), Some(800), &a), 184);
        packets.extend(pes_packets(
            PID,
            packets.len() as u8,
            &pes_bytes(Some(1900), None, &b),
            184,
        ));
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        assert!(sink.messages.is_empty());
        assert_eq!(concat(&got), [a.clone(), b.clone()].concat());
        // Premier PES : en-tête de 19 octets, 165 octets d'ES dans le 1er paquet.
        assert_eq!(got[0].data.len(), 184 - 19);
        assert_eq!(
            got[0].pos,
            EsPos {
                packet_offset: 0,
                skip: 0
            }
        );
        assert_eq!(
            got[0].start,
            Some(PesStart {
                pts: Some(900),
                dts: Some(800),
                packet_offset: 0,
                packet_index: 0
            })
        );
        assert_eq!(
            got[1].pos,
            EsPos {
                packet_offset: 188,
                skip: 0
            }
        );
        assert_eq!(got[1].start, None);
        // Début du second PES : paquet 3 (500 + 19 octets = 3 paquets).
        let second = &got[3];
        assert_eq!(
            second.pos,
            EsPos {
                packet_offset: 3 * 188,
                skip: 0
            }
        );
        assert_eq!(second.start.unwrap().packet_index, 3);
        assert_eq!(second.start.unwrap().pts, Some(1900));
        assert!(got.iter().all(|c| !c.gap));
    }

    #[test]
    fn pes_header_split_over_two_packets() {
        let body = es(300, 5);
        let pes = pes_bytes(Some(4242), Some(4000), &body);
        // 10 octets d'en-tête dans le 1er paquet (l'en-tête en compte 19), 9 dans le 2e.
        let packets = pes_packets(PID, 3, &pes, 10);
        assert_eq!(packets.len(), 3);
        let mut asm = PesAssembler::new(PID);
        let mut sink = VecSink::default();
        let sp0 = ScannedPacket {
            offset: 1000,
            index: 7,
            packet: parse_packet(&packets[0]).unwrap(),
        };
        assert!(
            asm.push(&sp0, &mut sink).is_none(),
            "en-tête incomplet : pas de chunk"
        );
        let sp1 = ScannedPacket {
            offset: 1188,
            index: 8,
            packet: parse_packet(&packets[1]).unwrap(),
        };
        let c1 = asm.push(&sp1, &mut sink).unwrap();
        assert_eq!(c1.data, &pes[19..10 + 184]);
        // Ancrage au paquet du PUSI (voir la doc du module), skip compté à partir de 0.
        assert_eq!(
            c1.pos,
            EsPos {
                packet_offset: 1000,
                skip: 0
            }
        );
        assert_eq!(
            c1.pes_start,
            Some(PesStart {
                pts: Some(4242),
                dts: Some(4000),
                packet_offset: 1000,
                packet_index: 7
            })
        );
        let sp2 = ScannedPacket {
            offset: 1376,
            index: 9,
            packet: parse_packet(&packets[2]).unwrap(),
        };
        let c2 = asm.push(&sp2, &mut sink).unwrap();
        assert_eq!(
            c2.pos,
            EsPos {
                packet_offset: 1376,
                skip: 0
            }
        );
        assert_eq!(c2.pes_start, None);
        assert_eq!([c1.data, c2.data].concat(), body);
        assert!(sink.messages.is_empty());
    }

    #[test]
    fn pes_header_over_three_packets_is_bounded() {
        // 9 + 255 octets d'en-tête, répartis 5 + 184 + le reste : l'en-tête reste borné.
        let mut pes = pes_header(0, Some(1), None, 250);
        assert_eq!(pes.len(), 264);
        let body = es(40, 9);
        pes.extend(&body);
        let packets = pes_packets(PID, 0, &pes, 5);
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, body);
        assert_eq!(
            got[0].pos,
            EsPos {
                packet_offset: 0,
                skip: 0
            }
        );
        assert!(sink.messages.is_empty());
    }

    #[test]
    fn invalid_pes_header_is_reported_and_skipped() {
        let good = pes_packets(PID, 2, &pes_bytes(Some(1), None, &es(50, 1)), 184);
        // Début de PES sans préfixe 00 00 01, suivi d'une suite qui doit être ignorée.
        let mut junk = vec![packet(
            PID,
            true,
            0,
            None,
            &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 1, 2],
        )];
        junk.push(packet(PID, false, 1, None, &[7; 184]));
        junk.extend(good);
        let mut sink = VecSink::default();
        let got = run(&junk, &mut sink);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, es(50, 1));
        assert!(got[0].gap, "le PES perdu précède le suivant");
        assert_eq!(sink.messages.len(), 1);
        let m = &sink.messages[0];
        assert_eq!(
            (m.id, m.pid, m.offset),
            (PES_INVALID.id, Some(PID), Some(0))
        );
        assert_eq!(m.params, vec![("pid", Param::Pid(PID))]);
        // packet_length incohérent avec l'en-tête : invalide aussi.
        let mut h = pes_header(3, Some(1), None, 0);
        h.extend([1, 2, 3]);
        let mut sink = VecSink::default();
        assert!(run(&pes_packets(PID, 0, &h, 184), &mut sink).is_empty());
        assert_eq!(sink.messages[0].id, PES_INVALID.id);
        // En-tête commencé puis interrompu par un nouveau début de PES.
        let mut packets = pes_packets(PID, 0, &pes_bytes(Some(1), None, &es(50, 1)), 6);
        packets.truncate(1);
        packets.extend(pes_packets(
            PID,
            1,
            &pes_bytes(Some(2), None, &es(20, 2)),
            184,
        ));
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, es(20, 2));
        assert!(got[0].gap);
        assert_eq!(sink.messages.len(), 1);
    }

    #[test]
    fn data_before_the_first_pes_start_is_ignored() {
        let mut packets = vec![packet(PID, false, 0, None, &[1; 184])];
        packets.extend(pes_packets(PID, 1, &pes_bytes(None, None, &es(30, 4)), 184));
        packets.push(packet(0x200, true, 0, None, &[1; 10]));
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, es(30, 4));
        assert!(!got[0].gap && sink.messages.is_empty());
    }

    /// Trois PES de 1000 octets d'ES (cc de 0 à 17 environ).
    fn three_pes() -> Vec<[u8; 188]> {
        let mut packets = Vec::new();
        for k in 0..3u8 {
            let cc = packets.len() as u8;
            packets.extend(pes_packets(
                PID,
                cc,
                &pes_bytes(Some(u64::from(k) * 3600), None, &es(1000, k)),
                184,
            ));
        }
        packets
    }

    #[test]
    fn lost_packet_marks_gap() {
        let packets = three_pes();
        let ccs: Vec<u8> = packets.iter().map(|p| p[3] & 0x0F).collect();
        // Retire le 3e paquet du 2e PES (indice 8, cc 8).
        let lost = 8;
        let mut cut = packets.clone();
        cut.remove(lost);
        let mut sink = VecSink::default();
        let got = run(&cut, &mut sink);
        assert_eq!(sink.messages.len(), 1, "{:?}", sink.messages);
        let m = &sink.messages[0];
        assert_eq!((m.id, m.pid), (CC_GAP.id, Some(PID)));
        let n = ccs[lost - 1];
        assert_eq!(
            m.params,
            vec![
                ("pid", Param::Pid(PID)),
                ("expected", Param::UInt(u64::from((n + 1) & 15))),
                ("got", Param::UInt(u64::from((n + 2) & 15))),
            ]
        );
        assert_eq!(
            m.offset,
            Some(lost as u64 * 188),
            "paquet qui révèle le trou"
        );
        // Seul le chunk qui suit le trou le signale.
        let flagged: Vec<usize> = got
            .iter()
            .enumerate()
            .filter(|(_, c)| c.gap)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(got[flagged[0]].pos.packet_offset, lost as u64 * 188);
        // Les octets perdus sont ceux du paquet retiré, rien d'autre.
        let full = concat(&run(&packets, &mut VecSink::default()));
        assert_eq!(concat(&got).len(), full.len() - 184);

        // Doublon unique : ignoré, sans message ni trou ; données identiques.
        let mut dup = packets.clone();
        dup.insert(lost, packets[lost - 1]);
        let mut sink = VecSink::default();
        let got = run(&dup, &mut sink);
        assert!(sink.messages.is_empty());
        assert!(got.iter().all(|c| !c.gap));
        assert_eq!(concat(&got), full);

        // Doublon répété deux fois : le 3e exemplaire est un trou.
        let mut dup3 = packets.clone();
        dup3.insert(lost, packets[lost - 1]);
        dup3.insert(lost, packets[lost - 1]);
        let mut sink = VecSink::default();
        let got = run(&dup3, &mut sink);
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(got.iter().filter(|c| c.gap).count(), 1);

        // discontinuity_indicator : le saut de compteur n'est pas un trou (le paquet réécrit perd
        // 2 octets d'ES pour loger le champ d'adaptation).
        let mut disc = cut.clone();
        let af = AfSpec {
            discontinuity: true,
            ..AfSpec::default()
        };
        disc[lost] = packet(
            PID,
            false,
            cut[lost][3] & 0x0F,
            Some(af),
            &cut[lost][4..186],
        );
        let mut sink = VecSink::default();
        let got = run(&disc, &mut sink);
        assert!(sink.messages.is_empty(), "{:?}", sink.messages);
        assert!(got.iter().all(|c| !c.gap));
        assert_eq!(concat(&got).len(), full.len() - 184 - 2);
    }

    #[test]
    fn adaptation_only_packets_do_not_touch_the_counter() {
        let mut packets = pes_packets(PID, 0, &pes_bytes(Some(1), None, &es(400, 3)), 184);
        // Paquet sans charge (afc = 2) entre deux paquets : cc identique au précédent.
        let only_af = packet(
            PID,
            false,
            packets[0][3] & 0x0F,
            Some(AfSpec {
                pcr: Some(ma_core::Pcr { base: 5, ext: 0 }),
                ..AfSpec::default()
            }),
            &[],
        );
        assert_eq!(only_af[3] >> 4 & 3, 2);
        packets.insert(1, only_af);
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        assert!(sink.messages.is_empty());
        assert_eq!(concat(&got), es(400, 3));
    }

    #[test]
    fn tei_packet_is_a_gap_not_data() {
        let mut packets = three_pes();
        packets[8][1] |= 0x80;
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        let full = concat(&run(&three_pes(), &mut VecSink::default()));
        assert_eq!(concat(&got).len(), full.len() - 184);
        assert_eq!(got.iter().filter(|c| c.gap).count(), 1);
        assert!(
            sink.messages.is_empty(),
            "le compteur reste cohérent : pas de ts-cc-gap"
        );
    }

    #[test]
    fn scrambled_payload_is_a_gap_not_data() {
        let mut packets = three_pes();
        for i in [8, 9] {
            packets[i][3] |= 0x80;
        }
        let mut sink = VecSink::default();
        let got = run(&packets, &mut sink);
        let full = concat(&run(&three_pes(), &mut VecSink::default()));
        assert_eq!(concat(&got).len(), full.len() - 2 * 184);
        assert_eq!(got.iter().filter(|c| c.gap).count(), 1);
        // Un seul message pour le PID, et le compteur reste suivi.
        assert_eq!(sink.messages.len(), 1);
        let m = &sink.messages[0];
        assert_eq!((m.id, m.pid), (SCRAMBLED.id, Some(PID)));
        assert_eq!(m.params, vec![("pid", Param::Pid(PID))]);
        // Un en-tête de PES brouillé n'est pas décodé : tout ce PES est ignoré.
        let mut head = three_pes();
        head[0][3] |= 0x80;
        let mut sink = VecSink::default();
        let got = run(&head, &mut sink);
        assert_eq!(concat(&got), [es(1000, 1), es(1000, 2)].concat());
        assert!(got[0].gap);
        assert_eq!(sink.messages.len(), 1);
        assert_eq!(sink.messages[0].id, SCRAMBLED.id);
    }

    #[test]
    fn other_pids_are_ignored() {
        let mut sink = VecSink::default();
        let other = [packet(
            0x200,
            true,
            0,
            None,
            &pes_bytes(Some(1), None, &[1; 100]),
        )];
        assert!(run(&other, &mut sink).is_empty());
    }

    proptest! {
        #[ma_trace::covers("SYN.ROBUST.01")]
        #[test]
        fn pes_assembler_never_panics(
            packets in prop::collection::vec(
                (any::<bool>(), any::<bool>(), 0u8..16, 0u8..4, 0u8..4, prop::collection::vec(any::<u8>(), 0..184), any::<bool>()),
                0..60,
            ),
            starts in prop::collection::vec(any::<bool>(), 0..60),
        ) {
            let mut asm = PesAssembler::new(PID);
            let mut sink = VecSink::default();
            let mut total_in = 0usize;
            let mut total_out = 0usize;
            for (i, (tei, pusi, cc, scr, afc, payload, valid_pes)) in packets.iter().enumerate() {
                let mut body = Vec::new();
                if *valid_pes && starts.get(i).copied().unwrap_or(false) {
                    body.extend(pes_header(0, Some(77), None, usize::from(*cc) % 3));
                }
                body.extend(payload);
                let mut raw = packet(PID, *pusi, *cc, None, &body);
                raw[1] |= u8::from(*tei) << 7;
                raw[3] = raw[3] & 0x0F | scr << 6 | afc << 4;
                // Champ d'adaptation éventuellement invalide : ignoré par le scanner réel.
                let Ok(parsed) = parse_packet(&raw) else { continue };
                let sp = ScannedPacket { offset: i as u64 * 188, index: i as u64, packet: parsed };
                total_in += sp.packet.payload.len();
                if let Some(c) = asm.push(&sp, &mut sink) {
                    total_out += c.data.len();
                    prop_assert!(c.data.len() <= sp.packet.payload.len());
                }
            }
            prop_assert!(total_out <= total_in);
        }
    }
}
