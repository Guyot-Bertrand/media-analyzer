//! Mode de décodage réel, contrat `Decoder` sur le décodeur réel, AU corrompues ou trop grandes.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::{band, check, navigator, never, quiet_ffmpeg, require_vpp, source, whole};
use ma_decode::{
    AuSource, Codec, ConvertPath, DecodeError, DecodeErrorKind, DecodeEvent, DecodeMode, Decoder,
    DecoderFactory, FrameError, FrameService, ModePref, NavConfig, Navigator, PictureType,
    PixelLayout, PrefetchConfig, Received, Rect, Size, Viewport,
};
use ma_ffmpeg::{FfmpegFactory, MAX_AU_BYTES, vaapi_available};
use ma_testkit::{CorruptingSource, corpus_file, manifest, require_vaapi};
use ma_ts::TsAuSource;

fn factory() -> FfmpegFactory {
    quiet_ffmpeg();
    FfmpegFactory { render_node: None }
}

fn open(codec: Codec, pref: ModePref) -> Box<dyn Decoder> {
    factory().open(codec, pref).unwrap()
}

/// Envoie les AU de `src` à partir de 0 et rend le premier résultat de `receive` qui n'est pas
/// `NeedInput`.
fn first_output(
    d: &mut dyn Decoder,
    src: &TsAuSource,
    want: &dyn Fn(i64) -> Option<Viewport>,
) -> Received {
    let mut buf = Vec::new();
    for i in 0..src.len() {
        src.read_au(i, &mut buf).unwrap();
        d.send(src.entry(i).unwrap().pts, &buf).unwrap();
        match d.receive(want).unwrap() {
            Received::NeedInput => {}
            r => return r,
        }
    }
    d.send_eof().unwrap();
    d.receive(want).unwrap()
}

/// Fabrique qui note, pour chaque décodeur ouvert, la préférence demandée et la latence
/// annoncée ([`Decoder::latency`] : images de retard, soit le nombre de threads d'images).
struct Recording {
    inner: FfmpegFactory,
    opened: Mutex<Vec<(ModePref, u32)>>,
}

impl DecoderFactory for Recording {
    fn open(&self, codec: Codec, pref: ModePref) -> Result<Box<dyn Decoder>, DecodeError> {
        let d = self.inner.open(codec, pref)?;
        self.opened.lock().unwrap().push((pref, d.latency()));
        Ok(d)
    }
}

/// Profil refusé par VA-API (H.264 High 10) : `FFmpeg` repasse en silence en logiciel sur un
/// seul thread ; le navigateur rouvre une fois un décodeur logiciel multi-thread, sans
/// `FellBackToSoftware`.
#[ma_trace::covers("PERF.HW.01")]
#[test]
fn unsupported_profile_reopens_multithread() {
    if !require_vaapi() {
        return;
    }
    let name = "h264_720p_high10";
    let factory = Arc::new(Recording {
        inner: factory(),
        opened: Mutex::new(Vec::new()),
    });
    let mut nav = Navigator::new(
        source(name),
        factory.clone(),
        ModePref::Auto,
        NavConfig::default(),
    )
    .unwrap();
    assert_eq!(nav.mode(), DecodeMode::Unknown, "périphérique attaché");
    let vp = whole(manifest(name).dims());
    for p in [0, 1, 30, 29] {
        check(&mut nav, name, p, vp, 0);
    }
    assert_eq!(nav.mode(), DecodeMode::Software);
    assert_eq!(nav.stats().software_reopens, 1);
    assert_eq!(nav.stats().decoder.full_res_transfers, 0);
    let events = nav.take_events();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. })),
        "{events:?}"
    );
    let opened = factory.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 2, "{opened:?}");
    assert_eq!(opened[0], (ModePref::Auto, 1), "VA-API : un seul thread");
    let (pref, latency) = opened[1];
    assert_eq!(pref, ModePref::SoftwareOnly);
    if std::thread::available_parallelism().is_ok_and(|n| n.get() > 1) {
        assert!(
            latency > 1,
            "décodeur rouvert multi-thread : latence {latency}"
        );
    }
}

