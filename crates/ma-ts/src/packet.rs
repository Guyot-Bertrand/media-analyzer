//! Paquets de transport MPEG-2 (ISO/IEC 13818-1 §2.4.3) : formats de paquet, détection par
//! périodicité de l'octet de synchronisation, décodage de l'en-tête et du champ d'adaptation.

use ma_core::Pcr;

/// Octet de synchronisation.
pub(crate) const SYNC_BYTE: u8 = 0x47;
/// Taille d'un paquet TS nu.
pub const TS_PACKET_SIZE: usize = 188;
/// Nombre de synchronisations consécutives exigées pour reconnaître le format.
pub(crate) const SYNC_RUN: usize = 5;
/// Zone explorée par `detect_format` pour la première synchronisation.
const DETECT_WINDOW: usize = 64 * 1024;

/// Format d'enregistrement des paquets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketFormat {
    /// 188 octets.
    Ts188,
    /// 192 octets : horodatage de 4 octets (27 MHz) devant le paquet (TTS, M2TS).
    Tts192,
    /// 204 octets : 16 octets de Reed-Solomon derrière le paquet.
    Rs204,
    /// 208 octets : 20 octets de Reed-Solomon derrière le paquet.
    Ts208,
}

impl PacketFormat {
    /// Période (octets) entre deux paquets.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            PacketFormat::Ts188 => 188,
            PacketFormat::Tts192 => 192,
            PacketFormat::Rs204 => 204,
            PacketFormat::Ts208 => 208,
        }
    }

    /// Position de l'octet 0x47 dans la période (4 pour `Tts192`, 0 sinon).
    #[must_use]
    pub fn sync_offset(self) -> usize {
        match self {
            PacketFormat::Tts192 => 4,
            _ => 0,
        }
    }

    /// Faux pour `Tts192` : l'horodatage d'enregistrement remplace le temps d'arrivée réel,
    /// les analyses de timing (PCR, tampons) ne sont pas garanties.
    #[must_use]
    pub fn timing_reliable(self) -> bool {
        self != PacketFormat::Tts192
    }

    /// Octets à lire à partir de l'octet 0x47 pour que le paquet soit complet.
    pub(crate) fn tail(self) -> usize {
        self.size() - self.sync_offset()
    }
}

const FORMATS: [PacketFormat; 4] = [
    PacketFormat::Ts188,
    PacketFormat::Tts192,
    PacketFormat::Rs204,
    PacketFormat::Ts208,
];

/// Cherche le plus petit offset (< 64 Kio) où `SYNC_RUN` octets 0x47 consécutifs tombent à la
/// période d'un format (essayés dans l'ordre 188, 192, 204, 208). Renvoie le format et l'offset
/// du premier octet 0x47 (pour `Tts192`, le préfixe horodaté de 4 octets le précède).
#[must_use]
pub fn detect_format(head: &[u8]) -> Option<(PacketFormat, usize)> {
    let last = head.len().min(DETECT_WINDOW + 1);
    (0..last).find_map(|off| {
        if head[off] != SYNC_BYTE {
            return None;
        }
        FORMATS.into_iter().find_map(|f| {
            (1..SYNC_RUN)
                .all(|k| head.get(off + k * f.size()) == Some(&SYNC_BYTE))
                .then_some((f, off))
        })
    })
}

/// En-tête d'un paquet TS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "drapeaux de l'en-tête normatif"
)]
pub struct PacketHeader {
    pub tei: bool,
    pub pusi: bool,
    pub priority: bool,
    pub pid: u16,
    pub scrambling: u8,
    pub afc: u8,
    pub cc: u8,
}

/// Champ d'adaptation (drapeaux utiles et PCR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptationField {
    pub discontinuity: bool,
    pub random_access: bool,
    pub pcr: Option<Pcr>,
}

/// Paquet décodé ; `payload` emprunte le tampon d'origine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub header: PacketHeader,
    pub adaptation: Option<AdaptationField>,
    pub payload: &'a [u8],
}

/// Erreur de décodage d'un paquet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("octet de synchronisation invalide")]
    SyncByte,
    #[error("longueur du champ d'adaptation supérieure à la place disponible")]
    AdaptationLength,
}

