//! Tables PAT, PMT, SDT, descripteurs et classification des flux élémentaires.

use ma_core::Codec;

use crate::psi::Section;

/// Descripteur MPEG/DVB brut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub tag: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pat {
    pub ts_id: u16,
    /// (numéro de programme, PID de PMT). L'entrée du programme 0 (PID de NIT) est omise.
    pub programs: Vec<(u16, u16)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pmt {
    pub program_number: u16,
    pub version: u8,
    pub pcr_pid: u16,
    pub descriptors: Vec<Descriptor>,
    pub streams: Vec<PmtStream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdtService {
    pub service_id: u16,
    pub provider: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sdt {
    pub ts_id: u16,
    pub services: Vec<SdtService>,
}

const TABLE_PAT: u8 = 0x00;
const TABLE_PMT: u8 = 0x02;
/// SDT du flux courant (`actual`).
const TABLE_SDT_ACTUAL: u8 = 0x42;
const TAG_SERVICE: u8 = 0x48;

/// Lit une boucle de descripteurs ; `None` si un descripteur dépasse la boucle.
fn descriptors(mut b: &[u8]) -> Option<Vec<Descriptor>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let (&tag, rest) = b.split_first()?;
        let (&len, rest) = rest.split_first()?;
        let data = rest.get(..usize::from(len))?;
        out.push(Descriptor {
            tag,
            data: data.to_vec(),
        });
        b = &rest[usize::from(len)..];
    }
    Some(out)
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from(*b.get(at)?) << 8 | u16::from(*b.get(at + 1)?))
}

/// PAT. Le CRC n'est pas vérifié ici (voir `Section::crc_ok`).
#[must_use]
pub fn parse_pat(s: &Section) -> Option<Pat> {
    if s.table_id != TABLE_PAT || !s.body.len().is_multiple_of(4) {
        return None;
    }
    let programs = s
        .body
        .as_chunks::<4>()
        .0
        .iter()
        .filter_map(|e| {
            let number = u16::from(e[0]) << 8 | u16::from(e[1]);
            let pid = (u16::from(e[2]) << 8 | u16::from(e[3])) & 0x1FFF;
            (number != 0).then_some((number, pid))
        })
        .collect();
    Some(Pat {
        ts_id: s.table_id_ext,
        programs,
    })
}

/// PMT.
#[must_use]
pub fn parse_pmt(s: &Section) -> Option<Pmt> {
    if s.table_id != TABLE_PMT {
        return None;
    }
    let b = &s.body;
    let pcr_pid = be16(b, 0)? & 0x1FFF;
    let info_len = usize::from(be16(b, 2)? & 0x0FFF);
    let program_descriptors = descriptors(b.get(4..4 + info_len)?)?;
    let mut rest = &b[4 + info_len..];
    let mut streams = Vec::new();
    while !rest.is_empty() {
        let es_len = usize::from(be16(rest, 3)? & 0x0FFF);
        streams.push(PmtStream {
            stream_type: rest[0],
            pid: be16(rest, 1)? & 0x1FFF,
            descriptors: descriptors(rest.get(5..5 + es_len)?)?,
        });
        rest = &rest[5 + es_len..];
    }
    Some(Pmt {
        program_number: s.table_id_ext,
        version: s.version,
        pcr_pid,
        descriptors: program_descriptors,
        streams,
    })
}

/// SDT du flux courant (`table_id` 0x42) : nom et fournisseur de chaque service.
#[must_use]
pub fn parse_sdt(s: &Section) -> Option<Sdt> {
    if s.table_id != TABLE_SDT_ACTUAL {
        return None;
    }
    // original_network_id (2) et un octet réservé.
    let mut rest = s.body.get(3..)?;
    let mut services = Vec::new();
    while !rest.is_empty() {
        let loop_len = usize::from(be16(rest, 3)? & 0x0FFF);
        let descs = descriptors(rest.get(5..5 + loop_len)?)?;
        let (provider, name) = descs
            .iter()
            .find(|d| d.tag == TAG_SERVICE)
            .and_then(|d| service_names(&d.data))
            .unwrap_or_default();
        services.push(SdtService {
            service_id: be16(rest, 0)?,
            provider,
            name,
        });
        rest = &rest[5 + loop_len..];
    }
    Some(Sdt {
        ts_id: s.table_id_ext,
        services,
    })
}