/// Sans périphérique, le décodeur est logiciel dès l'ouverture : aucune erreur avant la 1re
/// image ne peut passer pour une panne matérielle (pas de `FellBackToSoftware` parasite).
#[ma_trace::covers("PERF.HW.01")]
#[test]
fn no_device_reports_software_from_open() {
    quiet_ffmpeg();
    let f = FfmpegFactory {
        render_node: Some("/dev/dri/absent".into()),
    };
    for codec in [Codec::H264, Codec::Hevc, Codec::Mpeg2] {
        let d = f.open_decoder(codec, ModePref::Auto).unwrap();
        assert!(d.hw_unavailable().is_some());
        assert_eq!(d.mode(), DecodeMode::Software, "{codec:?}");
        assert!(!d.silent_software());
    }
    let name = "h264_720p_closed";
    let mut nav = Navigator::new(
        source(name),
        Arc::new(f),
        ModePref::Auto,
        NavConfig::default(),
    )
    .unwrap();
    assert_eq!(nav.mode(), DecodeMode::Software, "avant toute image");
    let vp = band(manifest(name).dims());
    for p in [0, 1, 30, 29, 100] {
        check(&mut nav, name, p, vp, 0);
    }
    assert_eq!(nav.mode(), DecodeMode::Software);
    assert_eq!(nav.stats().software_reopens, 0);
    let events = nav.take_events();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. })),
        "{events:?}"
    );
}

#[test]
fn mode_is_unknown_before_the_first_frame_then_detected() {
    let src = source("hevc_720p_main10");
    let mut prefs = vec![(ModePref::SoftwareOnly, DecodeMode::Software)];
    if require_vaapi() {
        prefs.push((ModePref::Auto, DecodeMode::Hardware));
    }
    for (pref, mode) in prefs {
        let mut d = open(Codec::Hevc, pref);
        // `Unknown` : périphérique attaché, mode pas encore confirmé ; sans périphérique, logiciel
        // dès l'ouverture.
        let at_open = if pref == ModePref::Auto {
            DecodeMode::Unknown
        } else {
            DecodeMode::Software
        };
        assert_eq!(d.mode(), at_open);
        assert!(d.latency() >= 1);
        if pref == ModePref::Auto {
            assert_eq!(d.latency(), 1, "un seul thread en VA-API");
        }
        let r = first_output(&mut *d, &src, &|_| None);
        assert!(matches!(r, Received::Discarded { .. }), "{r:?}");
        assert_eq!(d.mode(), mode);
        // Image jetée : ni conversion ni transfert.
        assert_eq!(d.stats().converted, 0);
        assert_eq!(d.stats().full_res_transfers, 0);
    }
}

#[test]
fn native_layout_and_byte_strides() {
    let full = Size { w: 1280, h: 720 };
    let region = Rect {
        x: 100,
        y: 50,
        w: 301,
        h: 101,
    };
    // Région impaire : normalisée en 302 × 102 (origine et fin paires), toujours en 1:1.
    let vp = Viewport {
        region,
        out: Size { w: 302, h: 102 },
    };
    let mut cases = vec![
        (
            "h264_720p_closed",
            ModePref::SoftwareOnly,
            PixelLayout::Planar8,
        ),
        (
            "hevc_720p_main10",
            ModePref::SoftwareOnly,
            PixelLayout::Planar10,
        ),
    ];
    if require_vpp() {
        cases.push(("h264_720p_closed", ModePref::Auto, PixelLayout::SemiPlanar8));
        cases.push((
            "hevc_720p_main10",
            ModePref::Auto,
            PixelLayout::SemiPlanar10,
        ));
    }
    for (name, pref, layout) in cases {
        let src = source(name);
        let mut d = open(src.codec(), pref);
        let Received::Frame {
            image,
            full: got,
            corrupt,
            ..
        } = first_output(&mut *d, &src, &|_| Some(vp))
        else {
            panic!("{name} : image attendue");
        };
        assert_eq!(got, full);
        assert!(!corrupt);
        // Région normalisée : origine paire, fin paire.
        let size = Size { w: 302, h: 102 };
        assert_eq!((image.layout, image.size), (layout, size), "{name}");
        let bps = match layout {
            PixelLayout::Planar8 | PixelLayout::SemiPlanar8 => 1,
            PixelLayout::Planar10 | PixelLayout::SemiPlanar10 => 2,
        };
        let expected: Vec<(u32, u32)> = match layout {
            PixelLayout::Planar8 | PixelLayout::Planar10 => {
                vec![(302 * bps, 102), (151 * bps, 51), (151 * bps, 51)]
            }
            PixelLayout::SemiPlanar8 | PixelLayout::SemiPlanar10 => {
                vec![(302 * bps, 102), (302 * bps, 51)]
            }
        };
        let got: Vec<(u32, u32)> = image
            .strides
            .iter()
            .zip(&image.planes)
            .map(|(&s, p)| (s, u32::try_from(p.len()).unwrap() / s))
            .collect();
        assert_eq!(got, expected, "{name} {pref:?}");
        assert_eq!(d.stats().converted, 1);
        // Région plus petite que l'image : VPP en VA-API, aucun transfert pleine résolution.
        assert_eq!(d.stats().full_res_transfers, 0);
        let path = if pref == ModePref::Auto {
            ConvertPath::VaapiVpp
        } else {
            ConvertPath::Software
        };
        assert_eq!(d.stats().path, path);
    }
}