/// Longueur déclarée du champ d'adaptation si le paquet en porte un.
pub(crate) fn adaptation_length(p: &[u8; TS_PACKET_SIZE]) -> Option<u8> {
    (p[3] & 0x20 != 0).then_some(p[4])
}

/// Décode un paquet de 188 octets. Pour 192/204/208, l'appelant fournit la fenêtre de 188 octets
/// qui commence à l'octet 0x47.
pub fn parse_packet(p: &[u8; TS_PACKET_SIZE]) -> Result<Packet<'_>, PacketError> {
    if p[0] != SYNC_BYTE {
        return Err(PacketError::SyncByte);
    }
    let header = PacketHeader {
        tei: p[1] & 0x80 != 0,
        pusi: p[1] & 0x40 != 0,
        priority: p[1] & 0x20 != 0,
        pid: u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]),
        scrambling: p[3] >> 6,
        afc: (p[3] >> 4) & 3,
        cc: p[3] & 0x0F,
    };
    let mut payload_start = 4;
    let mut adaptation = None;
    if let Some(len) = adaptation_length(p) {
        let len = usize::from(len);
        if len > TS_PACKET_SIZE - 5 {
            return Err(PacketError::AdaptationLength);
        }
        payload_start = 5 + len;
        adaptation = Some(if len == 0 {
            AdaptationField {
                discontinuity: false,
                random_access: false,
                pcr: None,
            }
        } else {
            let flags = p[5];
            let pcr = (flags & 0x10 != 0 && len >= 7).then(|| {
                let b = &p[6..12];
                Pcr {
                    base: u64::from(b[0]) << 25
                        | u64::from(b[1]) << 17
                        | u64::from(b[2]) << 9
                        | u64::from(b[3]) << 1
                        | u64::from(b[4] >> 7),
                    ext: u16::from(b[4] & 1) << 8 | u16::from(b[5]),
                }
            });
            AdaptationField {
                discontinuity: flags & 0x80 != 0,
                random_access: flags & 0x40 != 0,
                pcr,
            }
        });
    }
    let payload = if header.afc & 1 != 0 {
        &p[payload_start..]
    } else {
        &[]
    };
    Ok(Packet {
        header,
        adaptation,
        payload,
    })
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::testutil::{AfSpec, packet, repacketize};
    use ma_core::Pcr;

    fn stream(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| packet(0x100, i == 0, (i % 16) as u8, None, &[i as u8; 10]))
            .collect()
    }

    #[test]
    fn parse_packet_fields() {
        let pcr = Pcr {
            base: 0x1_2345_6789,
            ext: 299,
        };
        let af = AfSpec {
            discontinuity: true,
            random_access: true,
            pcr: Some(pcr),
        };
        let raw = packet(0x1FFE, true, 7, Some(af), &[9, 8, 7]);
        let p = parse_packet(&raw).unwrap();
        assert_eq!(p.header.pid, 0x1FFE);
        assert_eq!(p.header.cc, 7);
        assert!(p.header.pusi && !p.header.tei && !p.header.priority);
        assert_eq!((p.header.scrambling, p.header.afc), (0, 3));
        let a = p.adaptation.unwrap();
        assert!(a.discontinuity && a.random_access);
        assert_eq!(a.pcr, Some(pcr));
        assert_eq!(p.payload, &[9, 8, 7]);
    }

    #[test]
    fn parse_packet_flags_and_payload_only() {
        let mut raw = packet(0x20, false, 15, None, &[1, 2]);
        raw[1] |= 0x80 | 0x20; // tei + priority
        raw[3] |= 0xC0; // scrambling 3
        let p = parse_packet(&raw).unwrap();
        assert!(p.header.tei && p.header.priority && !p.header.pusi);
        assert_eq!((p.header.scrambling, p.header.afc, p.header.cc), (3, 1, 15));
        assert!(p.adaptation.is_none());
        assert_eq!(p.payload.len(), 184);
        assert_eq!(&p.payload[..2], &[1, 2]);
    }

    #[test]
    fn adaptation_only_and_reserved_have_no_payload() {
        let raw = packet(0x30, false, 0, Some(AfSpec::default()), &[]);
        let p = parse_packet(&raw).unwrap();
        assert!(p.payload.is_empty());
        assert_eq!(p.adaptation.unwrap().pcr, None);
        let mut raw = packet(0x30, false, 0, None, &[]);
        raw[3] &= !0x30; // afc = 0 (réservé)
        let p = parse_packet(&raw).unwrap();
        assert!(p.payload.is_empty() && p.adaptation.is_none());
    }

    #[test]
    fn pcr_extension_is_masked_to_nine_bits_and_kept_above_299() {
        let mut raw = packet(0x30, false, 0, Some(AfSpec::default()), &[]);
        raw[5] |= 0x10; // drapeau PCR
        raw[6..12].copy_from_slice(&[0xFF; 6]);
        let a = parse_packet(&raw).unwrap().adaptation.unwrap();
        assert_eq!(
            a.pcr,
            Some(Pcr {
                base: (1 << 33) - 1,
                ext: 511
            })
        );
    }

    #[test]
    fn pcr_flag_with_short_field_is_ignored() {
        let mut raw = packet(0x30, false, 0, None, &[]);
        raw[3] = 0x30; // afc 3
        raw[4] = 3; // champ trop court pour un PCR
        raw[5] = 0x10;
        let p = parse_packet(&raw).unwrap();
        assert_eq!(p.adaptation.unwrap().pcr, None);
        assert_eq!(p.payload.len(), 180);
    }

    #[test]
    fn zero_length_adaptation_field_is_valid() {
        let mut raw = packet(0x30, false, 0, None, &[]);
        raw[3] = 0x30;
        raw[4] = 0;
        let p = parse_packet(&raw).unwrap();
        let a = p.adaptation.unwrap();
        assert!(!a.discontinuity && !a.random_access && a.pcr.is_none());
        assert_eq!(p.payload.len(), 183);
    }

    #[test]
    fn bad_sync_byte_is_an_error() {
        let mut raw = packet(0x30, false, 0, None, &[]);
        raw[0] = 0x46;
        assert_eq!(parse_packet(&raw).err(), Some(PacketError::SyncByte));
    }

    #[test]
    fn adaptation_length_overflow_is_an_error() {
        let mut raw = packet(0x30, false, 0, None, &[]);
        raw[3] = 0x30;
        raw[4] = 190;
        assert_eq!(
            parse_packet(&raw).err(),
            Some(PacketError::AdaptationLength)
        );
        assert_eq!(adaptation_length(&raw), Some(190));
        raw[3] = 0x10;
        assert_eq!(adaptation_length(&raw), None);
    }

    #[test]
    fn format_properties() {
        use PacketFormat::*;
        assert_eq!(
            [Ts188, Tts192, Rs204, Ts208].map(PacketFormat::size),
            [188, 192, 204, 208]
        );
        assert_eq!(
            [Ts188, Tts192, Rs204, Ts208].map(PacketFormat::sync_offset),
            [0, 4, 0, 0]
        );
        assert_eq!(
            [Ts188, Tts192, Rs204, Ts208].map(PacketFormat::timing_reliable),
            [true, false, true, true]
        );
    }

    #[test]
    fn detects_every_format_at_offset_zero_or_four() {
        let ts = stream(12);
        for (f, off) in [
            (PacketFormat::Ts188, 0),
            (PacketFormat::Tts192, 4),
            (PacketFormat::Rs204, 0),
            (PacketFormat::Ts208, 0),
        ] {
            assert_eq!(detect_format(&repacketize(&ts, f)), Some((f, off)), "{f:?}");
        }
    }

    #[test]
    fn detection_skips_leading_garbage_and_needs_five_syncs() {
        let ts = stream(12);
        let mut data = vec![0x47, 0x00, 0x47]; // faux départs
        data.extend(&ts);
        assert_eq!(detect_format(&data), Some((PacketFormat::Ts188, 3)));
        assert_eq!(detect_format(&ts[..4 * 188]), None);
        assert_eq!(
            detect_format(&ts[..=4 * 188]),
            Some((PacketFormat::Ts188, 0))
        );
        assert_eq!(detect_format(&[]), None);
        assert_eq!(detect_format(&[0u8; 5000]), None);
    }

    #[test]
    fn detection_gives_up_beyond_64_kib() {
        let mut data = vec![0u8; 70_000];
        data.extend(stream(10));
        assert_eq!(detect_format(&data), None);
        let mut data = vec![0u8; 65_536];
        data.extend(stream(10));
        assert_eq!(detect_format(&data), Some((PacketFormat::Ts188, 65_536)));
    }
}
