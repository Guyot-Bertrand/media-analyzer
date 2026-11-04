//! Source fichier : lectures positionnelles (`pread`), identité à l'ouverture.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::locality::{Locality, path_locality};
use crate::{IoError, ReadAt};

/// Identité du fichier à l'ouverture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub len: u64,
    pub mtime_ns: i128,
    pub inode: u64,
}

impl FileIdentity {
    fn from_metadata(m: &std::fs::Metadata) -> Self {
        Self {
            len: m.len(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            inode: m.ino(),
        }
    }
}

pub struct FileSource {
    file: File,
    path: PathBuf,
    identity: FileIdentity,
    locality: Locality,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<FileSource, IoError> {
        let file = File::open(path)?;
        let identity = FileIdentity::from_metadata(&file.metadata()?);
        Ok(FileSource {
            file,
            path: path.to_path_buf(),
            identity,
            locality: path_locality(path),
        })
    }

    #[must_use]
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    #[must_use]
    pub fn locality(&self) -> Locality {
        self.locality
    }

    /// Compare l'état actuel à l'identité d'ouverture : taille réduite ⇒ `Truncated` ;
    /// taille, date ou inode différents (y compris fichier remplacé ou supprimé) ⇒ `Modified`.
    pub fn check_unchanged(&self) -> Result<(), IoError> {
        let now = FileIdentity::from_metadata(&self.file.metadata()?);
        if now.len < self.identity.len {
            return Err(IoError::Truncated {
                expected_len: self.identity.len,
                actual_len: now.len,
            });
        }
        if now != self.identity {
            return Err(IoError::Modified);
        }
        match std::fs::metadata(&self.path) {
            Ok(m) if m.ino() == self.identity.inode => Ok(()),
            Ok(_) => Err(IoError::Modified),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(IoError::Modified),
            Err(e) => Err(e.into()),
        }
    }
}

impl ReadAt for FileSource {
    fn len(&self) -> u64 {
        self.identity.len
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let Some(avail) = self.identity.len.checked_sub(offset) else {
            return Ok(0);
        };
        let want = usize::try_from(avail).map_or(buf.len(), |a| a.min(buf.len()));
        let mut done = 0;
        while done < want {
            match self
                .file
                .read_at(&mut buf[done..want], offset + done as u64)
            {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if done < want {
            self.check_unchanged()?;
        }
        Ok(done)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::cache::{BlockCache, CacheConfig};
    use std::io::Write;

    const MI: u64 = 1 << 20;

    fn pattern(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8)
            .collect()
    }

    fn temp_file(data: &[u8]) -> (tempfile::NamedTempFile, FileSource) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(data).unwrap();
        f.flush().unwrap();
        let s = FileSource::open(f.path()).unwrap();
        (f, s)
    }

    #[test]
    fn read_at_matches_file_content() {
        let data = pattern(100_000);
        let (_f, s) = temp_file(&data);
        assert_eq!(s.len(), 100_000);
        assert!(!s.is_empty());
        let mut buf = vec![0u8; 5000];
        assert_eq!(s.read_at(1234, &mut buf).unwrap(), 5000);
        assert_eq!(buf, data[1234..6234]);
        // Lecture courte en fin de fichier.
        assert_eq!(s.read_at(99_990, &mut buf).unwrap(), 10);
        assert_eq!(buf[..10], data[99_990..]);
        // Au-delà de la fin.
        assert_eq!(s.read_at(200_000, &mut buf).unwrap(), 0);
        let id = s.identity();
        assert_eq!(id.len, 100_000);
        assert!(id.inode > 0 && id.mtime_ns > 0);
        assert_eq!(s.locality(), Locality::Local);
        s.check_unchanged().unwrap();
    }

    #[test]
    fn empty_file_and_missing_file() {
        let (_f, s) = temp_file(&[]);
        assert!(s.is_empty());
        let mut b = [0u8; 4];
        assert_eq!(s.read_at(0, &mut b).unwrap(), 0);
        let e = FileSource::open(std::path::Path::new("/nonexistent/zzz.ts"))
            .err()
            .unwrap();
        assert!(matches!(e, IoError::Os(_)));
    }

    #[ma_trace::covers("PERF.NET.01")]
    #[test]
    fn truncated_after_open_is_reported() {
        let data = pattern(3 * MI as usize);
        let (f, s) = temp_file(&data);
        f.as_file().set_len(MI).unwrap();
        // Directement sur la source.
        let mut b = [0u8; 16];
        assert!(matches!(
            s.read_at(2 * MI, &mut b),
            Err(IoError::Truncated { expected_len, actual_len }) if expected_len == 3 * MI && actual_len == MI
        ));
        // À travers le cache.
        let cache = BlockCache::new(s, CacheConfig::for_locality(Locality::Local));
        let mut buf = vec![0u8; 4096];
        let err = cache.read_exact_at(2 * MI, &mut buf).unwrap_err();
        assert!(
            matches!(err, IoError::Truncated { expected_len, actual_len }
                if expected_len == 3 * MI && actual_len == MI),
            "{err:?}"
        );
        // Le début est encore lisible (sans anticipation qui déborderait sur la zone coupée).
        let s = FileSource::open(f.path()).unwrap();
        let cache = BlockCache::new(
            s,
            CacheConfig {
                read_ahead: 0,
                ..CacheConfig::for_locality(Locality::Local)
            },
        );
        cache.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(buf, data[..4096]);
    }

    #[test]
    fn rewritten_file_is_modified() {
        let data = pattern(10_000);
        let (f, s) = temp_file(&data);
        s.check_unchanged().unwrap();
        std::fs::write(f.path(), pattern(10_000)).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        f.as_file().set_modified(later).unwrap();
        assert!(matches!(s.check_unchanged(), Err(IoError::Modified)));
    }

    #[test]
    fn grown_file_is_modified() {
        let (f, s) = temp_file(&pattern(100));
        f.as_file().set_len(200).unwrap();
        assert!(matches!(s.check_unchanged(), Err(IoError::Modified)));
    }

    #[test]
    fn replaced_by_rename_is_modified() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.ts");
        std::fs::write(&p, pattern(1000)).unwrap();
        let s = FileSource::open(&p).unwrap();
        let q = dir.path().join("b.ts");
        std::fs::write(&q, pattern(1000)).unwrap();
        std::fs::rename(&q, &p).unwrap();
        assert!(matches!(s.check_unchanged(), Err(IoError::Modified)));
        std::fs::remove_file(&p).unwrap();
        assert!(matches!(s.check_unchanged(), Err(IoError::Modified)));
    }
}
