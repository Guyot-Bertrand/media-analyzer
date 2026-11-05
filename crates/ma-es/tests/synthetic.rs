//! Flux synthétiques (`BitWriter`) : cas qu'aucun fichier du corpus ne porte (PAFF, images de
//! champ MPEG-2, BLA, `broken_link`…), discontinuités et robustesse.

// Offsets et tailles de tests : bornés par construction.
#![allow(clippy::cast_possible_truncation)]

use ma_bits::{BitWriter, escape};
use ma_core::{Codec, PictureType};
use ma_es::{
    AuBoundary, AuMeta, Field, NalLeading, ParamSet, RapClass, first_slice_payload_offset, framer,
};
use proptest::prelude::*;

/// Concatène des NAL précédés d'un code de début de 4 octets ; rend le flux et l'offset du
/// code de début de chaque NAL.
fn es(nals: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
    es_with(nals, &[0, 0, 0, 1])
}

fn es_with(nals: &[Vec<u8>], sc: &[u8]) -> (Vec<u8>, Vec<u64>) {
    let mut v = Vec::new();
    let mut at = Vec::new();
    for n in nals {
        at.push(v.len() as u64);
        v.extend_from_slice(sc);
        v.extend_from_slice(n);
    }
    (v, at)
}

fn run(codec: Codec, data: &[u8]) -> Vec<AuBoundary> {
    let mut f = framer(codec);
    let mut out = Vec::new();
    f.push(data, &mut out);
    f.finish(&mut out);
    out
}

/// NAL = en-tête + charge utile échappée (avec `rbsp_trailing_bits`).
fn nal(header: &[u8], w: BitWriter) -> Vec<u8> {
    let mut v = header.to_vec();
    v.extend(escape(&w.finish()));
    v
}

// ---------------------------------------------------------------- H.264

#[derive(Clone, Copy)]
struct Sps {
    id: u32,
    profile: u8,
    chroma_format_idc: u32,
    separate_colour_plane: bool,
    scaling: bool,
    log2_max_frame_num_minus4: u32,
    poc_type: u32,
    frame_mbs_only: bool,
}

const MAIN: Sps = Sps {
    id: 0,
    profile: 77,
    chroma_format_idc: 1,
    separate_colour_plane: false,
    scaling: false,
    log2_max_frame_num_minus4: 0,
    poc_type: 0,
    frame_mbs_only: true,
};

fn h264_sps(s: Sps) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(8, u64::from(s.profile));
    w.u(8, 0);
    w.u(8, 40);
    w.ue(s.id);
    if matches!(s.profile, 100 | 244) {
        w.ue(s.chroma_format_idc);
        if s.chroma_format_idc == 3 {
            w.u(1, u64::from(s.separate_colour_plane));
        }
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.u(1, 0); // qpprime_y_zero_transform_bypass_flag
        w.u(1, u64::from(s.scaling));
        if s.scaling {
            let lists = if s.chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                let present = i % 3 == 0 || i == 1;
                w.u(1, u64::from(present));
                if present {
                    let size = if i < 6 { 16 } else { 64 };
                    if i == 1 {
                        // delta_scale = -8 : nextScale = 0 dès la 1re valeur (matrice par défaut).
                        w.se(-8);
                    } else {
                        // nextScale reste 8 : `size` valeurs lues.
                        for _ in 0..size {
                            w.se(0);
                        }
                    }
                }
            }
        }
    }
    w.ue(s.log2_max_frame_num_minus4);
    w.ue(s.poc_type);
    match s.poc_type {
        0 => w.ue(2),
        1 => {
            w.u(1, 0);
            w.se(-3);
            w.se(2);
            w.ue(3);
            w.se(1);
            w.se(-1);
            w.se(7);
        }
        _ => {}
    }
    w.ue(1); // max_num_ref_frames
    w.u(1, 0);
    w.ue(79);
    w.ue(44);
    w.u(1, u64::from(s.frame_mbs_only));
    if !s.frame_mbs_only {
        w.u(1, 0); // mb_adaptive_frame_field_flag
    }
    w.u(1, 1);
    w.u(1, 0);
    w.u(1, 0);
    nal(&[0x67], w)
}

fn h264_pps(pps_id: u32, sps_id: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(pps_id);
    w.ue(sps_id);
    w.u(1, 0);
    w.u(1, 0);
    w.ue(0);
    nal(&[0x68], w)
}

#[derive(Clone, Copy)]
struct Slice {
    nal_type: u8,
    ref_idc: u8,
    first_mb: u32,
    /// `slice_type`.
    kind: u32,
    pps_id: u32,
    /// `colour_plane_id` écrit, si présent.
    colour_plane: Option<u64>,
    frame_num_bits: u32,
    frame_num: u64,
    coding: Coding,
}