/// Viewport réduit : image rendue, sans transfert pleine résolution ; viewport hors de l'image :
/// `Unsupported` (R7), sans transfert.
#[test]
fn reduced_viewport_is_rendered_and_empty_is_unsupported() {
    let src = source("h264_720p_closed");
    let full = Size { w: 1280, h: 720 };
    let mut prefs = vec![ModePref::SoftwareOnly];
    if require_vpp() {
        prefs.push(ModePref::Auto);
    }
    let reduced = Viewport::fit(full, Size { w: 640, h: 360 });
    let outside = Viewport::region_1to1(
        full,
        Rect {
            x: 2000,
            y: 0,
            w: 10,
            h: 10,
        },
    );
    for pref in prefs {
        for vp in [reduced, outside] {
            let mut d = open(Codec::H264, pref);
            let mut buf = Vec::new();
            src.read_au(0, &mut buf).unwrap();
            d.send(src.entry(0).unwrap().pts, &buf).unwrap();
            d.send_eof().unwrap();
            let r = d.receive(&|_| Some(vp));
            if vp == reduced {
                let Ok(Received::Frame { image, .. }) = r else {
                    panic!("{pref:?} : image attendue, {r:?}");
                };
                assert_eq!(image.size, Size { w: 640, h: 360 });
            } else {
                let err = r.unwrap_err();
                assert_eq!(err.kind, DecodeErrorKind::Unsupported, "{pref:?} {err}");
                assert!(err.detail.contains("viewport"), "{err}");
            }
            assert_eq!(d.stats().full_res_transfers, 0, "aucun transfert inutile");
        }
    }
}

#[test]
fn eof_drain_and_reset() {
    let src = source("h264_720p_closed");
    let mut d = open(Codec::H264, ModePref::SoftwareOnly);
    assert_eq!(d.receive(&|_| None).unwrap(), Received::NeedInput);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    for i in 0..5 {
        src.read_au(i, &mut buf).unwrap();
        d.send(src.entry(i).unwrap().pts, &buf).unwrap();
        while let Received::Discarded { pts } = d.receive(&|_| None).unwrap() {
            out.push(pts);
        }
    }
    d.send_eof().unwrap();
    d.send_eof().unwrap(); // sans effet
    loop {
        match d.receive(&|_| None).unwrap() {
            Received::Discarded { pts } => out.push(pts),
            Received::Drained => break,
            r => panic!("{r:?}"),
        }
    }
    assert_eq!(d.receive(&|_| None).unwrap(), Received::Drained);
    // Les 5 premières AU d'un GOP fermé : 5 images, par PTS croissants.
    assert_eq!(out.len(), 5);
    assert!(out.windows(2).all(|w| w[0] < w[1]));
    // Après `reset`, le décodeur repart d'un RAP.
    d.reset();
    assert_eq!(d.receive(&|_| None).unwrap(), Received::NeedInput);
    let r = first_output(&mut *d, &src, &|_| None);
    assert!(matches!(r, Received::Discarded { .. }), "{r:?}");
}

#[test]
fn every_codec_opens_in_software() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Mpeg2] {
        let d = open(codec, ModePref::SoftwareOnly);
        assert_eq!(d.mode(), DecodeMode::Software);
        assert!(!d.silent_software());
    }
}

