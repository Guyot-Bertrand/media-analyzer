//! L'image rendue est l'image demandée : numéro incrusté lu sur des images décodées par
//! libavcodec, en logiciel et en VA-API.

mod common;

use common::{
    band, check, is_not_decodable, navigator, never, oracle_walk, require_vpp, source, whole,
};
use ma_decode::{
    AuSource, ConvertPath, DecodeEvent, DecodeMode, Leading, ModePref, NavConfig, NotDecodable,
    RapKind, start_for,
};
use ma_ffmpeg::FfmpegFactory;
use ma_testkit::{manifest, require_vaapi};

#[ma_trace::covers("NAV.ORACLE.01")]
#[test]
fn oracle_h264_hevc_sw() {
    for name in ["h264_720p_closed", "hevc_720p_main10"] {
        oracle_walk(name, ModePref::SoftwareOnly, 50, whole);
    }
}

#[ma_trace::covers("NAV.ORACLE.01")]
#[test]
fn oracle_hevc_vaapi() {
    if !require_vaapi() {
        return;
    }
    let name = "hevc_720p_main10";
    oracle_walk(name, ModePref::Auto, 50, whole);
    if !require_vpp() {
        return;
    }
    // Mode réel et transferts : une bande 1:1 passe par la VPP (seule la bande est transférée),
    // l'image entière en 1:1 est un transfert pleine résolution.
    let mut nav = navigator(source(name), ModePref::Auto);
    let dims = manifest(name).dims();
    for p in [0, 1, 2, 120] {
        check(&mut nav, name, p, band(dims), 0);
    }
    assert_eq!(nav.mode(), DecodeMode::Hardware);
    let stats = nav.stats().decoder;
    assert!(stats.converted >= 4, "{stats:?}");
    assert_eq!(stats.path, ConvertPath::VaapiVpp);
    assert_eq!(stats.full_res_transfers, 0);
    check(&mut nav, name, 121, whole(dims), 0);
    let after = nav.stats().decoder;
    assert_eq!(
        after.full_res_transfers,
        after.converted - stats.converted,
        "{after:?}"
    );
    assert!(
        nav.take_events()
            .iter()
            .all(|e| !matches!(e, DecodeEvent::FellBackToSoftware { .. }))
    );
}

#[test]
fn h264_and_mpeg2_vaapi() {
    if !require_vaapi() {
        return;
    }
    for name in ["h264_720p_closed", "mpeg2_576i_closed"] {
        oracle_walk(name, ModePref::Auto, 10, band);
        let mut nav = navigator(source(name), ModePref::Auto);
        check(&mut nav, name, 3, band(manifest(name).dims()), 0);
        assert_eq!(nav.mode(), DecodeMode::Hardware, "{name}");
    }
}

/// Toutes les images RASL de `name`, demandées une à une de la dernière à la première (chaque
/// demande repart d'un RAP) : bon numéro (décalé de `offset`), sauf celles du premier CRA,
/// signalées non décodables. Rend (RASL correctes, RASL refusées).
fn rasl_frames(name: &str, pref: ModePref, offset: u32) -> (u32, u32) {
    let src = source(name);
    let vp = band(manifest(name).dims());
    let mut nav = navigator(src.clone(), pref);
    let mut rasl: Vec<_> = (0..src.len())
        .map(|d| src.entry(d).unwrap())
        .filter(|e| e.leading == Leading::Skippable)
        .collect();
    rasl.sort_by_key(|e| std::cmp::Reverse(e.pres_idx));
    let (mut ok, mut refused) = (0, 0);
    for e in rasl {
        match start_for(&*src, e.decode_idx) {
            Ok(start) => {
                assert!(start < e.decode_idx);
                check(&mut nav, name, e.pres_idx, vp, offset);
                ok += 1;
            }
            Err(reason) => {
                assert_eq!(reason, NotDecodable::LeadingOfFirstRap);
                let err = nav.frame(e.pres_idx, vp, &never).unwrap_err();
                assert!(is_not_decodable(&err), "{err}");
                refused += 1;
            }
        }
    }
    assert_eq!(nav.take_events(), []);
    (ok, refused)
}

fn prefs() -> Vec<ModePref> {
    let mut prefs = vec![ModePref::SoftwareOnly];
    if require_vaapi() {
        prefs.push(ModePref::Auto);
    }
    prefs
}