/// Champs de structure d'image écrits dans la tranche.
#[derive(Clone, Copy)]
enum Coding {
    /// `frame_mbs_only_flag = 1` : rien d'écrit.
    FrameOnly,
    /// `field_pic_flag = 0`.
    Frame,
    /// `field_pic_flag = 1`, puis `bottom_field_flag`.
    Field { bottom: bool },
}

const IDR: Slice = Slice {
    nal_type: 5,
    ref_idc: 3,
    first_mb: 0,
    kind: 7,
    pps_id: 0,
    colour_plane: None,
    frame_num_bits: 4,
    frame_num: 0,
    coding: Coding::FrameOnly,
};

fn h264_slice(s: Slice) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(s.first_mb);
    w.ue(s.kind);
    w.ue(s.pps_id);
    if let Some(plane) = s.colour_plane {
        w.u(2, plane);
    }
    w.u(s.frame_num_bits, s.frame_num);
    match s.coding {
        Coding::FrameOnly => {}
        Coding::Frame => w.u(1, 0),
        Coding::Field { bottom } => {
            w.u(1, 1);
            w.u(1, u64::from(bottom));
        }
    }
    w.ue(0);
    w.u(16, 0xA5A5);
    nal(&[(s.ref_idc << 5) | s.nal_type], w)
}

fn aud() -> Vec<u8> {
    vec![0x09, 0xF0]
}

/// SEI H.264 : données utilisateur (type 5, 2 octets), puis « recovery point » si demandé.
fn h264_sei(recovery: Option<u32>) -> Vec<u8> {
    let mut v = vec![0x06, 5, 2, 0xAA, 0xBB];
    if let Some(cnt) = recovery {
        let mut w = BitWriter::new();
        w.ue(cnt);
        w.u(1, 1);
        w.u(1, 0);
        w.u(2, 0);
        let payload = w.finish();
        v.push(6);
        v.push(payload.len() as u8);
        v.extend(escape(&payload));
    }
    v.push(0x80);
    v
}

#[test]
fn h264_paff_fields_are_detected() {
    let sps = h264_sps(Sps {
        frame_mbs_only: false,
        ..MAIN
    });
    let top = h264_slice(Slice {
        coding: Coding::Field { bottom: false },
        frame_num: 3,
        ..IDR
    });
    let bottom = h264_slice(Slice {
        nal_type: 1,
        kind: 5,
        coding: Coding::Field { bottom: true },
        frame_num: 3,
        ..IDR
    });
    let (data, at) = es(&[sps, h264_pps(0, 0), top, bottom]);
    let aus = run(Codec::H264, &data);
    assert_eq!(aus.len(), 2, "{aus:?}");
    assert_eq!((aus[0].start, aus[0].end), (0, at[3]));
    assert_eq!((aus[1].start, aus[1].end), (at[3], data.len() as u64));
    assert_eq!(aus[0].meta.field, Field::Top);
    assert_eq!(aus[1].meta.field, Field::Bottom);
    assert_eq!(aus[0].meta.frame_num, Some(3));
    assert_eq!(aus[1].meta.frame_num, Some(3));
    assert_eq!(aus[0].meta.rap, RapClass::Idr);
    assert_eq!(aus[0].meta.picture, PictureType::I);
    assert_eq!(aus[1].meta.rap, RapClass::None);
    assert_eq!(aus[1].meta.picture, PictureType::P);
    assert_eq!(aus[1].meta.first_vcl_type, 1);
    assert!(aus[0].meta.params.contains(ParamSet::SPS));
    assert!(aus[0].meta.params.contains(ParamSet::PPS));
    // Image entière d'un SPS entrelacé (MBAFF ou trame) : field_pic_flag = 0.
    let frame = h264_slice(Slice {
        coding: Coding::Frame,
        ..IDR
    });
    let (data, _) = es(&[
        h264_sps(Sps {
            frame_mbs_only: false,
            ..MAIN
        }),
        h264_pps(0, 0),
        frame,
    ]);
    assert_eq!(run(Codec::H264, &data)[0].meta.field, Field::Frame);
}

#[test]
fn h264_high_profile_sps_is_parsed() {
    for (chroma, separate, poc_type) in [(3, true, 1), (1, false, 0), (3, false, 2)] {
        let sps = h264_sps(Sps {
            id: 7,
            profile: 100,
            chroma_format_idc: chroma,
            separate_colour_plane: separate,
            scaling: true,
            log2_max_frame_num_minus4: 3,
            poc_type,
            frame_mbs_only: true,
        });
        let slice = h264_slice(Slice {
            nal_type: 1,
            kind: 2,
            pps_id: 200,
            colour_plane: separate.then_some(0),
            frame_num_bits: 7,
            frame_num: 93,
            ..IDR
        });
        let (data, _) = es(&[sps, h264_pps(200, 7), slice]);
        let aus = run(Codec::H264, &data);
        assert_eq!(aus.len(), 1);
        assert_eq!(aus[0].meta.frame_num, Some(93), "chroma {chroma}");
        assert_eq!(aus[0].meta.field, Field::Frame);
        assert_eq!(aus[0].meta.picture, PictureType::I);
        // I non IDR sans SEI : pas un point d'accès.
        assert_eq!(aus[0].meta.rap, RapClass::None);
    }
}

