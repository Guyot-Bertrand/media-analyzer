//! Indexeur progressif sur le corpus généré : oracles `ffprobe`, préfixe publié stable, reprise,
//! horodatages, dégâts, troncature, annulation et mémoire.

// Positions, tailles et compteurs de tests : bornés par construction.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ma_core::{Codec, FrameEntry, Leading, RapKind, VecSink};
use ma_io::testing::{LatencySource, MemSource, TruncatingSource};
use ma_io::{IoError, ReadAt};
use ma_testkit::{
    corpus_file, corpus_file_if_present, ffmpeg_es, ffprobe_frames, ffprobe_packets, manifest,
};
use ma_ts::{
    AuInfo, IndexError, IndexOptions, PacketFormat, StreamKind, TsIndex, index, probe, read_es,
    spawn_index,
};
use proptest::prelude::*;

const WRAP: i64 = 1 << 33;

/// Fichier du corpus en mémoire : octets, format, offset de la 1re synchronisation.
struct Loaded {
    data: Vec<u8>,
    format: PacketFormat,
    start: u64,
}

fn load(name: &str) -> Loaded {
    let path = corpus_file(name);
    let (_, format, start) = probe(&path).unwrap();
    Loaded {
        data: std::fs::read(&path).unwrap(),
        format,
        start,
    }
}

/// Résultat d'une indexation.
struct Run {
    index: Arc<TsIndex>,
    result: Result<(), IndexError>,
    sink: VecSink,
}

fn run_on(src: &dyn ReadAt, format: PacketFormat, start: u64, opts: &IndexOptions) -> Run {
    let index = Arc::new(TsIndex::new(format, src.len()));
    let mut sink = VecSink::default();
    let result = index_into(src, start, &index, opts, &mut sink);
    Run {
        index,
        result,
        sink,
    }
}

fn index_into(
    src: &dyn ReadAt,
    start: u64,
    out: &TsIndex,
    opts: &IndexOptions,
    sink: &mut VecSink,
) -> Result<(), IndexError> {
    index(src, start, out, opts, sink, &AtomicBool::new(false))
}

fn run_full(l: &Loaded) -> Run {
    let src = MemSource::new(l.data.clone());
    let r = run_on(&src, l.format, l.start, &IndexOptions::default());
    r.result.as_ref().unwrap();
    r
}

/// PID de la 1re vidéo analysée, dans l'ordre des programmes puis de la PMT.
fn main_video(idx: &TsIndex) -> u16 {
    idx.structure()
        .programs
        .iter()
        .flat_map(|p| &p.streams)
        .find(|s| s.analysed && matches!(s.kind, StreamKind::Video(Some(_))))
        .map(|s| s.pid)
        .expect("aucune vidéo analysée")
}

type Entries = Vec<(FrameEntry, AuInfo)>;

/// Entrées publiées d'une vidéo, en ordre de décodage.
fn entries(idx: &TsIndex, pid: u16) -> Entries {
    idx.with_video(pid, |v| {
        (0..v.published())
            .map(|d| (v.entry(d).unwrap(), v.au(d).unwrap()))
            .collect()
    })
    .unwrap_or_default()
}

/// PTS en ordre de présentation (vérifie au passage que `pres_idx` est une permutation et que
/// `decode_idx_of` en est l'inverse).
fn pres_pts(idx: &TsIndex, pid: u16) -> Vec<i64> {
    let e = entries(idx, pid);
    let mut by_pres = vec![None; e.len()];
    for (f, _) in &e {
        assert!(by_pres[f.pres_idx as usize].is_none(), "pres_idx en double");
        by_pres[f.pres_idx as usize] = Some(f.pts);
        let back = idx
            .with_video(pid, |v| v.decode_idx_of(f.pres_idx))
            .flatten();
        assert_eq!(back, Some(f.decode_idx));
    }
    by_pres.into_iter().map(Option::unwrap).collect()
}

/// Noms du manifeste (lignes `name = "…"`).
fn corpus_names() -> Vec<String> {
    let text = include_str!("../../../corpus/manifest.toml");
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .map(str::to_owned)
        .collect()
}

