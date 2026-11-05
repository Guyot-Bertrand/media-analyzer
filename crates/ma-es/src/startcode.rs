//! Codes de début (`00 00 01`) : suivi des zéros en flux et recherche dans une AU complète.

use ma_core::Codec;

/// Nombre de zéros consécutifs en fin de flux après avoir vu `bytes`, sachant qu'il y en avait
/// `zeros` avant.
pub(crate) fn zeros_after(zeros: u64, bytes: &[u8]) -> u64 {
    match bytes.iter().rposition(|&b| b != 0) {
        None => zeros.saturating_add(bytes.len() as u64),
        Some(last) => (bytes.len() - 1 - last) as u64,
    }
}

/// Position du début de chaque NAL d'une AU complète (premier octet après `00 00 01`).
fn nal_starts(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                i += 3;
                return Some(i);
            }
            i += 1;
        }
        None
    })
}

/// Voir [`crate::first_slice_payload_offset`].
pub(crate) fn first_slice_payload_offset(codec: Codec, au: &[u8]) -> Option<usize> {
    nal_starts(au).find_map(|s| {
        let b = au[s];
        match codec {
            Codec::H264 => matches!(b & 0x1F, 1 | 5).then_some(s + 1),
            Codec::Hevc => (((b >> 1) & 0x3F) < 32).then_some(s + 2),
            Codec::Mpeg2 => (0x01..=0xAF).contains(&b).then_some(s + 1),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_run_tracking() {
        assert_eq!(zeros_after(0, &[]), 0);
        assert_eq!(zeros_after(2, &[0, 0]), 4);
        assert_eq!(zeros_after(5, &[1, 0]), 1);
        assert_eq!(zeros_after(5, &[0, 7]), 0);
        assert_eq!(zeros_after(u64::MAX, &[0]), u64::MAX);
    }
}
