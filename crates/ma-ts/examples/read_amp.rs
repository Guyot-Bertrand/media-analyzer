//! Amplification de lecture de la navigation (07 §3, décision R19) : lit les AU d'un TS par
//! `TsAuSource::read_au`, selon trois motifs, et rapporte les octets lus par le cache de blocs
//! aux octets d'AU rendus. Rien n'est décodé : c'est le motif de lecture du navigateur, sans son
//! préchargement.
//!
//! ```text
//! cargo run --release -p ma-ts --example read_amp -- <fichier.ts>
//! ```
//!
//! Motifs, à graine fixe :
//! - **sauts** : 100 cibles au hasard ; pour chacune, de la RAP qui la précède (ordre de
//!   décodage) jusqu'à la cible ;
//! - **séquentiel** : jusqu'à 2 000 AU consécutives depuis total/4 ;
//! - **AU isolées** : 200 AU au hasard (pire cas, aucune voisine lue).

use std::path::PathBuf;
use std::process::ExitCode;

use ma_core::{AuSource, RapKind};
use ma_ts::TsAuSource;

const SEED: u64 = 0x5eed;
const JUMPS: u32 = 100;
const SEQUENTIAL: u32 = 2000;
const ISOLATED: u32 = 200;

/// Générateur congruentiel (graine fixe, reproductible).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        // Le reste est < n ≤ u32::MAX : la conversion ne perd rien.
        u32::try_from((self.0 >> 33) % u64::from(n.max(1))).unwrap_or(0)
    }
}

/// Lit les AU `aus` et rend (AU lues, octets d'AU, octets lus, lectures).
fn measure(
    src: &TsAuSource,
    aus: impl IntoIterator<Item = u32>,
) -> Result<(u64, u64, u64, u64), String> {
    let before = src.io_stats();
    let mut buf = Vec::new();
    let (mut count, mut bytes) = (0u64, 0u64);
    for i in aus {
        src.read_au(i, &mut buf)
            .map_err(|e| format!("AU {i} : {e}"))?;
        count += 1;
        bytes += buf.len() as u64;
    }
    let after = src.io_stats();
    Ok((
        count,
        bytes,
        after.bytes_read - before.bytes_read,
        after.syscalls - before.syscalls,
    ))
}

#[allow(clippy::cast_precision_loss)]
fn print(name: &str, (count, bytes, read, calls): (u64, u64, u64, u64)) {
    let ratio = read as f64 / bytes.max(1) as f64;
    let per_au = read as f64 / count.max(1) as f64;
    let au_mean = bytes as f64 / count.max(1) as f64;
    println!(
        "{name} : {count} AU, {bytes} o d'AU, {read} o lus, {calls} lectures, \
         amplification {ratio:.3}, {per_au:.0} o lus par AU, AU moyenne {au_mean:.0} o"
    );
}

fn real_main() -> Result<(), String> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage : read_amp <fichier.ts>")?;
    let src = TsAuSource::open(&path).map_err(|e| format!("ouverture : {e}"))?;
    let total = src.len();
    if total == 0 {
        return Err("aucune image indexée".into());
    }
    let mut rng = Rng(SEED);

    let mut jumps = Vec::new();
    for _ in 0..JUMPS {
        let target = rng.below(total);
        let mut rap = target;
        while rap > 0 && src.entry(rap).is_some_and(|e| e.rap == RapKind::None) {
            rap -= 1;
        }
        jumps.extend(rap..=target);
    }
    print("sauts", measure(&src, jumps)?);

    let from = total / 4;
    let to = from.saturating_add(SEQUENTIAL).min(total);
    print("séquentiel", measure(&src, from..to)?);

    let isolated: Vec<u32> = (0..ISOLATED).map(|_| rng.below(total)).collect();
    print("AU isolées", measure(&src, isolated)?);
    Ok(())
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("read_amp : {e}");
            ExitCode::FAILURE
        }
    }
}
