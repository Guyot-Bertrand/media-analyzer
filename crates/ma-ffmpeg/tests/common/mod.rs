//! Outils partagés par les tests sur le corpus.

#![allow(dead_code)] // chaque fichier de test n'utilise qu'une partie des outils

use std::sync::Arc;

use ma_decode::{
    AuSource, DecodedFrame, FrameError, ModePref, NavConfig, Navigator, Rect, Size, Viewport,
};
use ma_ffmpeg::{FfmpegFactory, vaapi_has_vpp};
use ma_testkit::{corpus_file, manifest, read_frame_code, require_vaapi};
use ma_ts::TsAuSource;

/// Garde de la VPP (R14, R16) : `true` si `has_vpp`. Sinon affiche `VPP VA-API absente : test non
/// exécuté` et rend `false`, sauf si `required` (`MA_REQUIRE_VPP=1` : alors le test échoue).
///
/// # Panics
/// Si `required` et `!has_vpp`.
pub fn vpp_gate(has_vpp: bool, required: bool) -> bool {
    if has_vpp {
        return true;
    }
    assert!(
        !required,
        "VPP VA-API absente alors que MA_REQUIRE_VPP=1 (pilote iHD libre ? voir BUILD.md)"
    );
    eprintln!("VPP VA-API absente : test non exécuté");
    false
}

/// `MA_REQUIRE_VPP=1` (fixé par `cargo xtask ci` quand `vainfo` annonce la VPP).
pub fn vpp_required() -> bool {
    std::env::var("MA_REQUIRE_VPP").is_ok_and(|v| v == "1")
}

/// Garde des tests (ou des cas) qui demandent la VPP VA-API : périphérique présent
/// ([`require_vaapi`]) et traitement vidéo offert. Sans VPP, le test est sauté, sauf si
/// `MA_REQUIRE_VPP=1`.
pub fn require_vpp() -> bool {
    quiet_ffmpeg();
    require_vaapi() && vpp_gate(vaapi_has_vpp(None), vpp_required())
}

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
    oracle_walk_steps(name, pref, (jumps, 20), vp_of);
}

/// `jumps` sauts (graine fixe), puis `steps` images suivantes et `steps` précédentes depuis le
/// milieu du fichier.
pub fn oracle_walk_steps(
    name: &str,
    pref: ModePref,
    (jumps, steps): (u32, u32),
    vp_of: fn(Size) -> Viewport,
) {
    let src = source(name);
    let len = src.len();
    let vp = vp_of(manifest(name).dims());
    let mut nav = navigator(src, pref);
    let mut rng = Lcg(0x5EED_0001);
    for _ in 0..jumps {
        check(&mut nav, name, rng.below(len), vp, 0);
    }
    let mid = len / 2;
    for p in mid..mid + steps {
        check(&mut nav, name, p, vp, 0);
    }
    for p in (mid - 1..mid + steps - 1).rev() {
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

/// Somme MD5 (RFC 1321) de `data`, en hexadécimal minuscule (format de `ffmpeg -f framemd5`).
#[allow(clippy::many_single_char_names)]
pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // K[i] = floor(|sin(i + 1)| × 2^32).
    let k: Vec<u32> = (0..64)
        .map(|i| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = (f64::from(i + 1).sin().abs() * 4_294_967_296.0) as u32;
            v
        })
        .collect();
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    let mut h: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in msg.as_chunks::<64>().0 {
        let m: Vec<u32> = block
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        let [mut a, mut b, mut c, mut d] = h;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter()
        .flat_map(|w| w.to_le_bytes())
        .fold(String::new(), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Octets d'une image à la manière de `framemd5` : chaque plan, ligne par ligne, sans le
/// remplissage de fin de ligne (plans de chrominance 4:2:0 arrondis vers le haut).
pub fn frame_bytes(img: &ma_decode::Planes) -> Vec<u8> {
    use ma_decode::PixelLayout;
    let (w, h) = (img.size.w as usize, img.size.h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let dims: Vec<(usize, usize)> = match img.layout {
        PixelLayout::Planar8 => vec![(w, h), (cw, ch), (cw, ch)],
        PixelLayout::Planar10 => vec![(2 * w, h), (2 * cw, ch), (2 * cw, ch)],
        PixelLayout::SemiPlanar8 => vec![(w, h), (2 * cw, ch)],
        PixelLayout::SemiPlanar10 => vec![(2 * w, h), (4 * cw, ch)],
    };
    let mut out = Vec::new();
    for ((plane, &stride), (row, rows)) in img.planes.iter().zip(&img.strides).zip(dims) {
        for y in 0..rows {
            let at = y * stride as usize;
            out.extend_from_slice(&plane[at..at + row]);
        }
    }
    out
}
