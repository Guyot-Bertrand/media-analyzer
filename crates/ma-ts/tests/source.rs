//! Source d'AU réelle (`TsAuSource`) sur le corpus généré : oracles `ffprobe` et `ffmpeg`,
//! relecture des AU, ouverture progressive, lecture à travers le cache, fichier modifié ou
//! tronqué. Reprend aussi les tests de corpus de l'ancienne source de test de `ma-testkit`.

// Positions, tailles et compteurs de tests : bornés par construction.
#![allow(clippy::cast_possible_truncation)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ma_core::{AuSource, Codec, Leading, PictureType, RapKind};
use ma_decode::{NotDecodable, start_for};
use ma_es::RapClass;
use ma_io::{BlockCache, CacheConfig, FileSource, Locality};
use ma_testkit::{
    CorruptingSource, corpus_file, corpus_file_if_present, ffmpeg_es, ffprobe_frames,
    ffprobe_packets, manifest,
};
use ma_ts::testutil::{pat, pmt, psi_packets};
use ma_ts::{MAX_AU_BYTES, OpenError, REVALIDATE_EVERY, TsAuSource};

fn open(name: &str) -> TsAuSource {
    TsAuSource::open(&corpus_file(name)).unwrap()
}

/// Copie d'un fichier du corpus dans un répertoire temporaire (modifiable par le test).
fn scratch_copy(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{name}.ts"));
    std::fs::copy(corpus_file(name), &path).unwrap();
    (dir, path)
}

fn rap_decode_indices(src: &TsAuSource, kind: impl Fn(RapKind) -> bool) -> Vec<u32> {
    (0..src.len())
        .filter(|&d| kind(src.entry(d).unwrap().rap))
        .collect()
}

// --- Oracles ----------------------------------------------------------------------------------

/// DATA.TS.01 : PTS et DTS de chaque AU (ordre de décodage) égaux à ceux des paquets `ffprobe` ;
/// position du paquet de début de PES égale au `pos` de `ffprobe` quand l'AU commence un PES.
/// Dans `h264_720p_pes_split`, seule l'AU 0 commence un PES (manifeste) ; ailleurs, chaque PES
/// porte une AU. En MPEG-2, le PES d'une AU est celui qui contient `start + 2` (R13 : un octet
/// nul de fin peut rester dans le PES précédent) : c'est bien celui que `ffprobe` désigne.
#[ma_trace::covers("DATA.TS.01")]
#[test]
fn pts_dts_pos_match_ffprobe() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_main10",
        "mpeg2_576i_closed",
        "h264_720p_pes_split",
    ] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let packets = ffprobe_packets(&path);
        assert_eq!(src.len() as usize, packets.len(), "{name} : nombre d'AU");
        let split = manifest(name).has("pes_split");
        let mut compared = 0;
        for (d, p) in (0..src.len()).zip(&packets) {
            let au = src.au_info(d).unwrap();
            assert_eq!(Some(au.pts), p.pts, "{name} AU {d} : PTS");
            assert_eq!(au.dts, p.dts, "{name} AU {d} : DTS");
            assert_eq!(src.entry(d).unwrap().pts, au.pts);
            if !split || d == 0 {
                assert_eq!(
                    Some(au.pes_packet),
                    p.pos,
                    "{name} AU {d} : position du PES"
                );
                compared += 1;
            }
        }
        assert!(compared >= 1, "{name}");
    }
}

