//! Détection du format et dénombrement des paquets sur le corpus généré.

// Lecture de valeurs hexadécimales de TSDuck : bornées par construction (PID 13 bits, type 8 bits).
#![allow(clippy::cast_possible_truncation)]

use std::collections::BTreeSet;
use std::process::Command;

use ma_core::{Codec, Message, VecSink};
use ma_io::ReadAt;
use ma_testkit::{corpus_file, tsduck_analyze};
use ma_ts::testutil::{pat, pmt, psi_packets};
use ma_ts::{
    AudioKind, PacketFormat, PacketScanner, ScannedPacket, StreamKind, StructureBuilder,
    TsStructure, parse_packet, probe,
};

/// Ouvre `name` et renvoie (format, offset, taille du fichier, nombre de paquets, messages).
fn scan(name: &str) -> (PacketFormat, u64, u64, u64, usize) {
    let path = corpus_file(name);
    let (src, format, start) = probe(&path).unwrap();
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut n = 0;
    while let Some(p) = sc.next(&mut sink).unwrap() {
        assert_eq!(p.index, n);
        assert_eq!(
            p.offset,
            start + n * format.size() as u64,
            "offset du paquet {n}"
        );
        n += 1;
    }
    (format, start, src.len(), n, sink.messages.len())
}

#[ma_trace::covers("STRUCT.OPEN.01")]
#[test]
fn detects_188_204_208() {
    for (name, format) in [
        ("h264_720p_closed", PacketFormat::Ts188),
        ("ts204_h264", PacketFormat::Rs204),
        ("ts208_h264", PacketFormat::Ts208),
    ] {
        let (f, start, len, n, msgs) = scan(name);
        assert_eq!(f, format, "{name}");
        assert_eq!(start, 0, "{name}");
        assert_eq!(len % format.size() as u64, 0, "{name} non tronqué");
        assert_eq!(n, len / format.size() as u64, "{name}");
        assert_eq!(msgs, 0, "{name}");
    }
}

#[ma_trace::covers("STRUCT.OPEN.02")]
#[test]
fn detects_tts_and_flags_timing() {
    let (_, _, _, reference, _) = scan("h264_720p_closed");
    let (f, start, len, n, msgs) = scan("tts_h264");
    assert_eq!(f, PacketFormat::Tts192);
    assert!(!f.timing_reliable());
    assert_eq!(start, 4, "premier 0x47 après le préfixe horodaté");
    assert_eq!(n, reference);
    assert_eq!(n, len / 192);
    assert_eq!(msgs, 0);
}

/// Structure d'un fichier du corpus (tout le fichier est lu) et messages émis.
fn structure_of(name: &str) -> (TsStructure, Vec<Message>) {
    let path = corpus_file(name);
    let (src, format, start) = probe(&path).unwrap();
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut b = StructureBuilder::new(format);
    while let Some(p) = sc.next(&mut sink).unwrap() {
        b.push(&p, &mut sink);
    }
    assert!(b.complete(), "{name} : PAT et PMT non vues");
    (b.structure(), sink.messages)
}

/// Couples (PID, nature) des flux du premier programme.
fn kinds(st: &TsStructure) -> Vec<(u16, StreamKind)> {
    st.programs[0]
        .streams
        .iter()
        .map(|s| (s.pid, s.kind))
        .collect()
}

#[ma_trace::covers("STRUCT.TREE.01")]
#[test]
fn reference_tree_has_expected_pids() {
    let (st, msgs) = structure_of("ts_ref");
    assert!(msgs.is_empty(), "{msgs:?}");
    assert_eq!(st.programs.len(), 1);
    assert_eq!(st.programs[0].number, 1);
    assert_eq!(
        kinds(&st),
        vec![
            (0x100, StreamKind::Video(Some(Codec::Hevc))),
            (0x101, StreamKind::Audio(Some(AudioKind::Aac))),
            (0x102, StreamKind::Audio(Some(AudioKind::Eac3))),
            (0x103, StreamKind::Subtitles),
            (0x1F4, StreamKind::Scte35),
        ]
    );
    let analysed: Vec<bool> = st.programs[0].streams.iter().map(|s| s.analysed).collect();
    assert_eq!(analysed, vec![true, false, false, false, false]);
}