#[test]
fn h264_separate_colour_planes_are_one_au() {
    // 4:4:4 à plans séparés (§7.4.1.2.4) : chaque plan a sa tranche `first_mb_in_slice = 0` ;
    // seule celle du plan 0 ouvre une AU.
    let sps = h264_sps(Sps {
        profile: 244,
        chroma_format_idc: 3,
        separate_colour_plane: true,
        ..MAIN
    });
    let plane = |p, nal_type| {
        h264_slice(Slice {
            nal_type,
            colour_plane: Some(p),
            frame_num: 2,
            ..IDR
        })
    };
    let (data, at) = es(&[
        sps,
        h264_pps(0, 0),
        plane(0, 5),
        plane(1, 5),
        plane(2, 5),
        plane(0, 1), // 5 : image suivante
        plane(1, 1),
        plane(2, 1),
    ]);
    let aus = run(Codec::H264, &data);
    let spans: Vec<(u64, u64)> = aus.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(spans, [(0, at[5]), (at[5], data.len() as u64)]);
    assert_eq!(aus[0].meta.rap, RapClass::Idr);
    assert_eq!(aus[0].meta.frame_num, Some(2));
    assert_eq!(aus[1].meta.first_vcl_type, 1);
}

#[test]
fn h264_missing_parameter_sets_give_defaults() {
    // PPS inconnu.
    let slice = h264_slice(Slice {
        pps_id: 5,
        coding: Coding::Field { bottom: true },
        ..IDR
    });
    let (data, _) = es(&[
        h264_sps(Sps {
            frame_mbs_only: false,
            ..MAIN
        }),
        slice,
    ]);
    let m = run(Codec::H264, &data)[0].meta;
    assert_eq!((m.field, m.frame_num), (Field::Frame, None));
    assert_eq!(m.picture, PictureType::I);
    assert_eq!(m.rap, RapClass::Idr);
    // PPS connu, SPS inconnu.
    let (data, _) = es(&[h264_pps(0, 9), h264_slice(IDR)]);
    let m = run(Codec::H264, &data)[0].meta;
    assert_eq!((m.field, m.frame_num), (Field::Frame, None));
    // Identifiants hors plage : ignorés, sans panique.
    let mut w = BitWriter::new();
    w.u(24, 0x4D_0028);
    w.ue(40);
    let (data, _) = es(&[nal(&[0x67], w), h264_pps(300, 0), h264_slice(IDR)]);
    assert_eq!(run(Codec::H264, &data)[0].meta.frame_num, None);
    // En-tête de tranche tronqué : valeurs par défaut.
    let (data, _) = es(&[h264_sps(MAIN), h264_pps(0, 0), vec![0x65, 0x88]]);
    let m = run(Codec::H264, &data)[0].meta;
    assert_eq!(m.picture, PictureType::I);
    assert_eq!(m.frame_num, None);
}

#[test]
fn h264_recovery_point_on_i_only() {
    let i = Slice {
        nal_type: 1,
        kind: 2,
        ..IDR
    };
    let au = |sei: Vec<u8>, s: Slice| {
        let (data, _) = es(&[aud(), h264_sps(MAIN), h264_pps(0, 0), sei, h264_slice(s)]);
        run(Codec::H264, &data)[0].meta
    };
    assert_eq!(
        au(h264_sei(Some(3)), i).rap,
        RapClass::Recovery { frame_cnt: 3 }
    );
    assert!(au(h264_sei(Some(3)), i).params.contains(ParamSet::SEI));
    assert!(au(h264_sei(Some(3)), i).params.contains(ParamSet::AUD));
    assert_eq!(au(h264_sei(None), i).rap, RapClass::None);
    let p = Slice { kind: 0, ..i };
    assert_eq!(au(h264_sei(Some(3)), p).rap, RapClass::None);
    assert_eq!(au(h264_sei(Some(3)), IDR).rap, RapClass::Idr);
    // SI (slice_type 4) = I ; SP (3) = P ; B (6).
    assert_eq!(
        au(h264_sei(None), Slice { kind: 4, ..i }).picture,
        PictureType::I
    );
    assert_eq!(
        au(h264_sei(None), Slice { kind: 3, ..i }).picture,
        PictureType::P
    );
    assert_eq!(
        au(h264_sei(None), Slice { kind: 6, ..i }).picture,
        PictureType::B
    );
    // Type et taille de charge codés sur plusieurs octets (0xFF), SEI tronqué.
    let mut long = vec![0x06, 0xFF, 0x01, 0xFF, 0x00];
    long.extend(std::iter::repeat_n(0x11, 255));
    long.extend([6, 1, 0b0101_1000, 0x80]);
    assert_eq!(au(long, i).rap, RapClass::Recovery { frame_cnt: 1 });
    assert_eq!(au(vec![0x06, 6, 9, 0x40], i).rap, RapClass::None);
    assert_eq!(au(vec![0x06, 0xFF], i).rap, RapClass::None);
}

