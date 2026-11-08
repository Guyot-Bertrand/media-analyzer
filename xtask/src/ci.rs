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

/// Crate dont la couverture n'est garantie qu'avec un périphérique VA-API (chemin matériel).
const GPU_CRATE: &str = "ma-ffmpeg";

/// Feature de `ma-ffmpeg` pour la version de libavcodec `modversion` (`pkg-config --modversion
/// libavcodec`) : 60 ⇒ `ffmpeg6`, 61 ⇒ `ffmpeg7`, 62 ⇒ `ffmpeg8` ; `None` pour une autre version.
pub fn ffmpeg_feature(modversion: &str) -> Option<&'static str> {
    match modversion.trim().split('.').next()? {
        "60" => Some("ffmpeg6"),
        "61" => Some("ffmpeg7"),
        "62" => Some("ffmpeg8"),
        _ => None,
    }
}

/// Vrai si l'un des noms d'entrée de `/dev/dri` est un nœud de rendu (`renderD*`).
pub fn has_render_node<I: IntoIterator<Item = String>>(names: I) -> bool {
    names.into_iter().any(|n| n.starts_with("renderD"))
}

/// Vrai si la sortie de `vainfo` annonce le traitement vidéo VA-API (VPP), nécessaire à la
/// réduction sur le GPU.
pub fn vainfo_has_vpp(vainfo: &str) -> bool {
    vainfo.contains("VAEntrypointVideoProc")
}

/// Sépare les crates sous le seuil en échecs et en avertissements : sans GPU complet (VA-API et
/// VPP), la couverture de `ma-ffmpeg` n'est pas bloquante.
pub fn split_gpu_exempt(
    below: Vec<(String, Option<f64>)>,
    gpu: bool,
) -> (Vec<(String, Option<f64>)>, bool) {
    let (warned, failing): (Vec<_>, Vec<_>) =
        below.into_iter().partition(|(n, _)| !gpu && n == GPU_CRATE);
    (failing, !warned.is_empty())
}

fn libavcodec_feature() -> anyhow::Result<(String, &'static str)> {
    let out = Command::new("pkg-config")
        .args(["--modversion", "libavcodec"])
        .output()
        .map_err(|e| anyhow::anyhow!("pkg-config introuvable ({e}) : voir BUILD.md"))?;
    anyhow::ensure!(
        out.status.success(),
        "pkg-config ne trouve pas libavcodec : installer les paquets de développement FFmpeg (BUILD.md)"
    );
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let feature = ffmpeg_feature(&version).ok_or_else(|| {
        anyhow::anyhow!("libavcodec {version} non prise en charge (60, 61 ou 62 attendue)")
    })?;
    Ok((version, feature))
}

/// VPP détectée par `vainfo` (absent ou en échec : non détectée).
fn vpp_present() -> bool {
    Command::new("vainfo")
        .output()
        .is_ok_and(|o| o.status.success() && vainfo_has_vpp(&String::from_utf8_lossy(&o.stdout)))
}