/// NAV.THUMB.01 : appariement par PTS avec les images de `ffprobe` ; type d'image et drapeau
/// clé égaux ; `pres_idx` = rang dans `ffprobe_frames` (triées par PTS).
#[ma_trace::covers("NAV.THUMB.01")]
#[test]
fn picture_types_and_indices_match_ffprobe() {
    for name in [
        "h264_720p_closed",
        "h264_720p_high10",
        "h264_720p_recovery",
        "hevc_720p_main10",
        "hevc_720p_open_gop",
        "mpeg2_576i_closed",
    ] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let frames = ffprobe_frames(&path);
        assert_eq!(frames.len(), src.len() as usize, "{name}");
        let by_pts: HashMap<i64, u32> = (0..src.len())
            .map(|d| (src.entry(d).unwrap().pts, d))
            .collect();
        for (rank, f) in frames.iter().enumerate() {
            let d = by_pts[&f.pts];
            let e = src.entry(d).unwrap();
            let expected = match f.pict_type {
                'I' => PictureType::I,
                'P' => PictureType::P,
                'B' => PictureType::B,
                other => panic!("{name} : pict_type {other}"),
            };
            assert_eq!(e.picture, expected, "{name} AU {d} (PTS {})", f.pts);
            assert_eq!(e.rap != RapKind::None, f.key, "{name} AU {d} : image clé");
            assert_eq!(e.pres_idx as usize, rank, "{name} AU {d} : rang");
            assert_eq!(src.decode_idx_of(e.pres_idx), Some(d));
        }
    }
}

/// La concaténation de `read_au` sur toutes les AU, en ordre de décodage, est l'ES extrait par
/// `ffmpeg -c copy`.
#[test]
fn au_bytes_match_ffmpeg_es() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_main10",
        "mpeg2_576i_closed",
        "h264_720p_pes_split",
        "h264_1080i_mbaff",
    ] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let mut all = Vec::new();
        let mut buf = Vec::new();
        for d in 0..src.len() {
            src.read_au(d, &mut buf).unwrap();
            assert_eq!(
                buf.len(),
                src.au_info(d).unwrap().size as usize,
                "{name} AU {d}"
            );
            all.extend_from_slice(&buf);
        }
        let oracle = ffmpeg_es(&path);
        assert_eq!(all.len(), oracle.len(), "{name} : taille de l'ES");
        assert!(all == oracle, "{name} : octets de l'ES");
    }
}

/// Taille de chaque AU égale à celle du paquet `ffprobe` (son analyseur coupe au `00 00 01`).
/// MPEG-2 : un `00` qui précède le code de début appartient à l'AU précédente ; le donner à la
/// suivante tronquait la dernière tranche de l'image 26 de `mpeg2_576i_closed` (décodage faux au
/// coin inférieur droit, constaté par `framemd5`).
#[test]
fn mpeg2_au_sizes_match_ffprobe() {
    let path = corpus_file("mpeg2_576i_closed");
    let src = TsAuSource::open(&path).unwrap();
    let packets = ffprobe_packets(&path);
    assert_eq!(src.len() as usize, packets.len());
    for (d, p) in (0..src.len()).zip(&packets) {
        assert_eq!(src.au_info(d).unwrap().size, p.size, "AU {d}");
    }
}

/// Lecture séquentielle de toutes les AU : au plus 1,1 × la taille du fichier, plus un bloc,
/// lus par le cache. L'indexation (lecture directe, hors cache) n'y figure pas.
#[test]
fn read_amplification_is_bounded() {
    let path = corpus_file("hevc_720p_main10");
    let size = std::fs::metadata(&path).unwrap().len();
    let src = TsAuSource::open(&path).unwrap();
    assert_eq!(
        src.io_stats().bytes_read,
        0,
        "l'indexation ne passe pas par le cache"
    );
    let indexer = src.indexer_io_stats();
    assert!(
        indexer.bytes_read >= size && indexer.bytes_read <= size + (4 << 20),
        "{indexer:?}"
    );
    let mut buf = Vec::new();
    for d in 0..src.len() {
        src.read_au(d, &mut buf).unwrap();
    }
    let block = u64::from(CacheConfig::for_locality(Locality::Local).block_size);
    let stats = src.io_stats();
    assert!(
        stats.bytes_read * 10 <= size * 11 + block * 10,
        "{stats:?}, fichier de {size} octets"
    );
}

