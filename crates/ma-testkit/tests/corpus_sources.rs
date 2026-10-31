//! Tests sur le corpus généré (`cargo xtask corpus gen`) : la source TS est comparée à `ffprobe`
//! (oracle de test) et le numéro incrusté à des images décodées par `ffmpeg`.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use ma_decode::{
    AuSource, Codec, Leading, NotDecodable, PictureType, PixelLayout, Planes, RapKind, Rect, Size,
    start_for,
};
use ma_testkit::{
    CorruptingSource, MAX_AU_BYTES, TsAuSource, classify, corpus_file, corpus_file_if_present,
    manifest, read_frame_code,
};

fn open(name: &str) -> TsAuSource {
    TsAuSource::open(&corpus_file(name)).unwrap()
}

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} : {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// PTS de chaque paquet vidéo, dans l'ordre du fichier (ordre de décodage), d'après ffprobe.
fn ffprobe_pts(path: &Path) -> Vec<i64> {
    let out = run(Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts",
            "-of",
            "csv=p=0",
        ])
        .arg(path));
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| l.trim().trim_end_matches(',').parse().unwrap())
        .collect()
}

fn rap_decode_indices(src: &TsAuSource, kind: impl Fn(RapKind) -> bool) -> Vec<u32> {
    (0..src.len())
        .filter(|&d| kind(src.entry(d).unwrap().rap))
        .collect()
}

#[test]
fn ts_source_matches_ffprobe() {
    for name in ["h264_720p_closed", "hevc_720p_main10"] {
        let path = corpus_file(name);
        let src = TsAuSource::open(&path).unwrap();
        let oracle = ffprobe_pts(&path);
        assert_eq!(src.len() as usize, oracle.len(), "{name} : nombre d'images");
        assert_eq!(src.len(), manifest(name).frames);
        let ours: Vec<i64> = (0..src.len()).map(|d| src.entry(d).unwrap().pts).collect();
        assert_eq!(ours, oracle, "{name} : PTS en ordre de décodage");
        // L'ordre de présentation est l'ordre des PTS, et les deux index se correspondent.
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
        ("mpeg2_576i_closed", Codec::Mpeg2),
    ] {
        let src = open(name);
        assert_eq!(src.codec(), codec, "{name}");
        assert_eq!(src.len(), manifest(name).frames, "{name}");
    }
}

#[test]
fn au_bytes_are_consistent_with_the_index() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_open_gop",
        "mpeg2_576i_closed",
    ] {
        let src = open(name);
        let mut buf = Vec::new();
        for d in 0..src.len() {
            src.read_au(d, &mut buf).unwrap();
            assert!(!buf.is_empty() && buf.len() <= MAX_AU_BYTES);
            assert!(
                buf.starts_with(&[0, 0, 1]) || buf.starts_with(&[0, 0, 0, 1]),
                "{name} AU {d} : code de début attendu"
            );
            let e = src.entry(d).unwrap();
            let (picture, rap, _) = classify(src.codec(), &buf);
            assert_eq!((picture, rap), (e.picture, e.rap), "{name} AU {d}");
        }
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
        // Au moins une RASL suit chaque CRA en décodage et le précède en présentation (x265 n'en
        // produit qu'une ; on n'en exige pas davantage).
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
    // Une image Skippable n'est atteignable qu'en partant du RAP précédent le CRA.
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
fn recovery_file_has_recovery_points_and_single_idr() {
    let src = open("h264_720p_recovery");
    let idr = rap_decode_indices(&src, |r| r == RapKind::Idr);
    let rec = rap_decode_indices(&src, |r| matches!(r, RapKind::Recovery { .. }));
    assert_eq!(idr, [0], "une seule IDR, la première");
    assert!(rec.len() >= 2, "points de reprise attendus : {rec:?}");
    for d in rec {
        assert_eq!(src.entry(d).unwrap().picture, PictureType::I);
    }
    // Les fichiers à IDR n'ont aucun point de reprise.
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
    // GOP fermé : aucune image de tête à ignorer.
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
        eprintln!("uhd_big absent (généré avec --big seulement) : test non exécuté");
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

// --- Numéro incrusté, comparé à des images décodées par ffmpeg --------------------------------

/// Image `n` de `name` décodée par ffmpeg dans `pix_fmt`, à la taille `out` (mise à l'échelle).
fn ffmpeg_frame(name: &str, n: u32, pix_fmt: &str, out: Size) -> Vec<u8> {
    let vf = format!("select=eq(n\\,{n}),scale={}:{}", out.w, out.h);
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(corpus_file(name))
        .args([
            "-vf",
            &vf,
            "-frames:v",
            "1",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pix_fmt,
            "-",
        ]))
}

fn luma_planes(layout: PixelLayout, raw: &[u8], size: Size, bytes: u32) -> Planes {
    let luma_len = (size.w * size.h * bytes) as usize;
    Planes {
        layout,
        size,
        planes: vec![raw[..luma_len].to_vec()],
        strides: vec![size.w * bytes],
    }
}

#[test]
fn frame_code_reads_real_decoded_frames() {
    let full = Size { w: 1280, h: 720 };
    let whole = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 720,
    };
    for (name, pix_fmt, layout, bytes) in [
        ("h264_720p_closed", "gray", PixelLayout::Planar8, 1),
        ("h264_720p_closed", "nv12", PixelLayout::SemiPlanar8, 1),
        ("hevc_720p_main10", "yuv420p10le", PixelLayout::Planar10, 2),
        ("hevc_720p_main10", "p010le", PixelLayout::SemiPlanar10, 2),
    ] {
        for n in [0, 1, 137] {
            // Pleine taille, puis réduite de moitié et au quart (zoom « Ajusté »).
            for out in [full, Size { w: 640, h: 360 }, Size { w: 320, h: 180 }] {
                let raw = ffmpeg_frame(name, n, pix_fmt, out);
                let img = luma_planes(layout, &raw, out, bytes);
                assert_eq!(
                    read_frame_code(&img, whole, full),
                    Some(n),
                    "{name} {pix_fmt} image {n} en {}x{}",
                    out.w,
                    out.h
                );
            }
        }
    }
}

#[test]
fn frame_code_reads_a_one_to_one_region() {
    let full = Size { w: 1280, h: 720 };
    // Région 1:1 contenant la bande : bande et un peu de l'image.
    let region = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 200,
    };
    let raw = ffmpeg_frame("h264_720p_closed", 42, "gray", full);
    let rows = (region.w * region.h) as usize;
    let img = Planes {
        layout: PixelLayout::Planar8,
        size: Size {
            w: region.w,
            h: region.h,
        },
        planes: vec![raw[..rows].to_vec()],
        strides: vec![region.w],
    };
    assert_eq!(read_frame_code(&img, region, full), Some(42));
    // Une région qui ne contient pas la bande ne donne rien.
    let below = Rect {
        x: 0,
        y: 300,
        w: 640,
        h: 200,
    };
    assert_eq!(read_frame_code(&img, below, full), None);
}