/// (fournisseur, nom) d'un `service_descriptor`.
fn service_names(d: &[u8]) -> Option<(String, String)> {
    // service_type, provider_name_length, provider, service_name_length, name.
    let plen = usize::from(*d.get(1)?);
    let provider = d.get(2..2 + plen)?;
    let nlen = usize::from(*d.get(2 + plen)?);
    let name = d.get(3 + plen..3 + plen + nlen)?;
    Some((dvb_string(provider), dvb_string(name)))
}

/// Symboles ISO 6937 (EN 300 468, annexe A) des octets 0xA0 à 0xFF ; `'\0'` = non défini.
/// Les octets 0xC0 à 0xCF sont des diacritiques (traités à part).
const ISO6937_HIGH: [char; 96] = [
    '\u{A0}', '¡', '¢', '£', '€', '¥', '\0', '§', '¤', '‘', '“', '«', '←', '↑', '→',
    '↓', // A0
    '°', '±', '²', '³', '×', 'µ', '¶', '·', '÷', '’', '”', '»', '¼', '½', '¾', '¿', // B0
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    '\0', // C0
    '―', '¹', '®', '©', '™', '♪', '¬', '¦', '\0', '\0', '\0', '\0', '⅛', '⅜', '⅝', '⅞', // D0
    'Ω', 'Æ', 'Đ', 'ª', 'Ħ', '\0', 'Ĳ', 'Ŀ', 'Ł', 'Ø', 'Œ', 'º', 'Þ', 'Ŧ', 'Ŋ', 'ŉ', // E0
    'ĸ', 'æ', 'đ', 'ð', 'ħ', 'ı', 'ĳ', 'ŀ', 'ł', 'ø', 'œ', 'ß', 'þ', 'ŧ', 'ŋ', '\u{AD}', // F0
];

/// Diacritique ISO 6937 (octet 0xC1 à 0xCF) : (octet, signe combinant, lettres de base, lettres
/// précomposées correspondantes).
const ISO6937_MARKS: [(u8, char, &str, &str); 13] = [
    (0xC1, '\u{300}', "AEIOUaeiou", "ÀÈÌÒÙàèìòù"),
    (
        0xC2,
        '\u{301}',
        "AEIOUYaeiouyCcLlNnRrSsZz",
        "ÁÉÍÓÚÝáéíóúýĆćĹĺŃńŔŕŚśŹź",
    ),
    (
        0xC3,
        '\u{302}',
        "AEIOUaeiouCcGgHhJjSsWwYy",
        "ÂÊÎÔÛâêîôûĈĉĜĝĤĥĴĵŜŝŴŵŶŷ",
    ),
    (0xC4, '\u{303}', "ANOanoIiUu", "ÃÑÕãñõĨĩŨũ"),
    (0xC5, '\u{304}', "AEIOUaeiou", "ĀĒĪŌŪāēīōū"),
    (0xC6, '\u{306}', "AGUagu", "ĂĞŬăğŭ"),
    (0xC7, '\u{307}', "CEGIZcegz", "ĊĖĠİŻċėġż"),
    (0xC8, '\u{308}', "AEIOUaeiouyY", "ÄËÏÖÜäëïöüÿŸ"),
    (0xCA, '\u{30A}', "AUau", "ÅŮåů"),
    (0xCB, '\u{327}', "CGKLNRSTcgklnrst", "ÇĢĶĻŅŖŞŢçģķļņŗşţ"),
    (0xCD, '\u{30B}', "OUou", "ŐŰőű"),
    (0xCE, '\u{328}', "AEIUaeiu", "ĄĘĮŲąęįų"),
    (0xCF, '\u{30C}', "CDELNRSTZcdelnrstz", "ČĎĚĽŇŘŠŤŽčďěľňřšťž"),
];