/// Les PTS de `ffprobe` sont ceux de l'index à un multiple de 2^33 près, le même pour tous
/// (`FFmpeg` ramène un flux qui boucle autour de 0, l'index déroule vers le haut).
fn same_modulo_wrap(ours: &[i64], theirs: &[i64]) -> bool {
    let Some(k) = ours.first().zip(theirs.first()).map(|(a, b)| a - b) else {
        return ours.len() == theirs.len();
    };
    k % WRAP == 0 && ours.len() == theirs.len() && ours.iter().zip(theirs).all(|(a, b)| a - b == k)
}

#[test]
fn index_matches_ffprobe() {
    let names: Vec<String> = corpus_names()
        .into_iter()
        .filter(|n| !manifest(n).big)
        .collect();
    assert!(names.len() >= 20, "{names:?}");
    for name in &names {
        let e = manifest(name);
        let run = run_full(&load(name));
        let pid = main_video(&run.index);
        let n = run
            .index
            .with_video(pid, ma_ts::VideoIndex::published)
            .unwrap();
        // `ffprobe` ne lit pas les paquets de 208 octets, et prend le flux VC-1 sans données de
        // `ts_unsupported` pour 1re vidéo : l'oracle est alors le fichier source, dont la vidéo
        // est identique (seuls l'emballage des paquets ou la PMT diffèrent).
        let oracle = if e.has("packet_208") || e.has("extra_streams") {
            corpus_file(e.derived_from.as_deref().unwrap())
        } else {
            corpus_file(name)
        };
        let pts = pres_pts(&run.index, pid);
        if e.has("pes_grouped") {
            // Une AU sur deux n'a pas de PTS : `ffprobe_frames` refuse ces images. Les PTS
            // présents (paquets `ffprobe`) sont ceux des AU non estimées, dans l'ordre.
            let packets = ffprobe_packets(&oracle);
            assert_eq!(n as usize, packets.len(), "{name}");
            let real: Vec<i64> = entries(&run.index, pid)
                .iter()
                .filter(|(_, a)| !a.pts_estimated)
                .map(|(f, _)| f.pts)
                .collect();
            let theirs: Vec<i64> = packets.iter().filter_map(|p| p.pts).collect();
            assert!(same_modulo_wrap(&real, &theirs), "{name}");
        } else if e.has("starts_mid_gop") || e.has("starts_at_cra") {
            // R1 : le fichier ne commence pas sur un RAP propre. L'index garde toutes les AU
            // (autant que de paquets `ffprobe`) ; `FFmpeg` ne sort pas les images non
            // décodables de tête, donc ses PTS sont un sous-ensemble de ceux de l'index.
            let packets = ffprobe_packets(&oracle);
            assert_eq!(n as usize, packets.len(), "{name}");
            let frames = ffprobe_frames(&oracle);
            assert!(!frames.is_empty() && frames.len() <= n as usize, "{name}");
            let ours: BTreeSet<i64> = pts.iter().copied().collect();
            // Multiple de 2^33 le plus proche de l'écart entre les deux premiers PTS.
            let d = ours.iter().next().unwrap() - frames[0].pts;
            let k = (d + WRAP / 2).div_euclid(WRAP) * WRAP;
            assert!(
                frames.iter().all(|f| ours.contains(&(f.pts + k))),
                "{name} : PTS de ffprobe absents de l'index"
            );
        } else {
            assert_eq!(n, e.frames, "{name}");
            let theirs: Vec<i64> = ffprobe_frames(&oracle).iter().map(|f| f.pts).collect();
            assert!(
                same_modulo_wrap(&pts, &theirs),
                "{name} : PTS en ordre de présentation différents de ffprobe\n{:?}\n{:?}",
                (pts.len(), theirs.len()),
                pts.iter()
                    .zip(&theirs)
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
            );
        }
        assert!(run.index.progress().complete, "{name}");
    }
}

/// Source qui relève l'index publié à chaque lecture (l'indexeur ne tient aucun verrou pendant
/// une lecture).
struct Snapshotting {
    inner: MemSource,
    index: Arc<TsIndex>,
    snaps: Mutex<Vec<Entries>>,
}

impl ReadAt for Snapshotting {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        if let Some(&(pid, _)) = self.index.videos().first() {
            let snap = entries(&self.index, pid);
            self.snaps.lock().unwrap().push(snap);
        }
        self.inner.read_at(offset, buf)
    }
}

