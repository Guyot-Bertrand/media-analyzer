//! Entrées/sorties du moteur : lectures positionnelles (jamais de `mmap`), cache de blocs
//! alignés avec lecture anticipée, détection de fichier tronqué ou modifié.

mod cache;
mod locality;
mod source;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::io;

pub use cache::{BlockCache, CacheConfig, IoStats};
pub use locality::{Locality, fs_locality, path_locality};
pub use source::{FileIdentity, FileSource};

/// Erreur d'entrée/sortie. Aucune ne provoque de panique : le fichier tronqué ou
/// remplacé est signalé, l'appelant garde ce qu'il a déjà lu.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// Le fichier est plus court que lors de l'ouverture.
    #[error("fichier tronqué : {expected_len} octets attendus, {actual_len} présents")]
    Truncated { expected_len: u64, actual_len: u64 },
    /// Le fichier a été modifié ou remplacé depuis l'ouverture.
    #[error("fichier modifié depuis l'ouverture")]
    Modified,
    /// Paramètre invalide (identifiant stable, non traduit).
    #[error("données invalides : {0}")]
    InvalidData(&'static str),
    /// Erreur du système d'exploitation.
    #[error(transparent)]
    Os(#[from] io::Error),
}

impl From<IoError> for io::Error {
    fn from(e: IoError) -> Self {
        match e {
            IoError::Truncated { .. } => io::Error::new(io::ErrorKind::UnexpectedEof, e),
            IoError::Modified => io::Error::other(e),
            IoError::InvalidData(_) => io::Error::new(io::ErrorKind::InvalidData, e),
            IoError::Os(inner) => inner,
        }
    }
}

/// Source lisible par position, partageable entre fils.
pub trait ReadAt: Send + Sync {
    /// Taille du fichier à l'ouverture.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lit à partir de `offset`. Une lecture courte signifie la fin du fichier.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_maps_to_std_kinds() {
        use std::io::{Error, ErrorKind};
        let k = |e: IoError| Error::from(e).kind();
        assert_eq!(
            k(IoError::Truncated {
                expected_len: 3,
                actual_len: 1
            }),
            ErrorKind::UnexpectedEof
        );
        assert_eq!(k(IoError::Modified), ErrorKind::Other);
        assert_eq!(k(IoError::InvalidData("x")), ErrorKind::InvalidData);
        assert_eq!(
            k(IoError::Os(Error::from(ErrorKind::NotFound))),
            ErrorKind::NotFound
        );
        let e: IoError = Error::from(ErrorKind::PermissionDenied).into();
        assert!(matches!(e, IoError::Os(_)));
        assert!(
            IoError::Truncated {
                expected_len: 3,
                actual_len: 1
            }
            .to_string()
            .contains('3')
        );
    }
}
