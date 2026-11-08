//! Découpage en unités d'accès d'un flux élémentaire H264, en blocs de tailles variées.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ma_core::Codec;
use ma_es::framer;

fuzz_target!(|data: &[u8]| {
    // Le 1er octet choisit la taille des blocs : les frontières de bloc tombent partout.
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    let chunk = usize::from(sel) % 251 + 1;
    let mut f = framer(Codec::H264);
    let mut out = Vec::new();
    for part in rest.chunks(chunk) {
        f.push(part, &mut out);
        if out.len() > 4096 {
            out.clear();
        }
    }
    f.finish(&mut out);
});
