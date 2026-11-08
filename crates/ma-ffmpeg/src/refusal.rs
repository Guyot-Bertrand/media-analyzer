//! Mémo des viewports que la VPP VA-API refuse, tolérant aux erreurs passagères.
//!
//! Un viewport n'est refusé qu'après [`FAILURES_TO_REFUSE`] échecs consécutifs de la VPP ; il est
//! réessayé après [`RETRY_AFTER`] conversions. Un succès efface ses échecs. Le mémo garde au plus
//! [`MAX_ENTRIES`] viewports.

use ma_decode::Viewport;

/// Échecs consécutifs de la VPP avant de refuser un viewport.
pub(crate) const FAILURES_TO_REFUSE: u32 = 2;
/// Conversions après lesquelles un viewport refusé est réessayé.
pub(crate) const RETRY_AFTER: u64 = 250;
/// Viewports retenus au plus.
pub(crate) const MAX_ENTRIES: usize = 16;

/// Viewport retenu : échecs consécutifs de la VPP, et rang de la conversion du dernier échec.
#[derive(Debug, Clone, Copy)]
struct Entry {
    vp: Viewport,
    failures: u32,
    failed_at: u64,
}

/// Mémo des refus de la VPP.
///
/// Éviction : quand le mémo est plein, l'entrée dont le dernier échec est le plus ancien est
/// retirée (les entrées sont rangées par dernier échec, du plus ancien au plus récent).
#[derive(Debug, Default)]
pub struct RefusalMemo {
    /// Du dernier échec le plus ancien au plus récent.
    entries: Vec<Entry>,
    /// Conversions comptées par [`RefusalMemo::tick`].
    conversions: u64,
}

impl RefusalMemo {
    /// Vrai si la VPP est réputée refuser `vp` : au moins [`FAILURES_TO_REFUSE`] échecs
    /// consécutifs, le dernier il y a moins de [`RETRY_AFTER`] conversions.
    #[must_use]
    pub fn refuses(&self, vp: &Viewport) -> bool {
        self.entries.iter().any(|e| {
            e.vp == *vp
                && e.failures >= FAILURES_TO_REFUSE
                && self.conversions - e.failed_at < RETRY_AFTER
        })
    }

    /// La VPP a échoué sur `vp` : un échec consécutif de plus. Le viewport passe en dernière
    /// position (le plus récent) ; si le mémo est plein, le plus ancien est évincé.
    pub fn failed(&mut self, vp: Viewport) {
        let failures = if let Some(i) = self.entries.iter().position(|e| e.vp == vp) {
            self.entries.remove(i).failures.saturating_add(1)
        } else {
            if self.entries.len() >= MAX_ENTRIES {
                self.entries.remove(0);
            }
            1
        };
        self.entries.push(Entry {
            vp,
            failures,
            failed_at: self.conversions,
        });
    }

    /// La VPP a produit `vp` : ses échecs sont oubliés.
    pub fn succeeded(&mut self, vp: &Viewport) {
        self.entries.retain(|e| e.vp != *vp);
    }

    /// Une conversion a eu lieu.
    pub fn tick(&mut self) {
        self.conversions = self.conversions.saturating_add(1);
    }

    /// Viewports retenus.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ma_decode::{Rect, Size};

    fn vp(w: u32) -> Viewport {
        Viewport {
            region: Rect {
                x: 0,
                y: 0,
                w,
                h: 64,
            },
            out: Size { w, h: 64 },
        }
    }

    #[test]
    fn refusal_memo_needs_two_failures() {
        let mut m = RefusalMemo::default();
        let (a, b) = (vp(64), vp(128));
        m.failed(a);
        assert!(!m.refuses(&a), "un seul échec : erreur passagère tolérée");
        m.tick();
        m.failed(a);
        assert!(m.refuses(&a), "deux échecs consécutifs");
        assert!(!m.refuses(&b), "un autre viewport n'est pas touché");
    }

    #[test]
    fn refusal_memo_retries_after_250_conversions() {
        let mut m = RefusalMemo::default();
        let a = vp(64);
        m.failed(a);
        m.failed(a);
        for _ in 0..RETRY_AFTER - 1 {
            m.tick();
            assert!(m.refuses(&a));
        }
        m.tick();
        assert!(!m.refuses(&a), "réessayé après 250 conversions");
        // Nouvel échec : refusé de nouveau, pour 250 conversions.
        m.failed(a);
        assert!(m.refuses(&a));
        for _ in 0..RETRY_AFTER {
            m.tick();
        }
        assert!(!m.refuses(&a));
        assert_eq!(RETRY_AFTER, 250);
        assert_eq!(FAILURES_TO_REFUSE, 2);
    }

    #[test]
    fn success_resets_failure_count() {
        let mut m = RefusalMemo::default();
        let a = vp(64);
        m.failed(a);
        m.succeeded(&a);
        m.failed(a);
        assert!(!m.refuses(&a), "échecs non consécutifs");
        m.failed(a);
        assert!(m.refuses(&a));
        // Réessai réussi : le viewport n'est plus refusé, et un échec isolé est de nouveau toléré.
        m.succeeded(&a);
        assert!(!m.refuses(&a));
        m.failed(a);
        assert!(!m.refuses(&a));
    }

    #[test]
    fn memo_is_bounded_and_evicts_the_oldest() {
        let mut m = RefusalMemo::default();
        let all: Vec<Viewport> = (1..=17).map(|i| vp(16 * i)).collect();
        for v in &all[..MAX_ENTRIES] {
            m.failed(*v);
            m.failed(*v);
        }
        assert!(all[..MAX_ENTRIES].iter().all(|v| m.refuses(v)));
        // Le 17e évince le plus ancien (le premier échec le plus lointain).
        m.failed(all[16]);
        m.failed(all[16]);
        assert_eq!(m.len(), MAX_ENTRIES);
        assert!(!m.refuses(&all[0]), "le plus ancien est évincé");
        assert!(all[1..].iter().all(|v| m.refuses(v)));
        assert_eq!(MAX_ENTRIES, 16);
    }
}