#[ma_trace::covers("NAV.ORACLE.02")]
#[test]
fn open_gop_rasl_frames_correct() {
    for pref in prefs() {
        let (ok, refused) = rasl_frames("hevc_720p_open_gop", pref, 0);
        assert!(ok >= 3, "{pref:?} : {ok} RASL décodées");
        assert_eq!(refused, 0, "le fichier commence par une IDR");
    }
}

#[ma_trace::covers("NAV.ORACLE.02")]
#[test]
fn rasl_of_the_first_cra_are_not_decodable() {
    let name = "hevc_720p_open_gop_from_cra";
    // Numérotation du fichier d'origine : décalage lu sur le CRA de tête.
    let src = source(name);
    let vp = band(manifest(name).dims());
    let cra = src.entry(0).unwrap();
    assert_eq!(cra.rap, RapKind::Cra);
    let mut nav = navigator(src, ModePref::SoftwareOnly);
    let f = nav.frame(cra.pres_idx, vp, &never).unwrap();
    let offset = common::code_of(&f, vp).unwrap() - cra.pres_idx;
    for pref in prefs() {
        let (ok, refused) = rasl_frames(name, pref, offset);
        assert!(refused >= 1, "{pref:?} : RASL du premier CRA attendues");
        assert!(ok >= 2, "{pref:?} : {ok} RASL décodées");
    }
}

#[ma_trace::covers("NAV.ORACLE.03")]
#[test]
fn recovery_point_seek_mid_file() {
    let name = "h264_720p_recovery";
    let m = manifest(name);
    let src = source(name);
    // Première image du milieu du fichier qui n'est pas une image de tête.
    let target = (src.len() / 2..src.len())
        .map(|p| src.entry(src.decode_idx_of(p).unwrap()).unwrap())
        .find(|e| e.leading == Leading::None)
        .unwrap();
    let start = start_for(&*src, target.decode_idx).unwrap();
    assert!(
        matches!(src.entry(start).unwrap().rap, RapKind::Recovery { .. }),
        "départ sur un recovery point"
    );
    let latency = {
        use ma_decode::{Codec, DecoderFactory};
        let factory = FfmpegFactory { render_node: None };
        let decoder = factory.open(Codec::H264, ModePref::SoftwareOnly).unwrap();
        decoder.latency()
    };
    let mut nav = navigator(src.clone(), ModePref::SoftwareOnly);
    check(&mut nav, name, target.pres_idx, whole(m.dims()), 0);
    // Au plus un intervalle de reprise (GOP et B de tête), plus la marge de réordonnancement et
    // la latence du pipeline ; bien moins que depuis le début du fichier.
    let bound = u64::from(m.gop + m.bframes + NavConfig::default().reorder_margin + latency);
    let sent = nav.stats().aus_sent;
    assert!(sent <= bound, "{sent} AU envoyées (borne {bound})");
    assert!(bound < u64::from(target.decode_idx), "borne non probante");
}

#[ma_trace::covers("NAV.ORACLE.04")]
#[test]
fn cut_file_frames_before_rap_not_decodable() {
    let name = "hevc_720p_cut";
    let src = source(name);
    let vp = band(manifest(name).dims());
    let mut nav = navigator(src.clone(), ModePref::SoftwareOnly);
    let first_rap = (0..src.len())
        .find(|&d| src.entry(d).unwrap().rap != RapKind::None)
        .unwrap();
    assert!(first_rap > 0);
    for d in 0..first_rap {
        let pres = src.entry(d).unwrap().pres_idx;
        let err = nav.frame(pres, vp, &never).unwrap_err();
        assert!(is_not_decodable(&err), "AU {d} : {err}");
    }
    // Les images suivantes sont décodables ; le numéro incrusté est celui du fichier d'origine,
    // décalé d'une constante.
    let rap = src.entry(first_rap).unwrap().pres_idx;
    let f = nav.frame(rap, vp, &never).unwrap();
    let code = common::code_of(&f, vp).unwrap();
    let offset = code - rap;
    for d in first_rap..src.len() {
        let pres = src.entry(d).unwrap().pres_idx;
        check(&mut nav, name, pres, vp, offset);
    }
}

#[test]
fn mpeg2_oracle() {
    oracle_walk("mpeg2_576i_closed", ModePref::SoftwareOnly, 20, whole);
}
