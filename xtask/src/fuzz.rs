//! Fuzzing des parseurs TS et ES (`cargo xtask fuzz`).
//!
//! Les cibles `cargo-fuzz` vivent dans `fuzz/` (hors workspace, nightly). La commande prépare un
//! corpus d'amorce (les 64 premiers Kio des fichiers du corpus généré, ou l'ES pour les cibles
//! `es_*`), lance `cargo +nightly fuzz run` cible par cible, résume l'exécution et échoue si un
//! artefact de plantage apparaît.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Cibles du dossier `fuzz/fuzz_targets/` (un test vérifie la correspondance).
pub const TARGETS: [&str; 5] = ["ts_scan", "ts_index", "es_h264", "es_hevc", "es_mpeg2"];
/// Durée par défaut d'une session (secondes).
pub const DEFAULT_SECS: u64 = 600;
/// Durée de l'étape de fuzzing court de la CI (secondes par cible).
pub const SHORT_SECS: u64 = 20;
/// Taille d'une amorce : les 64 premiers Kio d'un fichier.
const SEED_BYTES: usize = 64 << 10;
/// Taille maximale d'une entrée générée par libFuzzer.
const MAX_LEN: usize = SEED_BYTES;

/// Noms (sans extension) des fichiers `.rs` de `dir`.
#[cfg(test)]
pub fn targets_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    v.sort();
    v
}

/// Version de `cargo +nightly --version` (« cargo 1.97.0-nightly (…) ») ; `None` si ce n'est pas
/// un cargo nightly.
pub fn parse_nightly_version(output: &str) -> Option<String> {
    let v = output
        .trim()
        .strip_prefix("cargo ")?
        .split_whitespace()
        .next()?;
    v.contains("-nightly").then(|| v.to_owned())
}