/// L'ouverture progressive rend la main avant la fin de l'indexation de `uhd_big` (4 Go).
#[test]
fn progressive_open_returns_before_full_index() {
    let Some(path) = corpus_file_if_present("uhd_big") else {
        eprintln!("uhd_big absent : test non exécuté (cargo xtask corpus gen --big)");
        return;
    };
    let (src, handle) = TsAuSource::open_progressive(&path).unwrap();
    assert!(src.len() > 0);
    assert!(!src.index().progress().complete);
    let mut buf = Vec::new();
    src.read_au(0, &mut buf).unwrap();
    assert_ne!(buf.len(), 0);
    assert_eq!(src.entry(0).unwrap().rap, RapKind::Idr);
    // L'abandon du handle annule l'indexation ; l'index partiel reste lisible.
    drop(handle);
    let n = src.len();
    assert!(n > 0 && !src.index().progress().complete);
    assert_eq!(src.len(), n, "index figé après l'annulation");
}

/// Ouverture progressive d'un petit fichier : même index qu'à l'ouverture complète, une fois
/// l'indexation terminée.
#[test]
fn progressive_open_converges_to_the_full_index() {
    let path = corpus_file("h264_720p_closed");
    let full = TsAuSource::open(&path).unwrap();
    let (src, handle) = TsAuSource::open_progressive(&path).unwrap();
    assert!(src.len() > 0);
    handle.join().unwrap();
    assert!(src.index().progress().complete);
    assert_eq!(src.len(), full.len());
    assert_eq!(src.pid(), full.pid());
    for d in 0..src.len() {
        assert_eq!(src.entry(d), full.entry(d));
        assert_eq!(src.au_info(d), full.au_info(d));
    }
    let indexer = src.indexer_io_stats();
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(indexer.bytes_read >= size, "{indexer:?}");
}

// --- Sélection de la vidéo, erreurs d'ouverture ------------------------------------------------

/// STRUCT.TREE.06 (niveau unitaire seulement) : la 1re vidéo de la PMT est sélectionnée.
#[test]
fn first_video_of_the_pmt_is_selected() {
    let src = open("ts_ref");
    assert_eq!((src.pid(), src.codec()), (0x100, Codec::Hevc));
    assert!(src.len() > 0);
    // MPTS : la vidéo du 1er programme ; les autres restent accessibles par `for_video`.
    let mpts = open("mpts_3prog");
    let st = mpts.index().structure();
    let first = st.programs[0].streams[0].pid;
    assert_eq!(mpts.pid(), first);
    let path = corpus_file("mpts_3prog");
    let io = Arc::new(BlockCache::new(
        FileSource::open(&path).unwrap(),
        CacheConfig::for_locality(Locality::Local),
    ));
    let other = st.programs[2].streams[0].pid;
    let third = TsAuSource::for_video(mpts.index().clone(), io.clone(), other).unwrap();
    assert_eq!(third.pid(), other);
    assert_eq!(third.len(), 50);
    let mut buf = Vec::new();
    third.read_au(0, &mut buf).unwrap();
    assert!(buf.starts_with(&[0, 0, 0, 1]) || buf.starts_with(&[0, 0, 1]));
    assert_eq!(third.indexer_io_stats(), ma_io::IoStats::default());
    assert!(TsAuSource::for_video(mpts.index().clone(), io, 0x1FFE).is_none());
}