#[test]
fn vaapi_detection() {
    quiet_ffmpeg();
    assert!(!vaapi_available(Some(Path::new("/nonexistent/renderD999"))));
    assert!(!vaapi_available(Some(Path::new("bad\0path"))));
    // Une fabrique dont le périphérique est absent décode en logiciel.
    let f = FfmpegFactory {
        render_node: Some("/nonexistent/renderD999".into()),
    };
    let src = source("h264_720p_closed");
    let mut d = f.open_decoder(Codec::H264, ModePref::Auto).unwrap();
    let cause = d.hw_unavailable().unwrap();
    assert!(cause.contains("/nonexistent/renderD999"), "{cause}");
    first_output(&mut d, &src, &|_| None);
    assert_eq!(d.mode(), DecodeMode::Software);
    let bad_path = FfmpegFactory {
        render_node: Some("bad\0path".into()),
    };
    let d = bad_path.open_decoder(Codec::H264, ModePref::Auto).unwrap();
    assert!(d.hw_unavailable().unwrap().contains("invalide"));
    // Logiciel demandé : aucun diagnostic.
    let d = f.open_decoder(Codec::H264, ModePref::SoftwareOnly).unwrap();
    assert_eq!(d.hw_unavailable(), None);
    if require_vaapi() {
        assert!(vaapi_available(None));
        assert!(vaapi_available(Some(Path::new("/dev/dri/renderD128"))));
        let d = factory().open_decoder(Codec::H264, ModePref::Auto).unwrap();
        assert_eq!(d.hw_unavailable(), None);
    }
}

#[test]
fn oversized_au_rejected() {
    assert_eq!(MAX_AU_BYTES, ma_ts::MAX_AU_BYTES);
    let mut d = open(Codec::H264, ModePref::SoftwareOnly);
    let big = vec![0u8; MAX_AU_BYTES + 1];
    let err = d.send(0, &big).unwrap_err();
    assert_eq!(err.kind, DecodeErrorKind::InvalidData);
    assert!(err.detail.contains("trop grande"), "{err}");
    // Le décodeur reste utilisable.
    assert_eq!(d.receive(&|_| None).unwrap(), Received::NeedInput);
}

#[test]
fn empty_au_rejected() {
    let src = source("h264_720p_closed");
    let mut d = open(Codec::H264, ModePref::SoftwareOnly);
    let err = d.send(0, &[]).unwrap_err();
    assert_eq!(err.kind, DecodeErrorKind::InvalidData);
    assert!(err.detail.contains("AU vide"), "{err}");
    // Le décodeur reste utilisable : une image valide décode normalement.
    let r = first_output(&mut *d, &src, &|_| None);
    assert!(matches!(r, Received::Discarded { .. }), "{r:?}");
}

/// P au milieu d'un GOP (au moins 10 images de décodage après le RAP précédent).
fn mid_gop_p(src: &TsAuSource) -> u32 {
    (10..src.len())
        .find(|&d| {
            let e = src.entry(d).unwrap();
            e.picture == PictureType::P
                && (d - 8..d).all(|i| src.entry(i).unwrap().rap == ma_decode::RapKind::None)
        })
        .unwrap()
}

#[test]
fn corrupted_au_real_decoder() {
    for name in ["h264_720p_closed", "hevc_720p_main10"] {
        let inner = TsAuSource::open(&corpus_file(name)).unwrap();
        let bad = mid_gop_p(&inner);
        let bad_pres = inner.entry(bad).unwrap().pres_idx;
        let len = inner.len();
        let src = Arc::new(CorruptingSource {
            inner,
            corrupt: HashSet::from([bad]),
        });
        let m = manifest(name);
        let vp = band(m.dims());
        let mut nav = navigator(src.clone(), ModePref::SoftwareOnly);
        // Images décodées avant la P corrompue : correctes (les B qui la précèdent en
        // présentation la suivent en décodage et en dépendent).
        for d in 0..bad {
            check(&mut nav, name, src.entry(d).unwrap().pres_idx, vp, 0);
        }
        // Les suivantes du même GOP et du suivant : livrées ou refusées, sans panique.
        let (mut delivered, mut refused) = (0, 0);
        for p in bad_pres..(bad_pres + m.gop).min(len) {
            match nav.frame(p, vp, &never) {
                Ok(f) => {
                    assert_eq!(f.pres_idx, p);
                    delivered += 1;
                }
                Err(FrameError::NotDecodable { .. } | FrameError::Decoder(_)) => refused += 1,
                Err(e) => panic!("{name} : image {p} : {e}"),
            }
        }
        let events = nav.take_events();
        assert!(delivered + refused > 0, "{name} : AU {bad}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, DecodeEvent::CorruptAu { .. })),
            "{name} : événement CorruptAu attendu ({events:?})"
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. }))
        );
        // Le GOP suivant redevient correct.
        let next_gop = (bad_pres / m.gop + 1) * m.gop;
        check(&mut nav, name, next_gop + 1, vp, 0);
    }
}