fn render_node_present() -> bool {
    std::fs::read_dir("/dev/dri").is_ok_and(|d| {
        has_render_node(
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        )
    })
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

fn step(
    name: &str,
    program: &str,
    args: &[&str],
    root: &Path,
    env: &[(&str, &str)],
) -> anyhow::Result<bool> {
    println!("==> {name}");
    let status = Command::new(program)
        .args(args)
        .envs(env.iter().copied())
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
    let (version, feature) = libavcodec_feature()?;
    let features = format!("ma-ffmpeg/{feature}");
    println!("==> FFmpeg : libavcodec {version} ⇒ feature {feature}");
    let gpu = render_node_present();
    let vpp = gpu && vpp_present();
    let env: &[(&str, &str)] = match (gpu, vpp) {
        (true, true) => {
            println!(
                "==> VA-API : périphérique et VPP présents ⇒ MA_REQUIRE_GPU=1, MA_REQUIRE_VPP=1"
            );
            &[("MA_REQUIRE_GPU", "1"), ("MA_REQUIRE_VPP", "1")]
        }
        (true, false) => {
            println!(
                "==> VA-API : périphérique présent, VPP non détectée (vainfo) ⇒ MA_REQUIRE_GPU=1, \
                 tests VPP sautés"
            );
            &[("MA_REQUIRE_GPU", "1")]
        }
        _ => {
            println!("==> VA-API : aucun périphérique /dev/dri/renderD*");
            &[]
        }
    };
    let steps: Vec<(&str, Vec<&str>)> = vec![
        ("formatage", vec!["fmt", "--check"]),
        (
            "clippy",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--features",
                &features,
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("licences et avis (cargo deny)", vec!["deny", "check"]),
        ("corpus gen", vec!["xtask", "corpus", "gen"]),
        (
            "tests + couverture",
            vec![
                "llvm-cov",
                "nextest",
                "--workspace",
                "--features",
                &features,
                "--profile",
                "ci",
                "--lcov",
                "--output-path",
                &lcov_arg,
            ],
        ),
    ];
    for (name, args) in &steps {
        if !step(name, &cargo, args, root, env)? {
            return Ok(false);
        }
    }

    println!("==> seuil de couverture ({COVERAGE_THRESHOLD} %)");
    let cov = coverage_by_crate(&std::fs::read_to_string(&lcov)?, root);
    for (name, pct) in &cov {
        println!("    {name} : {pct:.1} %");
    }
    let below = crates_below(&cov, COVERAGE_THRESHOLD, &engine_crates(root));
    let (below, warned) = split_gpu_exempt(below, vpp);
    if warned {
        eprintln!("avertissement : couverture de ma-ffmpeg non garantie : pas de VA-API avec VPP");
    }
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

    if !crate::fuzz::short_run(root)? {
        return Ok(false);
    }

    let junit = root.join("target/nextest/ci/junit.xml");
    let junit_arg = format!("unit={}", junit.to_string_lossy());
    let mut trace = vec!["xtask", "trace", "--results", &junit_arg];
    if release {
        trace.push("--release");
    }
    step("traçabilité", &cargo, &trace, root, &[])
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
    fn ffmpeg_feature_follows_the_libavcodec_major() {
        assert_eq!(ffmpeg_feature("60.31.102"), Some("ffmpeg6"));
        assert_eq!(ffmpeg_feature("61.19.100\n"), Some("ffmpeg7"));
        assert_eq!(ffmpeg_feature("62.11.100"), Some("ffmpeg8"));
        assert_eq!(ffmpeg_feature("59.37.100"), None);
        assert_eq!(ffmpeg_feature("600.1"), None);
        assert_eq!(ffmpeg_feature(""), None);
    }

    #[test]
    fn render_nodes_are_detected() {
        let names = |v: &[&str]| v.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert!(has_render_node(names(&["by-path", "card0", "renderD128"])));
        assert!(!has_render_node(names(&["by-path", "card0"])));
        assert!(!has_render_node(Vec::new()));
    }

    #[test]
    fn vpp_is_detected_in_vainfo_output() {
        let with = "VAProfileNone                   :\tVAEntrypointVideoProc\n";
        let without = "VAProfileHEVCMain10             :\tVAEntrypointVLD\n";
        assert!(vainfo_has_vpp(with));
        assert!(!vainfo_has_vpp(without));
        assert!(!vainfo_has_vpp(""));
    }

    #[test]
    fn ffmpeg_coverage_is_not_blocking_without_gpu() {
        let below = vec![
            ("ma-ffmpeg".to_owned(), Some(70.0)),
            ("ma-bad".to_owned(), Some(80.0)),
        ];
        let (failing, warned) = split_gpu_exempt(below.clone(), false);
        assert_eq!(failing, vec![("ma-bad".to_owned(), Some(80.0))]);
        assert!(warned);
        let (failing, warned) = split_gpu_exempt(below, true);
        assert_eq!(failing.len(), 2);
        assert!(!warned);
        let (failing, warned) = split_gpu_exempt(vec![("ma-bad".to_owned(), None)], false);
        assert_eq!(failing.len(), 1);
        assert!(!warned);
    }

    #[test]
    fn engine_crate_without_coverage_data_fails() {
        let cov = coverage_by_crate(LCOV, Path::new("/repo"));
        let below = crates_below(&cov, 90.0, &engine(&["ma-good", "ma-missing"]));
        assert_eq!(below, vec![("ma-missing".to_owned(), None)]);
    }
}