/// Version du nightly installé, `None` s'il est absent.
pub fn nightly_version() -> Option<String> {
    let out = Command::new("cargo")
        .args(["+nightly", "--version"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_nightly_version(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

/// Chiffres d'une session libFuzzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub runs: u64,
    pub secs: u64,
    /// Blocs de base couverts (`cov:`).
    pub cov: u64,
}

impl Stats {
    /// Exécutions par seconde.
    #[allow(clippy::cast_precision_loss)]
    pub fn exec_per_sec(&self) -> f64 {
        self.runs as f64 / self.secs.max(1) as f64
    }
}

/// Extrait `cov:` d'une ligne de progression (`#123 REDUCE cov: 456 ft: …`) ou `runs`/`secs` de la
/// ligne finale (`Done 1234 runs in 10 second(s)`), et les cumule dans `stats`.
pub fn absorb_line(stats: &mut Stats, line: &str) {
    if let Some(rest) = line.strip_prefix("Done ") {
        let mut it = rest.split_whitespace();
        if let (Some(runs), Some("runs"), Some("in"), Some(secs)) =
            (it.next(), it.next(), it.next(), it.next())
        {
            stats.runs = runs.parse().unwrap_or(stats.runs);
            stats.secs = secs.parse().unwrap_or(stats.secs);
        }
    } else if line.starts_with('#') {
        let mut it = line.split_whitespace();
        while let Some(w) = it.next() {
            if w == "cov:"
                && let Some(n) = it.next().and_then(|n| n.parse().ok())
            {
                stats.cov = n;
            }
        }
    }
}

/// Format `ffmpeg` (`-f`) de l'ES brut d'une cible `es_*`, et préfixe des fichiers du corpus.
fn es_source(target: &str) -> Option<(&'static str, &'static str)> {
    match target {
        "es_h264" => Some(("h264", "h264_")),
        "es_hevc" => Some(("hevc", "hevc_")),
        "es_mpeg2" => Some(("mpeg2video", "mpeg2_")),
        _ => None,
    }
}

fn write_seed(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::write(dir.join(name), &bytes[..bytes.len().min(SEED_BYTES)])?;
    Ok(())
}

/// Amorces de `target` dans `dest` ; renvoie le nombre de fichiers écrits. Un corpus absent donne
/// zéro amorce (avertissement), pas une erreur.
fn seed(target: &str, corpus: &Path, dest: &Path) -> anyhow::Result<usize> {
    std::fs::create_dir_all(dest)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ts"))
        // Les gros fichiers ne servent qu'à la mesure de débit : leurs 64 Kio valent ceux des autres.
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("uhd_big"))
        })
        .collect();
    files.sort();
    let mut n = 0;
    for p in files {
        let name = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match es_source(target) {
            None => {
                let mut head = Vec::new();
                std::fs::File::open(&p)?
                    .take(SEED_BYTES as u64)
                    .read_to_end(&mut head)?;
                write_seed(dest, &name, &head)?;
                n += 1;
            }
            Some((fmt, prefix)) => {
                if !name.starts_with(prefix) {
                    continue;
                }
                // L'ES est extrait par ffmpeg (outil de génération, jamais le moteur).
                let out = Command::new("ffmpeg")
                    .args(["-v", "error", "-y", "-i"])
                    .arg(&p)
                    .args([
                        "-map",
                        "0:v:0",
                        "-c",
                        "copy",
                        "-frames:v",
                        "40",
                        "-f",
                        fmt,
                        "-",
                    ])
                    .stderr(Stdio::null())
                    .output();
                if let Ok(o) = out
                    && o.status.success()
                    && !o.stdout.is_empty()
                {
                    write_seed(dest, &name, &o.stdout)?;
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

/// Artefacts de plantage présents pour `target`.
fn crash_artifacts(fuzz_dir: &Path, target: &str) -> Vec<PathBuf> {
    std::fs::read_dir(fuzz_dir.join("artifacts").join(target))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect()
}

/// Lance une cible `secs` secondes ; renvoie ses chiffres, ou une erreur si elle plante.
fn run_target(root: &Path, corpus: &Path, target: &str, secs: u64) -> anyhow::Result<Stats> {
    let fuzz_dir = root.join("fuzz");
    let seeds = seed(target, corpus, &fuzz_dir.join("corpus").join(target))?;
    println!("==> fuzz {target} : {secs} s, {seeds} amorce(s)");
    if seeds == 0 {
        eprintln!(
            "avertissement : aucune amorce pour {target} (corpus absent : cargo xtask corpus gen)"
        );
    }
    let mut child = Command::new("cargo")
        .args([
            "+nightly",
            "fuzz",
            "run",
            target,
            "--",
            "-print_final_stats=1",
        ])
        .arg(format!("-max_total_time={secs}"))
        .arg(format!("-max_len={MAX_LEN}"))
        .current_dir(root)
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stats = Stats::default();
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("stderr non capturé"))?;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        eprintln!("{line}");
        absorb_line(&mut stats, &line);
    }
    let exit = child.wait()?;
    let crashes = crash_artifacts(&fuzz_dir, target);
    anyhow::ensure!(
        exit.success() && crashes.is_empty(),
        "{target} : plantage ({}) ; entrées dans fuzz/artifacts/{target}/ : {}",
        exit,
        crashes
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(stats)
}

/// Étape « fuzzing court » de la CI : `SHORT_SECS` par cible si nightly est présent, sinon un
/// avertissement (D5). Renvoie `false` si une cible plante.
pub fn short_run(root: &Path) -> anyhow::Result<bool> {
    if nightly_version().is_none() {
        eprintln!("avertissement : nightly absent : fuzzing court non exécuté");
        return Ok(true);
    }
    println!("==> fuzzing court ({SHORT_SECS} s par cible)");
    let ok = run(root, SHORT_SECS, None)?;
    if !ok {
        eprintln!("échec de l'étape : fuzzing court");
    }
    Ok(ok)
}

/// `cargo xtask fuzz` : lance `targets` (toutes si vide) `secs` secondes chacune.
pub fn run(root: &Path, secs: u64, only: Option<&str>) -> anyhow::Result<bool> {
    let Some(version) = nightly_version() else {
        anyhow::bail!(
            "nightly absent : rustup toolchain install nightly --profile minimal ; cargo install cargo-fuzz --locked (BUILD.md)"
        );
    };
    println!("==> fuzz : cargo {version}");
    let targets: Vec<&str> = match only {
        Some(t) => {
            anyhow::ensure!(
                TARGETS.contains(&t),
                "cible inconnue « {t} » (cibles : {})",
                TARGETS.join(", ")
            );
            vec![t]
        }
        None => TARGETS.to_vec(),
    };
    let corpus = crate::corpus::corpus_dir(None)?;
    let mut ok = true;
    println!("cible       durée  exécutions   exec/s    cov");
    let mut rows = Vec::new();
    for t in targets {
        match run_target(root, &corpus, t, secs) {
            Ok(s) => rows.push(format!(
                "{t:<10} {:>4} s {:>11} {:>8.0} {:>6}",
                s.secs,
                s.runs,
                s.exec_per_sec(),
                s.cov
            )),
            Err(e) => {
                eprintln!("erreur : {e}");
                ok = false;
                break;
            }
        }
    }
    for r in rows {
        println!("{r}");
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_targets_match_directory() {
        let dir = crate::repo_root().join("fuzz/fuzz_targets");
        let mut on_disk = targets_in(&dir);
        on_disk.sort();
        let mut listed: Vec<String> = TARGETS.iter().map(|t| (*t).to_owned()).collect();
        listed.sort();
        assert_eq!(listed, on_disk);
    }

    #[test]
    fn nightly_detection_parses_version() {
        assert_eq!(
            parse_nightly_version("cargo 1.97.0-nightly (abc123def 2026-11-01)\n"),
            Some("1.97.0-nightly".to_owned())
        );
        assert_eq!(parse_nightly_version("cargo 1.95.0 (abc 2026-01-01)"), None);
        assert_eq!(
            parse_nightly_version("error: toolchain 'nightly' is not installed"),
            None
        );
        assert_eq!(parse_nightly_version(""), None);
    }

    #[test]
    fn libfuzzer_output_is_summarised() {
        let mut s = Stats::default();
        absorb_line(
            &mut s,
            "#2\tINITED cov: 120 ft: 300 corp: 2/9Kb exec/s: 0 rss: 40Mb",
        );
        absorb_line(
            &mut s,
            "#9000\tREDUCE cov: 150 ft: 340 corp: 5/20Kb lim: 4096 exec/s: 900",
        );
        absorb_line(&mut s, "Done 9500 runs in 10 second(s)");
        assert_eq!(
            s,
            Stats {
                runs: 9500,
                secs: 10,
                cov: 150
            }
        );
        assert!((s.exec_per_sec() - 950.0).abs() < 1e-9);
    }

    #[test]
    fn es_targets_map_to_ffmpeg_formats() {
        assert_eq!(es_source("es_hevc"), Some(("hevc", "hevc_")));
        assert_eq!(es_source("ts_scan"), None);
    }
}
