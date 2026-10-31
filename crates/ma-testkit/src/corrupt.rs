//! Source d'AU qui corrompt des images choisies, pour les tests de tolérance aux erreurs.

use std::collections::HashSet;
use std::io;

use ma_decode::{AuSource, Codec, FrameEntry};

use crate::nal::first_slice_payload_offset;

/// Enveloppe une source et met à zéro, pour les AU de `corrupt` (index de décodage), tous les octets
/// qui suivent l'en-tête NAL de la première tranche (1 octet en H.264, 2 en HEVC, le code de
/// tranche en MPEG-2), **en-tête de tranche compris**. Les ensembles de paramètres qui précèdent
/// et l'en-tête NAL restent intacts : le flux reste analysable, mais l'image est inutilisable.
pub struct CorruptingSource<S> {
    pub inner: S,
    pub corrupt: HashSet<u32>,
}

impl<S: AuSource> AuSource for CorruptingSource<S> {
    fn codec(&self) -> Codec {
        self.inner.codec()
    }

    fn len(&self) -> u32 {
        self.inner.len()
    }

    fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
        self.inner.entry(decode_idx)
    }

    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
        self.inner.decode_idx_of(pres_idx)
    }

    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        self.inner.read_au(decode_idx, buf)?;
        if self.corrupt.contains(&decode_idx) {
            let from = first_slice_payload_offset(self.codec(), buf).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("AU {decode_idx} : aucune tranche à corrompre"),
                )
            })?;
            buf[from..].fill(0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ma_decode::{Leading, PictureType, RapKind};

    struct One(Vec<u8>, Codec);

    impl AuSource for One {
        fn codec(&self) -> Codec {
            self.1
        }
        fn len(&self) -> u32 {
            1
        }
        fn entry(&self, i: u32) -> Option<FrameEntry> {
            (i == 0).then_some(FrameEntry {
                decode_idx: 0,
                pres_idx: 0,
                pts: 0,
                picture: PictureType::I,
                rap: RapKind::Idr,
                leading: Leading::None,
            })
        }
        fn decode_idx_of(&self, p: u32) -> Option<u32> {
            (p == 0).then_some(0)
        }
        fn read_au(&self, i: u32, buf: &mut Vec<u8>) -> io::Result<()> {
            assert_eq!(i, 0);
            buf.clear();
            buf.extend_from_slice(&self.0);
            Ok(())
        }
    }

    fn src(au: &[u8]) -> CorruptingSource<One> {
        CorruptingSource {
            inner: One(au.to_vec(), Codec::H264),
            corrupt: HashSet::from([0]),
        }
    }

    #[test]
    fn zeroes_everything_after_the_nal_header() {
        let au = [0, 0, 0, 1, 0x67, 7, 0, 0, 0, 1, 0x65, 0x88, 9, 9, 9];
        let s = src(&au);
        let mut buf = Vec::new();
        s.read_au(0, &mut buf).unwrap();
        assert_eq!(buf.len(), au.len());
        assert_eq!(&buf[..11], &au[..11]);
        assert!(buf[11..].iter().all(|&b| b == 0));
        assert_eq!(
            (s.codec(), s.len(), s.decode_idx_of(0)),
            (Codec::H264, 1, Some(0))
        );
        assert_eq!(s.entry(0).unwrap().pts, 0);
        assert!(!s.is_empty());
    }

    #[test]
    fn untouched_when_not_listed() {
        let au = [0, 0, 0, 1, 0x65, 0x88, 9];
        let mut s = src(&au);
        s.corrupt.clear();
        let mut buf = Vec::new();
        s.read_au(0, &mut buf).unwrap();
        assert_eq!(buf, au);
    }

    #[test]
    fn an_au_without_slice_is_an_error() {
        let s = src(&[0, 0, 0, 1, 0x67, 7]);
        let err = s.read_au(0, &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