/// Indexe `l` par blocs de `chunk_size`, publication à chaque bloc ; rend les relevés.
fn snapshots(l: &Loaded, chunk_size: usize) -> (Vec<Entries>, Run) {
    let index = Arc::new(TsIndex::new(l.format, l.data.len() as u64));
    let src = Snapshotting {
        inner: MemSource::new(l.data.clone()),
        index: index.clone(),
        snaps: Mutex::new(Vec::new()),
    };
    let opts = IndexOptions {
        chunk_size,
        publish_every: Duration::ZERO,
    };
    let mut sink = VecSink::default();
    let result = index_into(&src, l.start, &index, &opts, &mut sink);
    let snaps = src.snaps.into_inner().unwrap();
    (
        snaps,
        Run {
            index,
            result,
            sink,
        },
    )
}

const STABLE_FILES: [&str; 3] = [
    "h264_720p_closed",
    "hevc_720p_open_gop",
    "h264_720p_recovery",
];

/// Fichier et index complet de référence, calculés une fois.
fn reference(i: usize) -> &'static (Loaded, Entries) {
    static REFS: OnceLock<Vec<(Loaded, Entries)>> = OnceLock::new();
    &REFS.get_or_init(|| {
        STABLE_FILES
            .iter()
            .map(|n| {
                let l = load(n);
                let run = run_full(&l);
                let e = entries(&run.index, main_video(&run.index));
                (l, e)
            })
            .collect()
    })[i]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    #[test]
    fn published_entries_never_change(k in 1usize..=64, file in 0usize..STABLE_FILES.len()) {
        let (l, full) = reference(file);
        let (snaps, run) = snapshots(l, 188 * k);
        run.result.unwrap();
        let fin = entries(&run.index, main_video(&run.index));
        prop_assert_eq!(&fin, full);
        let partial = snaps.iter().filter(|s| !s.is_empty() && s.len() < full.len()).count();
        prop_assert!(partial > 0, "aucun instantané partiel");
        for s in &snaps {
            prop_assert!(s.len() <= full.len());
            prop_assert_eq!(&s[..], &full[..s.len()]);
        }
    }
}

#[test]
fn partial_index_ends_on_a_rap() {
    for (i, name) in STABLE_FILES.iter().enumerate() {
        let (l, full) = reference(i);
        let (snaps, run) = snapshots(l, 1 << 16);
        run.result.unwrap();
        let mut partial = 0;
        for s in snaps
            .iter()
            .filter(|s| !s.is_empty() && s.len() < full.len())
        {
            partial += 1;
            assert_ne!(full[s.len()].0.rap, RapKind::None, "{name}");
        }
        assert!(partial > 0, "{name}");
    }
}

/// Indexe `l` coupé à `cut` octets (`TruncatingSource`).
fn run_truncated(l: &Loaded, cut: u64) -> Run {
    let src = TruncatingSource::new(MemSource::new(l.data.clone()), cut);
    run_on(&src, l.format, l.start, &IndexOptions::default())
}

#[test]
fn open_gop_cra_waits_for_next_rap() {
    let l = load("hevc_720p_open_gop");
    let full = run_full(&l);
    let pid = main_video(&full.index);
    let e = entries(&full.index, pid);
    let raps: Vec<usize> = (0..e.len())
        .filter(|&d| e[d].0.rap != RapKind::None)
        .collect();
    assert!(raps.len() >= 4, "{raps:?}");
    assert!(raps[1..].iter().all(|&d| e[d].0.rap == RapKind::Cra));
    // La CRA n° 2 est vue (l'AU qui la suit aussi), pas la n° 3 : le préfixe publié s'arrête à
    // la CRA n° 1 ; la CRA n° 2 et ses RASL attendent le RAP suivant.
    for w in 1..3 {
        let (cra, next) = (raps[w + 1], raps[w + 2]);
        let cut = e[cra + 2].1.start.packet_offset;
        assert!(cut < e[next].1.start.packet_offset);
        let run = run_truncated(&l, cut);
        assert!(matches!(
            run.result,
            Err(IndexError::Io(IoError::Truncated { .. }))
        ));
        let got = entries(&run.index, pid);
        assert_eq!(got.len(), raps[w], "coupure après la CRA {cra}");
        assert_eq!(got[..], e[..got.len()]);
        assert!(!run.index.progress().complete);
    }
}

