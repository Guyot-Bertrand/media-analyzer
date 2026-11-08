//! Balayage de paquets, structure PSI et assemblage PES sur des octets quelconques.
#![no_main]

use std::collections::HashMap;

use libfuzzer_sys::fuzz_target;
use ma_core::VecSink;
use ma_io::testing::MemSource;
use ma_io::ReadAt;
use ma_ts::{detect_format, PacketFormat, PacketScanner, PesAssembler, StructureBuilder};

/// PID suivis au plus : borne la mémoire quel que soit le flux.
const MAX_PIDS: usize = 8;

fuzz_target!(|data: &[u8]| {
    let (format, start) = detect_format(data).unwrap_or((PacketFormat::Ts188, 0));
    let src = MemSource::new(data.to_vec());
    let mut sink = VecSink::default();
    let mut scanner = PacketScanner::new(&src, format, start as u64, None);
    let mut structure = StructureBuilder::new(format);
    let mut assemblers: HashMap<u16, PesAssembler> = HashMap::new();
    while let Ok(Some(pkt)) = scanner.next(&mut sink) {
        structure.push(&pkt, &mut sink);
        let pid = pkt.packet.header.pid;
        if assemblers.len() < MAX_PIDS || assemblers.contains_key(&pid) {
            let asm = assemblers.entry(pid).or_insert_with(|| PesAssembler::new(pid));
            let _ = asm.push(&pkt, &mut sink);
        }
        // Les messages sont jetés régulièrement : la mémoire ne dépend pas du nombre de paquets.
        if sink.messages.len() > 1024 {
            sink.messages.clear();
        }
    }
    let _ = structure.structure();
    let _ = structure.complete();
    let _ = src.len();
});