/// Instants (ticks 27 MHz) des paquets à champ d'adaptation seul portant un PCR sur `pid`.
fn lone_pcrs(name: &str, pid: u16) -> Vec<u64> {
    let (src, format, start) = probe(&corpus_file(name)).unwrap();
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut out = Vec::new();
    while let Some(p) = sc.next(&mut sink).unwrap() {
        let pcr = p.packet.adaptation.and_then(|a| a.pcr);
        if let (true, Some(pcr)) = (p.packet.header.pid == pid && p.packet.header.afc == 2, pcr) {
            out.push(pcr.ticks());
        }
    }
    out
}

#[ma_trace::covers("STRUCT.TREE.02")]
#[test]
fn pcr_on_video_and_on_dedicated_pid() {
    let (st, _) = structure_of("ts_ref");
    let prog = &st.programs[0];
    assert_eq!(prog.pcr_pid, Some(0x100));
    let carriers: Vec<u16> = prog
        .streams
        .iter()
        .filter(|s| s.carries_pcr)
        .map(|s| s.pid)
        .collect();
    assert_eq!(carriers, vec![0x100]);
    assert_eq!(prog.dedicated_pcr_pid(), None);

    let (st, msgs) = structure_of("ts_pcr_pid");
    assert!(msgs.is_empty(), "{msgs:?}");
    let prog = &st.programs[0];
    assert_eq!(prog.pcr_pid, Some(0x1FF));
    assert_eq!(prog.dedicated_pcr_pid(), Some(0x1FF));
    assert!(prog.streams.iter().all(|s| !s.carries_pcr));
    // Les PCR seuls sont croissants, au plus 30 ms d'écart.
    let pcrs = lone_pcrs("ts_pcr_pid", 0x1FF);
    assert!(pcrs.len() >= 50, "{} PCR seuls", pcrs.len());
    assert!(pcrs.windows(2).all(|w| w[0] < w[1]));
    let max_gap = pcrs.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(
        max_gap <= 27_000_000 * 30 / 1000,
        "écart max {max_gap} ticks"
    );
}

#[ma_trace::covers("STRUCT.TREE.03")]
#[test]
fn sdt_service_name_and_provider() {
    let (st, _) = structure_of("ts_ref");
    let svc = st.programs[0].service.clone().unwrap();
    assert_eq!(svc.service_id, 1);
    assert_eq!(svc.name, "MA Référence");
    assert_eq!(svc.provider, "Media Analyzer");

    // Flux synthétique sans SDT.
    let mut b = StructureBuilder::new(PacketFormat::Ts188);
    let mut sink = VecSink::default();
    let mut packets = psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), 0);
    packets.extend(psi_packets(
        0x1000,
        &pmt(1, 0, 0x100, &[], &[(0x1B, 0x100, &[])]),
        0,
    ));
    for (i, raw) in packets.iter().enumerate() {
        let packet = parse_packet(raw).unwrap();
        b.push(
            &ScannedPacket {
                offset: i as u64 * 188,
                index: i as u64,
                packet,
            },
            &mut sink,
        );
    }
    assert!(b.complete());
    assert_eq!(b.structure().programs[0].service, None);
}

#[ma_trace::covers("STRUCT.TREE.04")]
#[test]
fn unsupported_streams_are_generic() {
    let (st, msgs) = structure_of("ts_unsupported");
    assert!(msgs.is_empty(), "{msgs:?}");
    let by_pid = |pid: u16| {
        st.programs[0]
            .streams
            .iter()
            .find(|s| s.pid == pid)
            .unwrap()
            .clone()
    };
    let vc1 = by_pid(0x104);
    assert_eq!(
        (vc1.stream_type, vc1.kind, vc1.analysed),
        (0xEA, StreamKind::Video(None), false)
    );
    let ac4 = by_pid(0x105);
    assert_eq!(
        (ac4.stream_type, ac4.kind, ac4.analysed),
        (0x06, StreamKind::Audio(Some(AudioKind::Ac4)), false)
    );
    // Les flux de ts_ref restent identiques.
    assert_eq!(kinds(&st)[..5], kinds(&structure_of("ts_ref").0)[..]);
    assert!(by_pid(0x100).analysed);
}

