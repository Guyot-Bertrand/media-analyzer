//! Lecteur de bits instrumenté.
//!
//! Deux modes de lecture partagent le même code de syntaxe : `Fast` (aucune
//! trace, coût quasi nul, utilisé par l'indexation) et `TreeRecorder` (arbre
//! syntaxique aplati avec offsets et longueurs en bits).

mod rbsp;
mod reader;
mod syntax;
#[cfg(feature = "writer")]
mod writer;

pub use rbsp::{Rbsp, rbsp};
pub use reader::{BitError, BitReader};
pub use syntax::{Fast, Recorder, Syntax, SyntaxNode, TreeRecorder, Value};
#[cfg(feature = "writer")]
pub use writer::{BitWriter, escape};