fn write_tmp(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// TS sans vidéo : PAT et PMT d'un programme audio seul, répétées.
fn audio_only() -> Vec<u8> {
    let mut v = Vec::new();
    for cc in 0..8u8 {
        for p in psi_packets(0, &pat(1, 0, &[(1, 0x1000)]), cc) {
            v.extend_from_slice(&p);
        }
        for p in psi_packets(0x1000, &pmt(1, 0, 0x101, &[], &[(0x0F, 0x101, &[])]), cc) {
            v.extend_from_slice(&p);
        }
    }
    v
}

#[test]
fn unusable_files_are_open_errors() {
    let dir = tempfile::tempdir().unwrap();
    let empty = write_tmp(&dir, "empty.ts", &[]);
    let not_ts = write_tmp(&dir, "text.ts", &[b'x'; 4096]);
    let audio = write_tmp(&dir, "audio.ts", &audio_only());
    let missing = dir.path().join("absent.ts");
    for open in [
        |p: &Path| TsAuSource::open(p).map(drop),
        |p: &Path| TsAuSource::open_progressive(p).map(drop),
    ] {
        assert!(matches!(open(&empty), Err(OpenError::Empty)));
        assert!(matches!(open(&not_ts), Err(OpenError::NotTs)));
        assert!(matches!(open(&audio), Err(OpenError::NoVideo)));
        assert!(matches!(open(&missing), Err(OpenError::Io(_))));
    }
}

/// Fichier tronqué à l'ouverture (paquets coupés en fin de fichier) : l'indexation s'arrête
/// proprement et l'ouverture réussit avec les AU complètes.
#[test]
fn file_cut_in_the_middle_of_a_packet_opens() {
    let dir = tempfile::tempdir().unwrap();
    let data = std::fs::read(corpus_file("h264_720p_closed")).unwrap();
    let cut = write_tmp(&dir, "cut.ts", &data[..data.len() / 2 + 77]);
    let src = TsAuSource::open(&cut).unwrap();
    assert!(src.len() > 50 && src.len() < 250, "{}", src.len());
    let mut buf = Vec::new();
    src.read_au(src.len() - 1, &mut buf).unwrap();
}

// --- Lecture : bornes, fichier modifié ou tronqué ----------------------------------------------

#[test]
fn read_out_of_index_is_an_error() {
    let src = open("hevc_720p_cut");
    let n = src.len();
    let err = src.read_au(n, &mut Vec::new()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        (src.entry(n), src.au_info(n), src.decode_idx_of(n)),
        (None, None, None)
    );
}

/// Fichier réécrit en place (même taille, autre contenu) : au plus `REVALIDATE_EVERY` lectures
/// plus tard, toute lecture échoue (`Modified`), définitivement ; l'index reste consultable.
#[test]
fn modified_file_is_detected_within_the_revalidation_period() {
    let (_dir, path) = scratch_copy("h264_720p_closed");
    let src = TsAuSource::open(&path).unwrap();
    let mut buf = Vec::new();
    src.read_au(0, &mut buf).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut data = std::fs::read(&path).unwrap();
    data.reverse();
    std::fs::write(&path, &data).unwrap();
    let mut failed_at = None;
    for i in 0..=REVALIDATE_EVERY {
        if let Err(e) = src.read_au(1, &mut buf) {
            failed_at = Some(i);
            assert!(e.to_string().contains("modifié"), "{e}");
            break;
        }
    }
    let i = failed_at.expect("modification non détectée");
    assert!(i < REVALIDATE_EVERY, "{i}");
    // Définitif : même les AU déjà lues sont refusées.
    for d in [0, 1, 100] {
        assert!(src.read_au(d, &mut buf).is_err(), "AU {d}");
    }
    assert_eq!(src.len(), 250);
    assert!(src.entry(100).is_some());
}

/// Fichier remplacé (renommage d'un autre fichier par-dessus) : `Modified`.
#[test]
fn replaced_file_is_detected() {
    let (dir, path) = scratch_copy("hevc_720p_main10");
    let src = TsAuSource::open(&path).unwrap();
    let other = dir.path().join("other.ts");
    std::fs::copy(corpus_file("hevc_720p_main10"), &other).unwrap();
    std::fs::rename(&other, &path).unwrap();
    let mut buf = Vec::new();
    let failures = (0..=REVALIDATE_EVERY)
        .filter(|_| src.read_au(3, &mut buf).is_err())
        .count();
    assert!(failures >= 1);
}

/// Troncature après l'ouverture (Review Focus 2) : une AU au-delà de la coupure est une erreur
/// (`UnexpectedEof`), une AU avant la coupure reste lisible, y compris après revalidation.
#[test]
fn truncated_file_keeps_serving_the_aus_before_the_cut() {
    let (_dir, path) = scratch_copy("h264_720p_closed");
    let src = TsAuSource::open(&path).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    let mut early = Vec::new();
    src.read_au(2, &mut early).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(size / 2)
        .unwrap();
    let last = src.len() - 1;
    let mut buf = Vec::new();
    let err = src.read_au(last, &mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "{err}");
    for _ in 0..2 * REVALIDATE_EVERY {
        src.read_au(2, &mut buf).unwrap();
        assert_eq!(buf, early);
    }
    // Refus immédiat, sans relecture, une fois la troncature connue.
    let before = src.io_stats();
    let err = src.read_au(last - 1, &mut buf).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "{err}");
    assert_eq!(src.io_stats().syscalls, before.syscalls);
}