#[ma_trace::covers("STRUCT.TREE.05")]
#[test]
fn mpts_has_three_programs() {
    let (st, msgs) = structure_of("mpts_3prog");
    assert!(msgs.is_empty(), "{msgs:?}");
    assert_eq!(st.programs.len(), 3);
    let numbers: Vec<u16> = st.programs.iter().map(|p| p.number).collect();
    assert_eq!(numbers, vec![1, 2, 3]);
    let names: Vec<String> = st
        .programs
        .iter()
        .map(|p| p.service.clone().unwrap().name)
        .collect();
    assert_eq!(names, vec!["P1", "P2", "P3"]);
    let pmt_pids: BTreeSet<u16> = st.programs.iter().map(|p| p.pmt_pid).collect();
    assert_eq!(pmt_pids.len(), 3);
    let mut all_pids = BTreeSet::new();
    for p in &st.programs {
        assert_eq!(p.streams.len(), 1, "programme {}", p.number);
        let v = &p.streams[0];
        assert_eq!(v.kind, StreamKind::Video(Some(Codec::H264)));
        assert!(v.carries_pcr);
        assert!(all_pids.insert(v.pid), "PID {:#x} partagé", v.pid);
    }
}

#[ma_trace::covers("STRUCT.OPEN.02")]
#[test]
fn tts_tree_equals_ts_tree() {
    let (mut tts, m1) = structure_of("tts_h264");
    let (ts, m2) = structure_of("h264_720p_closed");
    assert!(m1.is_empty() && m2.is_empty());
    assert_eq!(tts.format, PacketFormat::Tts192);
    assert_eq!(ts.format, PacketFormat::Ts188);
    tts.format = PacketFormat::Ts188;
    assert_eq!(tts, ts);
}

/// `stream_type` de chaque composant d'une PMT, d'après `tstables` (`TSDuck`).
fn tsduck_stream_types(path: &std::path::Path, pmt_pid: u16) -> Vec<(u16, u8)> {
    let out = Command::new("tstables")
        .args([
            "--pid",
            &pmt_pid.to_string(),
            "--tid",
            "2",
            "--max-tables",
            "1",
            "--xml-output",
            "-",
        ])
        .arg(path)
        .output()
        .expect("tstables (TSDuck) absent : voir BUILD.md §4");
    assert!(out.status.success(), "tstables a échoué");
    let xml = String::from_utf8_lossy(&out.stdout).into_owned();
    let attr = |line: &str, key: &str| -> u32 {
        let at = line.find(&format!("{key}=\"0x")).unwrap() + key.len() + 4;
        let hex: String = line[at..]
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        u32::from_str_radix(&hex, 16).unwrap()
    };
    xml.lines()
        .filter(|l| l.contains("<component "))
        .map(|l| {
            (
                attr(l, "elementary_PID") as u16,
                attr(l, "stream_type") as u8,
            )
        })
        .collect()
}

#[test]
fn structure_matches_tsduck() {
    for name in ["ts_ref", "mpts_3prog"] {
        let path = corpus_file(name);
        let (st, _) = structure_of(name);
        let v = tsduck_analyze(&path);
        let services = v["services"].as_array().unwrap();
        assert_eq!(services.len(), st.programs.len(), "{name}");
        for p in &st.programs {
            let svc = services
                .iter()
                .find(|s| s["id"].as_u64() == Some(u64::from(p.number)))
                .unwrap_or_else(|| panic!("{name} : service {} absent de TSDuck", p.number));
            let ours = p.service.clone().unwrap();
            assert_eq!(svc["name"].as_str(), Some(ours.name.as_str()), "{name}");
            assert_eq!(
                svc["provider"].as_str(),
                Some(ours.provider.as_str()),
                "{name}"
            );
            assert_eq!(
                svc["pmt-pid"].as_u64(),
                Some(u64::from(p.pmt_pid)),
                "{name}"
            );
            assert_eq!(svc["pcr-pid"].as_u64(), p.pcr_pid.map(u64::from), "{name}");
            // TSDuck liste le PID de PMT et tous les composants du service.
            let theirs: BTreeSet<u64> = svc["pids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .collect();
            let mut mine: BTreeSet<u64> = p.streams.iter().map(|s| u64::from(s.pid)).collect();
            mine.insert(u64::from(p.pmt_pid));
            assert_eq!(theirs, mine, "{name} programme {}", p.number);
            let types = tsduck_stream_types(&path, p.pmt_pid);
            let ours_types: Vec<(u16, u8)> =
                p.streams.iter().map(|s| (s.pid, s.stream_type)).collect();
            assert_eq!(types, ours_types, "{name} programme {}", p.number);
        }
    }
}