/// Métadonnées `ma-es` des AU de l'ES extrait par `ffmpeg` (ordre de décodage).
fn es_metas(path: &std::path::Path, codec: Codec) -> Vec<ma_es::AuMeta> {
    let es = ffmpeg_es(path);
    let mut f = ma_es::framer(codec);
    let mut out = Vec::new();
    f.push(&es, &mut out);
    f.finish(&mut out);
    out.iter().map(|b| b.meta).collect()
}

#[ma_trace::covers("NAV.ORACLE.03")]
#[test]
fn recovery_span_from_reference_count() {
    let name = "h264_720p_recovery";
    let run = run_full(&load(name));
    let pid = main_video(&run.index);
    let e = entries(&run.index, pid);
    let metas = es_metas(&corpus_file(name), Codec::H264);
    assert_eq!(metas.len(), e.len());
    let gop = manifest(name).gop;
    let mut seen = 0;
    for (j, m) in metas.iter().enumerate() {
        let ma_es::RapClass::Recovery { frame_cnt } = m.rap else {
            continue;
        };
        // R : 1re AU à partir du RAP telle que [RAP, R) compte `frame_cnt` AU de référence.
        let mut refs = 0;
        let r = (j..metas.len())
            .find(|&i| {
                if refs == frame_cnt {
                    return true;
                }
                refs += u32::from(metas[i].reference);
                false
            })
            .expect("point de reprise non résolu");
        let span = e[r].0.pres_idx.saturating_sub(e[j].0.pres_idx);
        assert_eq!(e[j].0.rap, RapKind::Recovery { pres_span: span }, "AU {j}");
        assert!(span <= gop);
        seen += 1;
    }
    assert!(seen >= 5, "{seen}");
    // Les B qui précèdent un point de reprise (GOP ouvert) dépendent du GOP précédent.
    assert!(e.iter().any(|(f, _)| f.leading == Leading::Skippable));
}

#[test]
fn pts_wrap_keeps_presentation_order() {
    let run = run_full(&load("h264_720p_pts_wrap"));
    let pid = main_video(&run.index);
    let pts = pres_pts(&run.index, pid);
    assert_eq!(pts.len(), 250);
    assert!(pts.windows(2).all(|w| w[0] < w[1]), "PTS non croissants");
    assert!(pts[0] < WRAP && *pts.last().unwrap() > WRAP, "{pts:?}");
}

#[test]
fn pcr_samples_are_unwrapped() {
    let run = run_full(&load("h264_720p_pts_wrap"));
    let s = run.index.structure();
    let pcr_pid = s.programs[0].pcr_pid.unwrap();
    let pcrs = run.index.pcrs(pcr_pid);
    assert!(pcrs.len() > 50, "{}", pcrs.len());
    assert!(pcrs.windows(2).all(|w| w[0].pcr < w[1].pcr
        && w[0].offset < w[1].offset
        && w[0].packet_index < w[1].packet_index));
    assert!(pcrs[0].pcr < WRAP * 300 && pcrs.last().unwrap().pcr > WRAP * 300);
    assert!(pcrs.iter().all(|p| !p.discontinuity));
}

#[test]
fn au_without_pts_gets_estimated_pts() {
    let run = run_full(&load("h264_720p_pes_grouped"));
    let pid = main_video(&run.index);
    let e = entries(&run.index, pid);
    assert_eq!(e.len(), 100);
    let unique: BTreeSet<i64> = e.iter().map(|(f, _)| f.pts).collect();
    assert_eq!(unique.len(), 100);
    assert_eq!(e.iter().filter(|(_, a)| a.pts_estimated).count(), 50);
    assert!(e.iter().all(|(f, _)| f.pres_idx == f.decode_idx));
    let msgs: Vec<_> = run
        .sink
        .messages
        .iter()
        .filter(|m| m.id == "ts-au-no-pts")
        .collect();
    assert_eq!(msgs.len(), 50);
    assert!(
        msgs.iter()
            .all(|m| m.pid == Some(pid)
                && m.decode_idx.is_some_and(|d| e[d as usize].1.pts_estimated))
    );
}

