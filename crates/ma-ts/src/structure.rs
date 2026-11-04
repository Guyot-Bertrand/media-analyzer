//! Structure d'un flux : PAT, CAT, PMT, SDT assemblées en arbre de programmes.

use std::collections::BTreeMap;

use ma_core::{Family, Message, MessageSink, Param, Severity};

use crate::messages::{PMT_CHANGED, PSI_CRC, PSI_INVALID};
use crate::packet::PacketFormat;
use crate::psi::{Section, SectionAssembler};
use crate::scan::ScannedPacket;
use crate::tables::{
    Pat, Pmt, SdtService, StreamKind, parse_pat, parse_pmt, parse_sdt, stream_kind,
};

const PID_PAT: u16 = 0x0000;
const PID_CAT: u16 = 0x0001;
const PID_SDT: u16 = 0x0011;
const TABLE_CAT: u8 = 0x01;
/// `PCR_PID` des programmes sans PCR.
const NO_PCR: u16 = 0x1FFF;
const SUB_FAMILY: &str = "psi";

/// Flux élémentaire d'un programme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EsInfo {
    pub pid: u16,
    pub stream_type: u8,
    pub kind: StreamKind,
    /// Le PID est celui du `PCR_PID` du programme.
    pub carries_pcr: bool,
    /// Flux analysé par le moteur (M2a : vidéo H.264, HEVC et MPEG-2) ; les autres sont
    /// affichés génériquement.
    pub analysed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub number: u16,
    pub pmt_pid: u16,
    /// `None` : PMT non vue ou `PCR_PID = 0x1FFF`.
    pub pcr_pid: Option<u16>,
    pub service: Option<SdtService>,
    pub streams: Vec<EsInfo>,
}

