//! Oracles de test (dev uniquement) : `ffprobe`, `ffmpeg` et `TSDuck`.
//!
//! Le moteur ne démultiplexe pas avec `FFmpeg` : ces fonctions servent uniquement à comparer ses
//! résultats à ceux d'un outil indépendant. Chaque commande est lancée avec `-v error` ; un outil
//! absent ou une erreur d'exécution fait échouer le test avec un message explicite.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

/// Paquet vidéo vu par `ffprobe` (ordre du fichier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePacket {
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub size: u32,
    pub pos: Option<u64>,
    pub key: bool,
}

/// Image décodée vue par `ffprobe` (ordre de présentation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFrame {
    pub pts: i64,
    pub pict_type: char,
    pub key: bool,
}

fn run_tool(tool: &str, args: &[&str], path: &Path) -> Output {
    let out = Command::new(tool)
        .args(args)
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("{tool} introuvable ({e}) : installer le paquet « ffmpeg »"));
    assert!(
        out.status.success(),
        "{tool} a échoué sur {} : {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn ffprobe_json(path: &Path, args: &[&str]) -> Value {
    let mut full = vec!["-v", "error", "-select_streams", "v:0"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["-of", "json"]);
    let out = run_tool("ffprobe", &full, path);
    serde_json::from_slice(&out.stdout).expect("sortie JSON de ffprobe invalide")
}

/// Entier que `ffprobe` écrit soit en nombre, soit en chaîne.
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// Paquets du 1er flux vidéo, dans l'ordre du fichier.
///
/// # Panics
/// Si `ffprobe` est absent, échoue ou produit une sortie inattendue.
#[must_use]
pub fn ffprobe_packets(path: &Path) -> Vec<ProbePacket> {
    let json = ffprobe_json(path, &["-show_entries", "packet=pts,dts,size,pos,flags"]);
    json["packets"]
        .as_array()
        .expect("ffprobe : « packets » absent")
        .iter()
        .map(|p| ProbePacket {
            pts: int(&p["pts"]),
            dts: int(&p["dts"]),
            size: u32::try_from(int(&p["size"]).expect("ffprobe : taille absente"))
                .expect("ffprobe : taille hors plage"),
            pos: int(&p["pos"]).and_then(|v| u64::try_from(v).ok()),
            key: p["flags"].as_str().is_some_and(|f| f.starts_with('K')),
        })
        .collect()
}

/// Images décodées du 1er flux vidéo, triées par PTS (ordre de présentation).
///
/// # Panics
/// Si `ffprobe` est absent, échoue ou produit une sortie inattendue.
#[must_use]
pub fn ffprobe_frames(path: &Path) -> Vec<ProbeFrame> {
    let json = ffprobe_json(
        path,
        &[
            "-show_frames",
            "-show_entries",
            "frame=pts,pict_type,key_frame",
        ],
    );
    let mut frames: Vec<ProbeFrame> = json["frames"]
        .as_array()
        .expect("ffprobe : « frames » absent")
        .iter()
        .map(|f| ProbeFrame {
            pts: int(&f["pts"]).expect("ffprobe : pts d'image absent"),
            pict_type: f["pict_type"]
                .as_str()
                .and_then(|s| s.chars().next())
                .expect("ffprobe : pict_type absent"),
            key: int(&f["key_frame"]) == Some(1),
        })
        .collect();
    frames.sort_by_key(|f| f.pts);
    frames
}

/// Flux élémentaire du 1er flux vidéo, extrait par `ffmpeg -c copy` (Annex B / MPEG-2 brut).
///
/// # Panics
/// Si `ffmpeg` est absent ou échoue, ou si le codec n'est pas H.264, HEVC ou MPEG-2.
#[must_use]
pub fn ffmpeg_es(path: &Path) -> Vec<u8> {
    let codec =
        ffprobe_json(path, &["-show_entries", "stream=codec_name"])["streams"][0]["codec_name"]
            .as_str()
            .expect("ffprobe : codec absent")
            .to_owned();
    let format = match codec.as_str() {
        "h264" => "h264",
        "hevc" => "hevc",
        "mpeg2video" => "mpeg2video",
        other => panic!("ffmpeg_es : codec non géré : {other}"),
    };
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c", "copy", "-f", format, "-"])
        .output()
        .unwrap_or_else(|e| panic!("ffmpeg introuvable ({e}) : installer le paquet « ffmpeg »"));
    assert!(
        out.status.success(),
        "ffmpeg a échoué sur {} : {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Somme MD5 de chaque image décodée (`-f framemd5`), `(pts, md5)` dans l'ordre de sortie.
///
/// # Panics
/// Si `ffmpeg` est absent, échoue ou produit une sortie inattendue.
#[must_use]
pub fn ffmpeg_framemd5(path: &Path) -> Vec<(i64, String)> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-f", "framemd5", "-"])
        .output()
        .unwrap_or_else(|e| panic!("ffmpeg introuvable ({e}) : installer le paquet « ffmpeg »"));
    assert!(
        out.status.success(),
        "ffmpeg a échoué sur {} : {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let cols: Vec<&str> = l.split(',').map(str::trim).collect();
            assert!(cols.len() >= 6, "ligne framemd5 inattendue : {l}");
            (
                cols[2].parse().expect("framemd5 : pts invalide"),
                cols[5].to_owned(),
            )
        })
        .collect()
}

