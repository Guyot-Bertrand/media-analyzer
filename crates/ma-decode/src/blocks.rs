//! Modèle des blocs de codage (arbre de partition). Toujours absent (`None`) en MVP.

use std::ops::Range;

use crate::types::Rect;

/// Arbre de blocs d'une image ; les nœuds sont à plat, les enfants référencés par intervalle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockTree {
    pub nodes: Vec<BlockNode>,
}

/// Un bloc : sa région, son type de prédiction, ses attributs et ses enfants (indices dans `nodes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockNode {
    pub rect: Rect,
    pub kind: Option<BlockKind>,
    pub attrs: Vec<(BlockAttr, i32)>,
    pub children: Range<u32>,
}

/// Type de prédiction d'un bloc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BlockKind {
    Intra,
    Inter,
    Skip,
}

/// Attribut numérique d'un bloc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BlockAttr {
    Qp,
    MvX,
    MvY,
    RefIdx,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_tree_default_is_empty() {
        let tree = BlockTree::default();
        assert_eq!(tree, BlockTree { nodes: Vec::new() });
    }

    #[test]
    fn block_node_holds_children_range() {
        let node = BlockNode {
            rect: Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            kind: Some(BlockKind::Inter),
            attrs: vec![(BlockAttr::Qp, 26)],
            children: 1..5,
        };
        assert_eq!(node.clone(), node);
        assert_eq!(node.children.len(), 4);
    }
}
