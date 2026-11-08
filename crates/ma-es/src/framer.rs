//! Cœur du découpeur : codes de début en flux, NAL en cours, AU ouverte.
//!
//! Un NAL est jugé quand il est complet, c'est-à-dire au code de début suivant (ou à la fin du
//! flux). Seuls ses premiers octets sont gardés : la décision ne dépend
//! donc que de ces octets, jamais du découpage des `push`.

use crate::startcode::zeros_after;
use crate::{AuBoundary, AuFramer, AuMeta, MAX_HEADER_BYTES};

/// Rôle d'un NAL (ou d'un code de début MPEG-2) dans le découpage en AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NalKind {
    /// N'ouvre jamais d'AU (bourrage, fin de séquence, SEI suffixe…).
    Other,
    /// Ouvre une AU s'il suit une tranche de l'AU courante (AUD, SPS, SEI préfixe…).
    Opener,
    /// Première tranche d'une image : ouvre une AU s'il suit une tranche de l'AU courante.
    FirstSlice,
    /// Autre tranche : appartient à l'AU courante.
    Slice,
}

/// Analyse propre à un codec.
pub(crate) trait NalParser: Send {
    /// Octets à garder pour un NAL dont le premier octet est `first`.
    fn header_cap(&self, first: u8) -> usize;
    /// Rôle du NAL `nal` (premiers octets, non vide).
    fn kind(&self, nal: &[u8]) -> NalKind;
    /// Ajoute le NAL `nal` (premiers octets, non vide) à l'AU courante.
    fn add(&mut self, nal: &[u8]);
    /// Métadonnées de l'AU courante ; l'état de l'AU est remis à zéro (pas les jeux de
    /// paramètres).
    fn take(&mut self) -> AuMeta;
    /// Vrai si un zéro qui précède `00 00 01` est le `zero_byte` d'un code de 4 octets
    /// (H.264, HEVC : il appartient au NAL qui suit). Faux en MPEG-2, où le code fait toujours
    /// 3 octets : les zéros d'avant finissent l'unité précédente.
    fn four_byte_codes(&self) -> bool {
        true
    }
}

/// NAL en cours de réception.
struct Pending {
    /// Offset du code de début (y compris le `zero_byte` d'un code de 4 octets, H.264 et HEVC).
    start: u64,
    /// Octets reçus après `00 00 01` (y compris les zéros du code de début suivant).
    len: u64,
    /// Octets gardés au plus dans `Framer::buf`.
    cap: usize,
}

/// AU ouverte.
struct OpenAu {
    start: u64,
    has_slice: bool,
}

pub(crate) struct Framer<P> {
    parser: P,
    /// Octets poussés depuis le début.
    offset: u64,
    /// Zéros consécutifs juste avant `offset`.
    zeros: u64,
    pending: Option<Pending>,
    /// Premiers octets du NAL en cours (capacité fixe `MAX_HEADER_BYTES`).
    buf: Vec<u8>,
    au: Option<OpenAu>,
}

impl<P: NalParser> Framer<P> {
    pub(crate) fn new(parser: P) -> Self {
        Self {
            parser,
            offset: 0,
            zeros: 0,
            pending: None,
            buf: Vec::with_capacity(MAX_HEADER_BYTES),
            au: None,
        }
    }

    /// Octets d'en-tête gardés et capacité du tampon.
    #[cfg(test)]
    fn buffered(&self) -> (usize, usize) {
        (self.buf.len(), self.buf.capacity())
    }

    /// Octets de NAL (aucun code de début dedans) : compte, garde le début, suit les zéros.
    fn consume(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(p) = &mut self.pending {
            if p.len == 0 {
                p.cap = self.parser.header_cap(bytes[0]).min(MAX_HEADER_BYTES);
            }
            p.len += bytes.len() as u64;
            let room = p.cap.saturating_sub(self.buf.len());
            self.buf.extend_from_slice(&bytes[..room.min(bytes.len())]);
        }
        self.zeros = zeros_after(self.zeros, bytes);
    }

    /// Termine le NAL en cours : les zéros qui le suivent (code de début suivant,
    /// `trailing_zero_8bits`) n'en font pas partie.
    fn end_nal(&mut self, out: &mut Vec<AuBoundary>) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let real = p.len.saturating_sub(self.zeros);
        self.buf
            .truncate(usize::try_from(real).unwrap_or(usize::MAX));
        if self.buf.is_empty() {
            return;
        }
        let kind = self.parser.kind(&self.buf);
        let opens = matches!(kind, NalKind::Opener | NalKind::FirstSlice)
            && self.au.as_ref().is_none_or(|a| a.has_slice);
        if opens {
            if let Some(a) = self.au.take() {
                out.push(AuBoundary {
                    start: a.start,
                    end: p.start,
                    meta: self.parser.take(),
                });
            }
            self.au = Some(OpenAu {
                start: p.start,
                has_slice: false,
            });
        }
        if let Some(a) = &mut self.au {
            self.parser.add(&self.buf);
            a.has_slice |= matches!(kind, NalKind::FirstSlice | NalKind::Slice);
        }
    }
}

impl<P: NalParser> AuFramer for Framer<P> {
    fn push(&mut self, data: &[u8], out: &mut Vec<AuBoundary>) {
        let mut rest = data;
        while !rest.is_empty() {
            let Some(k) = rest.iter().position(|&b| b == 1) else {
                self.consume(rest);
                self.offset += rest.len() as u64;
                return;
            };
            self.consume(&rest[..k]);
            let one_at = self.offset + k as u64;
            if self.zeros >= 2 {
                // `00 00 01`, précédé d'un `zero_byte` s'il existe (code de 4 octets, sauf en
                // MPEG-2).
                let back = if self.zeros >= 3 && self.parser.four_byte_codes() {
                    3
                } else {
                    2
                };
                self.end_nal(out);
                self.buf.clear();
                self.pending = Some(Pending {
                    start: one_at.saturating_sub(back),
                    len: 0,
                    cap: 0,
                });
                self.zeros = 0;
            } else {
                self.consume(&rest[k..=k]);
            }
            self.offset = one_at + 1;
            rest = &rest[k + 1..];
        }
    }

    fn finish(&mut self, out: &mut Vec<AuBoundary>) {
        self.end_nal(out);
        self.buf.clear();
        self.zeros = 0;
        if let Some(a) = self.au.take() {
            out.push(AuBoundary {
                start: a.start,
                end: self.offset,
                meta: self.parser.take(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_HEADER_BYTES, MAX_SLICE_HEADER_BYTES, h264::H264};

    #[test]
    fn header_buffer_is_bounded() {
        let big = vec![0xFF_u8; 10 << 20];
        for (header, bound) in [(0x06, MAX_HEADER_BYTES), (0x65, MAX_SLICE_HEADER_BYTES)] {
            let mut f = Framer::new(H264::default());
            let mut out = Vec::new();
            f.push(&[0, 0, 0, 1, header], &mut out);
            for c in big.chunks(1 << 16) {
                f.push(c, &mut out);
                assert!(f.buffered().1 <= MAX_HEADER_BYTES);
            }
            let (len, cap) = f.buffered();
            assert_eq!(len, bound, "NAL {header:#x}");
            assert!(cap <= MAX_HEADER_BYTES, "capacité {cap}");
            assert_eq!(out, Vec::new());
        }
    }
}
