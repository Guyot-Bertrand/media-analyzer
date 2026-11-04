//! Accès au corpus généré par `cargo xtask corpus gen` et à son manifeste.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ma_decode::Size;
use serde::Deserialize;

/// Manifeste versionné, embarqué à la compilation (les bancs n'ont pas le dépôt sous la main).
const MANIFEST_TOML: &str = include_str!("../../../corpus/manifest.toml");

#[derive(Debug, Deserialize)]
struct Manifest {
    file: Vec<CorpusEntry>,
}

/// Description d'un fichier du corpus (voir `corpus/manifest.toml`).
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusEntry {
    pub name: String,
    pub file: String,
    /// Nom ffprobe du codec.
    pub codec: String,
    pub profile: String,
    /// `WxH` ; voir [`CorpusEntry::dims`].
    pub size: String,
    pub fps: u32,
    /// Nombre d'images (un maximum mesuré pour `hevc_720p_cut`).
    pub frames: u32,
    pub gop: u32,
    pub bframes: u32,
    pub code_bits: u32,
    pub code_modulo: u32,
    pub features: Vec<String>,
    pub big: bool,
    pub bitrate: Option<String>,
    pub derived_from: Option<String>,
}

impl CorpusEntry {
    /// Vrai si le fichier porte la caractéristique `feature`.
    #[must_use]
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Taille de l'image.
    ///
    /// # Panics
    /// Si `size` n'est pas de la forme `WxH` (le manifeste est validé au chargement).
    #[must_use]
    pub fn dims(&self) -> Size {
        parse_dims(&self.size).expect("taille « WxH » attendue")
    }
}

fn parse_dims(s: &str) -> Option<Size> {
    let (w, h) = s.split_once('x')?;
    Some(Size {
        w: w.parse().ok()?,
        h: h.parse().ok()?,
    })
}

fn entries() -> &'static [CorpusEntry] {
    static ENTRIES: OnceLock<Vec<CorpusEntry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let m: Manifest = toml::from_str(MANIFEST_TOML).expect("corpus/manifest.toml invalide");
        for e in &m.file {
            assert!(
                parse_dims(&e.size).is_some(),
                "manifeste : taille « {} » invalide pour {}",
                e.size,
                e.name
            );
        }
        m.file
    })
}

/// Entrée du manifeste pour `name`.
///
/// # Panics
/// Si le manifeste ne connaît pas `name`.
#[must_use]
pub fn manifest(name: &str) -> CorpusEntry {
    entries()
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("fichier de corpus inconnu du manifeste : {name}"))
        .clone()
}

fn corpus_dir_from(env: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(dir) = env.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    let home = home.expect("HOME non défini : fixer MA_CORPUS_DIR");
    PathBuf::from(home).join("media-analyzer-corpus/generated")
}

/// Dossier du corpus : `$MA_CORPUS_DIR`, sinon `~/media-analyzer-corpus/generated`.
///
/// # Panics
/// Si ni `MA_CORPUS_DIR` ni `HOME` ne sont définis.
#[must_use]
pub fn corpus_dir() -> PathBuf {
    corpus_dir_from(std::env::var_os("MA_CORPUS_DIR"), std::env::var_os("HOME"))
}

fn file_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let path = dir.join(manifest(name).file);
    path.is_file().then_some(path)
}

fn absent(dir: &Path, name: &str) -> ! {
    let hint = if manifest(name).big { " --big" } else { "" };
    panic!(
        "corpus absent : lancer cargo xtask corpus gen{hint} ({} manquant dans {})",
        name,
        dir.display()
    );
}

fn corpus_file_in(dir: &Path, name: &str) -> PathBuf {
    file_in(dir, name).unwrap_or_else(|| absent(dir, name))
}

/// Chemin du fichier de corpus `name`.
///
/// # Panics
/// Si `name` est inconnu, ou si le fichier est absent (« corpus absent : lancer cargo xtask corpus
/// gen »).
#[must_use]
pub fn corpus_file(name: &str) -> PathBuf {
    corpus_file_in(&corpus_dir(), name)
}

fn if_present_in(dir: &Path, name: &str) -> Option<PathBuf> {
    assert!(
        manifest(name).big,
        "corpus_file_if_present est réservé aux fichiers `big` : utiliser corpus_file({name})"
    );
    file_in(dir, name)
}

/// Chemin du fichier `big` `name` s'il existe (il n'est généré qu'avec `--big`), sinon `None`.
///
/// # Panics
/// Si `name` est inconnu ou n'est pas un fichier `big` : un fichier ordinaire absent doit faire
/// échouer le test, pas le sauter.
#[must_use]
pub fn corpus_file_if_present(name: &str) -> Option<PathBuf> {
    if_present_in(&corpus_dir(), name)
}

