//! Réduction (zoom Ajusté) et région visible (zoom ≥ 100 %) : VPP VA-API (`scale_vaapi`) en
//! décodage matériel, libswscale en logiciel. Seule l'image affichée est transférée en RAM.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{quiet_ffmpeg, require_vpp, vpp_gate, vpp_required};
use ma_decode::{
    AuSource, ConvertPath, DecodeMode, Decoder, ModePref, PixelLayout, Planes, Received, Rect,
    Size, Viewport,
};
use ma_ffmpeg::{FfmpegDecoder, FfmpegFactory};
use ma_testkit::{
    TsAuSource, corpus_file, corpus_file_if_present, manifest, read_frame_code, require_vaapi,
};

const MAIN10: &str = "hevc_720p_main10";

/// Région du test d'exactitude (256, 128, 640×360).
const REGION: Rect = Rect {
    x: 256,
    y: 128,
    w: 640,
    h: 360,
};

fn open(src: &TsAuSource, pref: ModePref) -> FfmpegDecoder {
    quiet_ffmpeg();
    FfmpegFactory { render_node: None }
        .open_decoder(src.codec(), pref)
        .unwrap()
}

/// Image rendue : numéro de présentation, plans et taille de l'image entière.
struct Out {
    pres: u32,
    image: Planes,
    full: Size,
}

/// Décode `src` depuis l'AU 0 et rend les `n` premières images sorties, chacune convertie pour le
/// viewport `vp_of(i)` (`i` : rang de l'image sortie).
fn decode(
    d: &mut FfmpegDecoder,
    src: &TsAuSource,
    n: usize,
    vp_of: &dyn Fn(usize) -> Viewport,
) -> Vec<Out> {
    let pres: HashMap<i64, u32> = (0..src.len())
        .map(|i| {
            let e = src.entry(i).unwrap();
            (e.pts, e.pres_idx)
        })
        .collect();
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut next = 0;
    let mut eof = false;
    while out.len() < n {
        let i = out.len();
        let want = |_| Some(vp_of(i));
        match d.receive(&want).unwrap() {
            Received::Frame {
                pts, image, full, ..
            } => out.push(Out {
                pres: pres[&pts],
                image,
                full,
            }),
            Received::NeedInput if next < src.len() => {
                src.read_au(next, &mut buf).unwrap();
                d.send(src.entry(next).unwrap().pts, &buf).unwrap();
                next += 1;
            }
            Received::NeedInput if !eof => {
                d.send_eof().unwrap();
                eof = true;
            }
            r => panic!("sortie inattendue : {r:?}"),
        }
    }
    out
}

fn whole_of(full: Size) -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: full.w,
        h: full.h,
    }
}

/// Vérifie le numéro incrusté (`pres mod modulo`) de chaque image rendue pour la région `region`.
fn check_codes(frames: &[Out], region: Rect, modulo: u32, what: &str) {
    for f in frames {
        assert_eq!(
            read_frame_code(&f.image, region, f.full),
            Some(f.pres % modulo),
            "{what} : numéro de l'image {}",
            f.pres
        );
    }
}