#[test]
fn h264_access_unit_boundaries() {
    let p = |first_mb, ref_idc| {
        h264_slice(Slice {
            nal_type: 1,
            ref_idc,
            first_mb,
            kind: 5,
            ..IDR
        })
    };
    let nals = vec![
        aud(),           // 0 : AU 0
        h264_sps(MAIN),  // 1
        h264_pps(0, 0),  // 2
        h264_sei(None),  // 3
        h264_slice(IDR), // 4
        h264_slice(Slice {
            first_mb: 40,
            ..IDR
        }), // 5 : 2e tranche, même AU
        h264_sei(None),  // 6 : AU 1 (SEI après une tranche)
        p(0, 2),         // 7
        p(0, 0),         // 8 : AU 2 (first_mb = 0)
        vec![0x0C, 0xFF], // 9 : bourrage (12), même AU
        h264_pps(0, 0),  // 10 : AU 3
        p(0, 1),         // 11
        vec![0x0A],      // 12 : fin de séquence, même AU
        vec![0x2E, 0x80], // 13 : NAL 14 (préfixe SVC) : AU 4
        vec![0x14, 0x80], // 14 : NAL 20, même AU
        vec![0x02, 0x80], // 15 : partition A sans en-tête lisible
    ];
    let (mut data, at) = es(&nals);
    data.extend([0, 0]); // trailing_zero_8bits
    let aus = run(Codec::H264, &data);
    let spans: Vec<(u64, u64)> = aus.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(
        spans,
        [
            (0, at[6]),
            (at[6], at[8]),
            (at[8], at[10]),
            (at[10], at[13]),
            (at[13], data.len() as u64),
        ]
    );
    assert!(aus[1].meta.reference);
    assert!(!aus[2].meta.reference);
    assert_eq!(aus[1].meta.params.labels(Codec::H264), ["SEI"]);
    assert_eq!(aus[3].meta.params.labels(Codec::H264), ["PPS"]);
    assert_eq!(aus[4].meta.first_vcl_type, 2);
    assert_eq!(aus[4].meta.picture, PictureType::P);
}

// ---------------------------------------------------------------- HEVC

fn hevc_nal(t: u8, w: BitWriter) -> Vec<u8> {
    nal(&[t << 1, 1], w)
}

fn hevc_pps(pps_id: u32, extra_bits: u64) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(pps_id);
    w.ue(0);
    w.u(1, 0);
    w.u(1, 0);
    w.u(3, extra_bits);
    w.u(8, 0x5A);
    hevc_nal(34, w)
}

fn hevc_slice(t: u8, first: bool, pps_id: u32, extra_bits: u32, slice_type: u32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(1, u64::from(first));
    if (16..=23).contains(&t) {
        w.u(1, 0);
    }
    w.ue(pps_id);
    if !first {
        w.u(6, 3); // slice_segment_address
    }
    w.u(extra_bits, 0b01);
    w.ue(slice_type);
    w.u(16, 0xA5A5);
    hevc_nal(t, w)
}

fn hevc_params() -> Vec<Vec<u8>> {
    vec![
        vec![35 << 1, 1, 0x50],
        vec![32 << 1, 1, 0x0C, 0x01, 0xFF],
        vec![33 << 1, 1, 0x01, 0x01],
        hevc_pps(0, 0),
    ]
}

#[test]
fn hevc_bla_is_rap() {
    for t in [16u8, 17, 18] {
        let mut nals = hevc_params();
        nals.push(hevc_slice(t, true, 0, 0, 2));
        nals.push(hevc_slice(1, true, 0, 0, 1));
        let (data, at) = es(&nals);
        let aus = run(Codec::Hevc, &data);
        assert_eq!(aus.len(), 2);
        assert_eq!((aus[0].start, aus[0].end), (0, at[5]));
        assert_eq!(aus[0].meta.rap, RapClass::Bla, "type {t}");
        assert_eq!(aus[0].meta.picture, PictureType::I);
        assert_eq!(aus[0].meta.first_vcl_type, t);
        assert!(aus[0].meta.reference);
        assert_eq!(
            aus[0].meta.params.labels(Codec::Hevc),
            ["VPS", "SPS", "PPS"]
        );
        assert_eq!(aus[1].meta.rap, RapClass::None);
        assert_eq!(aus[1].meta.picture, PictureType::P);
    }
    for (t, rap) in [
        (19, RapClass::Idr),
        (20, RapClass::Idr),
        (21, RapClass::Cra),
    ] {
        let mut nals = hevc_params();
        nals.push(hevc_slice(t, true, 0, 0, 2));
        assert_eq!(run(Codec::Hevc, &es(&nals).0)[0].meta.rap, rap);
    }
}