/// Offsets des paquets du PID `pid` dans `data`.
fn packets_of(l: &Loaded, pid: u16) -> Vec<u64> {
    let size = l.format.size() as u64;
    let count = (l.data.len() as u64 - l.start) / size;
    (0..count)
        .map(|i| l.start + i * size)
        .filter(|&o| {
            let p = &l.data[o as usize..];
            u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]) == pid
        })
        .collect()
}

/// Retire le paquet `lost` (188 octets) de `l`, réindexe et vérifie qu'une seule rupture de
/// continuité est signalée ; rend l'index sans perte et l'index avec perte.
fn index_without_packet(l: &Loaded, lost: u64) -> (Entries, Entries) {
    let full = run_full(l);
    let pid = main_video(&full.index);
    let mut data = l.data.clone();
    data.drain(lost as usize..lost as usize + 188);
    let cut = Loaded {
        data,
        format: l.format,
        start: l.start,
    };
    let run = run_full(&cut);
    let gaps = run
        .sink
        .messages
        .iter()
        .filter(|m| m.id == "ts-cc-gap")
        .count();
    assert_eq!(gaps, 1);
    (entries(&full.index, pid), entries(&run.index, pid))
}

/// Seule l'AU `hit` est `damaged` ; toutes les autres entrées sont identiques à l'index sans
/// perte (`FrameEntry`, taille, PTS, `damaged`). Les positions, elles, glissent d'un paquet.
fn only_damaged(e: &Entries, got: &Entries, hit: usize) {
    assert_eq!(got.len(), e.len(), "published() a changé");
    let damaged: Vec<usize> = (0..got.len()).filter(|&d| got[d].1.damaged).collect();
    assert_eq!(damaged, [hit]);
    for d in 0..e.len() {
        assert_eq!(got[d].0, e[d].0, "FrameEntry {d}");
        assert_eq!(got[d].1.pts, e[d].1.pts, "PTS {d}");
        if d != hit {
            assert_eq!(
                (got[d].1.size, got[d].1.damaged),
                (e[d].1.size, e[d].1.damaged),
                "AU {d}"
            );
        }
    }
}

/// Paquet perdu au milieu de l'AU 100 (R12) : le découpeur n'est pas réinitialisé, l'AU reste
/// dans l'index, `damaged`, avec 184 octets de moins.
///
/// Compromis de R12 (non exercés ici, voir la doc du module) : un code de début perdu dans le
/// trou fusionne deux AU (une entrée `damaged`, `published()` − 1) ; un faux `00 00 01` peut
/// naître des octets recollés de part et d'autre du trou.
#[test]
fn lost_packet_damages_only_its_au() {
    let l = load("h264_720p_closed");
    let full = run_full(&l);
    let pid = main_video(&full.index);
    let e = entries(&full.index, pid);
    let target = &e[100].1;
    // Paquet de données plein (sans champ d'adaptation) strictement à l'intérieur de l'AU.
    let inside: Vec<u64> = packets_of(&l, pid)
        .into_iter()
        .filter(|&o| o > target.start.packet_offset && o < target.end.packet_offset)
        .filter(|&o| l.data[o as usize + 3] & 0x30 == 0x10)
        .collect();
    assert!(inside.len() > 4);
    let (e, got) = index_without_packet(&l, inside[inside.len() / 2]);
    only_damaged(&e, &got, 100);
    assert_eq!(got[100].1.size, e[100].1.size - 184);
}

/// Perte du dernier paquet du PES de l'AU 100 : le trou est à la fin de l'AU (`g == end`),
/// qui est marquée ; l'AU 101, qui commence juste après le trou, est intacte.
#[test]
fn lost_last_packet_of_pes_damages_its_au() {
    let l = load("h264_720p_closed");
    let full = run_full(&l);
    let pid = main_video(&full.index);
    let e = entries(&full.index, pid);
    let next = e[101].1.start.packet_offset;
    let last = packets_of(&l, pid)
        .into_iter()
        .filter(|&o| o > e[100].1.start.packet_offset && o < next)
        .max()
        .unwrap();
    let (e, got) = index_without_packet(&l, last);
    only_damaged(&e, &got, 100);
    assert!(got[100].1.size < e[100].1.size);
}

