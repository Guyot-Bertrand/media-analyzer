//! Référentiel d'exigences : modèle, chargement des fichiers YAML et validation.

use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
pub enum Lot {
    #[serde(rename = "MVP")]
    Mvp,
    V1,
    V2,
    V3,
    V4,
    V5,
    #[serde(rename = "LATER")]
    Later,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Unit,
    Ui,
    E2e,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Unit => "unit",
            Level::Ui => "ui",
            Level::E2e => "e2e",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Draft,
    Validated,
    Implemented,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequirement {
    id: String,
    title: String,
    description: String,
    domain: String,
    lot: Lot,
    acceptance: Vec<String>,
    levels: Vec<Level>,
    source: String,
    status: Status,
}

#[derive(Debug, Clone)]
pub struct Requirement {
    pub id: String,
    pub title: String,
    pub description: String,
    /// `F1`..`F18` ou `TRANS`.
    pub domain: String,
    pub lot: Lot,
    pub acceptance: Vec<String>,
    pub levels: Vec<Level>,
    pub source: String,
    pub status: Status,
    pub file: PathBuf,
    pub line: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub current_lot: Lot,
}

/// Problème localisé dans un fichier (erreur de chargement, avertissement de scan, erreur de garde).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(l) => write!(f, "{}:{}: {}", self.file.display(), l, self.message),
            None => write!(f, "{}: {}", self.file.display(), self.message),
        }
    }
}

impl Diagnostic {
    pub fn new(file: impl Into<PathBuf>, line: Option<usize>, message: impl Into<String>) -> Self {
        Diagnostic {
            file: file.into(),
            line,
            message: message.into(),
        }
    }
}

/// Vrai si `id` respecte `^[A-Z][A-Z0-9]*(\.[A-Z0-9]+)*\.[0-9]{2}$`
/// (même règle que l'attribut `ma_trace::covers`, qui ne peut pas exporter de fonction).
pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    let Some((last, head)) = parts.split_last() else {
        return false;
    };
    if head.is_empty() || last.len() != 2 || !last.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if !head[0]
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_uppercase)
    {
        return false;
    }
    head.iter().all(|p| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

fn valid_domain(d: &str) -> bool {
    d == "TRANS"
        || d.strip_prefix('F')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=18).contains(&n))
}

/// Lignes (1-based) des éléments `- id:` d'un fichier, dans l'ordre.
fn id_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            l.trim_start()
                .strip_prefix('-')
                .is_some_and(|r| r.trim_start().starts_with("id:"))
        })
        .map(|(i, _)| i + 1)
        .collect()
}

/// Charge `config.yaml` et toutes les exigences (`*.yaml`) sous `dir`.
pub fn load(dir: &Path) -> Result<(Config, Vec<Requirement>), Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let config_path = dir.join("config.yaml");
    let config = match std::fs::read_to_string(&config_path) {
        Ok(t) => match serde_yaml_ng::from_str::<Config>(&t) {
            Ok(c) => Some(c),
            Err(e) => {
                diags.push(Diagnostic::new(
                    &config_path,
                    None,
                    format!("configuration invalide : {e}"),
                ));
                None
            }
        },
        Err(e) => {
            diags.push(Diagnostic::new(
                &config_path,
                None,
                format!("configuration illisible : {e}"),
            ));
            None
        }
    };

    let mut files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|p| p.extension().is_some_and(|e| e == "yaml") && p != &config_path)
        .collect();
    files.sort();

    let mut reqs = Vec::new();
    for file in files {
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => {
                diags.push(Diagnostic::new(
                    &file,
                    None,
                    format!("fichier illisible : {e}"),
                ));
                continue;
            }
        };
        let raws: Vec<RawRequirement> = match serde_yaml_ng::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let line = e.location().map(|l| l.line());
                diags.push(Diagnostic::new(&file, line, format!("YAML invalide : {e}")));
                continue;
            }
        };
        let lines = id_lines(&text);
        for (i, r) in raws.into_iter().enumerate() {
            let line = lines.get(i).copied().unwrap_or(0);
            let at = |msg: String| Diagnostic::new(&file, Some(line), msg);
            if !valid_id(&r.id) {
                diags.push(at(format!("identifiant invalide : {:?}", r.id)));
            }
            if !valid_domain(&r.domain) {
                diags.push(at(format!(
                    "{} : domaine invalide {:?} (F1..F18 ou TRANS)",
                    r.id, r.domain
                )));
            }
            if r.acceptance.is_empty() {
                diags.push(at(format!("{} : aucun critère d'acceptation", r.id)));
            }
            if r.levels.is_empty() {
                diags.push(at(format!("{} : aucun niveau de test", r.id)));
            }
            reqs.push(Requirement {
                id: r.id,
                title: r.title,
                description: r.description,
                domain: r.domain,
                lot: r.lot,
                acceptance: r.acceptance,
                levels: r.levels,
                source: r.source,
                status: r.status,
                file: file.clone(),
                line,
            });
        }
    }

    let mut seen: HashMap<&str, &Requirement> = HashMap::new();
    for r in &reqs {
        if let Some(prev) = seen.insert(&r.id, r) {
            diags.push(Diagnostic::new(
                &r.file,
                Some(r.line),
                format!(
                    "identifiant en double {} (déjà défini dans {}:{})",
                    r.id,
                    prev.file.display(),
                    prev.line
                ),
            ));
        }
    }

    match config {
        Some(c) if diags.is_empty() => Ok((c, reqs)),
        _ => Err(diags),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/req")
            .join(name)
    }

    #[test]
    fn loads_valid_requirements() {
        let (config, reqs) = load(&fixture("ok")).unwrap();
        assert_eq!(config.current_lot, Lot::Mvp);
        assert_eq!(reqs.len(), 3);
        let goto = reqs.iter().find(|r| r.id == "NAV.GOTO.01").unwrap();
        assert_eq!(goto.levels, vec![Level::Ui, Level::E2e]);
        assert_eq!(goto.status, Status::Validated);
        assert_eq!(goto.line, 1);
        let color = reqs.iter().find(|r| r.id == "UI.COLOR.01").unwrap();
        assert_eq!(
            (color.lot, color.line, color.domain.as_str()),
            (Lot::V1, 11, "TRANS")
        );
    }

    #[test]
    fn rejects_duplicate_id() {
        let diags = load(&fixture("dup")).unwrap_err();
        let d = diags
            .iter()
            .find(|d| d.message.contains("identifiant en double"))
            .unwrap();
        assert!(
            d.message.contains("a.yaml") && d.file.ends_with("b.yaml"),
            "{d}"
        );
    }

    #[test]
    fn rejects_invalid_id() {
        let diags = load(&fixture("bad_id")).unwrap_err();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("ts.tr1.01") && d.line == Some(1)),
            "{diags:?}"
        );
    }

    #[test]
    fn reports_malformed_yaml_without_panic() {
        let diags = load(&fixture("bad_yaml")).unwrap_err();
        assert!(
            diags
                .iter()
                .any(|d| d.file.ends_with("a.yaml") && d.message.contains("YAML invalide")),
            "{diags:?}"
        );
    }

    #[test]
    fn rejects_empty_acceptance_or_levels() {
        let diags = load(&fixture("incomplete")).unwrap_err();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("TS.X.01 : aucun critère"))
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("TS.X.02 : aucun niveau"))
        );
    }
}