#[test]
fn hevc_slice_type_uses_pps_extra_bits() {
    // PPS 3 : 2 bits d'en-tête supplémentaires ; slice_type 0 (B) n'est lu juste qu'en les
    // sautant.
    let (data, _) = es(&[hevc_pps(3, 2), hevc_slice(1, true, 3, 2, 0)]);
    assert_eq!(run(Codec::Hevc, &data)[0].meta.picture, PictureType::B);
    let (data, _) = es(&[hevc_pps(3, 0), hevc_slice(1, true, 3, 0, 2)]);
    assert_eq!(run(Codec::Hevc, &data)[0].meta.picture, PictureType::I);
    // IRAP : `no_output_of_prior_pics_flag` précède `slice_pic_parameter_set_id`. Une IRAP ne
    // porte que des tranches I ; le `slice_type` B ci-dessous est volontairement non conforme,
    // pour que le test distingue : sans le saut de ce drapeau, `pps_id` se lit 7 (inconnu) et le
    // type retombe à I.
    let (data, _) = es(&[hevc_pps(3, 2), hevc_slice(21, true, 3, 2, 0)]);
    let m = run(Codec::Hevc, &data)[0].meta;
    assert_eq!((m.picture, m.rap), (PictureType::B, RapClass::Cra));
    // PPS inconnu : P, ou I pour une IRAP.
    let (data, _) = es(&[hevc_slice(1, true, 9, 0, 0)]);
    assert_eq!(run(Codec::Hevc, &data)[0].meta.picture, PictureType::P);
    let (data, _) = es(&[hevc_slice(19, true, 9, 0, 0)]);
    let m = run(Codec::Hevc, &data)[0].meta;
    assert_eq!((m.picture, m.rap), (PictureType::I, RapClass::Idr));
    // PPS hors plage, en-tête tronqué : sans panique, valeurs par défaut.
    let (data, _) = es(&[hevc_pps(64, 0), vec![1 << 1, 1, 0x80]]);
    assert_eq!(run(Codec::Hevc, &data)[0].meta.picture, PictureType::P);
    let (data, _) = es(&[vec![1 << 1]]);
    assert_eq!(run(Codec::Hevc, &data), Vec::<AuBoundary>::new());
}

#[test]
fn hevc_leading_reference_and_boundaries() {
    let nals = vec![
        vec![35 << 1, 1, 0x50],         // 0 : AU 0
        hevc_pps(0, 0),                 // 1
        hevc_slice(21, true, 0, 0, 2),  // 2 : CRA
        hevc_slice(21, false, 0, 0, 2), // 3 : 2e tranche
        vec![40 << 1, 1, 0x80],         // 4 : SEI suffixe, même AU
        hevc_slice(8, true, 0, 0, 0),   // 5 : AU 1, RASL_N
        hevc_slice(7, true, 0, 0, 1),   // 6 : AU 2, RADL_R
        vec![39 << 1, 1, 0x80],         // 7 : AU 3, SEI préfixe
        hevc_slice(0, true, 0, 0, 1),   // 8 : TRAIL_N
        vec![38 << 1, 1],               // 9 : bourrage, même AU
        vec![36 << 1, 1],               // 10 : fin de séquence, même AU
        vec![48 << 1, 1, 0x80],         // 11 : AU 4 (non spécifié 48)
        hevc_slice(9, true, 0, 0, 0),   // 12 : RASL_R
        hevc_slice(6, true, 0, 0, 0),   // 13 : AU 5, RADL_N
    ];
    let (data, at) = es(&nals);
    let aus = run(Codec::Hevc, &data);
    let spans: Vec<(u64, u64)> = aus.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(
        spans,
        [
            (0, at[5]),
            (at[5], at[6]),
            (at[6], at[7]),
            (at[7], at[11]),
            (at[11], at[13]),
            (at[13], data.len() as u64),
        ]
    );
    let m: Vec<AuMeta> = aus.iter().map(|a| a.meta).collect();
    assert_eq!(m[0].rap, RapClass::Cra);
    assert_eq!(m[0].params.labels(Codec::Hevc), ["PPS", "SEI"]);
    assert_eq!(
        (m[1].leading, m[1].reference, m[1].picture),
        (Some(NalLeading::Skippable), false, PictureType::B)
    );
    assert_eq!(
        (m[2].leading, m[2].reference),
        (Some(NalLeading::Decodable), true)
    );
    assert_eq!((m[3].leading, m[3].reference), (None, false));
    assert_eq!(m[3].params.labels(Codec::Hevc), ["SEI"]);
    assert_eq!(
        (m[4].leading, m[4].reference),
        (Some(NalLeading::Skippable), true)
    );
    assert_eq!(
        (m[5].leading, m[5].reference),
        (Some(NalLeading::Decodable), false)
    );
    assert!(
        m.iter()
            .all(|x| x.field == Field::Frame && x.frame_num.is_none())
    );
}

