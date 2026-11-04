//! Génération du corpus de test (`cargo xtask corpus gen`).
//!
//! Chaque image porte son numéro de présentation (modulo `code_modulo`) sur 16 bits dans une bande
//! en haut de l'image. Les fichiers sont décrits par `corpus/manifest.toml` et créés dans
//! `$MA_CORPUS_DIR` (défaut `~/media-analyzer-corpus/generated`). La génération est idempotente.

use anyhow::{Context, bail};
use ma_ts::PacketFormat;
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
    // H.264 entrelacé : `+ildct+ilme` active l'entrelacement de x264 (MBAFF), `tff=1` fixe l'ordre
    // des trames (haut d'abord).
    let interlaced = if entry.codec == "h264" && entry.has("interlaced") {
        args.extend(strs(&["-flags", "+ildct+ilme"]));
        ":tff=1"
    } else {
        ""
    };
    let params = if entry.codec == "h264" {
        format!(
            "keyint={gop}:min-keyint={gop}:scenecut=0:bframes={bframes}:{b_adapt}:open-gop={open}{interlaced}"
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

/// Lignes encodées en plus de l'image affichée pour `crop_top` (rognées par le BSF).
const CROP_TOP_ROWS: u32 = 8;
/// Décalage des horodatages de `pts_wrap` (secondes). 2^33 / 90 kHz = 95 443,69 s ; le muxeur
/// `mpegts` ajoute 1,48 s au PTS initial (retard de mux et B), donc 95 438,52 s + 1,48 s fait franchir
/// 2^33 au PTS de l'image 93 (la valeur 95 440 s du plan le ferait dès l'image 56).
const PTS_WRAP_OFFSET: &str = "95438.52";

/// Encode la source de test `testsrc2` avec le numéro incrusté.
fn encode(e: &Entry, dest: &Path) -> anyhow::Result<()> {
    let (w, mut h) = e.dims()?;
    // `crop_top` : encodé 8 lignes plus haut, puis rogné en haut par le BSF `h264_metadata`.
    let crop_top = e.has("crop_top");
    if crop_top {
        h += CROP_TOP_ROWS;
    }
    let pix_fmt = if e.profile.ends_with("10") || e.profile == "High 10" {
        "yuv420p10le"
    } else {
        "yuv420p"
    };
    let src = format!("testsrc2=size={w}x{h}:rate={}", e.fps);
    let filter = code_filter(w, h, e.code_modulo, pix_fmt);
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
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
    .args(encoder_args(e)?);
    if crop_top {
        cmd.args([
            "-bsf:v",
            &format!("h264_metadata=crop_top={CROP_TOP_ROWS}:crop_bottom=0"),
        ]);
    }
    if e.has("pts_wrap") {
        cmd.args(["-output_ts_offset", PTS_WRAP_OFFSET]);
    }
    run(cmd.args(["-f", "mpegts"]).arg(tmp_path(dest)))
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

/// Format de paquet d'un fichier dérivé par re-paquetage (`packet_204`, `packet_208`, `packet_tts`).
fn packet_format(e: &Entry) -> Option<PacketFormat> {
    [
        ("packet_204", PacketFormat::Rs204),
        ("packet_208", PacketFormat::Ts208),
        ("packet_tts", PacketFormat::Tts192),
    ]
    .into_iter()
    .find_map(|(feature, format)| e.has(feature).then_some(format))
}

/// Fichier re-paqueté : détecte le format (`FFmpeg` n'est pas juge ici) et vérifie que la taille est
/// un multiple de la période.
fn verify_repacketized(e: &Entry, f: PacketFormat, path: &Path) -> anyhow::Result<()> {
    let data = std::fs::read(path)?;
    let detected = ma_ts::detect_format(&data[..data.len().min(1 << 20)]);
    let ok = detected.is_some_and(|(d, off)| d == f && off == f.sync_offset())
        && data.len() % f.size() == 0;
    if !ok {
        bail!("{} : format {detected:?} au lieu de {f:?}", e.name);
    }
    Ok(())
}

/// PID de la PMT du programme 1 écrit par le muxer `mpegts` de `FFmpeg`.
const PMT_PID: u16 = 0x1000;
/// PID dédié au PCR de `ts_pcr_pid`.
const PCR_PID: u16 = 0x1FF;
/// PID des sections SCTE-35 de `ts_ref`.
const SCTE35_PID: u16 = 0x1F4;
/// Écart maximal (paquets) entre deux PCR seuls de `ts_pcr_pid` : 100 paquets = 25 ms à 6 Mb/s.
const MAX_STEP: usize = 100;
/// Débit de multiplexage de `ts_ref` (`-muxrate 6M`), en paquets de 188 octets par seconde.
const REF_PACKETS_PER_SECOND: usize = 6_000_000 / 8 / TS_PACKET;

/// Flux de `ts_ref` : (`stream_type`, PID, boucle de descripteurs encodée).
fn ref_streams() -> Vec<(u8, u16, Vec<u8>)> {
    use ma_ts::testutil::descriptor;
    vec![
        (0x24, 0x100, vec![]),
        (0x0F, 0x101, vec![]),
        // E-AC-3 : `enhanced_AC-3_descriptor` sans champ optionnel.
        (0x06, 0x102, descriptor(0x7A, &[0x00])),
        // Sous-titres DVB « fra » (type 0x10, pages de composition et auxiliaire 1).
        (0x06, 0x103, descriptor(0x59, b"fra\x10\x00\x01\x00\x01")),
        // SCTE-35 : `registration_descriptor` « CUEI ».
        (0x86, SCTE35_PID, descriptor(0x05, b"CUEI")),
    ]
}

/// PMT du programme 1 avec le `PCR_PID` donné et des flux supplémentaires.
fn ref_pmt(pcr_pid: u16, extra: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
    let mut streams = ref_streams();
    streams.extend(extra.iter().cloned());
    let borrowed: Vec<(u8, u16, &[u8])> = streams
        .iter()
        .map(|(t, p, d)| (*t, *p, d.as_slice()))
        .collect();
    ma_ts::testutil::pmt(1, 0, pcr_pid, &[], &borrowed)
}

/// Charge utile d'un paquet portant une section SCTE-35 `splice_null` (`pointer_field` 0).
fn splice_null_payload() -> Vec<u8> {
    // table_id 0xFC, section_length 17, protocol_version 0, pts_adjustment 0, cw_index 0,
    // tier 0xFFF, splice_command_length 0, splice_null (0), descriptor_loop_length 0, CRC-32.
    let mut s = vec![
        0xFC, 0x30, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xF0, 0x00, 0x00, 0x00,
        0x00,
    ];
    s.extend(ma_ts::crc32_mpeg2(&s).to_be_bytes());
    let mut payload = vec![0];
    payload.extend(s);
    payload
}

/// `ts_ref` : PMT réécrite (même PID, mêmes positions de paquets : la nouvelle PMT remplace chaque
/// occurrence de l'ancienne, avec des paquets nuls voisins si elle est plus longue) puis une
/// section SCTE-35 `splice_null` par seconde à la place de paquets nuls.
fn finish_ref(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ts = ma_ts::testutil::replace_psi(raw, PMT_PID, &[ref_pmt(0x100, &[])])
        .map_err(anyhow::Error::msg)?;
    let payload = splice_null_payload();
    let packets = ts.len() / TS_PACKET;
    let targets: Vec<usize> = (200..packets).step_by(REF_PACKETS_PER_SECOND).collect();
    ma_ts::testutil::fill_nulls(&ts, &targets, |_, k| {
        ma_ts::testutil::packet(
            SCTE35_PID,
            true,
            u8::try_from(k % 16).unwrap_or(0),
            None,
            &payload,
        )
    })
    .map_err(anyhow::Error::msg)
}

/// Recette de `ts_ref` : `FFmpeg` (HEVC, AAC, E-AC-3, service) puis `finish_ref`.
fn make_ref(e: &Entry, dest: &Path) -> anyhow::Result<()> {
    let (w, h) = e.dims()?;
    let src = format!("testsrc2=size={w}x{h}:rate={}", e.fps);
    let filter = code_filter(w, h, e.code_modulo, "yuv420p10le");
    let part = tmp_path(dest);
    let raw = part.with_extension("raw");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y", "-f", "lavfi", "-i", &src])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "1:a"])
        .args(["-frames:v", &e.frames.to_string(), "-vf", &filter])
        .args(encoder_args(e)?)
        // VBV serré : l'image I n'occupe pas le multiplex plus de ~25 ms d'affilée.
        .args(["-maxrate", "1M", "-bufsize", "100k"])
        .args([
            "-c:a:0", "aac", "-b:a:0", "128k", "-c:a:1", "eac3", "-b:a:1", "192k", "-ac", "2",
        ])
        .args([
            "-shortest",
            "-streamid",
            "0:256",
            "-streamid",
            "1:257",
            "-streamid",
            "2:258",
        ])
        .args(["-muxrate", "6M"])
        .args(["-metadata", "service_name=MA Référence"])
        .args(["-metadata", "service_provider=Media Analyzer"])
        .args(["-f", "mpegts"])
        .arg(&raw);
    run(&mut cmd)?;
    let ts = finish_ref(&std::fs::read(&raw)?)?;
    std::fs::remove_file(&raw)?;
    std::fs::write(part, ts)?;
    Ok(())
}

/// Recette de `mpts_3prog` : trois vidéos H.264, un programme chacun.
fn make_mpts(e: &Entry, dest: &Path) -> anyhow::Result<()> {
    let (w, h) = e.dims()?;
    let src = format!("testsrc2=size={w}x{h}:rate={}", e.fps);
    let filter = code_filter(w, h, e.code_modulo, "yuv420p");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y"]);
    for _ in 0..3 {
        cmd.args(["-f", "lavfi", "-i", &src]);
    }
    cmd.args(["-map", "0:v", "-map", "1:v", "-map", "2:v"])
        .args(["-frames:v", &e.frames.to_string(), "-vf", &filter])
        .args(encoder_args(e)?);
    for k in 0..3 {
        cmd.args([
            "-program",
            &format!("title=P{}:program_num={}:st={k}", k + 1, k + 1),
        ]);
    }
    run(cmd.args(["-f", "mpegts"]).arg(tmp_path(dest)))
}

/// `ts_unsupported` : PMT de `ts_ref` avec un flux VC-1 (0xEA) et un flux AC-4 (0x06 + descripteur
/// d'extension 0x15), sans PID de données associé (la PMT les déclare seulement).
fn make_unsupported(source: &Path, dest: &Path) -> anyhow::Result<()> {
    use ma_ts::testutil::descriptor;
    let extra = [
        (0xEA, 0x104, vec![]),
        (0x06, 0x105, descriptor(0x7F, &[0x15])),
    ];
    let ts =
        ma_ts::testutil::replace_psi(&std::fs::read(source)?, PMT_PID, &[ref_pmt(0x100, &extra)])
            .map_err(anyhow::Error::msg)?;
    std::fs::write(tmp_path(dest), ts)?;
    Ok(())
}

/// Positions (indice de paquet) et valeurs (27 MHz) des PCR de la vidéo.
fn video_pcrs(ts: &[u8]) -> Vec<(usize, u64)> {
    ts.as_chunks::<TS_PACKET>()
        .0
        .iter()
        .enumerate()
        .filter_map(|(i, raw)| {
            let p = ma_ts::parse_packet(raw).ok()?;
            let pcr = p.adaptation?.pcr?;
            (p.header.pid == VIDEO_PID).then_some((i, pcr.ticks()))
        })
        .collect()
}

/// `ts_pcr_pid` : `PCR_PID = 0x1FF` et, entre deux PCR vidéo consécutifs, un paquet PCR
/// seul sur 0x1FF à la place du premier paquet nul qui suit le milieu de l'intervalle. Sa valeur est interpolée linéairement
/// (en position de paquet) entre les deux PCR vidéo qui l'encadrent. Les PCR de la vidéo restent.
fn make_lone_pcr(source: &Path, dest: &Path) -> anyhow::Result<()> {
    use ma_ts::testutil::{AfSpec, fill_nulls, packet, replace_psi};
    let raw = std::fs::read(source)?;
    let pcrs = video_pcrs(&raw);
    if pcrs.len() < 3 {
        bail!("{} : moins de 3 PCR vidéo", source.display());
    }
    let ts = replace_psi(&raw, PMT_PID, &[ref_pmt(PCR_PID, &[])]).map_err(anyhow::Error::msg)?;
    // Cibles : des paquets nuls entre le premier et le dernier PCR vidéo, au plus MAX_STEP paquets
    // (25 ms à 6 Mb/s) après le précédent PCR seul, le plus tard possible.
    let nulls: Vec<usize> = ts
        .as_chunks::<TS_PACKET>()
        .0
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            p[1] & 0x1F == 0x1F && p[2] == 0xFF && (pcrs[0].0..=pcrs[pcrs.len() - 1].0).contains(i)
        })
        .map(|(i, _)| i)
        .collect();
    let mut targets = Vec::new();
    let mut at = *nulls
        .first()
        .context("aucun paquet nul entre les PCR vidéo")?;
    loop {
        targets.push(at);
        let next = nulls
            .iter()
            .copied()
            .rfind(|&n| n > at && n <= at + MAX_STEP)
            .or_else(|| nulls.iter().copied().find(|&n| n > at));
        match next {
            Some(n) => at = n,
            None => break,
        }
    }
    let ts = fill_nulls(&ts, &targets, |at, _| {
        let k = pcrs
            .partition_point(|p| p.0 <= at)
            .saturating_sub(1)
            .min(pcrs.len() - 2);
        let ((ia, va), (ib, vb)) = (pcrs[k], pcrs[k + 1]);
        let at_pos = at.clamp(ia, ib);
        let ticks =
            u128::from(va) + u128::from(vb - va) * (at_pos - ia) as u128 / (ib - ia) as u128;
        let ticks = u64::try_from(ticks).unwrap_or(va);
        let pcr = ma_core::Pcr {
            base: ticks / 300,
            ext: u16::try_from(ticks % 300).unwrap_or(0),
        };
        packet(
            PCR_PID,
            false,
            0,
            Some(AfSpec {
                pcr: Some(pcr),
                ..AfSpec::default()
            }),
            &[],
        )
    })
    .map_err(anyhow::Error::msg)?;
    std::fs::write(tmp_path(dest), ts)?;
    Ok(())
}

/// Écrit la version re-paquetée de `source` dans `dest` (fichier temporaire).
fn make_repacketized(f: PacketFormat, source: &Path, dest: &Path) -> anyhow::Result<()> {
    let data = std::fs::read(source)?;
    std::fs::write(tmp_path(dest), ma_ts::testutil::repacketize(&data, f))?;
    Ok(())
}

/// `ts_unsupported` : `ffprobe` refuse d'ouvrir un flux VC-1 qui ne porte aucune donnée ; la
/// structure est donc relue par le démultiplexeur du projet (PID 0x104 VC-1 et 0x105 AC-4 déclarés).
fn verify_extra_streams(e: &Entry, path: &Path) -> anyhow::Result<()> {
    let (src, format, start) = ma_ts::probe(path)?;
    let mut sink = ma_core::VecSink::default();
    let mut scanner = ma_ts::PacketScanner::new(&src, format, start, None);
    let mut builder = ma_ts::StructureBuilder::new(format);
    while let Some(p) = scanner.next(&mut sink)? {
        builder.push(&p, &mut sink);
    }
    let pids: Vec<u16> = builder
        .structure()
        .programs
        .iter()
        .flat_map(|p| p.streams.iter().map(|s| s.pid))
        .collect();
    if !builder.complete()
        || !sink.messages.is_empty()
        || !pids.contains(&0x104)
        || !pids.contains(&0x105)
    {
        bail!("{} : structure inattendue ({pids:x?})", e.name);
    }
    Ok(())
}

/// Vérifie codec, profil et nombre d'images (ou taille pour `big`) d'un fichier généré.
fn verify(e: &Entry, path: &Path) -> anyhow::Result<()> {
    if e.has("extra_streams") {
        return verify_extra_streams(e, path);
    }
    if let Some(f) = packet_format(e) {
        return verify_repacketized(e, f, path);
    }
    let cp = probe(path, "stream=codec_name,profile", &[])?;
    let want = format!("{},{}", e.codec, e.profile);
    if cp != want {
        bail!("{} : codec/profil {cp:?} au lieu de {want:?}", e.name);
    }
    let dims = probe(path, "stream=width,height", &[])?;
    let want_dims = e.size.replace('x', ",");
    if dims != want_dims {
        bail!("{} : taille {dims:?} au lieu de {want_dims:?}", e.name);
    }
    if e.big {
        let size = std::fs::metadata(path)?.len();
        if size < BIG_TARGET_BYTES {
            bail!("{} : {size} octets < {BIG_TARGET_BYTES}", e.name);
        }
        let n: u64 = probe(path, "stream=nb_read_packets", &["-count_packets"])?.parse()?;
        if !whole_loops(n, e.frames) {
            bail!(
                "{} : {n} images, pas un multiple de {} (raccords de boucle défectueux)",
                e.name,
                e.frames
            );
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
    // Le démultiplexeur `concat` (et non `-stream_loop`) : en copie, `-stream_loop` laisse à
    // chaque raccord une cinquantaine d'images P/B sans leur image IDR, que FFmpeg ne sait pas
    // décoder, et ses boucles comptent 998 images au lieu de 1000.
    let list = tmp_path(&dir.join("uhd_big.concat.txt"));
    std::fs::write(
        &list,
        concat_list(&seg, BIG_TARGET_BYTES.div_ceil(seg_size)),
    )?;
    let result = run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-map", "0", "-c", "copy", "-f", "mpegts"])
        .arg(tmp_path(dest)));
    let _ = std::fs::remove_file(&list);
    result
}

/// Vrai si `n` images forment des boucles entières de `frames` images. Les fichiers générés avec
/// l'ancien `-stream_loop` en copie (998 images par boucle, raccords indécodables) échouent ici.
fn whole_loops(n: u64, frames: u32) -> bool {
    frames > 0 && n > 0 && n.is_multiple_of(u64::from(frames))
}

/// Liste du démultiplexeur `concat` de `FFmpeg` : `seg` répété `times` fois.
fn concat_list(seg: &Path, times: u64) -> String {
    let quoted = seg.display().to_string().replace('\'', "'\\''");
    let line = format!("file '{quoted}'\n");
    line.repeat(usize::try_from(times).unwrap_or(0))
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
            if !e.big || verify(e, &dest).is_ok() {
                println!("    {} : déjà présent", e.name);
                continue;
            }
            println!(
                "    {} : présent mais invalide (ancienne génération ?)",
                e.name
            );
        }
        println!("    {} : génération", e.name);
        match &e.derived_from {
            Some(src) => {
                let src_entry = entries
                    .iter()
                    .find(|s| &s.name == src)
                    .with_context(|| format!("source {src} inconnue"))?;
                let source = dir.join(&src_entry.file);
                match packet_format(e) {
                    Some(f) => make_repacketized(f, &source, &dest)?,
                    None if e.has("lone_pcr") => make_lone_pcr(&source, &dest)?,
                    None if e.has("extra_streams") => make_unsupported(&source, &dest)?,
                    None => make_cut(e, &source, &dest)?,
                }
            }
            None if e.big => make_big(e, &dest, &dir)?,
            None if e.has("ref_mux") => make_ref(e, &dest)?,
            None if e.has("mpts") => make_mpts(e, &dest)?,
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
    fn whole_loops_rejects_stream_loop_files() {
        assert!(whole_loops(62_000, 1000));
        assert!(!whole_loops(61_878, 1000));
        assert!(!whole_loops(0, 1000));
        assert!(!whole_loops(1000, 0));
    }

    #[test]
    fn concat_list_repeats_the_segment_and_quotes_the_path() {
        assert_eq!(
            concat_list(Path::new("/c/it's.ts"), 2),
            "file '/c/it'\\''s.ts'\nfile '/c/it'\\''s.ts'\n"
        );
        assert_eq!(concat_list(Path::new("/c/a.ts"), 0), "");
    }

    #[test]
    fn manifest_in_repo_is_complete() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let entries = load_manifest(root).unwrap();
        assert_eq!(entries.len(), 20);
        assert_eq!(entries.iter().filter(|e| e.big).count(), 1);
        for e in &entries {
            assert_eq!(e.code_bits, 16, "{}", e.name);
            assert!(e.dims().is_ok(), "{}", e.name);
            assert!(encoder_args(e).is_ok(), "{}", e.name);
        }
    }

    #[test]
    fn repacketized_files_are_verified_without_ffprobe() {
        let dir = std::env::temp_dir().join(format!("ma-xtask-repack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.ts");
        let ts: Vec<u8> = (0..10u8)
            .flat_map(|i| ma_ts::testutil::packet(0x100, false, i, None, &[i]))
            .collect();
        std::fs::write(&src, &ts).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let entries = load_manifest(root).unwrap();
        for (name, f) in [
            ("ts204_h264", PacketFormat::Rs204),
            ("ts208_h264", PacketFormat::Ts208),
            ("tts_h264", PacketFormat::Tts192),
        ] {
            let e = entries.iter().find(|e| e.name == name).unwrap();
            assert_eq!(packet_format(e), Some(f));
            let dest = dir.join(format!("{name}.ts"));
            make_repacketized(f, &src, &dest).unwrap();
            let part = tmp_path(&dest);
            verify(e, &part).unwrap();
            assert_eq!(
                std::fs::metadata(&part).unwrap().len(),
                10 * f.size() as u64
            );
        }
        // Un fichier 188 n'est pas un 204.
        let e = entries.iter().find(|e| e.name == "ts204_h264").unwrap();
        assert!(verify(e, &src).is_err());
        let plain = entries
            .iter()
            .find(|e| e.name == "h264_720p_closed")
            .unwrap();
        assert_eq!(packet_format(plain), None);
        std::fs::remove_dir_all(&dir).unwrap();
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

    #[test]
    fn splice_null_section_has_a_valid_crc() {
        let payload = splice_null_payload();
        assert_eq!(payload[0], 0, "pointer_field");
        assert_eq!(payload[1], 0xFC);
        let length = usize::from(payload[2] & 0x0F) << 8 | usize::from(payload[3]);
        assert_eq!(payload.len(), 1 + 3 + length);
        assert_eq!(ma_ts::crc32_mpeg2(&payload[1..]), 0);
    }

    #[test]
    fn reference_pmt_declares_the_five_streams_and_extras() {
        let section = ma_ts::testutil::section_of(&ref_pmt(PCR_PID, &[(0xEA, 0x104, vec![])]));
        let pmt = ma_ts::parse_pmt(&section).unwrap();
        assert_eq!(pmt.pcr_pid, PCR_PID);
        let pids: Vec<u16> = pmt.streams.iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![0x100, 0x101, 0x102, 0x103, 0x1F4, 0x104]);
    }
}
