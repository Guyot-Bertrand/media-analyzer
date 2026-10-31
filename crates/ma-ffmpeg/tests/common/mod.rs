//! Outils partagés par les tests sur le corpus.

#![allow(dead_code)] // chaque fichier de test n'utilise qu'une partie des outils

use std::sync::Arc;

use ma_decode::{
    AuSource, DecodedFrame, FrameError, ModePref, NavConfig, Navigator, Rect, Size, Viewport,
};
use ma_ffmpeg::FfmpegFactory;
use ma_testkit::{TsAuSource, corpus_file, manifest, read_frame_code};

pub fn never() -> bool {
    false
}

/// Coupe le journal de libavcodec : les tests d'AU corrompues le rempliraient d'erreurs
/// attendues (« no frame! », « Missing reference picture »).
pub fn quiet_ffmpeg() {
    // SAFETY: `av_log_set_level` écrit un entier global de libavutil, sans autre effet ; aucune
    // contrainte d'appel.
    unsafe { rsmpeg::ffi::av_log_set_level(rsmpeg::ffi::AV_LOG_QUIET) };
}

pub fn source(name: &str) -> Arc<TsAuSource> {
    quiet_ffmpeg();
    Arc::new(TsAuSource::open(&corpus_file(name)).unwrap())
}

pub fn navigator(src: Arc<dyn AuSource>, pref: ModePref) -> Navigator {
    Navigator::new(
        src,
        Arc::new(FfmpegFactory { render_node: None }),
        pref,
        NavConfig::default(),
    )
    .unwrap()
}

/// Image entière à l'échelle 1:1.
pub fn whole(full: Size) -> Viewport {
    Viewport::region_1to1(
        full,
        Rect {
            x: 0,
            y: 0,
            w: full.w,
            h: full.h,
        },
    )
}

/// Région 1:1 réduite à la bande du numéro incrusté (et un peu plus) : copie plus légère.
pub fn band(full: Size) -> Viewport {
    Viewport::region_1to1(
        full,
        Rect {
            x: 0,
            y: 0,
            w: full.w,
            h: full.h / 12,
        },
    )
}

/// Numéro lu dans l'image rendue pour le viewport `vp` (déjà normalisé : dimensions paires).
pub fn code_of(frame: &DecodedFrame, vp: Viewport) -> Option<u32> {
    let region = vp.normalized(frame.full).unwrap().region;
    read_frame_code(&frame.image, region, frame.full)
}

/// Demande l'image `pres` et vérifie son numéro (`(pres + offset) mod code_modulo`).
pub fn check(nav: &mut Navigator, name: &str, pres: u32, vp: Viewport, offset: u32) {
    let m = manifest(name);
    let f = nav
        .frame(pres, vp, &never)
        .unwrap_or_else(|e| panic!("{name} : image {pres} : {e}"));
    assert_eq!(f.pres_idx, pres);
    assert_eq!(f.full, m.dims(), "{name} : taille de l'image {pres}");
    assert_eq!(
        code_of(&f, vp),
        Some((pres + offset) % m.code_modulo),
        "{name} : numéro de l'image {pres}"
    );
}

/// Générateur pseudo-aléatoire déterministe (LCG).
pub struct Lcg(pub u64);

impl Lcg {
    pub fn below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        u32::try_from((self.0 >> 33) % u64::from(n)).unwrap()
    }
}

/// 50 sauts (graine fixe), puis 20 images suivantes et 20 précédentes depuis le milieu du fichier.
pub fn oracle_walk(name: &str, pref: ModePref, jumps: u32, vp_of: fn(Size) -> Viewport) {
    let src = source(name);
    let len = src.len();
    let vp = vp_of(manifest(name).dims());
    let mut nav = navigator(src, pref);
    let mut rng = Lcg(0x5EED_0001);
    for _ in 0..jumps {
        check(&mut nav, name, rng.below(len), vp, 0);
    }
    let mid = len / 2;
    for p in mid..mid + 20 {
        check(&mut nav, name, p, vp, 0);
    }
    for p in (mid - 1..mid + 19).rev() {
        check(&mut nav, name, p, vp, 0);
    }
    assert!(
        nav.take_events().is_empty(),
        "{name} : événements inattendus"
    );
}

/// Erreur attendue pour une image non décodable.
pub fn is_not_decodable(e: &FrameError) -> bool {
    matches!(e, FrameError::NotDecodable { .. })
}