// --- Tests de corpus repris de l'ancienne source de test (`ma-testkit`) ------------------------

#[test]
fn ts_source_matches_ffprobe() {
    for name in ["h264_720p_closed", "hevc_720p_main10"] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let oracle: Vec<i64> = ffprobe_packets(&path)
            .iter()
            .map(|p| p.pts.unwrap())
            .collect();
        assert_eq!(src.len() as usize, oracle.len(), "{name} : nombre d'images");
        assert_eq!(src.len(), manifest(name).frames);
        let ours: Vec<i64> = (0..src.len()).map(|d| src.entry(d).unwrap().pts).collect();
        assert_eq!(ours, oracle, "{name} : PTS en ordre de décodage");
        let mut previous = i64::MIN;
        for p in 0..src.len() {
            let d = src.decode_idx_of(p).unwrap();
            let e = src.entry(d).unwrap();
            assert_eq!((e.decode_idx, e.pres_idx), (d, p));
            assert!(e.pts > previous, "{name} : PTS croissants en présentation");
            previous = e.pts;
        }
        assert_eq!(src.decode_idx_of(src.len()), None);
        assert_eq!(src.entry(src.len()), None);
    }
}

#[test]
fn codecs_sizes_and_counts_follow_the_manifest() {
    for (name, codec) in [
        ("h264_720p_closed", Codec::H264),
        ("h264_720p_high10", Codec::H264),
        ("h264_720p_recovery", Codec::H264),
        ("hevc_720p_main10", Codec::Hevc),
        ("hevc_720p_open_gop", Codec::Hevc),
        ("hevc_720p_open_gop_from_cra", Codec::Hevc),
        ("mpeg2_576i_closed", Codec::Mpeg2),
    ] {
        let src = open(name);
        assert_eq!(src.codec(), codec, "{name}");
        assert_eq!(src.len(), manifest(name).frames, "{name}");
    }
}

/// Lecture non vide, bornée, débutant par un code de début ; le découpeur de `ma-es`, rejoué sur
/// les AU relues mises bout à bout, retrouve les mêmes frontières et la même classification que
/// l'index (relecture de `read_au`, pas seulement de l'index).
#[test]
fn read_au_returns_start_coded_units() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_open_gop",
        "mpeg2_576i_closed",
    ] {
        let src = open(name);
        let mut buf = Vec::new();
        let mut framer = ma_es::framer(src.codec());
        let (mut aus, mut starts, mut at) = (Vec::new(), Vec::new(), 0u64);
        for d in 0..src.len() {
            src.read_au(d, &mut buf).unwrap();
            assert!(!buf.is_empty() && buf.len() <= MAX_AU_BYTES);
            assert!(
                buf.starts_with(&[0, 0, 1]) || buf.starts_with(&[0, 0, 0, 1]),
                "{name} AU {d} : code de début attendu"
            );
            starts.push(at);
            at += buf.len() as u64;
            framer.push(&buf, &mut aus);
        }
        framer.finish(&mut aus);
        assert_eq!(
            aus.iter().map(|a| a.start).collect::<Vec<_>>(),
            starts,
            "{name}"
        );
        for (d, a) in (0..src.len()).zip(&aus) {
            let e = src.entry(d).unwrap();
            assert_eq!(a.meta.picture, e.picture, "{name} AU {d}");
            assert_eq!(
                a.meta.rap != RapClass::None,
                e.rap != RapKind::None,
                "{name} AU {d}"
            );
        }
    }
}