// ---------------------------------------------------------------- MPEG-2

const SH: [u8; 9] = [0xB3, 0x2D, 0x02, 0x40, 0x13, 0xFF, 0xFF, 0xE0, 0x18];
const SEQ_EXT: [u8; 7] = [0xB5, 0x14, 0x8A, 0x00, 0x01, 0x00, 0x00];
const SLICE: [u8; 4] = [0x01, 0x2B, 0x7E, 0x7F];

fn gop(closed: bool, broken: bool) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(25, 0x0800);
    w.u(1, u64::from(closed));
    w.u(1, u64::from(broken));
    w.u(5, 0);
    let mut v = vec![0xB8];
    v.extend(w.finish());
    v
}

fn picture(pct: u64) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(10, 5);
    w.u(3, pct);
    w.u(16, 0xFFFF);
    if pct >= 2 {
        w.u(4, 7);
    }
    if pct == 3 {
        w.u(4, 7);
    }
    let mut v = vec![0x00];
    v.extend(w.finish());
    v
}

fn pce(structure: u64) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.u(4, 8);
    w.u(16, 0xFFFF);
    w.u(2, 0);
    w.u(2, structure);
    w.u(8, 0x80);
    let mut v = vec![0xB5];
    v.extend(w.finish());
    v
}

fn mpeg2(nals: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
    es_with(nals, &[0, 0, 1])
}

#[test]
fn mpeg2_field_pictures_are_detected() {
    let nals = vec![
        SH.to_vec(),
        SEQ_EXT.to_vec(),
        gop(true, false),
        picture(1),
        pce(1),
        SLICE.to_vec(),
        SLICE.to_vec(),
        picture(2), // 7 : 2e champ
        pce(2),
        SLICE.to_vec(),
        picture(3), // 10 : image entière
        pce(3),
        SLICE.to_vec(),
    ];
    let (data, at) = mpeg2(&nals);
    let aus = run(Codec::Mpeg2, &data);
    let spans: Vec<(u64, u64)> = aus.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(
        spans,
        [(0, at[7]), (at[7], at[10]), (at[10], data.len() as u64)]
    );
    let f: Vec<Field> = aus.iter().map(|a| a.meta.field).collect();
    assert_eq!(f, [Field::Top, Field::Bottom, Field::Frame]);
    assert_eq!(
        aus[0].meta.rap,
        RapClass::Intra {
            closed_gop: true,
            broken_link: false
        }
    );
    assert_eq!(aus[0].meta.params.labels(Codec::Mpeg2), ["SH", "GH"]);
    assert!(aus[0].meta.params.contains(ParamSet::SEQ_EXT));
    let p: Vec<(PictureType, bool)> = aus
        .iter()
        .map(|a| (a.meta.picture, a.meta.reference))
        .collect();
    assert_eq!(
        p,
        [
            (PictureType::I, true),
            (PictureType::P, true),
            (PictureType::B, false)
        ]
    );
    assert_eq!(aus[1].meta.rap, RapClass::None);
}

#[test]
fn mpeg2_broken_link_is_reported() {
    let (data, _) = mpeg2(&[gop(false, true), picture(1), SLICE.to_vec()]);
    let m = run(Codec::Mpeg2, &data)[0].meta;
    assert_eq!(
        m.rap,
        RapClass::Intra {
            closed_gop: false,
            broken_link: true
        }
    );
    assert_eq!(m.params.labels(Codec::Mpeg2), ["GH"]);
    // En-tête de séquence sans GOP : point d'accès, drapeaux à faux.
    let (data, _) = mpeg2(&[SH.to_vec(), picture(1), SLICE.to_vec()]);
    assert_eq!(
        run(Codec::Mpeg2, &data)[0].meta.rap,
        RapClass::Intra {
            closed_gop: false,
            broken_link: false
        }
    );
    // I seule : pas un point d'accès ; D (4) = I ; type 0 ou en-tête tronqué : P.
    let one = |nals: &[Vec<u8>]| run(Codec::Mpeg2, &mpeg2(nals).0)[0].meta;
    assert_eq!(one(&[picture(1), SLICE.to_vec()]).rap, RapClass::None);
    assert_eq!(one(&[picture(4), SLICE.to_vec()]).picture, PictureType::I);
    assert_eq!(one(&[picture(0), SLICE.to_vec()]).picture, PictureType::P);
    assert_eq!(one(&[vec![0x00, 0x01]]).picture, PictureType::P);
    // GOP tronqué, extensions tronquée ou inconnue : sans effet.
    let m = one(&[
        vec![0xB8, 0x00],
        picture(1),
        vec![0xB5],
        vec![0xB5, 0x20, 0x00],
        vec![0xB5, 0x80],
        vec![0xB2, 0x41, 0x42],
        SLICE.to_vec(),
        vec![0xB7],
    ]);
    assert_eq!(
        (m.rap, m.field),
        (
            RapClass::Intra {
                closed_gop: false,
                broken_link: false
            },
            Field::Frame
        )
    );
}

