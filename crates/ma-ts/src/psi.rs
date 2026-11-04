//! Sections PSI/SI : CRC-32 MPEG-2 et assemblage des sections à partir des paquets d'un PID.

use ma_core::{Family, Message, MessageSink, Param, Severity};

use crate::messages::{PSI_INCOMPLETE, PSI_INVALID, PSI_TOO_LONG};
use crate::scan::ScannedPacket;

const SUB_FAMILY: &str = "psi";
/// `section_length` maximal des tables PSI (PAT, CAT, PMT, TSDT).
const MAX_PSI_LENGTH: usize = 1021;
/// `section_length` maximal des autres sections (SDT, EIT, SCTE-35…) : section de 4096 octets.
const MAX_LONG_LENGTH: usize = 4093;
/// En-tête court (`table_id` et `section_length`).
const SHORT_HEADER: usize = 3;
/// En-tête étendu (`ssi = 1`) : `table_id_extension`, version, numéros de section.
const LONG_HEADER: usize = 8;
const CRC_LEN: usize = 4;

#[allow(clippy::cast_possible_truncation, reason = "index de table < 256")]
const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 MPEG-2 : polynôme 0x04C11DB7, valeur initiale 0xFFFFFFFF, sans réflexion ni
/// complément final. Calculé sur une section complète (CRC compris), il vaut 0.
#[must_use]
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    data.iter().fold(0xFFFF_FFFF, |crc, &b| {
        (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ b)]
    })
}

/// Section PSI/SI complète. Pour une section courte (`section_syntax_indicator = 0`, par
/// exemple SCTE-35), `table_id_ext`, `version`, `section_number` et `last_section_number` valent
/// 0, `current` est vrai, `body` contient tout ce qui suit l'en-tête de 3 octets et `crc_ok` est
/// vrai (le CRC n'est pas vérifié).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub table_id: u8,
    pub table_id_ext: u16,
    pub version: u8,
    pub current: bool,
    pub section_number: u8,
    pub last_section_number: u8,
    /// Octets entre l'en-tête étendu et le CRC (sections longues).
    pub body: Vec<u8>,
    pub crc_ok: bool,
    /// Offset du paquet où la section commence.
    pub offset: u64,
}

/// Assemble les sections d'un PID : `pointer_field`, plusieurs sections par paquet, sections sur
/// plusieurs paquets, bourrage 0xFF. Mémoire bornée à une section de 4096 octets.
#[derive(Debug)]
pub struct SectionAssembler {
    pid: u16,
    /// Octets de la section en cours (vide hors section).
    buf: Vec<u8>,
    /// Longueur totale de la section en cours, connue après les 3 premiers octets.
    total: Option<usize>,
    start_offset: u64,
    last_cc: Option<u8>,
}

impl SectionAssembler {
    #[must_use]
    pub fn new(pid: u16) -> Self {
        SectionAssembler {
            pid,
            buf: Vec::new(),
            total: None,
            start_offset: 0,
            last_cc: None,
        }
    }

    fn message(&self, def: &'static ma_core::MsgDef, offset: u64) -> Message {
        Message::new(def, Severity::Warning, Family::Transport, SUB_FAMILY)
            .at(offset)
            .pid(self.pid)
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.total = None;
    }

    /// Abandonne la section en cours (si elle existe) avec `ts-psi-incomplete`.
    fn abandon(&mut self, sink: &mut dyn MessageSink) {
        if !self.buf.is_empty() {
            sink.push(
                self.message(&PSI_INCOMPLETE, self.start_offset)
                    .param("bytes", Param::UInt(self.buf.len() as u64)),
            );
            self.reset();
        }
    }