#[test]
fn truncated_during_indexing_keeps_partial_index() {
    let l = load("h264_720p_closed");
    let full = run_full(&l);
    let pid = main_video(&full.index);
    let e = entries(&full.index, pid);
    let run = run_truncated(&l, l.data.len() as u64 / 2);
    assert!(
        matches!(run.result, Err(IndexError::Io(IoError::Truncated { .. }))),
        "{:?}",
        run.result
    );
    let got = entries(&run.index, pid);
    assert!(!got.is_empty() && got.len() < e.len(), "{}", got.len());
    assert_eq!(got[..], e[..got.len()]);
    let p = run.index.progress();
    assert!(!p.complete && p.bytes_done > 0 && p.bytes_done <= p.bytes_total);
}

#[ma_trace::covers("PERF.JOB.01")]
#[test]
fn cancel_stops_within_500ms() {
    let l = load("hevc_720p_main10");
    let src: Arc<dyn ReadAt> = Arc::new(LatencySource::new(
        MemSource::new(l.data.clone()),
        Duration::from_millis(5),
    ));
    let out = Arc::new(TsIndex::new(l.format, l.data.len() as u64));
    let opts = IndexOptions {
        chunk_size: 64 << 10,
        ..IndexOptions::default()
    };
    let handle = spawn_index(
        src,
        l.start,
        out.clone(),
        opts,
        Box::new(VecSink::default()),
    );
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    handle.cancel();
    let r = handle.join();
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(500), "{took:?}");
    assert!(matches!(r, Err(IndexError::Cancelled)), "{r:?}");
    assert!(!out.progress().complete);
    let pid = main_video(&out);
    let n = out.with_video(pid, ma_ts::VideoIndex::published).unwrap();
    let e = entries(&out, pid);
    assert_eq!(e.len(), n as usize);
}

#[test]
fn spawned_index_completes_and_drop_cancels() {
    let l = load("h264_720p_nob");
    let src: Arc<dyn ReadAt> = Arc::new(MemSource::new(l.data.clone()));
    let out = Arc::new(TsIndex::new(l.format, l.data.len() as u64));
    let h = spawn_index(
        src.clone(),
        l.start,
        out.clone(),
        IndexOptions::default(),
        Box::new(VecSink::default()),
    );
    h.join().unwrap();
    assert!(out.progress().complete);
    assert_eq!(out.progress().bytes_done, out.progress().bytes_total);
    assert_eq!(entries(&out, main_video(&out)).len(), 100);
    // Abandon du handle : annulation puis attente, sans blocage.
    let slow: Arc<dyn ReadAt> = Arc::new(LatencySource::new(
        MemSource::new(l.data.clone()),
        Duration::from_millis(20),
    ));
    let out = Arc::new(TsIndex::new(l.format, l.data.len() as u64));
    let opts = IndexOptions {
        chunk_size: 16 << 10,
        ..IndexOptions::default()
    };
    let t0 = Instant::now();
    drop(spawn_index(
        slow,
        l.start,
        out.clone(),
        opts,
        Box::new(VecSink::default()),
    ));
    assert!(t0.elapsed() < Duration::from_millis(500));
    assert!(!out.progress().complete);
}

/// `VmHWM` du processus, en octets.
fn vm_hwm() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    let kib: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap();
    kib * 1024
}

#[ma_trace::covers("PERF.INDEX.02")]
#[test]
fn indexing_uhd_big_memory_under_200_mib() {
    let Some(path) = corpus_file_if_present("uhd_big") else {
        eprintln!("uhd_big absent : test non exécuté (cargo xtask corpus gen --big)");
        return;
    };
    let before = vm_hwm();
    let (src, format, start) = probe(&path).unwrap();
    let out = TsIndex::new(format, src.len());
    let t0 = Instant::now();
    index_into(
        &src,
        start,
        &out,
        &IndexOptions::default(),
        &mut VecSink::default(),
    )
    .unwrap();
    let after = vm_hwm();
    let pid = main_video(&out);
    let n = out.with_video(pid, ma_ts::VideoIndex::published).unwrap();
    eprintln!(
        "uhd_big : {n} images, VmHWM {} → {} Mio (+{} Mio), {:?}",
        before >> 20,
        after >> 20,
        (after - before) >> 20,
        t0.elapsed()
    );
    assert!(n >= 1000 && n.is_multiple_of(1000), "{n}");
    assert!(after - before < 200 << 20);
}