impl Program {
    /// `PCR_PID` qui n'est pas le PID d'un flux élémentaire du programme.
    #[must_use]
    pub fn dedicated_pcr_pid(&self) -> Option<u16> {
        self.pcr_pid
            .filter(|pcr| self.streams.iter().all(|s| s.pid != *pcr))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsStructure {
    pub format: PacketFormat,
    pub ts_id: Option<u16>,
    pub has_cat: bool,
    pub programs: Vec<Program>,
}

/// Construit la structure du flux au fil des paquets. Seules les sections valides (CRC bon,
/// `current_next = 1`) sont prises en compte.
#[derive(Debug)]
pub struct StructureBuilder {
    format: PacketFormat,
    pat_asm: SectionAssembler,
    cat_asm: SectionAssembler,
    sdt_asm: SectionAssembler,
    /// Assembleur de chaque PID de PMT annoncé par la PAT.
    pmt_asm: BTreeMap<u16, SectionAssembler>,
    pat: Option<Pat>,
    pmts: BTreeMap<u16, Pmt>,
    services: BTreeMap<u16, SdtService>,
    has_cat: bool,
    generation: u32,
    sections: Vec<Section>,
}

impl StructureBuilder {
    #[must_use]
    pub fn new(format: PacketFormat) -> Self {
        StructureBuilder {
            format,
            pat_asm: SectionAssembler::new(PID_PAT),
            cat_asm: SectionAssembler::new(PID_CAT),
            sdt_asm: SectionAssembler::new(PID_SDT),
            pmt_asm: BTreeMap::new(),
            pat: None,
            pmts: BTreeMap::new(),
            services: BTreeMap::new(),
            has_cat: false,
            generation: 0,
            sections: Vec::new(),
        }
    }

    /// Prend en compte un paquet. Un CRC faux ignore la section (`ts-psi-crc`) ; une nouvelle
    /// version de PMT remplace la précédente (`ts-pmt-changed`).
    pub fn push(&mut self, pkt: &ScannedPacket, sink: &mut dyn MessageSink) {
        let pid = pkt.packet.header.pid;
        let mut sections = std::mem::take(&mut self.sections);
        sections.clear();
        match pid {
            PID_PAT => self.pat_asm.push(pkt, &mut sections, sink),
            PID_CAT => self.cat_asm.push(pkt, &mut sections, sink),
            PID_SDT => self.sdt_asm.push(pkt, &mut sections, sink),
            _ => {
                if let Some(asm) = self.pmt_asm.get_mut(&pid) {
                    asm.push(pkt, &mut sections, sink);
                }
            }
        }
        for s in &sections {
            if !s.crc_ok {
                sink.push(
                    Message::new(&PSI_CRC, Severity::Warning, Family::Transport, SUB_FAMILY)
                        .param("table_id", Param::UInt(u64::from(s.table_id)))
                        .at(s.offset)
                        .pid(pid),
                );
            } else if s.current {
                self.apply(pid, s, sink);
            }
        }
        self.sections = sections;
    }

    fn invalid(pid: u16, s: &Section, sink: &mut dyn MessageSink) {
        sink.push(
            Message::new(
                &PSI_INVALID,
                Severity::Warning,
                Family::Transport,
                SUB_FAMILY,
            )
            .param("table_id", Param::UInt(u64::from(s.table_id)))
            .at(s.offset)
            .pid(pid),
        );
    }

    fn apply(&mut self, pid: u16, s: &Section, sink: &mut dyn MessageSink) {
        match pid {
            PID_PAT if s.table_id == 0x00 => match parse_pat(s) {
                Some(pat) => self.set_pat(pat),
                None => Self::invalid(pid, s, sink),
            },
            PID_CAT => {
                if s.table_id == TABLE_CAT && !self.has_cat {
                    self.has_cat = true;
                    self.generation += 1;
                }
            }
            PID_SDT if s.table_id == 0x42 => match parse_sdt(s) {
                Some(sdt) => {
                    let mut changed = false;
                    for svc in sdt.services {
                        changed |= self.services.insert(svc.service_id, svc.clone()) != Some(svc);
                    }
                    self.generation += u32::from(changed);
                }
                None => Self::invalid(pid, s, sink),
            },
            PID_PAT | PID_SDT => {}
            _ if s.table_id == 0x02 => match parse_pmt(s) {
                Some(pmt) => self.set_pmt(pid, pmt, s.offset, sink),
                None => Self::invalid(pid, s, sink),
            },
            _ => {}
        }
    }

    fn set_pat(&mut self, pat: Pat) {
        if self.pat.as_ref() == Some(&pat) {
            return;
        }
        // Les PID de PMT suivent la PAT ; les programmes disparus sont oubliés.
        self.pmt_asm
            .retain(|pid, _| pat.programs.iter().any(|p| p.1 == *pid));
        for &(_, pid) in &pat.programs {
            if pid != PID_PAT && pid != PID_CAT && pid != PID_SDT {
                self.pmt_asm
                    .entry(pid)
                    .or_insert_with(|| SectionAssembler::new(pid));
            }
        }
        self.pmts
            .retain(|n, _| pat.programs.iter().any(|p| p.0 == *n));
        self.pat = Some(pat);
        self.generation += 1;
    }

    fn set_pmt(&mut self, pid: u16, pmt: Pmt, offset: u64, sink: &mut dyn MessageSink) {
        // La PMT doit être celle que la PAT associe à ce PID.
        let announced = self
            .pat
            .as_ref()
            .is_some_and(|p| p.programs.contains(&(pmt.program_number, pid)));
        if !announced {
            return;
        }
        match self.pmts.get(&pmt.program_number) {
            Some(old) if *old == pmt => return,
            Some(_) => sink.push(
                Message::new(&PMT_CHANGED, Severity::Info, Family::Transport, SUB_FAMILY)
                    .param("program", Param::UInt(u64::from(pmt.program_number)))
                    .param("version", Param::UInt(u64::from(pmt.version)))
                    .at(offset)
                    .pid(pid),
            ),
            None => {}
        }
        self.pmts.insert(pmt.program_number, pmt);
        self.generation += 1;
    }

    /// Structure connue à cet instant.
    #[must_use]
    pub fn structure(&self) -> TsStructure {
        let programs = self
            .pat
            .iter()
            .flat_map(|pat| &pat.programs)
            .map(|&(number, pmt_pid)| {
                let pmt = self.pmts.get(&number);
                let pcr_pid = pmt.map(|p| p.pcr_pid).filter(|&p| p != NO_PCR);
                let streams = pmt.map_or_else(Vec::new, |p| {
                    p.streams
                        .iter()
                        .map(|s| {
                            let kind = stream_kind(s.stream_type, &s.descriptors);
                            EsInfo {
                                pid: s.pid,
                                stream_type: s.stream_type,
                                kind,
                                carries_pcr: pcr_pid == Some(s.pid),
                                analysed: matches!(kind, StreamKind::Video(Some(_))),
                            }
                        })
                        .collect()
                });
                Program {
                    number,
                    pmt_pid,
                    pcr_pid,
                    service: self.services.get(&number).cloned(),
                    streams,
                }
            });
        TsStructure {
            format: self.format,
            ts_id: self.pat.as_ref().map(|p| p.ts_id),
            has_cat: self.has_cat,
            programs: programs.collect(),
        }
    }

    /// Vrai quand la PAT et toutes ses PMT ont été vues (la SDT n'est pas requise).
    #[must_use]
    pub fn complete(&self) -> bool {
        self.pat
            .as_ref()
            .is_some_and(|p| p.programs.iter().all(|(n, _)| self.pmts.contains_key(n)))
    }

    /// Compteur incrémenté à chaque changement de la structure (PAT, PMT, SDT, CAT).
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::packet::{PacketFormat, parse_packet};
    use crate::scan::ScannedPacket;
    use crate::tables::{AudioKind, StreamKind};
    use crate::testutil::{pat, pmt, psi_packets, sdt, section};
    use ma_core::{Codec, VecSink};

    /// Pousse des paquets bruts (188 octets) dans le constructeur.
    fn feed(b: &mut StructureBuilder, sink: &mut VecSink, packets: &[[u8; 188]]) {
        for (i, raw) in packets.iter().enumerate() {
            let packet = parse_packet(raw).unwrap();
            b.push(
                &ScannedPacket {
                    offset: i as u64 * 188,
                    index: i as u64,
                    packet,
                },
                sink,
            );
        }
    }

    fn ids(sink: &VecSink) -> Vec<&'static str> {
        sink.messages.iter().map(|m| m.id).collect()
    }

    fn video_pmt(version: u8, pid: u16) -> Vec<[u8; 188]> {
        let bytes = pmt(
            1,
            version,
            pid,
            &[],
            &[(0x24, pid, &[]), (0x0F, 0x101, &[])],
        );
        psi_packets(0x1000, &bytes, version)
    }

    #[test]
    fn complete_needs_pat_and_all_pmts() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        assert!(!b.complete());
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000), (2, 0x1001)]), 0),
        );
        assert!(!b.complete());
        feed(&mut b, &mut sink, &video_pmt(0, 0x100));
        assert!(!b.complete());
        let p2 = pmt(2, 0, 0x200, &[], &[(0x1B, 0x200, &[])]);
        feed(&mut b, &mut sink, &psi_packets(0x1001, &p2, 0));
        assert!(b.complete());
        assert!(sink.messages.is_empty());
        let st = b.structure();
        assert_eq!(st.ts_id, Some(1));
        assert_eq!(st.programs.len(), 2);
        assert_eq!(
            st.programs[1].streams[0].kind,
            StreamKind::Video(Some(Codec::H264))
        );
        assert!(st.programs[1].streams[0].analysed);
        assert!(!st.has_cat);
    }

    #[test]
    fn empty_structure_before_any_table() {
        let b = StructureBuilder::new(PacketFormat::Rs204);
        let st = b.structure();
        assert_eq!(st.format, PacketFormat::Rs204);
        assert_eq!((st.ts_id, st.has_cat, st.programs.len()), (None, false, 0));
        assert_eq!(b.generation(), 0);
    }

    #[test]
    fn bad_crc_section_is_ignored_with_message() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        let mut bad = pat(1, 0, &[(1, 0x1000)]);
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        feed(&mut b, &mut sink, &psi_packets(0, &bad, 0));
        assert_eq!(ids(&sink), vec!["ts-psi-crc"]);
        assert_eq!(b.structure().programs.len(), 0);
        assert_eq!(b.generation(), 0);
        // La bonne copie qui suit est prise en compte.
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), 1),
        );
        assert_eq!(b.structure().programs.len(), 1);
    }

    #[test]
    fn invalid_table_content_is_reported() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        // PMT dont le corps est tronqué (CRC valide).
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), 0),
        );
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0x1000, &section(2, 1, 0, &[0xE1]), 0),
        );
        assert_eq!(ids(&sink), vec!["ts-psi-invalid"]);
        assert!(!b.complete());
    }

    #[test]
    fn pmt_version_change_is_followed() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), 0),
        );
        feed(&mut b, &mut sink, &video_pmt(0, 0x100));
        let g0 = b.generation();
        let pids = |b: &StructureBuilder| -> Vec<u16> {
            b.structure().programs[0]
                .streams
                .iter()
                .map(|s| s.pid)
                .collect()
        };
        assert_eq!(pids(&b), vec![0x100, 0x101]);
        // Répétition identique : rien ne change.
        feed(&mut b, &mut sink, &video_pmt(0, 0x100));
        assert_eq!(b.generation(), g0);
        assert!(sink.messages.is_empty());
        // Nouvelle version, nouveau PID vidéo.
        feed(&mut b, &mut sink, &video_pmt(1, 0x200));
        assert_eq!(pids(&b), vec![0x200, 0x101]);
        assert_eq!(b.generation(), g0 + 1);
        assert_eq!(ids(&sink), vec!["ts-pmt-changed"]);
        assert_eq!(b.structure().programs[0].pcr_pid, Some(0x200));
    }

    #[test]
    fn pat_change_drops_old_programs_and_next_sections_are_ignored() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000), (2, 0x1001)]), 0),
        );
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 1, &[(2, 0x1001)]), 1),
        );
        let nums: Vec<u16> = b.structure().programs.iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![2]);
        // Section « suivante » (current_next = 0) : ignorée.
        let mut next = pat(1, 2, &[(5, 0x1005)]);
        next[5] &= !1;
        let n = next.len();
        let crc = crate::psi::crc32_mpeg2(&next[..n - 4]).to_be_bytes();
        next[n - 4..].copy_from_slice(&crc);
        feed(&mut b, &mut sink, &psi_packets(0, &next, 2));
        assert_eq!(b.structure().programs.len(), 1);
    }

    #[test]
    fn sdt_cat_and_pcr_roles() {
        let mut b = StructureBuilder::new(PacketFormat::Ts188);
        let mut sink = VecSink::default();
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), 0),
        );
        // Sans SDT : pas de service.
        let pm = pmt(1, 0, 0x1FF, &[], &[(0x24, 0x100, &[]), (0x0F, 0x101, &[])]);
        feed(&mut b, &mut sink, &psi_packets(0x1000, &pm, 0));
        let prog = b.structure().programs[0].clone();
        assert_eq!(prog.service, None);
        assert_eq!(prog.dedicated_pcr_pid(), Some(0x1FF));
        assert!(prog.streams.iter().all(|s| !s.carries_pcr));
        assert_eq!(
            prog.streams[1].kind,
            StreamKind::Audio(Some(AudioKind::Aac))
        );
        assert!(!prog.streams[1].analysed);
        // Avec SDT (deux sections pour le même TS) et CAT.
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0x11, &sdt(1, 0, &[(1, "Prov", "Nom")]), 0),
        );
        feed(
            &mut b,
            &mut sink,
            &psi_packets(0x11, &sdt(1, 0, &[(9, "X", "Y")]), 1),
        );
        feed(
            &mut b,
            &mut sink,
            &psi_packets(1, &section(1, 0xFFFF, 0, &[]), 0),
        );
        let st = b.structure();
        let svc = st.programs[0].service.clone().unwrap();
        assert_eq!((svc.name.as_str(), svc.provider.as_str()), ("Nom", "Prov"));
        assert!(st.has_cat);
        // PCR sur la vidéo ; 0x1FFF = pas de PCR.
        let pm = pmt(1, 1, 0x100, &[], &[(0x24, 0x100, &[])]);
        feed(&mut b, &mut sink, &psi_packets(0x1000, &pm, 1));
        let prog = b.structure().programs[0].clone();
        assert!(prog.streams[0].carries_pcr);
        assert_eq!(prog.dedicated_pcr_pid(), None);
        let pm = pmt(1, 2, 0x1FFF, &[], &[(0x24, 0x100, &[])]);
        feed(&mut b, &mut sink, &psi_packets(0x1000, &pm, 2));
        let prog = b.structure().programs[0].clone();
        assert_eq!((prog.pcr_pid, prog.dedicated_pcr_pid()), (None, None));
    }
}
