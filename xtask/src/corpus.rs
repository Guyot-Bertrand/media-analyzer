//! Génération du corpus de test (`cargo xtask corpus gen`).
//!
//! Chaque image porte son numéro de présentation (modulo `code_modulo`) sur 16 bits dans une bande
//! en haut de l'image. Les fichiers sont décrits par `corpus/manifest.toml` et créés dans
//! `$MA_CORPUS_DIR` (défaut `~/media-analyzer-corpus/generated`). La génération est idempotente.

use anyhow::{Context, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Taille d'un paquet TS.
const TS_PACKET: usize = 188;
/// PID du flux vidéo écrit par le muxer `mpegts` de `FFmpeg`.
const VIDEO_PID: u16 = 0x100;
/// Taille minimale de `uhd_big`.
const BIG_TARGET_BYTES: u64 = 4_000_000_000;

#[derive(Debug, Deserialize)]
struct Manifest {
    file: Vec<Entry>,
}

/// Description d'un fichier du corpus (voir `corpus/manifest.toml`).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // champs de description : lus par les tests de ma-testkit, pas par le générateur
pub struct Entry {
    pub name: String,
    pub file: String,
    pub codec: String,
    pub profile: String,
    pub size: String,
    pub fps: u32,
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

impl Entry {
    fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    /// Largeur et hauteur de l'image.
    fn dims(&self) -> anyhow::Result<(u32, u32)> {
        let (w, h) = self
            .size
            .split_once('x')
            .context("taille « WxH » attendue")?;
        Ok((w.parse()?, h.parse()?))
    }
}

/// Charge le manifeste versionné.
pub fn load_manifest(root: &Path) -> anyhow::Result<Vec<Entry>> {
    let path = root.join("corpus/manifest.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("lecture de {}", path.display()))?;
    parse_manifest(&text)
}

fn parse_manifest(text: &str) -> anyhow::Result<Vec<Entry>> {
    Ok(toml::from_str::<Manifest>(text)?.file)
}

/// Dossier du corpus : `--out`, sinon `$MA_CORPUS_DIR`, sinon `~/media-analyzer-corpus/generated`.
pub fn corpus_dir(out: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(p) = out {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("MA_CORPUS_DIR") {
        return Ok(p.into());
    }
    let home =
        std::env::var_os("HOME").context("HOME non défini : utiliser --out ou MA_CORPUS_DIR")?;
    Ok(PathBuf::from(home).join("media-analyzer-corpus/generated"))
}

/// Filtre `ffmpeg` incrustant le numéro `n mod modulo` sur 16 bits dans la bande du haut.
fn code_filter(w: u32, h: u32, modulo: u32, pix_fmt: &str) -> String {
    let bw = w / 16;
    let bh = h / 24;
    let width = bw * 16;
    format!(
        "format=yuv420p,geq=lum='if(lt(Y,{bh})*lt(X,{width}), \
255*mod(floor(mod(N,{modulo})/pow(2,floor(X/{bw}))),2), lum(X,Y))':\
cb='if(lt(Y,{bh}),128,cb(X,Y))':cr='if(lt(Y,{bh}),128,cr(X,Y))',format={pix_fmt}"
    )
}

/// Arguments d'encodage (codec, profil, structure de GOP) d'une entrée.
fn encoder_args(entry: &Entry) -> anyhow::Result<Vec<String>> {
    let open = u8::from(entry.has("open_gop"));
    let (gop, bframes) = (entry.gop, entry.bframes);
    let strs = |v: &[&str]| v.iter().map(|item| (*item).to_owned()).collect::<Vec<_>>();
    let mut args = match (entry.codec.as_str(), entry.profile.as_str()) {
        ("h264", "High") => strs(&[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
        ]),
        ("h264", "High 10") => strs(&[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-profile:v",
            "high10",
            "-pix_fmt",
            "yuv420p10le",
        ]),
        ("hevc", "Main 10") => strs(&[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
        ]),
        ("mpeg2video", "Main") => {
            let mut args = strs(&[
                "-c:v",
                "mpeg2video",
                "-profile:v",
                "main",
                "-level:v",
                "main",
                "-pix_fmt",
                "yuv420p",
            ]);
            args.extend(strs(&[
                // GOP fermé : l'encodeur MPEG-2 produit des GOP de (bframes + 1) × k + 1 images
                // (le manifeste doit donner une telle valeur, 13 avec 2 B ; 12 donnerait 10).
                "-g",
                &gop.to_string(),
                "-bf",
                &bframes.to_string(),
                "-flags",
                "+ildct+ilme+cgop",
                "-sc_threshold",
                "1000000000",
                "-top",
                "1",
            ]));
            args.extend(strs(&[
                "-b:v", "6M", "-maxrate", "9M", "-bufsize", "1835008",
            ]));
            return Ok(args);
        }
        (codec, profile) => bail!("recette inconnue pour {codec} / {profile}"),
    };
    // x264 en GOP ouvert : avec `b-adapt=0` l'I de reprise tombe toujours après une P, sans B de
    // tête. `b-adapt=2` sans pyramide de B place des B qui précèdent l'I en présentation et le
    // suivent en décodage (images `Skippable`).
    let b_adapt = if entry.has("open_gop") && entry.codec == "h264" {
        "b-adapt=2:b-pyramid=none"
    } else {
        "b-adapt=0"
    };
    let params = if entry.codec == "h264" {
        format!(
            "keyint={gop}:min-keyint={gop}:scenecut=0:bframes={bframes}:{b_adapt}:open-gop={open}"
        )
    } else {
        format!(
            "keyint={gop}:min-keyint={gop}:scenecut=0:bframes={bframes}:b-adapt=0:open-gop={open}:repeat-headers=1:log-level=error"
        )
    };
    args.push(
        if entry.codec == "h264" {
            "-x264-params"
        } else {
            "-x265-params"
        }
        .to_owned(),
    );
    args.push(params);
    args.extend(strs(&["-b:v", entry.bitrate.as_deref().unwrap_or("4M")]));
    Ok(args)
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("lancement de {}", cmd.get_program().display()))?;
    if !status.success() {
        bail!("échec de {} ({status})", cmd.get_program().display());
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    s.into()
}

/// Encode la source de test `testsrc2` avec le numéro incrusté.
fn encode(e: &Entry, dest: &Path) -> anyhow::Result<()> {
    let (w, h) = e.dims()?;
    let pix_fmt = if e.profile.ends_with("10") || e.profile == "High 10" {
        "yuv420p10le"
    } else {
        "yuv420p"
    };
    let src = format!("testsrc2=size={}:rate={}", e.size, e.fps);
    let filter = code_filter(w, h, e.code_modulo, pix_fmt);
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &src,
            "-frames:v",
            &e.frames.to_string(),
            "-vf",
            &filter,
        ])
        .args(encoder_args(e)?)
        .args(["-f", "mpegts"])
        .arg(tmp_path(dest)))
}

/// Verbose mais explicite : lecture ffprobe d'une valeur de flux.
fn probe_raw(path: &Path, entries: &str, extra: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(extra)
        .args(["-show_entries", entries, "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("lancement de ffprobe")?;
    if !out.status.success() {
        bail!(
            "ffprobe a échoué sur {} : {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Première ligne de sortie de `ffprobe` (il répète les flux dans la section des programmes).
fn probe(path: &Path, entries: &str, extra: &[&str]) -> anyhow::Result<String> {
    let text = probe_raw(path, entries, extra)?;
    Ok(
        // ffprobe ajoute un champ vide final pour certains flux (mpeg2video).
        text.lines()
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches(',')
            .to_owned(),
    )
}

/// Vérifie codec, profil et nombre d'images (ou taille pour `big`) d'un fichier généré.
fn verify(e: &Entry, path: &Path) -> anyhow::Result<()> {
    let cp = probe(path, "stream=codec_name,profile", &[])?;
    let want = format!("{},{}", e.codec, e.profile);
    if cp != want {
        bail!("{} : codec/profil {cp:?} au lieu de {want:?}", e.name);
    }
    if e.big {
        let size = std::fs::metadata(path)?.len();
        if size < BIG_TARGET_BYTES {
            bail!("{} : {size} octets < {BIG_TARGET_BYTES}", e.name);
        }
        return Ok(());
    }
    let n: u32 = probe(path, "stream=nb_read_packets", &["-count_packets"])?.parse()?;
    let ok = if e.derived_from.is_some() {
        (1..=e.frames).contains(&n)
    } else {
        n == e.frames
    };
    if !ok {
        bail!("{} : {n} images au lieu de {}", e.name, e.frames);
    }
    Ok(())
}

/// Positions (octets) des paquets vidéo clés et fin, d'après `ffprobe`.
fn keyframe_positions(path: &Path) -> anyhow::Result<Vec<usize>> {
    let out = probe_raw(path, "packet=pos,flags", &[])?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let (pos, flags) = l.split_once(',')?;
            flags.starts_with('K').then(|| pos.parse().ok()).flatten()
        })
        .collect())
}

/// PID d'un paquet TS (0 si octet de synchro invalide).
fn pid_of(packet: &[u8]) -> u16 {
    u16::from(packet[1] & 0x1f) << 8 | u16::from(packet[2])
}

/// Début de PES vidéo (PUSI sur le PID vidéo).
fn is_video_pes_start(packet: &[u8]) -> bool {
    packet[0] == 0x47 && packet[1] & 0x40 != 0 && pid_of(packet) == VIDEO_PID
}

/// Construit le fichier « coupé » : tables d'avant le premier PES vidéo, puis les paquets de
/// `[mid_first, mid_second)` alignés sur 188 octets, en démarrant sur un début de PES vidéo.
fn cut(ts: &[u8], mid_first: usize, mid_second: usize) -> anyhow::Result<Vec<u8>> {
    let packets: Vec<&[u8]> = ts
        .as_chunks::<TS_PACKET>()
        .0
        .iter()
        .map(<[u8; TS_PACKET]>::as_slice)
        .collect();
    let first_pes = packets
        .iter()
        .position(|p| is_video_pes_start(p))
        .context("aucun PES vidéo")?;
    let start = (mid_first / TS_PACKET..packets.len())
        .find(|&i| is_video_pes_start(packets[i]))
        .context("aucun PES vidéo après le milieu du 1er GOP")?;
    let end = (mid_second / TS_PACKET).min(packets.len());
    if start >= end || start <= first_pes {
        bail!("découpe incohérente : {start}..{end}");
    }
    Ok(packets[..first_pes]
        .iter()
        .chain(&packets[start..end])
        .flat_map(|p| p.iter().copied())
        .collect())
}

/// Fenêtre de découpe (octets) d'un fichier dérivé, selon ses caractéristiques, à partir des
/// positions des images clés de la source :
/// - `starts_mid_gop` : du milieu du 1er GOP au milieu du 2e ;
/// - `starts_at_cra` : du 2e point d'accès (1er CRA après l'IDR initiale) à la fin.
fn cut_window(e: &Entry, keys: &[usize], len: usize) -> anyhow::Result<(usize, usize)> {
    if e.has("starts_mid_gop") {
        Ok((
            usize::midpoint(keys[0], keys[1]),
            usize::midpoint(keys[1], keys[2]),
        ))
    } else if e.has("starts_at_cra") {
        Ok((keys[1], len))
    } else {
        bail!(
            "{} : découpe inconnue (starts_mid_gop ou starts_at_cra attendu)",
            e.name
        )
    }
}

fn make_cut(e: &Entry, source: &Path, dest: &Path) -> anyhow::Result<()> {
    let keys = keyframe_positions(source)?;
    if keys.len() < 3 {
        bail!("{} : moins de 3 images clés dans la source", e.name);
    }
    let data = std::fs::read(source)?;
    let (from, to) = cut_window(e, &keys, data.len())?;
    let bytes = cut(&data, from, to)?;
    std::fs::write(tmp_path(dest), bytes)?;
    Ok(())
}

fn make_big(e: &Entry, dest: &Path, dir: &Path) -> anyhow::Result<()> {
    let seg = dir.join("uhd_big_segment.ts");
    if !non_empty(&seg) {
        let mut seg_entry = e.clone();
        seg_entry.big = false;
        encode(&seg_entry, &seg)?;
        std::fs::rename(tmp_path(&seg), &seg)?;
    }
    let seg_size = std::fs::metadata(&seg)?.len();
    let loops = BIG_TARGET_BYTES.div_ceil(seg_size) - 1;
    run(Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-stream_loop",
            &loops.to_string(),
            "-i",
        ])
        .arg(&seg)
        .args(["-map", "0", "-c", "copy", "-f", "mpegts"])
        .arg(tmp_path(dest)))
}

fn non_empty(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.len() > 0)
}

