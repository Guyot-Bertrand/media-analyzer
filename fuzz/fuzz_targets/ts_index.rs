//! Indexation complète (AU, PCR, structure) d'une source en mémoire.
#![no_main]

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use ma_core::VecSink;
use ma_io::testing::MemSource;
use ma_ts::{detect_format, index, IndexOptions, PacketFormat, TsIndex};

fuzz_target!(|data: &[u8]| {
    let (format, start) = detect_format(data).unwrap_or((PacketFormat::Ts188, 0));
    let src = MemSource::new(data.to_vec());
    let out = TsIndex::new(format, data.len() as u64);
    let opts = IndexOptions {
        chunk_size: 16 << 10,
        publish_every: Duration::ZERO,
    };
    let mut sink = VecSink::default();
    let _ = index(&src, start as u64, &out, &opts, &mut sink, &AtomicBool::new(false));
    // Lecture de tout ce qui a été publié.
    let _ = out.structure();
    let _ = out.progress();
    for (pid, _codec) in out.videos() {
        let _ = out.pcrs(pid);
        let _ = out.with_video(pid, |v| {
            for i in 0..v.published() {
                let _ = v.entry(i);
                let _ = v.au(i);
                let _ = v.decode_idx_of(i);
            }
        });
    }
});
