//! Horodatages MPEG : fréquences et déroulement des compteurs qui bouclent.

/// Fréquence de l'horloge PTS/DTS (Hz).
pub const PTS_HZ: u64 = 90_000;
/// Fréquence de l'horloge PCR (Hz).
pub const PCR_HZ: u64 = 27_000_000;

/// Déroule un compteur de `bits` bits qui boucle (PTS, DTS, base PCR) en une valeur continue.
#[derive(Debug, Clone)]
pub struct TsUnwrapper {
    modulus: i64,
    last: Option<i64>,
}

impl TsUnwrapper {
    /// `bits` vaut 33 pour PTS/DTS et pour la base PCR.
    #[must_use]
    pub fn new(bits: u32) -> Self {
        Self {
            modulus: 1i64 << bits.clamp(1, 62),
            last: None,
        }
    }

    /// Renvoie `raw + k·2^bits`, avec `k` tel que l'écart au dernier résultat soit minimal. Le
    /// premier appel renvoie `raw` tel quel.
    #[allow(
        clippy::should_implement_trait,
        reason = "interface du plan : méthode d'instance"
    )]
    pub fn unwrap(&mut self, raw: u64) -> i64 {
        let raw = i64::try_from(raw).unwrap_or(i64::MAX) & (self.modulus - 1);
        let v = match self.last {
            None => raw,
            Some(last) => {
                // Écart ramené dans [-modulus/2, modulus/2).
                let half = self.modulus / 2;
                let delta = (raw - last).rem_euclid(self.modulus);
                let delta = if delta >= half {
                    delta - self.modulus
                } else {
                    delta
                };
                last.saturating_add(delta)
            }
        };
        self.last = Some(v);
        v
    }
}

/// Référence d'horloge de programme : base 33 bits (90 kHz) et extension (0..300, 27 MHz).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pcr {
    /// 33 bits.
    pub base: u64,
    /// Strictement inférieure à 300.
    pub ext: u16,
}

impl Pcr {
    /// Valeur en ticks de 27 MHz : `base × 300 + ext`.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.base * 300 + u64::from(self.ext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pts_unwrap_crosses_33_bits() {
        let mut u = TsUnwrapper::new(33);
        let top = (1u64 << 33) - 90;
        assert_eq!(u.unwrap(top), i64::try_from(top).unwrap());
        assert_eq!(u.unwrap(10), (1i64 << 33) + 10);
    }

    #[test]
    fn small_backward_jump_is_not_a_wrap() {
        let mut u = TsUnwrapper::new(33);
        assert_eq!(u.unwrap(1000), 1000);
        assert_eq!(u.unwrap(900), 900);
    }

    #[test]
    fn pcr_ticks() {
        assert_eq!(Pcr { base: 1, ext: 299 }.ticks(), 599);
    }
}