// ---------------------------------------------------------------- découpage commun

fn h264_au(slice_type_p: bool) -> Vec<Vec<u8>> {
    let s = if slice_type_p {
        h264_slice(Slice {
            nal_type: 1,
            kind: 5,
            ..IDR
        })
    } else {
        h264_slice(IDR)
    };
    vec![aud(), s]
}

#[test]
fn boundaries_follow_start_codes() {
    // Octets parasites avant le 1er code de début, codes de 3 octets, zéros de fin.
    let mut nals = vec![h264_sps(MAIN), h264_pps(0, 0)];
    nals.extend(h264_au(false));
    nals.extend(h264_au(true));
    let (body, at) = es_with(&nals, &[0, 0, 1]);
    let mut data = vec![0x55, 0xAA, 0x00, 0x01, 0x07];
    let skip = data.len() as u64;
    data.extend(&body);
    data.extend([0, 0, 0]);
    let aus = run(Codec::H264, &data);
    let spans: Vec<(u64, u64)> = aus.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(
        spans,
        [(skip, skip + at[4]), (skip + at[4], data.len() as u64)]
    );
    // Flux vide ou sans code de début : aucune AU.
    assert_eq!(run(Codec::H264, &[]), Vec::<AuBoundary>::new());
    assert_eq!(run(Codec::Hevc, &[0; 100]), Vec::<AuBoundary>::new());
}

#[test]
fn gap_drops_partial_au_and_resyncs() {
    let mut nals = vec![h264_sps(MAIN), h264_pps(0, 0)];
    nals.extend(h264_au(false)); // AU 0 : 0..
    nals.extend(h264_au(true)); // AU 1 : at[4]..
    let (first, at) = es(&nals);
    // Coupure au milieu de la tranche de l'AU 1.
    let cut = first.len() - 3;
    // Après la coupure : fin de tranche, tranche qui n'ouvre pas d'AU, puis AU 2.
    let mut tail = vec![0x55; 20];
    let mut more = vec![h264_slice(Slice {
        nal_type: 1,
        first_mb: 9,
        kind: 5,
        ..IDR
    })];
    more.extend(h264_au(true));
    let (rest, rat) = es(&more);
    tail.extend(&rest);
    let mut f = framer(Codec::H264);
    let mut out = Vec::new();
    f.push(&first[..cut], &mut out);
    f.gap();
    f.push(&tail, &mut out);
    f.finish(&mut out);
    let base = cut as u64 + 20;
    let spans: Vec<(u64, u64)> = out.iter().map(|a| (a.start, a.end)).collect();
    assert_eq!(
        spans,
        [(0, at[4]), (base + rat[1], cut as u64 + tail.len() as u64)]
    );
    assert_eq!(out[0].meta.rap, RapClass::Idr);
    assert_eq!(out[1].meta.picture, PictureType::P);
}

#[test]
fn gap_keeps_au_completed_by_the_next_opener() {
    // La coupure tombe dans le NAL qui ouvre l'AU suivante : l'AU précédente est complète et
    // émise au prochain appel ; l'AU coupée est oubliée.
    for (codec, nals, opener) in [
        (
            Codec::H264,
            {
                let mut v = vec![h264_sps(MAIN), h264_pps(0, 0)];
                v.extend(h264_au(false));
                v
            },
            vec![0x09, 0xF0],
        ),
        (
            Codec::Mpeg2,
            vec![SH.to_vec(), picture(1), SLICE.to_vec()],
            picture(2),
        ),
    ] {
        let (mut first, _) = es(&nals);
        let opener_at = first.len() as u64;
        first.extend([0, 0, 0, 1]);
        first.extend(&opener);
        let mut f = framer(codec);
        let mut out = Vec::new();
        f.push(&first, &mut out);
        assert_eq!(out, Vec::new());
        f.gap();
        f.gap();
        let (second, _) = es(&nals);
        f.push(&second, &mut out);
        f.finish(&mut out);
        let spans: Vec<(u64, u64)> = out.iter().map(|a| (a.start, a.end)).collect();
        let total = (first.len() + second.len()) as u64;
        assert_eq!(
            spans,
            [(0, opener_at), (first.len() as u64, total)],
            "{codec:?}"
        );
    }
    // Coupure, puis fin immédiate.
    let (data, _) = es(&h264_au(false));
    let mut f = framer(Codec::H264);
    let mut out = Vec::new();
    f.push(&data, &mut out);
    f.gap();
    f.finish(&mut out);
    assert_eq!(out, Vec::<AuBoundary>::new());
}

