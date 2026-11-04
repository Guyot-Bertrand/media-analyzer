//! Assemblage des PES, positions dans l'ES et relecture sur le corpus généré.

// Positions et tailles de tests : bornées par construction.
#![allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]

use ma_core::{Message, VecSink};
use ma_testkit::{corpus_file, ffmpeg_es, ffprobe_packets};
use ma_ts::testutil::rewrite_video_pes;
use ma_ts::{
    EsPos, PacketFormat, PacketScanner, PesAssembler, PesStart, StreamKind, StructureBuilder,
    probe, read_es,
};

/// Chunk possédé.
struct Chunk {
    pos: EsPos,
    data: Vec<u8>,
    start: Option<PesStart>,
    gap: bool,
}

/// Source du fichier du corpus, format, 1er offset, PID vidéo et chunks du PID vidéo.
struct Loaded {
    src: ma_io::FileSource,
    format: PacketFormat,
    pid: u16,
    chunks: Vec<Chunk>,
    messages: Vec<Message>,
}

/// PID du premier flux vidéo de `ts`.
fn video_pid(structure: &ma_ts::TsStructure) -> u16 {
    structure
        .programs
        .iter()
        .flat_map(|p| &p.streams)
        .find(|s| matches!(s.kind, StreamKind::Video(_)))
        .map(|s| s.pid)
        .expect("aucun flux vidéo")
}

fn load(name: &str) -> Loaded {
    let (src, format, start) = probe(&corpus_file(name)).unwrap();
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut b = StructureBuilder::new(format);
    while let Some(p) = sc.next(&mut sink).unwrap() {
        b.push(&p, &mut sink);
    }
    let pid = video_pid(&b.structure());
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut asm = PesAssembler::new(pid);
    let mut chunks = Vec::new();
    while let Some(p) = sc.next(&mut sink).unwrap() {
        if let Some(c) = asm.push(&p, &mut sink) {
            chunks.push(Chunk {
                pos: c.pos,
                data: c.data.to_vec(),
                start: c.pes_start,
                gap: c.gap_before,
            });
        }
    }
    Loaded {
        src,
        format,
        pid,
        chunks,
        messages: sink.messages,
    }
}

fn concat(l: &Loaded) -> Vec<u8> {
    l.chunks.iter().flat_map(|c| c.data.clone()).collect()
}

#[test]
fn es_chunks_concatenate_to_ffmpeg_es() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_main10",
        "mpeg2_576i_closed",
        "h264_720p_pes_split",
    ] {
        let l = load(name);
        assert!(l.messages.is_empty(), "{name} : {:?}", l.messages);
        assert!(l.chunks.iter().all(|c| !c.gap), "{name}");
        let ours = concat(&l);
        assert!(ours.len() > 100_000, "{name} : ES vide ?");
        let theirs = ffmpeg_es(&corpus_file(name));
        assert_eq!(ours.len(), theirs.len(), "{name} : taille de l'ES");
        assert!(ours.iter().eq(theirs.iter()), "{name} : octets différents");
    }
}