/// Décode du texte ISO 6937 (table par défaut de DVB). Un diacritique suivi d'une lettre donne le
/// caractère précomposé s'il existe, sinon la lettre suivie du signe combinant ; sans lettre
/// derrière, ou pour un octet non défini : U+FFFD.
fn iso6937(b: &[u8]) -> String {
    let mut out = String::new();
    let mut bytes = b.iter().copied();
    while let Some(c) = bytes.next() {
        match c {
            0xC1..=0xCF => {
                let mark = ISO6937_MARKS.iter().find(|m| m.0 == c);
                match (mark, bytes.next()) {
                    (Some(&(_, combining, bases, composed)), Some(base)) => {
                        let base = char::from(base);
                        let pre = bases
                            .chars()
                            .position(|x| x == base)
                            .and_then(|i| composed.chars().nth(i));
                        if let Some(ch) = pre {
                            out.push(ch);
                        } else {
                            out.push(base);
                            out.push(combining);
                        }
                    }
                    _ => out.push('\u{FFFD}'),
                }
            }
            0xA0..=0xFF => match ISO6937_HIGH[usize::from(c - 0xA0)] {
                '\0' => out.push('\u{FFFD}'),
                ch => out.push(ch),
            },
            _ => out.push(char::from(c)),
        }
    }
    out
}

/// Texte DVB : 0x15 ⇒ UTF-8 ; `0x10 0x00 0x01` ⇒ Latin-1 ; sans préfixe ⇒ ISO 6937 (table par
/// défaut d'EN 300 468) ; tout autre jeu de caractères ⇒ un U+FFFD par octet. Les contrôles C1
/// (U+0080 à U+009F, dont les codes d'emphase 0x86/0x87 et le saut de ligne 0x8A) et leurs
/// équivalents UTF-8 privés U+E086, U+E087 et U+E08A sont retirés.
#[must_use]
pub fn dvb_string(b: &[u8]) -> String {
    let latin1 = |t: &[u8]| t.iter().map(|&c| char::from(c)).collect::<String>();
    let text = match b {
        [] => String::new(),
        [0x15, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0x10, 0x00, 0x01, rest @ ..] => latin1(rest),
        [0x10, rest @ ..] => "\u{FFFD}".repeat(rest.len().saturating_sub(2)),
        [first, rest @ ..] if *first < 0x20 => "\u{FFFD}".repeat(rest.len()),
        _ => iso6937(b),
    };
    text.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{80}'..='\u{9F}' | '\u{E086}' | '\u{E087}' | '\u{E08A}'
            )
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Aac,
    AacLatm,
    Ac3,
    Eac3,
    Ac4,
    Mpeg,
}

/// Nature d'un flux élémentaire d'après `stream_type` et ses descripteurs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// `None` : codec reconnu comme vidéo mais non analysé (VC-1…).
    Video(Option<Codec>),
    /// `None` : audio générique.
    Audio(Option<AudioKind>),
    Subtitles,
    Teletext,
    Scte35,
    Data,
}

const TAG_AC3: u8 = 0x6A;
const TAG_ENHANCED_AC3: u8 = 0x7A;
const TAG_SUBTITLING: u8 = 0x59;
const TAG_TELETEXT: u8 = 0x56;
const TAG_EXTENSION: u8 = 0x7F;
/// `descriptor_tag_extension` de l'AC-4.
const EXT_AC4: u8 = 0x15;

