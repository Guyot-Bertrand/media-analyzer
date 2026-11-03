//! Écrivain de bits (tests et génération de données uniquement).

/// Écrivain MSB d'abord.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    nbits: u64,
}

impl BitWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn push_bit(&mut self, b: bool) {
        if self.nbits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if b {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> (self.nbits % 8);
        }
        self.nbits += 1;
    }

    /// Écrit les `n` bits de poids faible de `v` (`n` ≤ 64, sinon borné à 64).
    pub fn u(&mut self, n: u32, v: u64) {
        for i in (0..n.min(64)).rev() {
            self.push_bit((v >> i) & 1 == 1);
        }
    }

    pub fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.u(len - 1, 0);
        self.u(len, x);
    }

    pub fn se(&mut self, v: i32) {
        let k = if v > 0 {
            2 * i64::from(v) - 1
        } else {
            -2 * i64::from(v)
        };
        self.ue(u32::try_from(k).unwrap_or(u32::MAX));
    }

    /// Ajoute `rbsp_trailing_bits` (un 1 puis des 0 jusqu'à l'octet) et rend les octets.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.push_bit(true);
        self.bytes
    }
}

/// Insère les octets d'échappement `0x03` (`00 00 0x` avec `x` ≤ 3).
#[must_use]
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64);
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BitReader, rbsp};
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        U(u32, u64),
        Ue(u32),
        Se(i32),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1u32..=64, any::<u64>())
                .prop_map(|(n, v)| { Op::U(n, if n == 64 { v } else { v & ((1 << n) - 1) }) }),
            any::<u32>().prop_map(|v| Op::Ue(v.min(u32::MAX - 1))),
            (i32::MIN + 1..=i32::MAX).prop_map(Op::Se),
        ]
    }

    proptest! {
        #[test]
        fn writer_roundtrip(ops in proptest::collection::vec(op(), 0..40)) {
            let mut w = BitWriter::new();
            for o in &ops {
                match *o { Op::U(n, v) => w.u(n, v), Op::Ue(v) => w.ue(v), Op::Se(v) => w.se(v) }
            }
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            for o in &ops {
                match *o {
                    Op::U(n, v) => prop_assert_eq!(r.u(n), Ok(v)),
                    Op::Ue(v) => prop_assert_eq!(r.ue(), Ok(v)),
                    Op::Se(v) => prop_assert_eq!(r.se(), Ok(v)),
                }
            }
            prop_assert!(!r.more_rbsp_data());
        }

        #[test]
        fn escape_roundtrip(data in proptest::collection::vec(prop_oneof![Just(0u8), Just(1), Just(3), any::<u8>()], 0..64)) {
            let e = escape(&data);
            prop_assert_eq!(rbsp(&e).data, data);
        }
    }

    #[test]
    fn escape_examples() {
        assert_eq!(escape(&[0, 0, 1]), [0, 0, 3, 1]);
        assert_eq!(escape(&[0, 0, 0, 0]), [0, 0, 3, 0, 0]);
        assert_eq!(escape(&[0, 0, 4]), [0, 0, 4]);
    }

    #[test]
    fn ue_zero_and_se_signs() {
        let mut w = BitWriter::new();
        w.ue(0);
        w.se(1);
        w.se(-1);
        w.se(0);
        w.u(70, 1); // borné à 64
        assert_eq!(w.finish()[0], 0b1010_0111);
    }
}