/// Générateur déterministe (xorshift).
fn rng(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn read_es_roundtrip() {
    for name in ["h264_720p_closed", "h264_720p_pes_split"] {
        let l = load(name);
        let all = concat(&l);
        // Début cumulé de chaque chunk dans l'ES.
        let mut begin = Vec::new();
        let mut total = 0;
        for c in &l.chunks {
            begin.push(total);
            total += c.data.len();
        }
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut buf = Vec::new();
        for k in 0..50 {
            let n = l.chunks.len() as u64;
            let (mut i, mut j) = ((rng(&mut seed) % n) as usize, (rng(&mut seed) % n) as usize);
            if i > j {
                std::mem::swap(&mut i, &mut j);
            }
            // Une fois sur deux, les positions tombent dans le chunk plutôt qu'à son début.
            let inner = |c: &Chunk, r: u64| {
                if k % 2 == 0 || c.data.is_empty() {
                    0
                } else {
                    (r % c.data.len() as u64) as usize
                }
            };
            let (di, dj) = (
                inner(&l.chunks[i], rng(&mut seed)),
                inner(&l.chunks[j], rng(&mut seed)),
            );
            let at = |c: &Chunk, d: usize| EsPos {
                packet_offset: c.pos.packet_offset,
                skip: c.pos.skip + u16::try_from(d).unwrap(),
            };
            let (from, to) = (begin[i] + di, begin[j] + dj);
            if from > to {
                continue;
            }
            read_es(
                &l.src,
                l.format,
                l.pid,
                at(&l.chunks[i], di),
                at(&l.chunks[j], dj),
                &mut buf,
            )
            .unwrap();
            assert!(
                buf == all[from..to],
                "{name} couple {k} ({i},{di})..({j},{dj})"
            );
        }
    }
}

#[test]
fn pes_start_carries_the_pts_of_the_pes() {
    let l = load("h264_720p_closed");
    let starts: Vec<&PesStart> = l.chunks.iter().filter_map(|c| c.start.as_ref()).collect();
    let packets = ffprobe_packets(&corpus_file("h264_720p_closed"));
    assert_eq!(starts.len(), packets.len());
    let ours: Vec<i64> = starts
        .iter()
        .map(|s| i64::try_from(s.pts.unwrap()).unwrap())
        .collect();
    let theirs: Vec<i64> = packets.iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(ours, theirs);
}

/// Retire le paquet vidéo numéro `n` (en comptant les paquets du PID) du fichier.
fn without_video_packet(name: &str, n: usize) -> (Vec<u8>, u16) {
    let data = std::fs::read(corpus_file(name)).unwrap();
    let pid = load(name).pid;
    let mut seen = 0;
    let mut out = Vec::new();
    for raw in data.chunks(188) {
        let this = u16::from(raw[1] & 0x1F) << 8 | u16::from(raw[2]) == pid;
        if this {
            seen += 1;
            if seen == n + 1 {
                continue;
            }
        }
        out.extend_from_slice(raw);
    }
    (out, pid)
}

#[test]
fn lost_packet_on_a_real_stream_marks_one_gap() {
    use ma_io::testing::MemSource;
    let name = "h264_720p_closed";
    let reference = concat(&load(name));
    // Un paquet au milieu d'une image (ni début de PES ni dernier paquet).
    let (data, pid) = without_video_packet(name, 700);
    let src = MemSource::new(data);
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, PacketFormat::Ts188, 0, None);
    let mut asm = PesAssembler::new(pid);
    let mut gaps = 0;
    let mut size = 0;
    while let Some(p) = sc.next(&mut sink).unwrap() {
        if let Some(c) = asm.push(&p, &mut sink) {
            gaps += usize::from(c.gap_before);
            size += c.data.len();
        }
    }
    assert_eq!(gaps, 1);
    assert_eq!(sink.messages.len(), 1, "{:?}", sink.messages);
    assert_eq!(sink.messages[0].id, "ts-cc-gap");
    assert_eq!(reference.len() - size, 184, "un paquet plein d'ES perdu");
}

/// Images (PTS, DTS) d'un fichier, dans l'ordre du fichier.
fn timestamps(name: &str) -> Vec<(Option<i64>, Option<i64>)> {
    ffprobe_packets(&corpus_file(name))
        .iter()
        .map(|p| (p.pts, p.dts))
        .collect()
}

#[test]
fn derived_pes_files_keep_every_frame_and_timestamp() {
    // pes_split : même ES, mêmes images, mêmes horodatages que la source.
    assert_eq!(
        timestamps("h264_720p_pes_split"),
        timestamps("h264_720p_closed")
    );
    let split = load("h264_720p_pes_split");
    assert_eq!(
        split.chunks.iter().filter(|c| c.start.is_some()).count(),
        251,
        "250 AU : un PES d'ouverture, 249 PES de raccord et un PES de fin"
    );
    // pes_grouped : mêmes images que h264_720p_nob (PTS du premier AU seulement dans le PES,
    // les autres sont déduits par FFmpeg : on vérifie le nombre d'images et l'ES).
    assert_eq!(
        timestamps("h264_720p_pes_grouped").len(),
        timestamps("h264_720p_nob").len()
    );
    let grouped = load("h264_720p_pes_grouped");
    assert!(grouped.messages.is_empty());
    assert_eq!(concat(&grouped), ffmpeg_es(&corpus_file("h264_720p_nob")));
    let starts = grouped.chunks.iter().filter(|c| c.start.is_some()).count();
    assert_eq!(starts, 50, "2 AU par PES : 100 images, 50 PES");
}

#[test]
fn rewrite_video_pes_with_identity_keeps_the_stream_playable() {
    let name = "h264_720p_closed";
    let original = std::fs::read(corpus_file(name)).unwrap();
    let pid = load(name).pid;
    let rewritten = rewrite_video_pes(&original, pid, |pes| pes);
    // Mêmes octets d'ES, mêmes horodatages ; seule la taille des PES peut différer.
    let es = |data: Vec<u8>| {
        let src = ma_io::testing::MemSource::new(data);
        let mut sink = VecSink::default();
        let mut sc = PacketScanner::new(&src, PacketFormat::Ts188, 0, None);
        let mut asm = PesAssembler::new(pid);
        let mut es = Vec::new();
        let mut pts = Vec::new();
        while let Some(p) = sc.next(&mut sink).unwrap() {
            if let Some(c) = asm.push(&p, &mut sink) {
                es.extend_from_slice(c.data);
                pts.extend(c.pes_start.map(|s| (s.pts, s.dts)));
            }
        }
        assert!(sink.messages.is_empty(), "{:?}", sink.messages);
        (es, pts)
    };
    assert_eq!(rewritten.len(), original.len());
    let (a, b) = (es(rewritten), es(original));
    assert_eq!(a.1, b.1);
    assert!(a.0.iter().eq(b.0.iter()));
}
