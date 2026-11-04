//! Sources de test : mémoire, latence simulée, troncature.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{IoError, ReadAt};

/// Source en mémoire qui compte les appels à `read_at`.
pub struct MemSource {
    data: Vec<u8>,
    calls: AtomicU64,
}

impl MemSource {
    #[must_use]
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            calls: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl ReadAt for MemSource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let Some(rest) = usize::try_from(offset)
            .ok()
            .and_then(|o| self.data.get(o..))
        else {
            return Ok(0);
        };
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

/// Ajoute une latence réelle (`thread::sleep`) à chaque lecture.
pub struct LatencySource<S> {
    inner: S,
    latency: Duration,
}

impl<S> LatencySource<S> {
    #[must_use]
    pub fn new(inner: S, latency: Duration) -> Self {
        Self { inner, latency }
    }
}

impl<S: ReadAt> ReadAt for LatencySource<S> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        std::thread::sleep(self.latency);
        self.inner.read_at(offset, buf)
    }
}

/// Simule un fichier tronqué : `len()` reste celui de l'ouverture, mais rien n'existe
/// au-delà de `cut_at` (`Truncated`).
pub struct TruncatingSource<S> {
    inner: S,
    cut_at: u64,
}

impl<S> TruncatingSource<S> {
    #[must_use]
    pub fn new(inner: S, cut_at: u64) -> Self {
        Self { inner, cut_at }
    }
}

impl<S: ReadAt> ReadAt for TruncatingSource<S> {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        if offset >= self.cut_at {
            return Err(IoError::Truncated {
                expected_len: self.inner.len(),
                actual_len: self.cut_at,
            });
        }
        let n = usize::try_from(self.cut_at - offset).map_or(buf.len(), |a| a.min(buf.len()));
        self.inner.read_at(offset, &mut buf[..n])
    }
}
