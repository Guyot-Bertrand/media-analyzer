//! Extraction de la RBSP : retrait des octets d'échappement `0x03`.

/// Charge utile d'une NAL sans octets d'échappement, avec de quoi retrouver
/// les offsets d'origine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rbsp {
    pub data: Vec<u8>,
    /// Positions (en octets, dans la NAL) des `0x03` retirés, croissantes.
    removed: Vec<u32>,
}

/// Retire les octets d'emulation prevention (`00 00 03` → `00 00`).
#[must_use]
pub fn rbsp(nal: &[u8]) -> Rbsp {
    let mut data = Vec::with_capacity(nal.len());
    let mut removed = Vec::new();
    let mut zeros = 0u32;
    for (i, &b) in nal.iter().enumerate() {
        if zeros >= 2 && b == 3 {
            removed.push(u32::try_from(i).unwrap_or(u32::MAX));
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        data.push(b);
    }
    Rbsp { data, removed }
}

impl Rbsp {
    /// Offset en bits dans la NAL correspondant à `rbsp_bit` dans la RBSP
    /// (pour la vue hexadécimale).
    #[must_use]
    pub fn nal_bit(&self, rbsp_bit: u64) -> u64 {
        let byte = rbsp_bit / 8;
        // Nombre d'octets retirés situés avant l'octet RBSP `byte` :
        // le k-ième retrait (position p dans la NAL) précède l'octet RBSP
        // d'indice p - k.
        let mut before = 0u64;
        for (k, &p) in self.removed.iter().enumerate() {
            if u64::from(p) - k as u64 <= byte {
                before += 1;
            } else {
                break;
            }
        }
        rbsp_bit + before * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbsp_removes_emulation_prevention_and_maps_offsets() {
        let r = rbsp(&[0, 0, 3, 1, 0xAA]);
        assert_eq!(r.data, [0, 0, 1, 0xAA]);
        assert_eq!(r.nal_bit(24), 32);
        assert_eq!(r.nal_bit(0), 0);
        assert_eq!(r.nal_bit(15), 15);
        assert_eq!(r.nal_bit(16), 24);
    }

    #[test]
    fn consecutive_escapes_and_trailing() {
        let r = rbsp(&[0, 0, 3, 0, 0, 3, 3, 0, 0, 3]);
        assert_eq!(r.data, [0, 0, 0, 0, 3, 0, 0]);
        assert_eq!(r.nal_bit(8 * 2), 8 * 3);
        assert_eq!(r.nal_bit(8 * 4), 8 * 6);
        assert_eq!(r.nal_bit(8 * 7), 8 * 10); // fin de données
        assert_eq!(rbsp(&[]).data.len(), 0);
    }
}