/// Vérifie la présence de `ffmpeg`, `ffprobe` et des encodeurs nécessaires.
fn check_tools() -> anyhow::Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("ffmpeg introuvable : installer le paquet « ffmpeg » (voir BUILD.md)")?;
    let list = String::from_utf8_lossy(&out.stdout);
    for enc in ["libx264", "libx265", "mpeg2video"] {
        if !list.contains(enc) {
            bail!(
                "ffmpeg sans l'encodeur {enc} : installer un ffmpeg avec libx264 et libx265 (voir BUILD.md)"
            );
        }
    }
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .context("ffprobe introuvable : installer le paquet « ffmpeg »")?;
    Ok(())
}

/// Génère les fichiers manquants du corpus ; renvoie les noms créés.
pub fn generate(root: &Path, big: bool, out: Option<PathBuf>) -> anyhow::Result<Vec<String>> {
    let dir = corpus_dir(out)?;
    let entries = load_manifest(root)?;
    check_tools()?;
    std::fs::create_dir_all(&dir)?;
    let mut created = Vec::new();
    for e in entries.iter().filter(|e| big || !e.big) {
        let dest = dir.join(&e.file);
        if non_empty(&dest) {
            println!("    {} : déjà présent", e.name);
            continue;
        }
        println!("    {} : génération", e.name);
        match &e.derived_from {
            Some(src) => {
                let src_entry = entries
                    .iter()
                    .find(|s| &s.name == src)
                    .with_context(|| format!("source {src} inconnue"))?;
                make_cut(e, &dir.join(&src_entry.file), &dest)?;
            }
            None if e.big => make_big(e, &dest, &dir)?,
            None => encode(e, &dest)?,
        }
        let part = tmp_path(&dest);
        verify(e, &part).with_context(|| format!("vérification de {}", e.name))?;
        std::fs::rename(&part, &dest)?;
        created.push(e.name.clone());
    }
    println!("corpus : {} ({} créé(s))", dir.display(), created.len());
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(pid: u16, pusi: bool, tag: u8) -> Vec<u8> {
        let mut p = vec![0u8; TS_PACKET];
        p[0] = 0x47;
        p[1] = u8::from(pusi) << 6 | (pid >> 8) as u8;
        p[2] = (pid & 0xff) as u8;
        p[3] = tag;
        p
    }

    #[test]
    fn manifest_in_repo_is_complete() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let entries = load_manifest(root).unwrap();
        assert_eq!(entries.len(), 9);
        assert_eq!(entries.iter().filter(|e| e.big).count(), 1);
        for e in &entries {
            assert_eq!(e.code_bits, 16, "{}", e.name);
            assert!(e.dims().is_ok(), "{}", e.name);
            assert!(encoder_args(e).is_ok(), "{}", e.name);
        }
    }

    #[test]
    fn code_filter_uses_block_geometry() {
        let f = code_filter(1280, 720, 65536, "yuv420p10le");
        assert!(f.contains("lt(Y,30)"), "{f}");
        assert!(f.contains("lt(X,1280)"), "{f}");
        assert!(f.contains("floor(X/80)"), "{f}");
        assert!(f.contains("mod(N,65536)"), "{f}");
        assert!(f.ends_with("format=yuv420p10le"), "{f}");
    }

    #[test]
    fn open_gop_flag_follows_features() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let entries = load_manifest(root).unwrap();
        let args = |n: &str| {
            encoder_args(entries.iter().find(|e| e.name == n).unwrap())
                .unwrap()
                .join(" ")
        };
        assert!(args("hevc_720p_open_gop").contains("open-gop=1"));
        assert!(args("hevc_720p_main10").contains("open-gop=0"));
        assert!(args("h264_720p_recovery").contains("open-gop=1"));
    }

    #[test]
    fn unknown_recipe_is_rejected() {
        let mut e = parse_manifest(include_str!("../../corpus/manifest.toml"))
            .unwrap()
            .remove(0);
        e.profile = "Baseline".into();
        assert!(encoder_args(&e).is_err());
    }

    #[test]
    fn cut_keeps_tables_and_starts_on_a_pes() {
        let mut ts = Vec::new();
        ts.extend(pkt(0, true, 0)); // PAT
        ts.extend(pkt(0x1000, true, 1)); // PMT
        ts.extend(pkt(VIDEO_PID, true, 2)); // PES 1
        ts.extend(pkt(VIDEO_PID, false, 3));
        ts.extend(pkt(VIDEO_PID, false, 4)); // milieu : continuation, à sauter
        ts.extend(pkt(VIDEO_PID, true, 5)); // PES 2 : début attendu
        ts.extend(pkt(VIDEO_PID, false, 6));
        ts.extend(pkt(VIDEO_PID, true, 7)); // hors fenêtre
        let out = cut(&ts, 4 * TS_PACKET + 10, 7 * TS_PACKET).unwrap();
        let tags: Vec<u8> = out.chunks(TS_PACKET).map(|p| p[3]).collect();
        assert_eq!(tags, vec![0, 1, 5, 6]);
    }

    #[test]
    fn cut_window_follows_features() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let entries = load_manifest(root).unwrap();
        let get = |n: &str| entries.iter().find(|e| e.name == n).unwrap();
        let keys = [0, 1000, 3000];
        assert_eq!(
            cut_window(get("hevc_720p_cut"), &keys, 5000).unwrap(),
            (500, 2000)
        );
        assert_eq!(
            cut_window(get("hevc_720p_open_gop_from_cra"), &keys, 5000).unwrap(),
            (1000, 5000)
        );
        assert!(cut_window(get("hevc_720p_main10"), &keys, 5000).is_err());
    }

    #[test]
    fn cut_rejects_inconsistent_window() {
        let mut ts = Vec::new();
        ts.extend(pkt(VIDEO_PID, true, 0));
        ts.extend(pkt(VIDEO_PID, true, 1));
        assert!(cut(&ts, 0, 0).is_err());
        assert!(cut(&ts[..TS_PACKET], 0, TS_PACKET).is_err());
        assert!(cut(&[], 0, 0).is_err());
    }

    #[test]
    fn corpus_dir_prefers_explicit_out() {
        assert_eq!(corpus_dir(Some("/x".into())).unwrap(), PathBuf::from("/x"));
    }
}
