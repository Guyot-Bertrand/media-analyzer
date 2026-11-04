//! Outils de test : construction de paquets et re-paquetage (feature `testutil`, utilisée aussi
//! par `xtask` pour fabriquer les fichiers dérivés du corpus).

// Champs de bits écrits octet par octet : valeurs masquées ou bornées avant la conversion.
#![allow(clippy::cast_possible_truncation)]

use ma_core::Pcr;

use crate::packet::{PacketFormat, SYNC_BYTE, TS_PACKET_SIZE};

/// Champ d'adaptation à écrire.
#[derive(Debug, Clone, Copy, Default)]
pub struct AfSpec {
    pub discontinuity: bool,
    pub random_access: bool,
    pub pcr: Option<Pcr>,
}

/// Construit un paquet de 188 octets. Sans champ d'adaptation, la charge utile est complétée par
/// des 0xFF ; avec, le champ d'adaptation est bourré pour que la charge utile finisse le paquet
/// (une charge trop longue est tronquée).
#[must_use]
pub fn packet(pid: u16, pusi: bool, cc: u8, af: Option<AfSpec>, payload: &[u8]) -> [u8; 188] {
    let mut p = [0xFFu8; TS_PACKET_SIZE];
    p[0] = SYNC_BYTE;
    p[1] = u8::from(pusi) << 6 | ((pid >> 8) & 0x1F) as u8;
    p[2] = (pid & 0xFF) as u8;
    let Some(af) = af else {
        p[3] = 0x10 | (cc & 0x0F);
        let n = payload.len().min(TS_PACKET_SIZE - 4);
        p[4..4 + n].copy_from_slice(&payload[..n]);
        return p;
    };
    let needed = if af.pcr.is_some() { 7 } else { 1 };
    let n = payload.len().min(TS_PACKET_SIZE - 5 - needed);
    let af_len = TS_PACKET_SIZE - 5 - n;
    p[3] = if n == 0 { 0x20 } else { 0x30 } | (cc & 0x0F);
    p[4] = af_len as u8;
    p[5] = u8::from(af.discontinuity) << 7
        | u8::from(af.random_access) << 6
        | u8::from(af.pcr.is_some()) << 4;
    if let Some(pcr) = af.pcr {
        let base = pcr.base & ((1 << 33) - 1);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) as u8) << 7 | 0x7E | ((pcr.ext >> 8) & 1) as u8;
        p[11] = (pcr.ext & 0xFF) as u8;
    }
    p[5 + af_len..].copy_from_slice(&payload[..n]);
    p
}

/// Réécrit un flux de paquets de 188 octets dans le format `f` (le reste incomplet est ignoré) :
/// 204 et 208 ajoutent 16 et 20 octets 0x00 ; TTS ajoute un préfixe de 4 octets (compteur 27 MHz
/// croissant, big-endian).
#[must_use]
pub fn repacketize(ts: &[u8], f: PacketFormat) -> Vec<u8> {
    let (packets, _) = ts.as_chunks::<TS_PACKET_SIZE>();
    let mut out = Vec::with_capacity(packets.len() * f.size());
    for (i, p) in packets.iter().enumerate() {
        if f == PacketFormat::Tts192 {
            let ticks = (i as u32).wrapping_mul(1000) & 0x3FFF_FFFF;
            out.extend(ticks.to_be_bytes());
        }
        out.extend(p);
        out.resize(out.len() + f.size() - f.sync_offset() - TS_PACKET_SIZE, 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_is_188_bytes_with_sync_and_padding() {
        let p = packet(0x100, false, 3, None, &[1]);
        assert_eq!(p.len(), 188);
        assert_eq!((p[0], p[4], p[5], p[187]), (0x47, 1, 0xFF, 0xFF));
        // Charge utile trop longue : tronquée, jamais de panique.
        let p = packet(0x100, false, 3, Some(AfSpec::default()), &[7; 400]);
        assert_eq!(p[0], 0x47);
        let p = packet(
            0x100,
            false,
            3,
            Some(AfSpec {
                pcr: Some(ma_core::Pcr { base: 1, ext: 2 }),
                ..AfSpec::default()
            }),
            &[7; 400],
        );
        assert_eq!(p[4], 7);
    }

    #[test]
    fn repacketize_sizes_and_tts_counter() {
        let ts: Vec<u8> = (0..3)
            .flat_map(|i| packet(0x100, false, i, None, &[]))
            .collect();
        assert_eq!(repacketize(&ts, PacketFormat::Ts188), ts);
        let r = repacketize(&ts, PacketFormat::Rs204);
        assert_eq!(r.len(), 3 * 204);
        assert!(r[188..204].iter().all(|&b| b == 0));
        assert_eq!(repacketize(&ts, PacketFormat::Ts208).len(), 3 * 208);
        let t = repacketize(&ts, PacketFormat::Tts192);
        assert_eq!(t.len(), 3 * 192);
        assert_eq!(t[4], 0x47);
        let c = |i: usize| u32::from_be_bytes(t[i * 192..i * 192 + 4].try_into().unwrap());
        assert!(c(0) < c(1) && c(1) < c(2));
        // Reste incomplet ignoré.
        assert_eq!(repacketize(&ts[..400], PacketFormat::Rs204).len(), 2 * 204);
    }
}
