//! Cache de blocs alignés (LRU) avec lecture anticipée.

use std::sync::{Mutex, MutexGuard};

use crate::locality::Locality;
use crate::{IoError, ReadAt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    pub block_size: u32,
    pub max_blocks: usize,
    pub read_ahead: u32,
}

impl CacheConfig {
    /// Local : 1 Mio, 64 blocs, 1 anticipé. Distant : 4 Mio, 32 blocs, 2 anticipés.
    #[must_use]
    pub fn for_locality(l: Locality) -> CacheConfig {
        match l {
            Locality::Local => CacheConfig {
                block_size: 1 << 20,
                max_blocks: 64,
                read_ahead: 1,
            },
            Locality::Remote => CacheConfig {
                block_size: 4 << 20,
                max_blocks: 32,
                read_ahead: 2,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Appels `read_at` à la source.
    pub syscalls: u64,
    pub bytes_read: u64,
    /// Blocs servis depuis le cache.
    pub hits: u64,
}

#[derive(Default)]
struct Inner {
    /// Blocs `(index, données)`, le plus récemment utilisé en dernier.
    blocks: Vec<(u64, Vec<u8>)>,
    stats: IoStats,
}

impl Inner {
    fn lookup(&mut self, idx: u64) -> Option<&[u8]> {
        let pos = self.blocks.iter().position(|(i, _)| *i == idx)?;
        let entry = self.blocks.remove(pos);
        self.blocks.push(entry);
        self.blocks.last().map(|(_, d)| d.as_slice())
    }

    fn insert(&mut self, idx: u64, data: Vec<u8>, max_blocks: usize) {
        self.blocks.retain(|(i, _)| *i != idx);
        if self.blocks.len() >= max_blocks {
            let excess = self.blocks.len() + 1 - max_blocks;
            self.blocks.drain(..excess);
        }
        self.blocks.push((idx, data));
    }
}

pub struct BlockCache<S: ReadAt> {
    src: S,
    cfg: CacheConfig,
    inner: Mutex<Inner>,
}

impl<S: ReadAt> BlockCache<S> {
    #[must_use]
    pub fn new(src: S, mut cfg: CacheConfig) -> Self {
        cfg.block_size = cfg.block_size.max(1);
        cfg.max_blocks = cfg.max_blocks.max(1);
        Self {
            src,
            cfg,
            inner: Mutex::new(Inner::default()),
        }
    }

    #[must_use]
    pub fn source(&self) -> &S {
        &self.src
    }

    #[must_use]
    pub fn stats(&self) -> IoStats {
        self.lock().stats
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lit le bloc `idx` et les `read_ahead` suivants (bornés à la fin) en un seul appel.
    fn load(&self, inner: &mut Inner, idx: u64) -> Result<(), IoError> {
        let bs = u64::from(self.cfg.block_size);
        let len = self.src.len();
        let total_blocks = len.div_ceil(bs);
        let count = (1 + u64::from(self.cfg.read_ahead)).min(total_blocks.saturating_sub(idx));
        let start = idx * bs;
        let want = (count * bs).min(len - start);
        let mut buf =
            vec![0u8; usize::try_from(want).map_err(|_| IoError::InvalidData("block-too-large"))?];
        inner.stats.syscalls += 1;
        let got = self.src.read_at(start, &mut buf)?;
        inner.stats.bytes_read += got as u64;
        if (got as u64) < want {
            return Err(IoError::Truncated {
                expected_len: len,
                actual_len: start + got as u64,
            });
        }
        // Les blocs anticipés d'abord : le bloc demandé finit le plus récent.
        let chunk = self.cfg.block_size as usize;
        for (k, data) in buf.chunks(chunk).enumerate().skip(1) {
            inner.insert(idx + k as u64, data.to_vec(), self.cfg.max_blocks);
        }
        buf.truncate(chunk);
        inner.insert(idx, buf, self.cfg.max_blocks);
        Ok(())
    }

    /// Lit exactement `buf.len()` octets ; au-delà de `len()` : erreur `UnexpectedEof`.
    pub fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(IoError::InvalidData("offset-overflow"))?;
        if end > self.src.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        let bs = u64::from(self.cfg.block_size);
        let mut inner = self.lock();
        let mut pos = offset;
        let mut done = 0;
        while done < buf.len() {
            let idx = pos / bs;
            if inner.lookup(idx).is_some() {
                inner.stats.hits += 1;
            } else {
                self.load(&mut inner, idx)?;
            }
            let block = inner
                .lookup(idx)
                .ok_or(IoError::InvalidData("block-missing"))?;
            let in_block = usize::try_from(pos - idx * bs).unwrap_or(usize::MAX);
            let avail = block
                .get(in_block..)
                .ok_or(IoError::InvalidData("block-short"))?;
            let n = avail.len().min(buf.len() - done);
            if n == 0 {
                return Err(IoError::InvalidData("block-short"));
            }
            buf[done..done + n].copy_from_slice(&avail[..n]);
            done += n;
            pos += n as u64;
        }
        Ok(())
    }
}

impl<S: ReadAt> ReadAt for BlockCache<S> {
    fn len(&self) -> u64 {
        self.src.len()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError> {
        let Some(avail) = self.src.len().checked_sub(offset) else {
            return Ok(0);
        };
        let n = usize::try_from(avail).map_or(buf.len(), |a| a.min(buf.len()));
        self.read_exact_at(offset, &mut buf[..n])?;
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::testing::{LatencySource, MemSource, TruncatingSource};
    use std::time::{Duration, Instant};

    const MI: u64 = 1 << 20;
    const KI: usize = 1 << 10;

    fn pattern(n: usize) -> Vec<u8> {
        (0..n)
            .map(|i| (i.wrapping_mul(131) ^ (i >> 9)) as u8)
            .collect()
    }

    fn cfg(block_size: u32, max_blocks: usize, read_ahead: u32) -> CacheConfig {
        CacheConfig {
            block_size,
            max_blocks,
            read_ahead,
        }
    }

    #[test]
    fn config_values_follow_locality() {
        assert_eq!(
            CacheConfig::for_locality(Locality::Local),
            cfg(1 << 20, 64, 1)
        );
        assert_eq!(
            CacheConfig::for_locality(Locality::Remote),
            cfg(4 << 20, 32, 2)
        );
    }

    #[test]
    fn overlapping_reads_share_one_block() {
        let data = pattern(3 * MI as usize);
        let c = BlockCache::new(MemSource::new(data.clone()), cfg(1 << 20, 64, 0));
        let mut a = vec![0u8; 1000];
        let mut b = vec![0u8; 1000];
        c.read_exact_at(100, &mut a).unwrap();
        c.read_exact_at(500, &mut b).unwrap();
        assert_eq!(a, data[100..1100]);
        assert_eq!(b, data[500..1500]);
        let s = c.stats();
        assert_eq!(s.syscalls, 1);
        assert_eq!(s.hits, 1);
        assert_eq!(s.bytes_read, MI);
        assert_eq!(c.source().calls(), 1);
    }

    #[test]
    fn sequential_reads_amplification_bounded() {
        let len = 8 * MI as usize;
        let data = pattern(len);
        let config = CacheConfig::for_locality(Locality::Local);
        let c = BlockCache::new(MemSource::new(data.clone()), config);
        let mut buf = vec![0u8; 64 * KI];
        for off in (0..len).step_by(64 * KI) {
            c.read_exact_at(off as u64, &mut buf).unwrap();
            assert_eq!(buf, data[off..off + 64 * KI]);
        }
        let s = c.stats();
        assert!(
            s.bytes_read
                <= len as u64 + u64::from(config.block_size) * u64::from(config.read_ahead),
            "{s:?}"
        );
        assert_eq!(s.bytes_read, len as u64);
    }

    #[test]
    fn read_spanning_blocks_and_tail() {
        let len = 5 * KI + 123;
        let data = pattern(len);
        let c = BlockCache::new(MemSource::new(data.clone()), cfg(KI as u32, 2, 1));
        let mut buf = vec![0u8; len - 700];
        c.read_exact_at(700, &mut buf).unwrap();
        assert_eq!(buf, data[700..]);
        // Dernier bloc partiel, lu seul.
        let mut tail = vec![0u8; 123];
        c.read_exact_at(5 * KI as u64, &mut tail).unwrap();
        assert_eq!(tail, data[5 * KI..]);
        // Lecture vide.
        c.read_exact_at(len as u64, &mut []).unwrap();
    }

    #[test]
    fn lru_evicts_oldest_block() {
        let data = pattern(4 * KI);
        let c = BlockCache::new(MemSource::new(data), cfg(KI as u32, 2, 0));
        let mut b = [0u8; 8];
        let rd = |blk: u64, b: &mut [u8]| c.read_exact_at(blk * KI as u64, b).unwrap();
        rd(0, &mut b);
        rd(1, &mut b);
        rd(0, &mut b); // rafraîchit le bloc 0
        assert_eq!(c.stats().syscalls, 2);
        rd(2, &mut b); // évince le bloc 1
        assert_eq!(c.stats().syscalls, 3);
        rd(0, &mut b); // toujours en cache
        assert_eq!(c.stats().syscalls, 3);
        rd(1, &mut b); // évincé : relu
        assert_eq!(c.stats().syscalls, 4);
    }

    #[test]
    fn zero_max_blocks_is_clamped() {
        let c = BlockCache::new(MemSource::new(pattern(3000)), cfg(1024, 0, 0));
        let mut b = [0u8; 2000];
        c.read_exact_at(500, &mut b).unwrap();
    }

    #[test]
    fn read_beyond_len_is_an_error() {
        let c = BlockCache::new(MemSource::new(pattern(1000)), cfg(256, 4, 1));
        let mut b = [0u8; 10];
        let e = c.read_exact_at(995, &mut b).unwrap_err();
        assert!(
            matches!(&e, IoError::Os(x) if x.kind() == std::io::ErrorKind::UnexpectedEof),
            "{e:?}"
        );
        let e = c.read_exact_at(u64::MAX - 3, &mut b).unwrap_err();
        assert!(matches!(e, IoError::InvalidData(_)));
    }

    #[test]
    fn read_at_trait_clamps_to_len() {
        let data = pattern(1000);
        let c = BlockCache::new(MemSource::new(data.clone()), cfg(256, 4, 1));
        assert_eq!(c.len(), 1000);
        assert!(!c.is_empty());
        let mut b = [0u8; 100];
        assert_eq!(c.read_at(950, &mut b).unwrap(), 50);
        assert_eq!(b[..50], data[950..]);
        assert_eq!(c.read_at(2000, &mut b).unwrap(), 0);
        assert_eq!(c.read_at(0, &mut b).unwrap(), 100);
    }

    #[test]
    fn source_short_read_is_truncated() {
        let data = pattern(4 * KI);
        let src = TruncatingSource::new(MemSource::new(data.clone()), 2 * KI as u64 + 10);
        let c = BlockCache::new(src, cfg(KI as u32, 8, 0));
        let mut b = [0u8; 4];
        c.read_exact_at(0, &mut b).unwrap();
        let e = c.read_exact_at(2 * KI as u64, &mut b).unwrap_err();
        assert!(
            matches!(e, IoError::Truncated { expected_len, actual_len }
                if expected_len == 4 * KI as u64 && actual_len == 2 * KI as u64 + 10),
            "{e:?}"
        );
        let e = c.read_exact_at(3 * KI as u64, &mut b).unwrap_err();
        assert!(matches!(e, IoError::Truncated { .. }));
    }

    #[test]
    fn mem_source_basics() {
        let m = MemSource::new(pattern(10));
        assert_eq!(m.len(), 10);
        let mut b = [0u8; 4];
        assert_eq!(m.read_at(8, &mut b).unwrap(), 2);
        assert_eq!(m.read_at(50, &mut b).unwrap(), 0);
        assert_eq!(m.calls(), 2);
    }

    #[ma_trace::covers("PERF.NET.01")]
    #[test]
    fn remote_read_ahead_reduces_round_trips() {
        let len = 16 * MI as usize;
        let data = pattern(len);
        let run = |locality| {
            let src = LatencySource::new(MemSource::new(data.clone()), Duration::from_millis(5));
            let c = BlockCache::new(src, CacheConfig::for_locality(locality));
            let mut buf = vec![0u8; 64 * KI];
            let t = Instant::now();
            for off in (0..len).step_by(64 * KI) {
                c.read_exact_at(off as u64, &mut buf).unwrap();
                assert_eq!(buf, data[off..off + 64 * KI]);
            }
            (c.stats(), t.elapsed())
        };
        let (remote, t_remote) = run(Locality::Remote);
        let (local, t_local) = run(Locality::Local);
        assert!(remote.syscalls <= 2, "{remote:?}");
        assert!(local.syscalls <= 8, "{local:?}");
        assert!(remote.syscalls < local.syscalls);
        // La latence simulée est réellement payée.
        assert!(t_local >= Duration::from_millis(5 * local.syscalls));
        assert!(t_remote < t_local + Duration::from_millis(100));
    }

    #[test]
    fn latency_source_delegates() {
        let l = LatencySource::new(MemSource::new(pattern(10)), Duration::ZERO);
        assert_eq!(l.len(), 10);
        let mut b = [0u8; 3];
        assert_eq!(l.read_at(0, &mut b).unwrap(), 3);
    }
}