#[test]
fn param_set_api() {
    let mut p = ParamSet::EMPTY;
    assert!(p.contains(ParamSet::EMPTY));
    assert!(!p.contains(ParamSet::SPS));
    p.insert(ParamSet::SPS);
    p.insert(ParamSet::GOP_HEADER);
    assert!(p.contains(ParamSet::SPS));
    assert_eq!(p.labels(Codec::H264), ["SPS"]);
    assert_eq!(p.labels(Codec::Mpeg2), ["GH"]);
    assert_eq!(ParamSet::default(), ParamSet::EMPTY);
    assert_eq!(AuMeta::default().picture, PictureType::P);
    assert_eq!(Field::default(), Field::Frame);
    assert_eq!(RapClass::default(), RapClass::None);
}

#[test]
fn slice_payload_offsets() {
    let au = |nals: &[&[u8]]| {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    };
    let h264 = au(&[&[0x67, 1], &[0x65, 0x88, 9]]);
    assert_eq!(
        first_slice_payload_offset(Codec::H264, &h264),
        Some(4 + 2 + 4 + 1)
    );
    let hevc = au(&[&[0x40, 1, 9], &[19 << 1, 1, 9]]);
    assert_eq!(
        first_slice_payload_offset(Codec::Hevc, &hevc),
        Some(4 + 3 + 4 + 2)
    );
    let mpeg2 = au(&[&[0xB3, 1], &[0x00, 0, 8, 0], &[0x01, 5]]);
    assert_eq!(
        first_slice_payload_offset(Codec::Mpeg2, &mpeg2),
        Some(4 + 2 + 4 + 4 + 4 + 1)
    );
    assert_eq!(
        first_slice_payload_offset(Codec::H264, &au(&[&[0x67, 1]])),
        None
    );
    assert_eq!(
        first_slice_payload_offset(Codec::Hevc, &au(&[&[0x40, 1]])),
        None
    );
    assert_eq!(first_slice_payload_offset(Codec::Mpeg2, &[1, 2, 3]), None);
    assert_eq!(first_slice_payload_offset(Codec::H264, &[0, 0, 1]), None);
}

// ---------------------------------------------------------------- robustesse

/// Octets favorisant les codes de début et les en-têtes de NAL des trois codecs.
const ALPHABET: [u8; 24] = [
    0, 0, 0, 0, 0, 0, 1, 1, 1, 0x65, 0x41, 0x09, 0x06, 0x67, 0x68, 0x26, 0x40, 0x44, 0x02, 0xB3,
    0xB5, 0xB8, 0x80, 0xFF,
];

fn byte() -> impl Strategy<Value = u8> {
    prop_oneof![3 => proptest::sample::select(&ALPHABET[..]), 1 => any::<u8>()]
}

fn codec() -> impl Strategy<Value = Codec> {
    prop_oneof![Just(Codec::H264), Just(Codec::Hevc), Just(Codec::Mpeg2)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[ma_trace::covers("SYN.ROBUST.01")]
    #[test]
    fn framers_never_panic(
        codec in codec(),
        data in proptest::collection::vec(byte(), 0..2000),
        cuts in proptest::collection::vec((1usize..64, any::<bool>()), 1..40),
    ) {
        let mut f = framer(codec);
        let mut out = Vec::new();
        let mut pos = 0;
        for &(n, gap) in cuts.iter().cycle() {
            if pos >= data.len() {
                break;
            }
            let end = (pos + n).min(data.len());
            f.push(&data[pos..end], &mut out);
            if gap {
                f.gap();
            }
            pos = end;
        }
        f.finish(&mut out);
        for w in out.windows(2) {
            prop_assert!(w[0].end <= w[1].start);
        }
        for a in &out {
            prop_assert!(a.start < a.end && a.end <= data.len() as u64);
        }
    }

    #[test]
    fn random_framing_is_independent_of_chunking(
        codec in codec(),
        data in proptest::collection::vec(byte(), 0..2000),
        sizes in proptest::collection::vec(1usize..64, 1..20),
    ) {
        let whole = run(codec, &data);
        let mut f = framer(codec);
        let mut out = Vec::new();
        let mut pos = 0;
        for &n in sizes.iter().cycle() {
            if pos >= data.len() {
                break;
            }
            let end = (pos + n).min(data.len());
            f.push(&data[pos..end], &mut out);
            pos = end;
        }
        f.finish(&mut out);
        prop_assert_eq!(out, whole);
    }
}