/// AU corrompue (P au milieu d'un GOP de `hevc_720p_main10`) décodée en VA-API : aucune bascule
/// en logiciel, le décodage reste matériel, et les images décodées avant elle sont correctes.
#[test]
fn corrupted_au_vaapi_does_not_fall_back() {
    if !require_vaapi() {
        return;
    }
    let name = "hevc_720p_main10";
    let inner = TsAuSource::open(&corpus_file(name)).unwrap();
    let bad = mid_gop_p(&inner);
    let bad_pres = inner.entry(bad).unwrap().pres_idx;
    let len = inner.len();
    let src = Arc::new(CorruptingSource {
        inner,
        corrupt: HashSet::from([bad]),
    });
    let m = manifest(name);
    let vp = band(m.dims());
    let mut nav = navigator(src.clone(), ModePref::Auto);
    for d in 0..bad {
        check(&mut nav, name, src.entry(d).unwrap().pres_idx, vp, 0);
    }
    assert_eq!(nav.mode(), DecodeMode::Hardware);
    for p in bad_pres..(bad_pres + m.gop).min(len) {
        match nav.frame(p, vp, &never) {
            Ok(f) => assert_eq!(f.pres_idx, p),
            Err(FrameError::NotDecodable { .. } | FrameError::Decoder(_)) => {}
            Err(e) => panic!("{name} : image {p} : {e}"),
        }
    }
    let events = nav.take_events();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. })),
        "{events:?}"
    );
    assert_eq!(nav.mode(), DecodeMode::Hardware);
    // Le GOP suivant redevient correct, toujours en matériel.
    let next_gop = (bad_pres / m.gop + 1) * m.gop;
    check(&mut nav, name, next_gop + 1, vp, 0);
    assert_eq!(nav.mode(), DecodeMode::Hardware);
    assert_eq!(nav.take_events(), vec![]);
}

/// Review Focus 2 : fichier tronqué à 50 % pendant la navigation (enregistrement coupé, NFS).
/// Une image au-delà de la coupure est une erreur `Io`, une image avant reste servie, et le
/// service répond encore.
#[ma_trace::covers("PERF.NET.01")]
#[test]
fn truncation_during_navigation_is_an_error_not_a_crash() {
    quiet_ffmpeg();
    let name = "h264_720p_closed";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copie.ts");
    std::fs::copy(corpus_file(name), &path).unwrap();
    let src = Arc::new(TsAuSource::open(&path).unwrap());
    let len = src.len();
    let vp = band(manifest(name).dims());
    let service = FrameService::start(
        navigator(src, ModePref::SoftwareOnly),
        PrefetchConfig::default(),
    );
    let code = |f: &ma_decode::DecodedFrame| common::code_of(f, vp);
    let f = service.get(3, vp).unwrap();
    assert_eq!(code(&f), Some(3));
    let size = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(size / 2)
        .unwrap();
    // Au-delà de la coupure (dernier GOP) : erreur d'E/S, sans panique.
    let beyond = len - 10;
    match service.get(beyond, vp).map(|f| f.pres_idx) {
        Err(FrameError::Io(e)) => assert_ne!(e.len(), 0),
        other => panic!("image {beyond} : FrameError::Io attendu, obtenu {other:?}"),
    }
    // Avant la coupure : servie, et le service répond toujours.
    for p in [30, 31, 12] {
        let f = service
            .get(p, vp)
            .unwrap_or_else(|e| panic!("image {p} : {e}"));
        assert_eq!(code(&f), Some(p), "image {p}");
    }
    assert!(matches!(
        service.get(beyond + 1, vp),
        Err(FrameError::Io(_))
    ));
    assert_eq!(code(&service.get(50, vp).unwrap()), Some(50));
}
