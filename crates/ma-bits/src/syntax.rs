//! Enregistrement de la syntaxe lue : mode rapide (rien) ou arbre aplati.

use crate::reader::{BitError, BitReader};
use std::ops::Range;

/// Valeur d'un champ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    U(u64),
    S(i64),
    Flag(bool),
}

/// Observateur de la lecture. Générique (pas `dyn`) pour que `Fast` soit
/// entièrement optimisé.
pub trait Recorder {
    fn enter(&mut self, name: &'static str, bit: u64);
    fn exit(&mut self, bit: u64, error: Option<&'static str>);
    fn field(&mut self, name: &'static str, bit: u64, len: u32, value: Value);
}

/// Mode « rapide » : ne mémorise rien.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fast;

impl Recorder for Fast {
    #[inline(always)]
    fn enter(&mut self, _: &'static str, _: u64) {}
    #[inline(always)]
    fn exit(&mut self, _: u64, _: Option<&'static str>) {}
    #[inline(always)]
    fn field(&mut self, _: &'static str, _: u64, _: u32, _: Value) {}
}

/// Nœud de l'arbre syntaxique aplati (ordre de lecture, parent avant enfants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxNode {
    pub name: &'static str,
    pub bit_offset: u64,
    pub bit_len: u64,
    /// `None` pour un groupe.
    pub value: Option<Value>,
    pub error: Option<&'static str>,
    /// Indices dans `TreeRecorder::nodes` de **tout le sous-arbre** (enfants
    /// directs et descendants) ; toujours contigu, vide pour une feuille.
    pub children: Range<u32>,
}

/// Mode « détaillé » : arbre aplati.
#[derive(Debug, Default)]
pub struct TreeRecorder {
    pub nodes: Vec<SyntaxNode>,
    open: Vec<usize>,
}

fn idx32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Recorder for TreeRecorder {
    fn enter(&mut self, name: &'static str, bit: u64) {
        let i = self.nodes.len();
        self.nodes.push(SyntaxNode {
            name,
            bit_offset: bit,
            bit_len: 0,
            value: None,
            error: None,
            children: idx32(i + 1)..idx32(i + 1),
        });
        self.open.push(i);
    }

    fn exit(&mut self, bit: u64, error: Option<&'static str>) {
        if let Some(i) = self.open.pop() {
            let end = idx32(self.nodes.len());
            let n = &mut self.nodes[i];
            n.bit_len = bit.saturating_sub(n.bit_offset);
            n.error = error;
            n.children.end = end;
        }
    }

    fn field(&mut self, name: &'static str, bit: u64, len: u32, value: Value) {
        let i = idx32(self.nodes.len());
        self.nodes.push(SyntaxNode {
            name,
            bit_offset: bit,
            bit_len: u64::from(len),
            value: Some(value),
            error: None,
            children: i + 1..i + 1,
        });
    }
}

/// Lecteur de syntaxe nommée : un `BitReader` et un observateur.
pub struct Syntax<'a, 'r, R: Recorder> {
    pub bits: BitReader<'a>,
    pub rec: &'r mut R,
}

impl<'a, 'r, R: Recorder> Syntax<'a, 'r, R> {
    pub fn new(data: &'a [u8], rec: &'r mut R) -> Self {
        Self {
            bits: BitReader::new(data),
            rec,
        }
    }
}

impl<R: Recorder> Syntax<'_, '_, R> {
    #[inline]
    pub fn u(&mut self, name: &'static str, n: u32) -> Result<u64, BitError> {
        let at = self.bits.bit_pos();
        let v = self.bits.u(n)?;
        self.rec.field(name, at, n, Value::U(v));
        Ok(v)
    }

    #[inline]
    pub fn flag(&mut self, name: &'static str) -> Result<bool, BitError> {
        let at = self.bits.bit_pos();
        let v = self.bits.flag()?;
        self.rec.field(name, at, 1, Value::Flag(v));
        Ok(v)
    }

    #[inline]
    pub fn ue(&mut self, name: &'static str) -> Result<u32, BitError> {
        let at = self.bits.bit_pos();
        let v = self.bits.ue()?;
        let len = u32::try_from(self.bits.bit_pos() - at).unwrap_or(u32::MAX);
        self.rec.field(name, at, len, Value::U(u64::from(v)));
        Ok(v)
    }

    #[inline]
    pub fn se(&mut self, name: &'static str) -> Result<i32, BitError> {
        let at = self.bits.bit_pos();
        let v = self.bits.se()?;
        let len = u32::try_from(self.bits.bit_pos() - at).unwrap_or(u32::MAX);
        self.rec.field(name, at, len, Value::S(i64::from(v)));
        Ok(v)
    }

    /// Groupe nommé. En cas d'erreur, le nœud est fermé avec
    /// `error = "truncated"` et l'erreur est propagée.
    pub fn group<T>(
        &mut self,
        name: &'static str,
        f: impl FnOnce(&mut Self) -> Result<T, BitError>,
    ) -> Result<T, BitError> {
        self.rec.enter(name, self.bits.bit_pos());
        let r = f(self);
        self.rec
            .exit(self.bits.bit_pos(), r.as_ref().err().map(|_| "truncated"));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sps<R: Recorder>(s: &mut Syntax<'_, '_, R>) -> Result<(u64, u64), BitError> {
        s.group("seq_parameter_set", |s| {
            let p = s.u("profile_idc", 8)?;
            s.u("constraints", 8)?;
            let l = s.u("level_idc", 8)?;
            Ok((p, l))
        })
    }

    fn run<R: Recorder>(s: &mut Syntax<'_, '_, R>) -> Vec<i64> {
        let mut v = vec![i64::try_from(s.u("a", 8).unwrap()).unwrap()];
        v.extend((0..4).map(|_| i64::from(s.ue("b").unwrap())));
        v.push(i64::from(s.flag("c").unwrap()));
        v.push(i64::from(s.se("d").unwrap()));
        v
    }

    #[ma_trace::covers("SYN.TREE.02")]
    #[test]
    fn tree_recorder_records_offsets_and_lengths() {
        let mut t = TreeRecorder::default();
        let mut s = Syntax::new(&[0x64, 0x00, 0x28], &mut t);
        assert_eq!(sps(&mut s), Ok((100, 40)));
        let n = &t.nodes;
        assert_eq!(n.len(), 4);
        assert_eq!(
            (n[0].name, n[0].bit_offset, n[0].bit_len),
            ("seq_parameter_set", 0, 24)
        );
        assert_eq!(n[0].value, None);
        assert_eq!(n[0].error, None);
        // children : tout le sous-arbre, contigu.
        assert_eq!(n[0].children, 1..4);
        assert_eq!(
            (n[1].name, n[1].bit_offset, n[1].bit_len),
            ("profile_idc", 0, 8)
        );
        assert_eq!(n[1].value, Some(Value::U(100)));
        assert_eq!(
            (n[3].name, n[3].bit_offset, n[3].bit_len),
            ("level_idc", 16, 8)
        );
        assert_eq!(n[3].value, Some(Value::U(40)));
        assert!(n[3].children.is_empty());
    }

    #[test]
    fn nested_groups_cover_whole_subtree() {
        let mut t = TreeRecorder::default();
        let mut s = Syntax::new(&[0xFF, 0xFF], &mut t);
        s.group("a", |s| {
            s.group("b", |s| s.u("x", 4))?;
            s.flag("y")
        })
        .unwrap();
        // a, b, x, y
        assert_eq!(t.nodes[0].children, 1..4);
        assert_eq!(t.nodes[1].children, 2..3);
        assert_eq!(t.nodes[1].bit_len, 4);
        assert_eq!(t.nodes[0].bit_len, 5);
    }

    #[test]
    fn fast_and_tree_give_same_values() {
        let data = [0x64, 0xA6, 0x42, 0x4C, 0x80, 0x55];
        let mut f = Fast;
        let mut t = TreeRecorder::default();
        let a = run(&mut Syntax::new(&data, &mut f));
        let b = run(&mut Syntax::new(&data, &mut t));
        assert_eq!(a, b);
        assert_eq!(t.nodes.len(), 7);
        let se = t.nodes.last().unwrap();
        assert!(matches!(se.value, Some(Value::S(_))));
    }

    #[test]
    fn error_inside_group_leaves_partial_tree() {
        let mut t = TreeRecorder::default();
        let mut s = Syntax::new(&[0x64, 0x00], &mut t);
        let r = sps(&mut s);
        assert_eq!(
            r,
            Err(BitError::Truncated {
                at_bit: 16,
                wanted: 8
            })
        );
        assert_eq!(t.nodes.len(), 3);
        assert_eq!(t.nodes[0].error, Some("truncated"));
        assert_eq!(t.nodes[0].bit_len, 16);
        assert_eq!(t.nodes[0].children, 1..3);
    }

    #[test]
    fn exit_without_enter_is_ignored() {
        let mut t = TreeRecorder::default();
        t.exit(3, None);
        assert_eq!(t.nodes.len(), 0);
    }
}
