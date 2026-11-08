//! Découpage en unités d'accès d'un flux élémentaire Mpeg2, en blocs de tailles variées.
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
    let mut f = framer(Codec::Mpeg2);
    let mut out = Vec::new();
    for (i, part) in rest.chunks(chunk).enumerate() {
        f.push(part, &mut out);
        // Une discontinuité de temps en temps, déterminée par le contenu.
        if part[0] == 0xFF && i % 7 == 0 {
            f.gap();
        }
        if out.len() > 4096 {
            out.clear();
        }
    }
    f.finish(&mut out);
});