/// Oracle ffprobe, par PTS : type d'image et ensemble des RAP (drapeau `K` des paquets).
#[test]
fn classification_matches_ffprobe_frame_by_frame() {
    for name in [
        "h264_720p_closed",
        "h264_720p_high10",
        "h264_720p_recovery",
        "hevc_720p_main10",
        "hevc_720p_open_gop",
        "mpeg2_576i_closed",
    ] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let types: HashMap<i64, char> = ffprobe_frames(&path)
            .iter()
            .map(|f| (f.pts, f.pict_type))
            .collect();
        let keys: HashSet<i64> = ffprobe_packets(&path)
            .iter()
            .filter(|p| p.key)
            .map(|p| p.pts.unwrap())
            .collect();
        assert_eq!(types.len(), src.len() as usize, "{name}");
        let gop = manifest(name).gop;
        let mut ours = HashSet::new();
        for d in 0..src.len() {
            let e = src.entry(d).unwrap();
            let expected = match types[&e.pts] {
                'I' => PictureType::I,
                'P' => PictureType::P,
                'B' => PictureType::B,
                other => panic!("{name} : pict_type {other}"),
            };
            assert_eq!(e.picture, expected, "{name} AU {d} (PTS {})", e.pts);
            if e.rap != RapKind::None {
                ours.insert(e.pts);
                assert_eq!(e.picture, PictureType::I, "{name} AU {d}");
                assert_eq!(
                    e.pres_idx % gop,
                    0,
                    "{name} : RAP en présentation {}",
                    e.pres_idx
                );
            }
        }
        assert_eq!(ours, keys, "{name} : ensemble des RAP");
        let expected = (0..src.len()).step_by(gop as usize).count();
        assert_eq!(ours.len(), expected, "{name} : nombre de RAP");
    }
}

#[test]
fn h264_open_gop_has_skippable_leading_pictures() {
    let src = open("h264_720p_recovery");
    let skippable: Vec<u32> = (0..src.len())
        .filter(|&d| src.entry(d).unwrap().leading == Leading::Skippable)
        .collect();
    assert!(
        !skippable.is_empty(),
        "des B doivent précéder l'I de reprise"
    );
    for d in skippable {
        let e = src.entry(d).unwrap();
        let rap = (0..d)
            .rev()
            .filter_map(|i| src.entry(i))
            .find(|r| r.rap != RapKind::None)
            .unwrap();
        assert!(matches!(rap.rap, RapKind::Recovery { .. }));
        assert!(e.pres_idx < rap.pres_idx, "AU {d}");
        let start = start_for(&src, d).unwrap();
        assert!(
            start < rap.decode_idx,
            "AU {d} : départ {start}, RAP {}",
            rap.decode_idx
        );
        assert_ne!(src.entry(start).unwrap().rap, RapKind::None);
    }
}

#[test]
fn open_gop_has_cra_and_rasl() {
    let src = open("hevc_720p_open_gop");
    let cras = rap_decode_indices(&src, |r| r == RapKind::Cra);
    assert!(cras.len() >= 2, "plusieurs CRA attendus : {cras:?}");
    assert_eq!(src.entry(0).unwrap().rap, RapKind::Idr);
    for &cra in &cras {
        let cra_pres = src.entry(cra).unwrap().pres_idx;
        let next_rap = (cra + 1..src.len())
            .find(|&d| src.entry(d).unwrap().rap != RapKind::None)
            .unwrap_or(src.len());
        let rasl = (cra + 1..next_rap)
            .filter(|&d| src.entry(d).unwrap().leading == Leading::Skippable)
            .count();
        assert!(rasl >= 1, "CRA {cra} sans RASL");
        for d in cra + 1..next_rap {
            let e = src.entry(d).unwrap();
            assert_eq!(e.leading != Leading::None, e.pres_idx < cra_pres);
        }
    }
    let rasl = (0..src.len())
        .find(|&d| src.entry(d).unwrap().leading == Leading::Skippable)
        .unwrap();
    let start = start_for(&src, rasl).unwrap();
    assert!(
        start < cras[0],
        "départ {start} avant le premier CRA {}",
        cras[0]
    );
}

#[test]
fn open_gop_from_cra_starts_on_a_cra_with_undecodable_rasl() {
    let src = open("hevc_720p_open_gop_from_cra");
    let first = src.entry(0).unwrap();
    assert_eq!(first.rap, RapKind::Cra, "le fichier commence sur un CRA");
    let next_rap = (1..src.len())
        .find(|&d| src.entry(d).unwrap().rap != RapKind::None)
        .unwrap();
    let rasl: Vec<u32> = (1..next_rap)
        .filter(|&d| src.entry(d).unwrap().leading == Leading::Skippable)
        .collect();
    assert!(!rasl.is_empty(), "RASL du premier CRA attendues");
    for d in rasl {
        assert_eq!(start_for(&src, d), Err(NotDecodable::LeadingOfFirstRap));
    }
}

