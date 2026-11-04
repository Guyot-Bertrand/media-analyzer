//! Détection du format et dénombrement des paquets sur le corpus généré.

use ma_core::VecSink;
use ma_io::ReadAt;
use ma_testkit::corpus_file;
use ma_ts::{PacketFormat, PacketScanner, probe};

/// Ouvre `name` et renvoie (format, offset, taille du fichier, nombre de paquets, messages).
fn scan(name: &str) -> (PacketFormat, u64, u64, u64, usize) {
    let path = corpus_file(name);
    let (src, format, start) = probe(&path).unwrap();
    let mut sink = VecSink::default();
    let mut sc = PacketScanner::new(&src, format, start, None);
    let mut n = 0;
    while let Some(p) = sc.next(&mut sink).unwrap() {
        assert_eq!(p.index, n);
        assert_eq!(
            p.offset,
            start + n * format.size() as u64,
            "offset du paquet {n}"
        );
        n += 1;
    }
    (format, start, src.len(), n, sink.messages.len())
}

#[ma_trace::covers("STRUCT.OPEN.01")]
#[test]
fn detects_188_204_208() {
    for (name, format) in [
        ("h264_720p_closed", PacketFormat::Ts188),
        ("ts204_h264", PacketFormat::Rs204),
        ("ts208_h264", PacketFormat::Ts208),
    ] {
        let (f, start, len, n, msgs) = scan(name);
        assert_eq!(f, format, "{name}");
        assert_eq!(start, 0, "{name}");
        assert_eq!(len % format.size() as u64, 0, "{name} non tronqué");
        assert_eq!(n, len / format.size() as u64, "{name}");
        assert_eq!(msgs, 0, "{name}");
    }
}

#[ma_trace::covers("STRUCT.OPEN.02")]
#[test]
fn detects_tts_and_flags_timing() {
    let (_, _, _, reference, _) = scan("h264_720p_closed");
    let (f, start, len, n, msgs) = scan("tts_h264");
    assert_eq!(f, PacketFormat::Tts192);
    assert!(!f.timing_reliable());
    assert_eq!(start, 4, "premier 0x47 après le préfixe horodaté");
    assert_eq!(n, reference);
    assert_eq!(n, len / 192);
    assert_eq!(msgs, 0);
}
