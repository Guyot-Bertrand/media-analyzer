//! Pipeline de vérification locale (`cargo xtask ci`), identique à celui de la CI.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Seuil de couverture des lignes pour les crates du moteur (`crates/ma-*`, hors `ma-trace`).
pub const COVERAGE_THRESHOLD: f64 = 90.0;
/// Crates exemptées du seuil : macro sans logique exécutable.
const EXEMPT: [&str; 1] = ["ma-trace"];

/// Couverture des lignes (en %) par crate `crates/<nom>`, à partir d'un rapport lcov.
pub fn coverage_by_crate(lcov: &str, root: &Path) -> BTreeMap<String, f64> {
    let crates = root.join("crates");
    let mut acc: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lcov.lines() {
        if let Some(file) = line.strip_prefix("SF:") {
            current = Path::new(file)
                .strip_prefix(&crates)
                .ok()
                .and_then(|r| r.components().next())
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
        } else if let Some(name) = &current {
            let entry = acc.entry(name.clone()).or_default();
            if let Some(v) = line.strip_prefix("LF:") {
                entry.0 += v.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("LH:") {
                entry.1 += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    acc.into_iter()
        .filter(|(_, (found, _))| *found > 0)
        .map(|(name, (found, hit))| {
            #[allow(clippy::cast_precision_loss)]
            let pct = hit as f64 * 100.0 / found as f64;
            (name, pct)
        })
        .collect()
}

/// Crates du moteur (`crates/ma-*` hors exemptées) sous le seuil ; `None` = aucune donnée de couverture.
pub fn crates_below(
    cov: &BTreeMap<String, f64>,
    threshold: f64,
    engine: &[String],
) -> Vec<(String, Option<f64>)> {
    engine
        .iter()
        .filter(|n| !EXEMPT.contains(&n.as_str()))
        .filter_map(|n| match cov.get(n) {
            Some(p) if *p >= threshold => None,
            other => Some((n.clone(), other.copied())),
        })
        .collect()
}

/// Noms des crates du moteur présentes sous `crates/` (dossiers `ma-*`).
pub fn engine_crates(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root.join("crates"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().join("Cargo.toml").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ma-"))
        .collect();
    v.sort();
    v
}

fn step(name: &str, program: &str, args: &[&str], root: &Path) -> anyhow::Result<bool> {
    println!("==> {name}");
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()?;
    if !status.success() {
        eprintln!("échec de l'étape : {name}");
    }
    Ok(status.success())
}

/// Enchaîne les étapes ; s'arrête au premier échec. Renvoie `false` en cas d'échec.
pub fn run(root: &Path, release: bool) -> anyhow::Result<bool> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let lcov = root.join("target/lcov.info");
    let lcov_arg = lcov.to_string_lossy().into_owned();
    let steps: Vec<(&str, Vec<&str>)> = vec![
        ("formatage", vec!["fmt", "--check"]),
        (
            "clippy",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("licences et avis (cargo deny)", vec!["deny", "check"]),
        (
            "tests + couverture",
            vec![
                "llvm-cov",
                "nextest",
                "--workspace",
                "--profile",
                "ci",
                "--lcov",
                "--output-path",
                &lcov_arg,
            ],
        ),
    ];
    for (name, args) in &steps {
        if !step(name, &cargo, args, root)? {
            return Ok(false);
        }
    }

    println!("==> seuil de couverture ({COVERAGE_THRESHOLD} %)");
    let cov = coverage_by_crate(&std::fs::read_to_string(&lcov)?, root);
    for (name, pct) in &cov {
        println!("    {name} : {pct:.1} %");
    }
    let below = crates_below(&cov, COVERAGE_THRESHOLD, &engine_crates(root));
    if !below.is_empty() {
        for (name, pct) in below {
            match pct {
                Some(p) => {
                    eprintln!("couverture insuffisante : {name} {p:.1} % < {COVERAGE_THRESHOLD} %");
                }
                None => {
                    eprintln!("couverture absente : {name} n'apparaît pas dans le rapport lcov");
                }
            }
        }
        return Ok(false);
    }

    let junit = root.join("target/nextest/ci/junit.xml");
    let junit_arg = format!("unit={}", junit.to_string_lossy());
    let mut trace = vec!["xtask", "trace", "--results", &junit_arg];
    if release {
        trace.push("--release");
    }
    step("traçabilité", &cargo, &trace, root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const LCOV: &str = "SF:/repo/crates/ma-good/src/lib.rs\nLF:100\nLH:95\nend_of_record\n\
SF:/repo/crates/ma-bad/src/lib.rs\nLF:50\nLH:40\nend_of_record\n\
SF:/repo/crates/ma-bad/src/x.rs\nLF:50\nLH:40\nend_of_record\n\
SF:/repo/crates/ma-trace/src/lib.rs\nLF:10\nLH:0\nend_of_record\n\
SF:/repo/xtask/src/main.rs\nLF:10\nLH:0\nend_of_record\n";

    #[test]
    fn coverage_is_aggregated_per_crate() {
        let cov = coverage_by_crate(LCOV, Path::new("/repo"));
        assert_eq!(cov.len(), 3);
        assert!((cov["ma-good"] - 95.0).abs() < 1e-9);
        assert!((cov["ma-bad"] - 80.0).abs() < 1e-9);
    }

    fn engine(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn coverage_gate_lists_crates_below_threshold() {
        let cov = coverage_by_crate(LCOV, Path::new("/repo"));
        let below = crates_below(&cov, 90.0, &engine(&["ma-good", "ma-bad", "ma-trace"]));
        assert_eq!(below, vec![("ma-bad".to_owned(), Some(80.0))]);
    }

    #[test]
    fn engine_crate_without_coverage_data_fails() {
        let cov = coverage_by_crate(LCOV, Path::new("/repo"));
        let below = crates_below(&cov, 90.0, &engine(&["ma-good", "ma-missing"]));
        assert_eq!(below, vec![("ma-missing".to_owned(), None)]);
    }
}