#[test]
fn recovery_file_has_recovery_points_and_single_idr() {
    let src = open("h264_720p_recovery");
    let idr = rap_decode_indices(&src, |r| r == RapKind::Idr);
    let rec = rap_decode_indices(&src, |r| matches!(r, RapKind::Recovery { .. }));
    assert_eq!(idr, [0], "une seule IDR, la première");
    assert!(rec.len() >= 2, "points de reprise attendus : {rec:?}");
    for d in rec {
        assert_eq!(src.entry(d).unwrap().picture, PictureType::I);
    }
    let closed = open("h264_720p_closed");
    assert_eq!(
        rap_decode_indices(&closed, |r| matches!(r, RapKind::Recovery { .. })),
        Vec::<u32>::new()
    );
    assert!(rap_decode_indices(&closed, |r| r == RapKind::Idr).len() >= 2);
}

#[test]
fn mpeg2_gop_flags() {
    let src = open("mpeg2_576i_closed");
    let raps = rap_decode_indices(&src, |r| r != RapKind::None);
    assert!(raps.len() >= 2);
    for d in raps {
        let e = src.entry(d).unwrap();
        assert_eq!(e.rap, RapKind::Intra { closed_gop: true }, "AU {d}");
        assert_eq!(e.picture, PictureType::I);
    }
    assert!((0..src.len()).all(|d| src.entry(d).unwrap().leading != Leading::Skippable));
}

#[test]
fn cut_file_starts_mid_gop() {
    let src = open("hevc_720p_cut");
    let frames = manifest("hevc_720p_cut").frames;
    assert!((1..=frames).contains(&src.len()), "{} images", src.len());
    let first_rap = rap_decode_indices(&src, |r| r != RapKind::None)[0];
    assert!(first_rap > 0, "les premières images précèdent tout RAP");
    for d in 0..first_rap {
        let e = src.entry(d).unwrap();
        assert_eq!(e.rap, RapKind::None);
        assert_eq!(
            start_for(&src, d),
            Err(NotDecodable::BeforeFirstRap),
            "AU {d}"
        );
    }
    assert_eq!(start_for(&src, first_rap), Ok(first_rap));
}

#[test]
fn corrupting_source_zeroes_only_the_listed_aus() {
    let plain = open("h264_720p_closed");
    let corrupt = CorruptingSource {
        inner: open("h264_720p_closed"),
        corrupt: HashSet::from([5]),
    };
    let (mut a, mut b) = (Vec::new(), Vec::new());
    assert_eq!(
        (corrupt.codec(), corrupt.len()),
        (plain.codec(), plain.len())
    );
    for d in [4, 5, 6] {
        plain.read_au(d, &mut a).unwrap();
        corrupt.read_au(d, &mut b).unwrap();
        assert_eq!(a.len(), b.len());
        if d == 5 {
            assert_ne!(a, b);
            assert!(b.iter().rev().take(100).all(|&x| x == 0));
        } else {
            assert_eq!(a, b);
        }
    }
    assert_eq!(corrupt.decode_idx_of(3), plain.decode_idx_of(3));
    assert_eq!(corrupt.entry(5), plain.entry(5));
}

#[test]
fn big_file_is_indexed_when_present() {
    let Some(path) = corpus_file_if_present("uhd_big") else {
        eprintln!("uhd_big absent : test non exécuté (cargo xtask corpus gen --big)");
        return;
    };
    let src = TsAuSource::open(&path).unwrap();
    let per_loop = manifest("uhd_big").frames;
    // Bouclé jusqu'à dépasser 4 Go : la dernière boucle peut être incomplète.
    assert!(src.len() >= per_loop * 2, "{} images", src.len());
    let mut buf = Vec::new();
    src.read_au(src.len() - 1, &mut buf).unwrap();
    assert_ne!(buf.len(), 0);
}
