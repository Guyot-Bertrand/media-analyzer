//! Lecteur de bits MSB d'abord, sans panique quelle que soit l'entrée.

use thiserror::Error;

/// Erreur de lecture de bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BitError {
    /// Fin des données atteinte : `wanted` bits demandés à partir de `at_bit`.
    #[error("données tronquées au bit {at_bit} ({wanted} bits demandés)")]
    Truncated { at_bit: u64, wanted: u32 },
    /// Code Exp-Golomb de plus de 31 zéros de tête.
    #[error("code Exp-Golomb trop long au bit {at_bit}")]
    ExpGolombTooLong { at_bit: u64 },
}

/// Lecteur de bits sur une tranche d'octets.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: u64,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Position courante, en bits depuis le début.
    #[must_use]
    pub fn bit_pos(&self) -> u64 {
        self.pos
    }

    /// Nombre de bits restants.
    #[must_use]
    pub fn bits_left(&self) -> u64 {
        (self.data.len() as u64 * 8).saturating_sub(self.pos)
    }

    /// Lit `n` bits (`n` ≤ 64). `u(0)` vaut 0 ; `n > 64` est une erreur
    /// `Truncated` (jamais une panique) et ne consomme rien.
    pub fn u(&mut self, n: u32) -> Result<u64, BitError> {
        if n == 0 {
            return Ok(0);
        }
        if n > 64 || u64::from(n) > self.bits_left() {
            return Err(BitError::Truncated {
                at_bit: self.pos,
                wanted: n,
            });
        }
        let mut value = 0u64;
        let mut left = n;
        while left > 0 {
            // Les octets existent : bits_left a été vérifié.
            let byte = self.data[(self.pos / 8) as usize];
            let used = (self.pos % 8) as u32;
            let take = (8 - used).min(left);
            let bits = (u64::from(byte) >> (8 - used - take)) & ((1u64 << take) - 1);
            value = if take == 64 {
                bits
            } else {
                (value << take) | bits
            };
            self.pos += u64::from(take);
            left -= take;
        }
        Ok(value)
    }

    pub fn flag(&mut self) -> Result<bool, BitError> {
        Ok(self.u(1)? == 1)
    }

    /// Exp-Golomb non signé, au plus 31 zéros de tête. En cas d'erreur la
    /// position n'est pas modifiée.
    pub fn ue(&mut self) -> Result<u32, BitError> {
        let start = self.pos;
        let mut zeros = 0u32;
        loop {
            match self.flag() {
                Ok(true) => break,
                Ok(false) => {
                    zeros += 1;
                    if zeros > 31 {
                        self.pos = start;
                        return Err(BitError::ExpGolombTooLong { at_bit: start });
                    }
                }
                Err(_) => {
                    self.pos = start;
                    return Err(BitError::Truncated {
                        at_bit: start,
                        wanted: zeros + 1,
                    });
                }
            }
        }
        let Ok(suffix) = self.u(zeros) else {
            self.pos = start;
            return Err(BitError::Truncated {
                at_bit: start,
                wanted: 2 * zeros + 1,
            });
        };
        // zeros ≤ 31 : (1 << zeros) - 1 + suffix ≤ 2^32 - 2 tient dans u32.
        Ok(u32::try_from((1u64 << zeros) - 1 + suffix).unwrap_or(u32::MAX))
    }

    /// Exp-Golomb signé : 1 → 1, 2 → −1, 3 → 2…
    pub fn se(&mut self) -> Result<i32, BitError> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(i32::try_from(v).unwrap_or(i32::MAX))
    }

    /// Saute `n` bits.
    pub fn skip(&mut self, n: u64) -> Result<(), BitError> {
        if n > self.bits_left() {
            return Err(BitError::Truncated {
                at_bit: self.pos,
                wanted: u32::try_from(n).unwrap_or(u32::MAX),
            });
        }
        self.pos += n;
        Ok(())
    }

    /// Avance jusqu'à la prochaine frontière d'octet (sans dépasser la fin).
    pub fn byte_align(&mut self) {
        self.pos = self.pos.next_multiple_of(8).min(self.data.len() as u64 * 8);
    }

    #[must_use]
    pub fn is_byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    /// Vrai s'il reste des bits de données avant le dernier bit à 1
    /// (`rbsp_stop_one_bit`) de la charge utile.
    #[must_use]
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        let stop_bit = last as u64 * 8 + 7 - u64::from(self.data[last].trailing_zeros());
        self.pos < stop_bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u_reads_across_bytes() {
        let mut r = BitReader::new(&[0xAC, 0x50]);
        assert_eq!(r.u(3), Ok(5));
        assert_eq!(r.u(7), Ok(49));
        assert_eq!(r.u(6), Ok(16));
        assert_eq!(r.bits_left(), 0);
    }

    #[test]
    fn u_zero_and_oversize() {
        let mut r = BitReader::new(&[0xFF; 16]);
        assert_eq!(r.u(0), Ok(0));
        assert_eq!(
            r.u(65),
            Err(BitError::Truncated {
                at_bit: 0,
                wanted: 65
            })
        );
        assert_eq!(r.u(64), Ok(u64::MAX));
        assert_eq!(r.u(63), Ok((1 << 63) - 1));
    }

    #[test]
    fn ue_se_values() {
        let mut r = BitReader::new(&[0xA6, 0x40]);
        assert_eq!(r.ue(), Ok(0));
        assert_eq!(r.ue(), Ok(1));
        assert_eq!(r.ue(), Ok(2));
        assert_eq!(r.ue(), Ok(3));
        // 010, 011, 00100 = 0100 1100 100 -> 0x4C 0x80
        let mut r = BitReader::new(&[0x4C, 0x80]);
        assert_eq!(r.se(), Ok(1));
        assert_eq!(r.se(), Ok(-1));
        assert_eq!(r.se(), Ok(2));
    }

    #[test]
    fn truncated_read_is_an_error_not_a_panic() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(
            r.u(9),
            Err(BitError::Truncated {
                at_bit: 0,
                wanted: 9
            })
        );
        assert_eq!(r.bit_pos(), 0);
        assert!(r.skip(9).is_err());
        assert!(r.skip(8).is_ok());
        assert!(r.flag().is_err());
    }

    #[test]
    fn exp_golomb_longer_than_31_is_rejected() {
        let mut r = BitReader::new(&[0, 0, 0, 0, 0]);
        assert_eq!(r.ue(), Err(BitError::ExpGolombTooLong { at_bit: 0 }));
        assert_eq!(r.bit_pos(), 0);
        assert_eq!(r.se(), Err(BitError::ExpGolombTooLong { at_bit: 0 }));
    }

    #[test]
    fn exp_golomb_truncated() {
        // 7 zéros, le 1, puis 7 bits de suffixe manquants
        let mut r = BitReader::new(&[0x01]);
        assert!(matches!(r.ue(), Err(BitError::Truncated { at_bit: 0, .. })));
        let mut r = BitReader::new(&[0x00]);
        assert!(matches!(r.ue(), Err(BitError::Truncated { .. })));
        assert_eq!(r.bit_pos(), 0);
    }

    #[test]
    fn max_ue() {
        // 31 zéros, 1, puis 31 bits à 1 => 2^32 - 2
        let w = [0u8, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xFE];
        let mut r = BitReader::new(&w);
        assert_eq!(r.ue(), Ok(u32::MAX - 1));
    }

    #[test]
    fn align_and_more_rbsp_data() {
        let mut r = BitReader::new(&[0xA0]);
        assert!(r.more_rbsp_data());
        r.u(1).unwrap();
        assert!(r.more_rbsp_data());
        r.u(1).unwrap();
        assert!(!r.more_rbsp_data());
        assert!(!BitReader::new(&[0, 0]).more_rbsp_data());
        let mut r = BitReader::new(&[0xFF, 0xFF]);
        r.u(3).unwrap();
        assert!(!r.is_byte_aligned());
        r.byte_align();
        assert!(r.is_byte_aligned());
        assert_eq!(r.bit_pos(), 8);
        r.byte_align();
        assert_eq!(r.bit_pos(), 8);
    }

    #[test]
    fn more_rbsp_data_detects_trailing_bits() {
        let mut r = BitReader::new(&[0xA0]);
        r.u(1).unwrap();
        assert!(r.more_rbsp_data());
        r.u(1).unwrap();
        assert!(!r.more_rbsp_data());
    }
}