/// Recadrage de `region` dans une image entière en plans compactés (4:2:0, origine paire).
fn crop(img: &Planes, region: Rect) -> Planes {
    let bps = match img.layout {
        PixelLayout::Planar8 | PixelLayout::SemiPlanar8 => 1,
        PixelLayout::Planar10 | PixelLayout::SemiPlanar10 => 2,
    };
    let (x, y, w, h) = (
        region.x as usize,
        region.y as usize,
        region.w as usize,
        region.h as usize,
    );
    // (origine en octets, première ligne, octets par ligne, lignes) de chaque plan.
    let dims: Vec<(usize, usize, usize, usize)> = match img.layout {
        PixelLayout::Planar8 | PixelLayout::Planar10 => vec![
            (x * bps, y, w * bps, h),
            (x / 2 * bps, y / 2, w.div_ceil(2) * bps, h.div_ceil(2)),
            (x / 2 * bps, y / 2, w.div_ceil(2) * bps, h.div_ceil(2)),
        ],
        PixelLayout::SemiPlanar8 | PixelLayout::SemiPlanar10 => vec![
            (x * bps, y, w * bps, h),
            (x * bps, y / 2, w.div_ceil(2) * 2 * bps, h.div_ceil(2)),
        ],
    };
    let mut planes = Vec::new();
    for (i, &(ox, oy, row, rows)) in dims.iter().enumerate() {
        let stride = img.strides[i] as usize;
        let mut plane = Vec::with_capacity(row * rows);
        for r in oy..oy + rows {
            plane.extend_from_slice(&img.planes[i][r * stride + ox..r * stride + ox + row]);
        }
        planes.push(plane);
    }
    Planes {
        layout: img.layout,
        size: Size {
            w: region.w,
            h: region.h,
        },
        planes,
        strides: dims
            .iter()
            .map(|&(_, _, row, _)| u32::try_from(row).unwrap())
            .collect(),
    }
}

/// Fichier 3840×2160 : `uhd_big` s'il est généré, sinon un court TS HEVC Main 10 généré à la volée
/// par la CLI `ffmpeg` (même incrustation que le corpus : 16 blocs, bande de `H/24`).
fn uhd_file() -> (PathBuf, u32) {
    if let Some(p) = corpus_file_if_present("uhd_big") {
        return (p, manifest("uhd_big").code_modulo);
    }
    // Nom stable : généré une fois, partagé par les tests (un processus par test avec nextest).
    let path = std::env::temp_dir().join("ma-ffmpeg-uhd-3840x2160-v1.ts");
    if !path.is_file() {
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        generate_uhd(&tmp);
        std::fs::rename(&tmp, &path).unwrap();
    }
    (path, 1 << 16)
}

