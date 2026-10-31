//! Paquet libavcodec construit à partir d'une unité d'accès.

use ma_decode::{DecodeError, DecodeErrorKind};
use rsmpeg::avcodec::AVPacket;
use rsmpeg::ffi;

/// Taille maximale d'une AU (32 Mio), comme la source d'AU : au-delà, le flux est jugé malformé.
pub const MAX_AU_BYTES: usize = 32 * 1024 * 1024;

/// Paquet portant une copie de `au` et le `pts` donné (pas de `dts` : ordre de décodage implicite).
///
/// Une AU vide est refusée en `InvalidData` : libavcodec rejette un paquet vide (non nul) avec
/// `EINVAL`, qui serait sinon imputé au GPU (repli injustifié) ou classé interne.
pub(crate) fn packet(pts: i64, au: &[u8]) -> Result<AVPacket, DecodeError> {
    if au.is_empty() {
        return Err(DecodeError {
            kind: DecodeErrorKind::InvalidData,
            detail: "AU vide".into(),
        });
    }
    let len = i32::try_from(au.len())
        .ok()
        .filter(|_| au.len() <= MAX_AU_BYTES)
        .ok_or_else(|| DecodeError {
            kind: DecodeErrorKind::InvalidData,
            detail: format!(
                "AU trop grande : {} octets (au plus {MAX_AU_BYTES})",
                au.len()
            ),
        })?;
    let mut pkt = AVPacket::new();
    // SAFETY: `pkt` est un paquet valide et vide ; `av_new_packet` y alloue un tampon de `len`
    // octets (plus le rembourrage nul exigé par libavcodec), et ne touche à rien d'autre.
    let r = unsafe { ffi::av_new_packet(pkt.as_mut_ptr(), len) };
    if r < 0 {
        return Err(DecodeError {
            kind: DecodeErrorKind::Internal,
            detail: format!("av_new_packet : code {r}"),
        });
    }
    // SAFETY: après le succès d'`av_new_packet`, `data` pointe sur au moins `len == au.len()`
    // octets inscriptibles, propres au paquet, donc distincts de `au`.
    unsafe {
        std::ptr::copy_nonoverlapping(au.as_ptr(), (*pkt.as_mut_ptr()).data, au.len());
    }
    pkt.set_pts(pts);
    pkt.set_dts(ffi::AV_NOPTS_VALUE);
    Ok(pkt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_copies_the_au() {
        let pkt = packet(42, &[0, 0, 1, 9, 7]).unwrap();
        assert_eq!((pkt.pts, pkt.dts, pkt.size), (42, ffi::AV_NOPTS_VALUE, 5));
        // SAFETY: `data` pointe sur `size` octets initialisés par `packet`.
        let data = unsafe { std::slice::from_raw_parts(pkt.data, 5) };
        assert_eq!(data, [0, 0, 1, 9, 7]);
    }

    #[test]
    fn empty_au_is_invalid_data() {
        let err = packet(0, &[]).unwrap_err();
        assert_eq!(
            (err.kind, err.detail.as_str()),
            (DecodeErrorKind::InvalidData, "AU vide")
        );
    }

    #[test]
    fn oversized_au_is_invalid_data() {
        let err = packet(0, &vec![0; MAX_AU_BYTES + 1]).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::InvalidData);
        assert!(packet(0, &vec![0; MAX_AU_BYTES]).is_ok());
    }
}