    /// Prend en compte un paquet du PID (les autres PID sont ignorés). Les sections terminées sont
    /// ajoutées à `out`.
    pub fn push(
        &mut self,
        pkt: &ScannedPacket,
        out: &mut Vec<Section>,
        sink: &mut dyn MessageSink,
    ) {
        let h = pkt.packet.header;
        if h.pid != self.pid {
            return;
        }
        // Brouillé, en erreur ou sans charge utile : rien à assembler (une donnée douteuse
        // interrompt la section en cours).
        if h.scrambling != 0 || h.tei {
            self.abandon(sink);
            self.last_cc = None;
            return;
        }
        let payload = pkt.packet.payload;
        if h.afc & 1 == 0 || payload.is_empty() {
            return;
        }
        let discontinuity = pkt.packet.adaptation.is_some_and(|a| a.discontinuity);
        if let Some(last) = self.last_cc {
            if h.cc == last && !discontinuity {
                return; // doublon autorisé par la norme
            }
            // Un saut de compteur abandonne la section, même avec `discontinuity_indicator`.
            if h.cc != (last + 1) & 0x0F {
                self.abandon(sink);
            }
        }
        self.last_cc = Some(h.cc);
        if h.pusi {
            let pointer = usize::from(payload[0]);
            let rest = &payload[1..];
            if pointer > rest.len() {
                sink.push(
                    self.message(&PSI_INVALID, pkt.offset)
                        .param("table_id", Param::UInt(0xFF)),
                );
                self.reset();
                return;
            }
            self.feed(&rest[..pointer], false, pkt.offset, out, sink);
            self.abandon(sink);
            self.feed(&rest[pointer..], true, pkt.offset, out, sink);
        } else {
            self.feed(payload, false, pkt.offset, out, sink);
        }
    }

