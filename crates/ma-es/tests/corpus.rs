//! Découpage et classification sur le corpus généré, comparés à `ffprobe`.
//!
//! Le flux élémentaire vient de `ffmpeg -c copy` (concaténation exacte des paquets vus par
//! `ffprobe`, vérifiée par `es_is_the_concatenation_of_ffprobe_packets`).

// Offsets et tailles de tests : bornés par construction (fichiers de quelques Mo).
#![allow(clippy::cast_possible_truncation)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ma_core::{Codec, PictureType};
use ma_es::{AuBoundary, Field, NalLeading, ParamSet, RapClass, framer};
use ma_testkit::{corpus_file, ffmpeg_es, ffprobe_packets, manifest};
use proptest::prelude::*;

fn codec_of(name: &str) -> Codec {
    match manifest(name).codec.as_str() {
        "h264" => Codec::H264,
        "hevc" => Codec::Hevc,
        "mpeg2video" => Codec::Mpeg2,
        other => panic!("codec inattendu : {other}"),
    }
}

/// Flux élémentaire de `name`, extrait une seule fois par processus.
fn es_of(name: &'static str) -> &'static [u8] {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static [u8]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let mut map = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(name)
        .or_insert_with(|| Vec::leak(ffmpeg_es(&corpus_file(name))))
}

/// Découpe `es` en morceaux de `chunk` octets.
fn frame(codec: Codec, es: &[u8], chunk: usize) -> Vec<AuBoundary> {
    let mut f = framer(codec);
    let mut out = Vec::new();
    for c in es.chunks(chunk) {
        f.push(c, &mut out);
    }
    f.finish(&mut out);
    out
}

fn frame_file(name: &'static str) -> Vec<AuBoundary> {
    frame(codec_of(name), es_of(name), 1 << 16)
}

/// Plage `[start, end)` privée de ses octets nuls de tête et de fin.
fn core(es: &[u8], start: u64, end: u64) -> (u64, u64) {
    let s = &es[start as usize..end as usize];
    let Some(last) = s.iter().rposition(|&b| b != 0) else {
        return (end, end);
    };
    let first = s.iter().position(|&b| b != 0).unwrap_or(0);
    (start + first as u64, start + last as u64 + 1)
}

#[test]
fn es_is_the_concatenation_of_ffprobe_packets() {
    for name in ["h264_720p_closed", "hevc_720p_main10", "mpeg2_576i_closed"] {
        let total: u64 = ffprobe_packets(&corpus_file(name))
            .iter()
            .map(|p| u64::from(p.size))
            .sum();
        assert_eq!(total, es_of(name).len() as u64, "{name}");
    }
}

#[ma_trace::covers("DATA.PIC.01")]
#[test]
fn au_count_and_sizes_match_ffprobe() {
    for name in [
        "h264_720p_closed",
        "hevc_720p_main10",
        "mpeg2_576i_closed",
        "h264_1080i_mbaff",
        "hevc_720p_open_gop",
        "h264_720p_recovery",
    ] {
        let es = es_of(name);
        let packets = ffprobe_packets(&corpus_file(name));
        let aus = frame_file(name);
        assert_eq!(aus.len(), packets.len(), "{name} : nombre d'AU");
        assert_eq!(aus.last().unwrap().end, es.len() as u64, "{name}");
        let mut p_start = 0u64;
        let mut raw_diff = 0usize;
        for (k, (au, p)) in aus.iter().zip(&packets).enumerate() {
            let p_end = p_start + u64::from(p.size);
            // `trailing_zero_8bits` : ffprobe coupe au `00 00 01` et laisse le `zero_byte` qui
            // le précède (code de début de 4 octets) à la fin de l'AU précédente (HEVC, MPEG-2) ;
            // nous coupons avant ce `zero_byte`. L'écart ne porte que sur des octets nuls : la
            // comparaison les retire aux deux bouts.
            assert_eq!(
                core(es, au.start, au.end),
                core(es, p_start, p_end),
                "{name} AU {k} : [{}, {}) contre [{p_start}, {p_end}) (ffprobe)",
                au.start,
                au.end
            );
            if au.end - au.start != u64::from(p.size) {
                raw_diff += 1;
            }
            assert_eq!(
                au.meta.rap != RapClass::None,
                p.key,
                "{name} AU {k} : RAP {:?} contre key={}",
                au.meta.rap,
                p.key
            );
            p_start = p_end;
        }
        eprintln!("{name} : {raw_diff} AU de taille brute différente (octets nuls seulement)");
    }
}

#[test]
fn hevc_open_gop_has_cra_and_rasl() {
    let aus = frame_file("hevc_720p_open_gop");
    assert_eq!(aus.len(), 250);
    assert_eq!(aus[0].meta.rap, RapClass::Idr);
    let cra: Vec<usize> = (0..aus.len())
        .filter(|&i| aus[i].meta.rap == RapClass::Cra)
        .collect();
    assert_eq!(cra.len(), 4, "CRA aux positions {cra:?}");
    assert!(cra.iter().all(|&i| aus[i].meta.picture == PictureType::I));
    assert!(cra.iter().all(|&i| aus[i].meta.first_vcl_type == 21));
    let rasl: Vec<usize> = (0..aus.len())
        .filter(|&i| aus[i].meta.leading == Some(NalLeading::Skippable))
        .collect();
    assert_ne!(rasl.len(), 0);
    for &r in &rasl {
        assert!(matches!(aus[r].meta.first_vcl_type, 8 | 9), "AU {r}");
        let last_rap = (0..r).rev().find(|&i| aus[i].meta.rap != RapClass::None);
        assert_eq!(
            last_rap.map(|i| aus[i].meta.rap),
            Some(RapClass::Cra),
            "RASL {r} : le RAP précédent doit être une CRA"
        );
    }
}

#[test]
fn h264_recovery_points() {
    let name = "h264_720p_recovery";
    let aus = frame_file(name);
    assert_eq!(aus.len(), 250);
    let idr: Vec<usize> = (0..aus.len())
        .filter(|&i| aus[i].meta.rap == RapClass::Idr)
        .collect();
    assert_eq!(idr, [0], "une seule IDR, en tête");
    let recovery: Vec<usize> = (0..aus.len())
        .filter(|&i| matches!(aus[i].meta.rap, RapClass::Recovery { .. }))
        .collect();
    assert_eq!(recovery.len(), 9);
    for &i in &recovery {
        // x264 (`open-gop=1`) écrit `recovery_frame_cnt = 0`.
        assert_eq!(aus[i].meta.rap, RapClass::Recovery { frame_cnt: 0 });
        assert_eq!(aus[i].meta.picture, PictureType::I);
        assert_eq!(aus[i].meta.first_vcl_type, 1);
    }
    // Même positions que les paquets `key` de ffprobe.
    let keys: Vec<usize> = ffprobe_packets(&corpus_file(name))
        .iter()
        .enumerate()
        .filter(|(_, p)| p.key)
        .map(|(i, _)| i)
        .collect();
    let mut raps = idr.clone();
    raps.extend(&recovery);
    assert_eq!(raps, keys);
}

#[test]
fn mpeg2_gop_flags() {
    let aus = frame_file("mpeg2_576i_closed");
    assert_eq!(aus.len(), 125);
    let intra: Vec<usize> = (0..aus.len())
        .filter(|&i| aus[i].meta.rap != RapClass::None)
        .collect();
    assert_eq!(intra, (0..10).map(|k| k * 13).collect::<Vec<_>>());
    for &i in &intra {
        assert_eq!(
            aus[i].meta.rap,
            RapClass::Intra {
                closed_gop: true,
                broken_link: false
            },
            "AU {i}"
        );
        assert_eq!(aus[i].meta.picture, PictureType::I);
        assert!(aus[i].meta.params.contains(ParamSet::SEQ_HEADER));
        assert!(aus[i].meta.params.contains(ParamSet::SEQ_EXT));
    }
    // Images entières (pas d'images de champ), références I et P seulement.
    for au in &aus {
        assert_eq!(au.meta.field, Field::Frame);
        assert_eq!(au.meta.reference, au.meta.picture != PictureType::B);
    }
    assert!(aus.iter().any(|a| a.meta.picture == PictureType::B));
}

#[ma_trace::covers("NAV.THUMB.02")]
#[test]
fn params_present_in_au() {
    let hevc = frame_file("hevc_720p_main10");
    assert_eq!(hevc[0].meta.rap, RapClass::Idr);
    assert_eq!(
        hevc[0].meta.params.labels(Codec::Hevc),
        ["VPS", "SPS", "PPS", "SEI"]
    );
    assert!(hevc[0].meta.params.contains(ParamSet::AUD));
    assert_eq!(hevc[1].meta.params.labels(Codec::Hevc), Vec::<&str>::new());
    let h264 = frame_file("h264_720p_closed");
    assert_eq!(
        h264[0].meta.params.labels(Codec::H264),
        ["SPS", "PPS", "SEI"]
    );
    let mpeg2 = frame_file("mpeg2_576i_closed");
    assert_eq!(mpeg2[0].meta.params.labels(Codec::Mpeg2), ["SH", "GH"]);
    assert_eq!(
        mpeg2[1].meta.params.labels(Codec::Mpeg2),
        Vec::<&str>::new()
    );
}

#[test]
fn mbaff_is_one_au_per_frame() {
    let aus = frame_file("h264_1080i_mbaff");
    assert_eq!(aus.len(), 100);
    assert!(aus.iter().all(|a| a.meta.field == Field::Frame));
    assert!(aus.iter().all(|a| a.meta.frame_num.is_some()));
    assert_eq!(
        aus.iter().filter(|a| a.meta.rap == RapClass::Idr).count(),
        4
    );
}

#[test]
fn closed_h264_frame_num_and_reference() {
    let aus = frame_file("h264_720p_closed");
    assert_eq!(aus.len(), 250);
    for au in &aus {
        if au.meta.rap == RapClass::Idr {
            assert_eq!(au.meta.frame_num, Some(0));
            assert!(au.meta.reference);
            assert_eq!(au.meta.first_vcl_type, 5);
        }
    }
    assert!(aus.iter().any(|a| !a.meta.reference));
}

/// 2 premiers Mo de l'ES de `name`.
fn head(name: &'static str) -> &'static [u8] {
    let es = es_of(name);
    &es[..es.len().min(2 << 20)]
}

fn chunked(codec: Codec, es: &[u8], sizes: &[usize]) -> Vec<AuBoundary> {
    let mut f = framer(codec);
    let mut out = Vec::new();
    let mut pos = 0;
    for &n in sizes.iter().cycle() {
        if pos >= es.len() {
            break;
        }
        let end = (pos + n).min(es.len());
        f.push(&es[pos..end], &mut out);
        pos = end;
    }
    f.finish(&mut out);
    out
}

fn reference(name: &'static str) -> &'static [AuBoundary] {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static [AuBoundary]>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let mut map = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(name).or_insert_with(|| {
        let aus = frame(codec_of(name), head(name), usize::MAX);
        assert!(aus.len() > 20, "{name} : {} AU seulement", aus.len());
        Vec::leak(aus)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn framing_is_independent_of_chunking(
        sizes in proptest::collection::vec(
            prop_oneof![1usize..=4, 5usize..=300, 1000usize..=300_000],
            1..48,
        ),
        hevc in any::<bool>(),
    ) {
        let name = if hevc { "hevc_720p_main10" } else { "h264_720p_closed" };
        let got = chunked(codec_of(name), head(name), &sizes);
        prop_assert_eq!(got.as_slice(), reference(name));
    }
}