/// Classe un flux élémentaire. Seuls H.264, HEVC et MPEG-2 sont des vidéos analysées.
#[must_use]
pub fn stream_kind(stream_type: u8, descriptors: &[Descriptor]) -> StreamKind {
    use AudioKind::{Aac, AacLatm, Ac3, Ac4, Eac3, Mpeg};
    match stream_type {
        0x1B => StreamKind::Video(Some(Codec::H264)),
        0x24 => StreamKind::Video(Some(Codec::Hevc)),
        0x02 => StreamKind::Video(Some(Codec::Mpeg2)),
        0xEA => StreamKind::Video(None),
        0x0F => StreamKind::Audio(Some(Aac)),
        0x11 => StreamKind::Audio(Some(AacLatm)),
        0x03 | 0x04 => StreamKind::Audio(Some(Mpeg)),
        0x81 => StreamKind::Audio(Some(Ac3)),
        0x87 => StreamKind::Audio(Some(Eac3)),
        0x86 => StreamKind::Scte35,
        0x06 => descriptors
            .iter()
            .find_map(|d| match d.tag {
                TAG_AC3 => Some(StreamKind::Audio(Some(Ac3))),
                TAG_ENHANCED_AC3 => Some(StreamKind::Audio(Some(Eac3))),
                TAG_SUBTITLING => Some(StreamKind::Subtitles),
                TAG_TELETEXT => Some(StreamKind::Teletext),
                TAG_EXTENSION if d.data.first() == Some(&EXT_AC4) => {
                    Some(StreamKind::Audio(Some(Ac4)))
                }
                _ => None,
            })
            .unwrap_or(StreamKind::Data),
        _ => StreamKind::Data,
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::psi::Section;
    use crate::testutil::{descriptor, pat, pmt, sdt, section_of};
    use ma_core::Codec;

    fn parse(bytes: &[u8]) -> Section {
        section_of(bytes)
    }

    #[test]
    fn dvb_string_charsets() {
        assert_eq!(dvb_string(b""), "");
        assert_eq!(dvb_string(b"P1"), "P1");
        // Latin-1 sous préfixe.
        assert_eq!(dvb_string(&[0x10, 0x00, 0x01, 0xE9]), "\u{e9}");
        assert_eq!(dvb_string(&[0x15, 0xC3, 0xA9]), "\u{e9}"); // UTF-8
        assert_eq!(dvb_string(&[0x05, b'a', b'b']), "\u{FFFD}\u{FFFD}"); // autre jeu
        assert_eq!(dvb_string(&[0x10, 0x00, 0x05, b'a']), "\u{FFFD}");
        assert_eq!(dvb_string(&[0x10]), "");
        // UTF-8 invalide : remplacement, sans panique.
        assert_eq!(dvb_string(&[0x15, 0xFF]), "\u{FFFD}");
        assert_eq!(dvb_string(&[0x15]), "");
    }

    #[test]
    fn dvb_string_iso6937_default_table() {
        assert_eq!(dvb_string(&[0xC2, b'e']), "\u{e9}");
        assert_eq!(dvb_string(&[0xC1, b'a']), "\u{e0}");
        assert_eq!(dvb_string(&[0xCB, b'c']), "\u{e7}");
        assert_eq!(
            dvb_string(&[b'M', b'A', b' ', b'R', 0xC2, b'e', b'f']),
            "MA R\u{e9}f"
        );
        // Diacritique sans forme précomposée : lettre + signe combinant.
        assert_eq!(dvb_string(&[0xC1, b'x']), "x\u{300}");
        // Diacritique en fin de texte ou octet non défini.
        assert_eq!(dvb_string(&[b'a', 0xC2]), "a\u{FFFD}");
        assert_eq!(dvb_string(&[0xC0]), "\u{FFFD}");
        // Symboles de 0xA0 à 0xFF (différents de Latin-1).
        assert_eq!(
            dvb_string(&[0xE9, 0xFB, 0xE1, 0xA4, 0xB5]),
            "\u{d8}\u{df}\u{c6}\u{20ac}\u{b5}"
        );
        assert_eq!(dvb_string(&[0xC8, b'u', 0xC8, b'O']), "\u{fc}\u{d6}");
    }

    #[test]
    fn dvb_string_strips_control_codes() {
        assert_eq!(dvb_string(&[b'a', 0x86, b'b', 0x87, 0x8A, b'c']), "abc");
        assert_eq!(
            dvb_string(&[0x10, 0x00, 0x01, b'a', 0x80, 0x9F, b'b']),
            "ab"
        );
        assert_eq!(
            dvb_string(&[
                0x15, b'a', 0xC2, 0x86, 0xEE, 0x82, 0x86, 0xEE, 0x82, 0x87, 0xEE, 0x82, 0x8A, b'b'
            ]),
            "ab"
        );
    }

    #[test]
    fn pat_parses_programs_and_skips_network_entry() {
        let s = parse(&pat(7, 1, &[(0, 0x10), (1, 0x1000), (2, 0x1001)]));
        let p = parse_pat(&s).unwrap();
        assert_eq!(p.ts_id, 7);
        assert_eq!(p.programs, vec![(1, 0x1000), (2, 0x1001)]);
        // Longueur non multiple de 4 : refusée.
        let mut bad = s.clone();
        bad.body.push(0);
        assert!(parse_pat(&bad).is_none());
        // Mauvaise table.
        let mut other = s;
        other.table_id = 2;
        assert!(parse_pat(&other).is_none());
    }

    #[test]
    fn pmt_parses_streams_and_descriptors() {
        let sub = descriptor(0x59, b"fra\x10\x00\x01\x00\x01");
        let reg = descriptor(0x05, b"CUEI");
        let bytes = pmt(
            1,
            2,
            0x100,
            &reg,
            &[(0x24, 0x100, &[]), (0x06, 0x103, &sub), (0x86, 0x1F4, &reg)],
        );
        let p = parse_pmt(&parse(&bytes)).unwrap();
        assert_eq!((p.program_number, p.version, p.pcr_pid), (1, 2, 0x100));
        assert_eq!(
            p.descriptors,
            vec![Descriptor {
                tag: 5,
                data: b"CUEI".to_vec()
            }]
        );
        assert_eq!(p.streams.len(), 3);
        assert_eq!(p.streams[1].pid, 0x103);
        assert_eq!(p.streams[1].descriptors[0].tag, 0x59);
        // Toute troncature du corps est refusée ou lue sans panique.
        let s = parse(&bytes);
        for n in 0..s.body.len() {
            let mut t = s.clone();
            t.body.truncate(n);
            let _ = parse_pmt(&t);
        }
        let mut t = s.clone();
        t.body.truncate(3);
        assert!(parse_pmt(&t).is_none());
        // Descripteur dont la longueur dépasse la boucle.
        let bad = pmt(1, 0, 0x100, &[0x05, 9, 1], &[]);
        assert!(parse_pmt(&parse(&bad)).is_none());
        let bad = pmt(1, 0, 0x100, &[], &[(0x1B, 0x100, &[0x05, 9, 1])]);
        assert!(parse_pmt(&parse(&bad)).is_none());
    }

    #[test]
    fn sdt_parses_service_names() {
        let bytes = sdt(3, 0, &[(1, "Media Analyzer", "MA Référence"), (2, "", "")]);
        let s = parse_sdt(&parse(&bytes)).unwrap();
        assert_eq!(s.ts_id, 3);
        assert_eq!(s.services.len(), 2);
        assert_eq!(s.services[0].service_id, 1);
        assert_eq!(s.services[0].provider, "Media Analyzer");
        assert_eq!(s.services[0].name, "MA Référence");
        assert_eq!(s.services[1].name, "");
        let t = parse(&bytes);
        for n in 0..t.body.len() {
            let mut u = t.clone();
            u.body.truncate(n);
            let _ = parse_sdt(&u);
        }
        let mut other = t;
        other.table_id = 0x02;
        assert!(parse_sdt(&other).is_none());
    }

    fn d(tag: u8, data: &[u8]) -> Vec<Descriptor> {
        vec![Descriptor {
            tag,
            data: data.to_vec(),
        }]
    }

    #[test]
    fn stream_kind_classification() {
        use AudioKind::{Aac, AacLatm, Ac3, Ac4, Eac3, Mpeg};
        use StreamKind::{Audio, Data, Scte35, Subtitles, Teletext, Video};
        let none: &[Descriptor] = &[];
        for (st, want) in [
            (0x1B, Video(Some(Codec::H264))),
            (0x24, Video(Some(Codec::Hevc))),
            (0x02, Video(Some(Codec::Mpeg2))),
            (0xEA, Video(None)),
            (0x0F, Audio(Some(Aac))),
            (0x11, Audio(Some(AacLatm))),
            (0x03, Audio(Some(Mpeg))),
            (0x04, Audio(Some(Mpeg))),
            (0x81, Audio(Some(Ac3))),
            (0x87, Audio(Some(Eac3))),
            (0x86, Scte35),
            (0x06, Data),
            (0x15, Data),
        ] {
            assert_eq!(stream_kind(st, none), want, "{st:#x}");
        }
        assert_eq!(stream_kind(0x06, &d(0x6A, &[])), Audio(Some(Ac3)));
        assert_eq!(stream_kind(0x06, &d(0x7A, &[])), Audio(Some(Eac3)));
        assert_eq!(stream_kind(0x06, &d(0x59, &[])), Subtitles);
        assert_eq!(stream_kind(0x06, &d(0x56, &[])), Teletext);
        assert_eq!(stream_kind(0x06, &d(0x7F, &[0x15, 0])), Audio(Some(Ac4)));
        assert_eq!(stream_kind(0x06, &d(0x7F, &[0x20])), Data);
        assert_eq!(stream_kind(0x06, &d(0x7F, &[])), Data);
    }
}