#[test]
fn multiple_videos_indexed() {
    let run = run_full(&load("mpts_3prog"));
    let videos = run.index.videos();
    assert_eq!(videos.len(), 3, "{videos:?}");
    let s = run.index.structure();
    let pids: Vec<u16> = s
        .programs
        .iter()
        .flat_map(|p| &p.streams)
        .filter(|s| s.analysed)
        .map(|s| s.pid)
        .collect();
    assert_eq!(videos.iter().map(|v| v.0).collect::<Vec<_>>(), pids);
    for (pid, codec) in videos {
        assert_eq!(codec, Codec::H264);
        assert_eq!(entries(&run.index, pid).len(), 50, "PID {pid:#x}");
    }
}

/// Les plages `[start, end)` de l'index relisent exactement l'ES extrait par `ffmpeg`, AU par
/// AU, quel que soit le découpage en PES (PES coupés en deux, groupés, paquets 204 et TTS).
#[test]
fn au_ranges_reread_the_es() {
    for name in [
        "h264_720p_pes_split",
        "h264_720p_pes_grouped",
        "ts204_h264",
        "tts_h264",
        "mpeg2_576i_closed",
    ] {
        let path = corpus_file(name);
        let (src, format, start) = probe(&path).unwrap();
        let out = TsIndex::new(format, src.len());
        index_into(
            &src,
            start,
            &out,
            &IndexOptions::default(),
            &mut VecSink::default(),
        )
        .unwrap();
        let pid = main_video(&out);
        let mut es = Vec::new();
        let mut buf = Vec::new();
        for (_, a) in entries(&out, pid) {
            read_es(&src, format, pid, a.start, a.end, &mut buf).unwrap();
            assert_eq!(buf.len(), a.size as usize, "{name}");
            es.extend_from_slice(&buf);
        }
        assert!(es == ffmpeg_es(&path), "{name} : ES relu différent");
    }
}

/// Indexe `src` avec une publication périodique très rare (1 h) : seuls les plafonds d'attente
/// déclenchent une publication avant la fin. Vérifie qu'aucune coupe n'est forcée ni aucune
/// discontinuité vue, et rend les entrées de chaque vidéo.
fn rare_publication(
    src: &dyn ReadAt,
    format: PacketFormat,
    start: u64,
    name: &str,
) -> Vec<Entries> {
    let opts = IndexOptions {
        publish_every: Duration::from_secs(3600),
        ..IndexOptions::default()
    };
    let run = run_on(src, format, start, &opts);
    run.result.as_ref().unwrap();
    for id in ["ts-index-forced-publish", "ts-pts-discontinuity"] {
        let n = run.sink.messages.iter().filter(|m| m.id == id).count();
        assert_eq!(n, 0, "{name} : {n} × {id}");
    }
    run.index
        .videos()
        .iter()
        .map(|&(pid, _)| entries(&run.index, pid))
        .collect()
}

fn default_publication(src: &dyn ReadAt, format: PacketFormat, start: u64) -> Vec<Entries> {
    let run = run_on(src, format, start, &IndexOptions::default());
    run.result.as_ref().unwrap();
    run.index
        .videos()
        .iter()
        .map(|&(pid, _)| entries(&run.index, pid))
        .collect()
}

/// Flux sains : la coupe normale (`cut`) est tentée avant toute publication forcée, même quand
/// la publication périodique n'a jamais lieu ; l'index est celui de la publication par défaut.
#[test]
fn rare_publication_never_forces_on_sane_corpus() {
    for name in corpus_names().iter().filter(|n| !manifest(n).big) {
        let l = load(name);
        let src = MemSource::new(l.data.clone());
        let rare = rare_publication(&src, l.format, l.start, name);
        assert!(
            rare == default_publication(&src, l.format, l.start),
            "{name}"
        );
    }
}

#[test]
fn rare_publication_never_forces_on_uhd_big() {
    let Some(path) = corpus_file_if_present("uhd_big") else {
        eprintln!("uhd_big absent : test non exécuté (cargo xtask corpus gen --big)");
        return;
    };
    let (src, format, start) = probe(&path).unwrap();
    let rare = rare_publication(&src, format, start, "uhd_big");
    assert!(rare[0].len() >= 62_000);
    assert!(rare == default_publication(&src, format, start), "uhd_big");
}