fn has_render_device() -> bool {
    std::fs::read_dir("/dev/dri").is_ok_and(|d| {
        d.filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
    })
}

fn gpu_gate(present: bool, required: bool) -> bool {
    if present {
        return true;
    }
    assert!(
        !required,
        "VA-API absent alors que MA_REQUIRE_GPU=1 : pas de périphérique /dev/dri/renderD*"
    );
    eprintln!("VA-API absent : test non exécuté");
    false
}

/// Garde des tests dépendant du GPU : `true` s'il existe un périphérique `/dev/dri/renderD*`.
/// Sinon affiche `VA-API absent : test non exécuté` et renvoie `false`, sauf si `MA_REQUIRE_GPU=1`
/// (alors le test échoue).
///
/// # Panics
/// Si `MA_REQUIRE_GPU=1` et qu'aucun périphérique n'existe.
#[must_use]
pub fn require_vaapi() -> bool {
    gpu_gate(
        has_render_device(),
        std::env::var("MA_REQUIRE_GPU").is_ok_and(|v| v == "1"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_the_corpus() {
        assert_eq!(entries().len(), 16);
        let e = manifest("hevc_720p_main10");
        assert_eq!(e.codec, "hevc");
        assert_eq!(e.dims(), Size { w: 1280, h: 720 });
        assert!(e.has("closed_gop") && !e.has("rasl"));
        assert_eq!(e.code_modulo, 65536);
        assert!(manifest("uhd_big").big);
        assert_eq!(manifest("uhd_big").bitrate.as_deref(), Some("25M"));
        assert_eq!(
            manifest("hevc_720p_cut").derived_from.as_deref(),
            Some("hevc_720p_main10")
        );
    }

    #[test]
    #[should_panic(expected = "inconnu du manifeste")]
    fn unknown_name_panics() {
        let _ = manifest("nope");
    }

    #[test]
    fn corpus_dir_resolution() {
        assert_eq!(
            corpus_dir_from(Some("/x".into()), Some("/home/u".into())),
            PathBuf::from("/x")
        );
        assert_eq!(
            corpus_dir_from(Some(OsString::new()), Some("/home/u".into())),
            PathBuf::from("/home/u/media-analyzer-corpus/generated")
        );
        assert_eq!(
            corpus_dir_from(None, Some("/home/u".into())),
            PathBuf::from("/home/u/media-analyzer-corpus/generated")
        );
    }

    #[test]
    #[should_panic(expected = "HOME non défini")]
    fn corpus_dir_without_home_panics() {
        let _ = corpus_dir_from(None, None);
    }

    #[test]
    #[should_panic(expected = "corpus absent : lancer cargo xtask corpus gen (")]
    fn missing_file_panics_with_instruction() {
        let _ = corpus_file_in(Path::new("/nonexistent-corpus"), "h264_720p_closed");
    }

    #[test]
    #[should_panic(expected = "corpus gen --big")]
    fn missing_big_file_suggests_big() {
        let _ = corpus_file_in(Path::new("/nonexistent-corpus"), "uhd_big");
    }

    #[test]
    fn if_present_only_for_big_files() {
        assert_eq!(
            if_present_in(Path::new("/nonexistent-corpus"), "uhd_big"),
            None
        );
        let dir = std::env::temp_dir().join(format!("ma-testkit-corpus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("uhd_big.ts"), b"x").unwrap();
        let found = if_present_in(&dir, "uhd_big");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(dir.join("uhd_big.ts")));
    }

    #[test]
    #[should_panic(expected = "réservé aux fichiers `big`")]
    fn if_present_rejects_regular_files() {
        let _ = if_present_in(Path::new("/nonexistent-corpus"), "h264_720p_closed");
    }

    #[test]
    fn gpu_gate_semantics() {
        assert!(gpu_gate(true, true));
        assert!(gpu_gate(true, false));
        assert!(!gpu_gate(false, false));
    }

    #[test]
    #[should_panic(expected = "MA_REQUIRE_GPU=1")]
    fn gpu_gate_fails_when_required() {
        let _ = gpu_gate(false, true);
    }

    #[test]
    fn require_vaapi_matches_device_presence() {
        // Sans MA_REQUIRE_GPU, le résultat suit la présence du périphérique ; avec, il vaut `true`
        // (le périphérique existe, sinon la fonction a paniqué).
        assert_eq!(require_vaapi(), has_render_device());
    }
}