/// Analyse `tsanalyze --json` (`TSDuck`), renvoyée telle quelle.
///
/// # Panics
/// Si l'outil est absent (message « `TSDuck` absent : voir BUILD.md §4 »), échoue ou produit du JSON invalide.
#[must_use]
pub fn tsduck_analyze(path: &Path) -> Value {
    let out = Command::new("tsanalyze")
        .arg("--json")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("TSDuck absent : voir BUILD.md §4 ({e})"));
    assert!(
        out.status.success(),
        "tsanalyze a échoué sur {} : {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("sortie JSON de tsanalyze invalide")
}

/// Racine du dépôt (deux niveaux au-dessus de la crate).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("racine du dépôt introuvable")
        .to_path_buf()
}

/// Fichiers `*.ts` de `dir` (ou du dossier par défaut `corpus/private`), triés.
fn private_corpus_in(dir: Option<&Path>) -> Vec<PathBuf> {
    let default = repo_root().join("corpus/private");
    let dir = dir.unwrap_or(&default);
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "ts"))
        .collect();
    files.sort();
    files
}

/// Fichiers `*.ts` du corpus privé : `$MA_CORPUS_PRIVATE`, sinon `<dépôt>/corpus/private`.
/// Liste vide ⇒ affiche `corpus privé absent : test non exécuté` (le test doit alors s'arrêter).
#[must_use]
pub fn private_corpus() -> Vec<PathBuf> {
    let env = std::env::var_os("MA_CORPUS_PRIVATE").filter(|d| !d.is_empty());
    let files = private_corpus_in(env.as_deref().map(Path::new));
    if files.is_empty() {
        eprintln!("corpus privé absent : test non exécuté");
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus_file;

    #[test]
    fn ffprobe_packets_of_closed_h264() {
        let p = ffprobe_packets(&corpus_file("h264_720p_closed"));
        assert_eq!(p.len(), 250);
        assert!(p[0].key);
        assert!(p.iter().all(|x| x.pts.is_some()));
    }

    #[test]
    fn ffprobe_frames_types() {
        let f = ffprobe_frames(&corpus_file("h264_720p_closed"));
        assert_eq!(f.len(), 250);
        assert_eq!(f.iter().filter(|x| x.pict_type == 'I').count(), 10);
        assert!(f.windows(2).all(|w| w[0].pts < w[1].pts));
    }

    #[test]
    fn manifest_gop_matches_ffprobe() {
        for name in [
            "mpeg2_576i_closed",
            "h264_1080p_crop_top",
            "h264_1080i_mbaff",
            "h264_720p_pts_wrap",
            "h264_720p_nob",
        ] {
            let e = crate::manifest(name);
            let frames = ffprobe_frames(&corpus_file(name));
            assert_eq!(frames.len(), e.frames as usize, "{name}");
            let intra: Vec<usize> = frames
                .iter()
                .enumerate()
                .filter(|(_, f)| f.pict_type == 'I')
                .map(|(i, _)| i)
                .collect();
            assert!(intra.len() >= 2, "{name}");
            assert!(
                intra.windows(2).all(|w| w[1] - w[0] == e.gop as usize),
                "{name} : I aux positions {intra:?}, gop manifeste {}",
                e.gop
            );
        }
    }

    #[test]
    fn new_files_have_their_announced_features() {
        let crop = corpus_file("h264_1080p_crop_top");
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .args(["stream=width,height,coded_height", "-of", "csv=p=0"])
            .arg(&crop)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).lines().next(),
            Some("1920,1080,1080")
        );
        // Le rognage figure dans le SPS : ffprobe affiche la taille rognée.
        // MBAFF : le SPS de h264_1080i_mbaff porte mb_adaptive_frame_field_flag = 1.
        let trace = Command::new("ffmpeg")
            .args(["-v", "debug", "-i"])
            .arg(corpus_file("h264_1080i_mbaff"))
            .args([
                "-map",
                "0:v:0",
                "-c",
                "copy",
                "-bsf:v",
                "trace_headers",
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&trace.stderr)
                .contains("mb_adaptive_frame_field_flag                                1 = 1")
        );
        let wrap = ffprobe_packets(&corpus_file("h264_720p_pts_wrap"));
        let pts: Vec<i64> = wrap.iter().filter_map(|p| p.pts).collect();
        assert!(pts.iter().any(|&t| t < 0) && pts.iter().any(|&t| t > 0 && t < 10_000));
    }

    #[test]
    fn ffmpeg_es_starts_with_start_code() {
        let es = ffmpeg_es(&corpus_file("h264_720p_closed"));
        assert!(es.starts_with(&[0, 0, 0, 1]) || es.starts_with(&[0, 0, 1]));
    }

    #[test]
    fn framemd5_has_one_entry_per_frame() {
        let m = ffmpeg_framemd5(&corpus_file("h264_720p_closed"));
        assert_eq!(m.len(), 250);
        assert_eq!(m[0].1.len(), 32);
    }

    #[test]
    fn tsduck_reports_the_video_service() {
        let v = tsduck_analyze(&corpus_file("h264_720p_closed"));
        assert!(v["pids"].as_array().is_some_and(|p| !p.is_empty()), "{v}");
    }

    #[test]
    fn private_corpus_empty_without_env() {
        let dir = std::env::temp_dir().join(format!("ma-oracle-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), b"x").unwrap();
        let found = private_corpus_in(Some(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Vec::<PathBuf>::new());
        assert_eq!(
            private_corpus_in(Some(Path::new("/nonexistent-private"))),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn private_corpus_reads_env_dir() {
        let dir = std::env::temp_dir().join(format!("ma-oracle-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ts"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"x").unwrap();
        let found = private_corpus_in(Some(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, vec![dir.join("a.ts")]);
    }
}