    /// Ajoute `data` à la section en cours ; `can_start` autorise le début d'une nouvelle section
    /// (hors bourrage 0xFF) quand aucune n'est en cours.
    fn feed(
        &mut self,
        mut data: &[u8],
        can_start: bool,
        offset: u64,
        out: &mut Vec<Section>,
        sink: &mut dyn MessageSink,
    ) {
        while !data.is_empty() {
            if self.buf.is_empty() {
                if !can_start || data[0] == 0xFF {
                    return;
                }
                self.start_offset = offset;
            }
            let want = self.total.unwrap_or(SHORT_HEADER) - self.buf.len();
            let take = want.min(data.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.total.is_none() && self.buf.len() == SHORT_HEADER {
                let length = usize::from(self.buf[1] & 0x0F) << 8 | usize::from(self.buf[2]);
                let max = if self.buf[0] <= 0x03 {
                    MAX_PSI_LENGTH
                } else {
                    MAX_LONG_LENGTH
                };
                if length > max {
                    sink.push(
                        self.message(&PSI_TOO_LONG, self.start_offset)
                            .param("length", Param::UInt(length as u64)),
                    );
                    self.reset();
                    // Impossible de retrouver le début de la section suivante dans ce paquet.
                    return;
                }
                self.total = Some(SHORT_HEADER + length);
            }
            if self.total == Some(self.buf.len()) {
                self.finish(out, sink);
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Section>, sink: &mut dyn MessageSink) {
        let b = &self.buf;
        let table_id = b[0];
        if b[1] & 0x80 == 0 {
            out.push(Section {
                table_id,
                table_id_ext: 0,
                version: 0,
                current: true,
                section_number: 0,
                last_section_number: 0,
                body: b[SHORT_HEADER..].to_vec(),
                crc_ok: true,
                offset: self.start_offset,
            });
        } else if b.len() < LONG_HEADER + CRC_LEN {
            sink.push(
                self.message(&PSI_INVALID, self.start_offset)
                    .param("table_id", Param::UInt(u64::from(table_id))),
            );
        } else {
            out.push(Section {
                table_id,
                table_id_ext: u16::from(b[3]) << 8 | u16::from(b[4]),
                version: (b[5] >> 1) & 0x1F,
                current: b[5] & 1 != 0,
                section_number: b[6],
                last_section_number: b[7],
                body: b[LONG_HEADER..b.len() - CRC_LEN].to_vec(),
                crc_ok: crc32_mpeg2(b) == 0,
                offset: self.start_offset,
            });
        }
        self.reset();
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::packet::parse_packet;
    use crate::scan::ScannedPacket;
    use crate::testutil::{AfSpec, packet, psi_packets, section};
    use ma_core::VecSink;
    use proptest::prelude::*;

    /// Pousse des paquets bruts (188 octets) dans un assembleur du PID `pid`.
    fn run(pid: u16, packets: &[[u8; 188]]) -> (Vec<Section>, VecSink) {
        let mut asm = SectionAssembler::new(pid);
        let mut out = Vec::new();
        let mut sink = VecSink::default();
        for (i, raw) in packets.iter().enumerate() {
            let packet = parse_packet(raw).unwrap();
            let sp = ScannedPacket {
                offset: i as u64 * 188,
                index: i as u64,
                packet,
            };
            asm.push(&sp, &mut out, &mut sink);
        }
        (out, sink)
    }

    fn ids(sink: &VecSink) -> Vec<&'static str> {
        sink.messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }

    #[test]
    fn section_spanning_packets_and_several_per_packet() {
        let body_a: Vec<u8> = (0..300u16).map(|i| i as u8).collect();
        let a = section(0x40, 7, 3, &body_a);
        let b = section(0x41, 1, 0, &[1, 2, 3]);
        let c = section(0x41, 2, 0, &[9]);
        // Une section sur deux paquets.
        let (out, sink) = run(0x30, &psi_packets(0x30, &a, 0));
        assert_eq!(out.len(), 1, "{:?}", ids(&sink));
        let s = &out[0];
        assert_eq!(
            (s.table_id, s.table_id_ext, s.version, s.current),
            (0x40, 7, 3, true)
        );
        assert_eq!((s.section_number, s.last_section_number), (0, 0));
        assert_eq!(s.body, body_a);
        assert!(s.crc_ok);
        assert!(sink.messages.is_empty());
        // Fin de A, B et C dans le même paquet, bourrage 0xFF ensuite.
        let split = 183;
        let mut first = vec![0];
        first.extend(&a[..split]);
        let mut second = vec![u8::try_from(a.len() - split).unwrap()];
        second.extend(&a[split..]);
        second.extend(&b);
        second.extend(&c);
        let packets = [
            packet(0x30, true, 0, None, &first),
            packet(0x30, true, 1, None, &second),
        ];
        let (out, sink) = run(0x30, &packets);
        assert!(sink.messages.is_empty(), "{:?}", ids(&sink));
        let got: Vec<(u8, u16, Vec<u8>, u64)> = out
            .iter()
            .map(|s| (s.table_id, s.table_id_ext, s.body.clone(), s.offset))
            .collect();
        assert_eq!(
            got,
            vec![
                (0x40, 7, body_a, 0),
                (0x41, 1, vec![1, 2, 3], 188),
                (0x41, 2, vec![9], 188)
            ]
        );
    }

    #[test]
    fn corrupted_section_has_crc_flag_cleared() {
        let mut a = section(0x40, 1, 0, &[1, 2, 3, 4]);
        a[9] ^= 0x01;
        let (out, _) = run(0x30, &psi_packets(0x30, &a, 0));
        assert_eq!(out.len(), 1);
        assert!(!out[0].crc_ok);
    }

    #[test]
    fn cc_break_abandons_the_section() {
        let long = section(0x40, 1, 0, &[0xAA; 300]);
        let mut p = psi_packets(0x30, &long, 0);
        // Le 2e paquet perd un compteur.
        p[1][3] = (p[1][3] & 0xF0) | 5;
        let ok = section(0x41, 9, 0, &[7]);
        p.extend(psi_packets(0x30, &ok, 6));
        let (out, sink) = run(0x30, &p);
        assert_eq!(ids(&sink), vec!["ts-psi-incomplete"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].table_id_ext, 9);
    }

    #[test]
    fn duplicated_packet_is_not_a_break() {
        let long = section(0x40, 1, 0, &[0xAA; 300]);
        let p = psi_packets(0x30, &long, 0);
        let (out, sink) = run(0x30, &[p[0], p[1], p[1]]);
        assert!(sink.messages.is_empty(), "{:?}", ids(&sink));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn section_is_abandoned_when_next_start_arrives_early() {
        let long = section(0x40, 1, 0, &[0xAA; 300]);
        let p = psi_packets(0x30, &long, 0);
        let ok = section(0x41, 2, 0, &[7]);
        let mut q = vec![p[0]];
        q.extend(psi_packets(0x30, &ok, 1));
        let (out, sink) = run(0x30, &q);
        assert_eq!(ids(&sink), vec!["ts-psi-incomplete"]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn oversized_section_is_dropped_with_message() {
        // section_length = 1022 > 1021 sur une PAT ; l'assembleur ne doit pas l'accumuler.
        let mut bad = vec![0x00, 0xB0 | 0x03, 0xFE];
        bad.extend(vec![0u8; 183 - 3]);
        let ok = section(0x00, 1, 0, &[0, 1, 0xE0, 0x20]);
        let mut p = vec![packet(0, true, 0, None, &[&[0u8][..], &bad].concat())];
        for k in 1..8 {
            p.push(packet(0, false, k, None, &[0u8; 184]));
        }
        p.extend(psi_packets(0, &ok, 8));
        let (out, sink) = run(0, &p);
        assert_eq!(ids(&sink), vec!["ts-psi-too-long"]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].table_id_ext, 1);
    }

    #[test]
    fn short_form_and_malformed_headers() {
        // Section courte (ssi = 0) : pas d'en-tête étendu.
        let short = [0xFC, 0x30, 0x03, 1, 2, 3];
        let mut pl = vec![0u8];
        pl.extend(short);
        let (out, sink) = run(0x1F4, &[packet(0x1F4, true, 0, None, &pl)]);
        assert!(sink.messages.is_empty());
        assert_eq!(
            (out[0].table_id, out[0].body.clone()),
            (0xFC, vec![1, 2, 3])
        );
        // ssi = 1 mais section_length trop petite pour l'en-tête et le CRC.
        let bad = [0x00, 0xB0, 0x02, 0, 0];
        let mut pl = vec![0u8];
        pl.extend(bad);
        let (out, sink) = run(0, &[packet(0, true, 0, None, &pl)]);
        assert!(out.is_empty());
        assert_eq!(ids(&sink), vec!["ts-psi-invalid"]);
        // pointer_field qui dépasse le paquet.
        let (out, sink) = run(0, &[packet(0, true, 0, None, &[250, 1, 2])]);
        assert!(out.is_empty());
        assert_eq!(ids(&sink), vec!["ts-psi-invalid"]);
    }

    #[test]
    fn discontinuity_with_cc_jump_abandons_the_section() {
        let long = section(0x40, 1, 0, &[0xAA; 300]);
        let p = psi_packets(0x30, &long, 0);
        let af = AfSpec {
            discontinuity: true,
            ..AfSpec::default()
        };
        let tail = packet(0x30, false, 7, Some(af), &p[1][4..]);
        let (out, sink) = run(0x30, &[p[0], tail]);
        assert!(out.is_empty());
        assert_eq!(ids(&sink), vec!["ts-psi-incomplete"]);
    }

    #[test]
    fn other_pids_scrambled_and_error_packets_are_ignored() {
        let s = section(0x40, 1, 0, &[1]);
        let mut p = psi_packets(0x30, &s, 0);
        let (out, _) = run(0x31, &p);
        assert!(out.is_empty());
        p[0][3] |= 0x80; // brouillé
        let (out, sink) = run(0x30, &p);
        assert!(out.is_empty() && sink.messages.is_empty());
        let mut p = psi_packets(0x30, &s, 0);
        p[0][1] |= 0x80; // transport_error_indicator
        let (out, _) = run(0x30, &p);
        assert!(out.is_empty());
        // Paquet sans charge utile : ignoré sans toucher au compteur.
        let af_only = packet(0x30, false, 0, Some(AfSpec::default()), &[]);
        let (out, sink) = run(0x30, &[af_only]);
        assert!(out.is_empty() && sink.messages.is_empty());
    }

    proptest! {
        #[ma_trace::covers("SYN.ROBUST.01")]
        #[test]
        fn section_assembler_never_panics(
            pkts in prop::collection::vec(
                (any::<bool>(), any::<u8>(), prop::collection::vec(any::<u8>(), 0..=184), 0u8..4),
                0..40
            )
        ) {
            let raw: Vec<[u8; 188]> = pkts
                .iter()
                .map(|(pusi, cc, payload, mode)| {
                    let af = (*mode == 1).then(AfSpec::default);
                    packet(0, *pusi, *cc, af, payload)
                })
                .collect();
            let (out, _) = run(0, &raw);
            prop_assert!(out.iter().all(|s| s.body.len() <= 4096));
        }
    }
}