fn generate_uhd(path: &Path) {
    // Numéro n sur 16 bits : bloc i blanc si le bit i de n est à 1, bande en haut de l'image.
    let bits: Vec<String> = (0..16)
        .map(|i| {
            format!(
                "drawbox=x={x}:y=0:w=240:h=90:color=white:t=fill:enable='eq(mod(floor(n/{p}),2),1)'",
                x = i * 240,
                p = 1u32 << i
            )
        })
        .collect();
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("color=c=black:s=3840x2160:r=25:d=0.4")
        .args(["-vf", &bits.join(",")])
        .args([
            "-c:v",
            "libx265",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("corpus absent : lancer cargo xtask corpus gen --big (ou installer la CLI ffmpeg)");
    assert!(status.success(), "génération de l'image 3840×2160");
}

#[ma_trace::covers("PERF.XFER.01")]
#[test]
fn fit_view_has_no_full_res_transfer() {
    if !require_vpp() {
        return;
    }
    let (path, name, area) = match corpus_file_if_present("uhd_big") {
        Some(p) => (p, "uhd_big", Size { w: 1920, h: 1080 }),
        None => (corpus_file(MAIN10), MAIN10, Size { w: 960, h: 540 }),
    };
    let m = manifest(name);
    let modulo = m.code_modulo;
    let src = TsAuSource::open(&path).unwrap();
    let mut d = open(&src, ModePref::Auto);
    let frames = decode(&mut d, &src, 30, &|_| Viewport::fit(m.dims(), area));
    assert_eq!(d.mode(), DecodeMode::Hardware);
    let full = frames[0].full;
    let vp = Viewport::fit(full, area);
    assert!(vp.is_reduced());
    for f in &frames {
        assert_eq!(
            (f.image.layout, f.image.size),
            (PixelLayout::SemiPlanar8, vp.out)
        );
    }
    check_codes(&frames, whole_of(full), modulo, "Ajusté (GPU)");
    let stats = d.stats();
    assert_eq!(
        stats.path,
        ConvertPath::VaapiVpp,
        "VPP VA-API absente (pilote iHD libre ?) : PERF.XFER.01 demande la réduction sur le GPU"
    );
    assert_eq!(stats.converted, 30);
    assert_eq!(
        stats.full_res_transfers, 0,
        "aucun transfert pleine résolution"
    );
}

#[ma_trace::covers("PERF.XFER.01")]
#[test]
fn fit_1920x1080_under_4_mib() {
    let (path, modulo) = uhd_file();
    let src = TsAuSource::open(&path).unwrap();
    let full = Size { w: 3840, h: 2160 };
    let vp = Viewport::fit(full, Size { w: 1920, h: 1080 });
    let mut prefs = vec![ModePref::SoftwareOnly];
    if require_vpp() {
        prefs.push(ModePref::Auto);
    }
    for pref in prefs {
        let mut d = open(&src, pref);
        let frames = decode(&mut d, &src, 3, &|_| vp);
        for f in &frames {
            assert_eq!(f.full, full);
            assert_eq!(f.image.size, Size { w: 1920, h: 1080 });
            assert!(
                f.image.byte_len() < 4 * 1024 * 1024,
                "{pref:?} : {} octets",
                f.image.byte_len()
            );
        }
        check_codes(&frames, whole_of(full), modulo, &format!("{pref:?}"));
        assert_eq!(d.stats().full_res_transfers, 0);
    }
}

/// Région 1:1 en `pref` (chemin VA-API forcé à `force`), comparée au même recadrage de l'image
/// entière décodée par un second décodeur, sur 5 images. Rend le décodeur de la région.
fn region_matches_whole(
    pref: ModePref,
    force: Option<ConvertPath>,
    layout: PixelLayout,
) -> FfmpegDecoder {
    let src = TsAuSource::open(&corpus_file(MAIN10)).unwrap();
    let full = manifest(MAIN10).dims();
    let region = Viewport::region_1to1(full, REGION);
    let mut d = open(&src, pref);
    if let Some(path) = force {
        d.force_vaapi_path(path);
    }
    let parts = decode(&mut d, &src, 5, &|_| region);
    let mut w = open(&src, pref);
    let wholes = decode(&mut w, &src, 5, &|_| {
        Viewport::region_1to1(full, whole_of(full))
    });
    for (p, f) in parts.iter().zip(&wholes) {
        assert_eq!(p.pres, f.pres);
        assert_eq!(p.image.layout, layout);
        assert!(
            p.image == crop(&f.image, REGION),
            "{pref:?} : image {} : la région diffère du recadrage de l'image entière",
            p.pres
        );
    }
    check_codes(
        &wholes,
        whole_of(full),
        manifest(MAIN10).code_modulo,
        "entière",
    );
    assert_eq!(d.stats().converted, 5);
    d
}

#[test]
fn vpp_region_1to1_is_bit_exact() {
    if !require_vpp() {
        return;
    }
    let d = region_matches_whole(ModePref::Auto, None, PixelLayout::SemiPlanar10);
    assert_eq!(d.stats().path, ConvertPath::VaapiVpp);
    // Aucun transfert complet pour une région plus petite que l'image.
    assert_eq!(d.stats().full_res_transfers, 0);
}

#[test]
fn cpu_region_1to1_is_bit_exact() {
    let d = region_matches_whole(ModePref::SoftwareOnly, None, PixelLayout::Planar10);
    assert_eq!(d.stats().path, ConvertPath::Software);
    assert_eq!(d.stats().full_res_transfers, 0);
}

/// Sans VPP (forcé : la machine de référence l'a), le décodage reste matériel : projection (ou
/// transfert) de la surface, puis conversion CPU. Région 1:1 exacte au format natif (P010),
/// réduction en `Planar8` avec le bon numéro ; chaque conversion lit la surface entière.
#[test]
fn degraded_path_without_vpp_stays_hardware() {
    if !require_vaapi() {
        return;
    }
    let src = TsAuSource::open(&corpus_file(MAIN10)).unwrap();
    let m = manifest(MAIN10);
    let full = m.dims();
    for path in [ConvertPath::VaapiMapped, ConvertPath::VaapiTransfer] {
        let d = region_matches_whole(ModePref::Auto, Some(path), PixelLayout::SemiPlanar10);
        assert_eq!((d.mode(), d.stats().path), (DecodeMode::Hardware, path));
        assert_eq!(d.stats().full_res_transfers, 5);
        let vp = Viewport::fit(full, Size { w: 640, h: 360 });
        let mut d = open(&src, ModePref::Auto);
        d.force_vaapi_path(path);
        // Un chemin non dégradé est ignoré.
        d.force_vaapi_path(ConvertPath::Software);
        let frames = decode(&mut d, &src, 10, &|_| vp);
        for f in &frames {
            assert_eq!(
                (f.image.layout, f.image.size),
                (PixelLayout::Planar8, vp.out)
            );
        }
        check_codes(&frames, whole_of(full), m.code_modulo, &format!("{path:?}"));
        assert_eq!((d.mode(), d.stats().path), (DecodeMode::Hardware, path));
        assert_eq!(d.stats().full_res_transfers, 10);
        assert_eq!(d.gpu_graphs(), 0, "aucun graphe VPP");
    }
}

#[test]
fn reduced_output_keeps_frame_code() {
    let mut cases = vec![(ModePref::SoftwareOnly, PixelLayout::Planar8)];
    if require_vpp() {
        cases.push((ModePref::Auto, PixelLayout::SemiPlanar8));
    }
    for name in [MAIN10, "h264_720p_closed"] {
        let src = TsAuSource::open(&corpus_file(name)).unwrap();
        let m = manifest(name);
        let full = m.dims();
        for &(pref, layout) in &cases {
            for area in [Size { w: 640, h: 360 }, Size { w: 427, h: 240 }] {
                let vp = Viewport::fit(full, area);
                let mut d = open(&src, pref);
                let frames = decode(&mut d, &src, 10, &|_| vp);
                for f in &frames {
                    assert_eq!((f.image.layout, f.image.size), (layout, vp.out), "{name}");
                }
                check_codes(
                    &frames,
                    whole_of(full),
                    m.code_modulo,
                    &format!("{name} {pref:?}"),
                );
                assert_eq!(d.stats().full_res_transfers, 0);
            }
        }
    }
}

#[test]
fn viewport_change_reuses_or_rebuilds_graph() {
    if !require_vpp() {
        return;
    }
    let src = TsAuSource::open(&corpus_file(MAIN10)).unwrap();
    let m = manifest(MAIN10);
    let full = m.dims();
    let band = Rect {
        x: 0,
        y: 0,
        w: full.w,
        h: full.h / 12,
    };
    let three = [
        (Viewport::fit(full, Size { w: 640, h: 360 }), whole_of(full)),
        (Viewport::region_1to1(full, band), band),
        (
            Viewport {
                region: band,
                out: Size {
                    w: full.w / 2,
                    h: full.h / 24,
                },
            },
            band,
        ),
    ];
    let mut d = open(&src, ModePref::Auto);
    let frames = decode(&mut d, &src, 12, &|i| three[i % 3].0);
    for (i, f) in frames.iter().enumerate() {
        let (vp, region) = three[i % 3];
        assert_eq!(f.image.size, vp.out);
        check_codes(
            std::slice::from_ref(f),
            region,
            m.code_modulo,
            "3 viewports",
        );
    }
    assert_eq!(d.gpu_graphs(), 3, "un graphe par viewport, réutilisé");
    // Six sorties différentes : le cache reste borné à 4 graphes.
    let six: Vec<Viewport> = (1..=6)
        .map(|k| {
            Viewport::fit(
                full,
                Size {
                    w: 160 * k,
                    h: 90 * k,
                },
            )
        })
        .collect();
    d.reset();
    let frames = decode(&mut d, &src, 12, &|i| six[i % 6]);
    for (i, f) in frames.iter().enumerate() {
        assert_eq!(f.image.size, six[i % 6].out);
    }
    check_codes(&frames, whole_of(full), m.code_modulo, "6 viewports");
    assert_eq!(d.gpu_graphs(), 4);
    assert_eq!(d.stats().full_res_transfers, 0);
}

/// Sur une source NV12 (H.264), une région 1:1 et une réduction de même sortie NV12 (640×180)
/// ont chacune leur graphe : la région 1:1 (`mode=fast`) reste exacte octet pour octet, la
/// réduction (`mode=hq`) garde le bon numéro.
#[test]
fn one_to_one_and_reduction_of_equal_output_use_distinct_graphs() {
    if !require_vpp() {
        return;
    }
    let name = "h264_720p_closed";
    let src = TsAuSource::open(&corpus_file(name)).unwrap();
    let m = manifest(name);
    let full = m.dims();
    let region = Rect {
        x: 256,
        y: 128,
        w: 640,
        h: 180,
    };
    let band = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 360,
    };
    let one = Viewport::region_1to1(full, region);
    let reduced = Viewport {
        region: band,
        out: Size { w: 640, h: 180 },
    };
    assert!(reduced.is_reduced() && !one.is_reduced());
    let mut d = open(&src, ModePref::Auto);
    let frames = decode(&mut d, &src, 6, &|i| if i % 2 == 0 { one } else { reduced });
    assert_eq!(
        d.gpu_graphs(),
        2,
        "une réduction et une région 1:1 ne partagent pas de graphe"
    );
    let mut w = open(&src, ModePref::Auto);
    let wholes = decode(&mut w, &src, 6, &|_| {
        Viewport::region_1to1(full, whole_of(full))
    });
    for (i, (f, whole)) in frames.iter().zip(&wholes).enumerate() {
        assert_eq!(f.pres, whole.pres);
        assert_eq!(f.image.layout, PixelLayout::SemiPlanar8);
        if i % 2 == 0 {
            assert!(
                f.image == crop(&whole.image, region),
                "image {} : la région 1:1 NV12 diffère du recadrage",
                f.pres
            );
        } else {
            check_codes(
                std::slice::from_ref(f),
                band,
                m.code_modulo,
                "réduction NV12",
            );
        }
    }
    assert_eq!(d.stats().full_res_transfers, 0);
}

/// R14, R16 : sans VPP, un test qui la demande est sauté, sauf si `MA_REQUIRE_VPP=1`
/// (`MA_REQUIRE_GPU=1` seul ne suffit pas : machine VA-API au pilote libre). Le chemin sans
/// VPP est forcé quand un périphérique existe (la machine de référence a la VPP).
#[test]
fn vpp_guard_skips_without_vpp() {
    let path = if require_vaapi() {
        let src = TsAuSource::open(&corpus_file(MAIN10)).unwrap();
        let mut d = open(&src, ModePref::Auto);
        d.force_vaapi_path(ConvertPath::VaapiMapped);
        d.vaapi_path()
    } else {
        ConvertPath::VaapiMapped
    };
    assert!(
        !vpp_gate(path == ConvertPath::VaapiVpp, false),
        "test sauté"
    );
    assert!(vpp_gate(true, true));
    let required = std::panic::catch_unwind(|| vpp_gate(false, true));
    assert!(required.is_err(), "MA_REQUIRE_VPP=1 : le test échoue");
    // Le garde du test suit `MA_REQUIRE_VPP`, et non `MA_REQUIRE_GPU`.
    if !vpp_required() {
        assert!(!vpp_gate(path == ConvertPath::VaapiVpp, vpp_required()));
    }
    // Cohérence avec la détection publique.
    if require_vaapi() && vpp_required() {
        let src = TsAuSource::open(&corpus_file(MAIN10)).unwrap();
        let d = open(&src, ModePref::Auto);
        assert_eq!(
            d.vaapi_path() == ConvertPath::VaapiVpp,
            ma_ffmpeg::vaapi_has_vpp(None)
        );
    }
}
