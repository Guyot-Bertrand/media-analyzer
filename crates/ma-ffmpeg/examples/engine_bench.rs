//! Banc du moteur d'images : mesure la latence des demandes explicites de `FrameService` sur un
//! fichier TS du corpus, avec le décodeur libavcodec.
//!
//! ```text
//! cargo run --release -p ma-ffmpeg --example engine_bench -- \
//!     --file <ts> --mode auto|sw --view fit:1920x1080|1to1:1920x1080 --dwell-ms <n> \
//!     [--progressive] [--json <out>]
//! ```
//!
//! Les scénarios de pas publient à part le premier pas (`first_ms`, juste après le placement,
//! démarrage à froid) ; les percentiles portent sur les pas suivants. Le point de départ arrière
//! (`total / 2`) est volontairement le pire cas si c'est une IDR : le premier pas recule alors
//! d'un GOP entier. Le `dwell` s'applique aussi après chaque placement.
//!
//! Scénarios, à graine fixe : 100 sauts aléatoires, 200 pas avant, 200 pas arrière, puis attente de
//! 1 s et lecture de `readiness`. `--dwell-ms` est le temps entre la réception d'une image et la
//! demande suivante. Chaque numéro incrusté est lu dans l'image reçue et comparé à l'attendu : le
//! banc s'arrête (code 1) au premier écart. En `1to1`, si la région ne couvre pas toute la bande
//! du numéro (image plus large que la zone), seuls les bits couverts sont comparés.
//!
//! Source d'AU : `ma_ts::TsAuSource`. Par défaut, le fichier est indexé entièrement avant les
//! mesures. Avec `--progressive`, il est ouvert par `open_progressive` : le banc mesure le
//! temps jusqu'à la 1re image (ouverture comprise), attend la fin de l'indexation (durée
//! publiée), puis déroule les scénarios. Le JSON reçoit alors un objet `progressive` : temps
//! jusqu'à la 1re image, durée d'indexation, octets lus (indexeur, navigation) rapportés à la
//! taille du fichier.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ma_decode::{
    AuSource, ConvertPath, DecodeEvent, DecodeMode, FrameService, ModePref, NavConfig, NavStats,
    Navigator, PrefetchConfig, Readiness, Rect, Size, Viewport,
};
use ma_ffmpeg::{FfmpegFactory, vaapi_available, vaapi_has_vpp};
use ma_testkit::{manifest, read_frame_code_bits};
use ma_ts::TsAuSource;

/// Graine fixe des sauts aléatoires.
const SEED: u64 = 0x4D41_5F42_454E_4348;
const JUMPS: usize = 100;
const STEPS: u32 = 200;

#[derive(Debug, Clone, Copy)]
enum View {
    Fit(Size),
    OneToOne(Size),
}

struct Args {
    file: PathBuf,
    pref: ModePref,
    view: View,
    dwell: Duration,
    json: Option<PathBuf>,
    progressive: bool,
}

fn parse_size(s: &str) -> Result<Size, String> {
    let (w, h) = s.split_once('x').ok_or("taille attendue : LxH")?;
    let (w, h) = (
        w.parse().map_err(|_| "largeur invalide")?,
        h.parse().map_err(|_| "hauteur invalide")?,
    );
    Ok(Size { w, h })
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut file, mut pref, mut view, mut dwell, mut json) = (None, None, None, None, None);
    let mut progressive = false;
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} attend une valeur"));
        match flag.as_str() {
            "--file" => file = Some(PathBuf::from(value()?)),
            "--mode" => {
                pref = Some(match value()?.as_str() {
                    "auto" => ModePref::Auto,
                    "sw" => ModePref::SoftwareOnly,
                    other => return Err(format!("mode inconnu : {other}")),
                });
            }
            "--view" => {
                let v = value()?;
                view = Some(match v.split_once(':') {
                    Some(("fit", s)) => View::Fit(parse_size(s)?),
                    Some(("1to1", s)) => View::OneToOne(parse_size(s)?),
                    _ => return Err(format!("vue inconnue : {v}")),
                });
            }
            "--dwell-ms" => {
                let ms = value()?.parse().map_err(|_| "--dwell-ms invalide")?;
                dwell = Some(Duration::from_millis(ms));
            }
            "--json" => json = Some(PathBuf::from(value()?)),
            "--progressive" => progressive = true,
            other => return Err(format!("option inconnue : {other}")),
        }
    }
    Ok(Args {
        file: file.ok_or("--file requis")?,
        pref: pref.ok_or("--mode requis")?,
        view: view.ok_or("--view requis")?,
        dwell: dwell.ok_or("--dwell-ms requis")?,
        json,
        progressive,
    })
}

/// xorshift64* : suffisant pour tirer des positions reproductibles.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let r = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33;
        u32::try_from(r % u64::from(n)).expect("reste < n")
    }
}

/// Résultat d'un scénario.
struct Scenario {
    name: &'static str,
    /// Latences des demandes en régime établi, en millisecondes (hors premier pas des scénarios
    /// de pas).
    ms: Vec<f64>,
    /// Latence du premier pas, juste après le placement (démarrage à froid), pour les scénarios
    /// de pas.
    first_ms: Option<f64>,
    /// Octets d'image en moyenne.
    bytes_per_frame: u64,
    /// Transferts GPU → RAM pleine résolution survenus pendant le scénario.
    full_res_transfers: u64,
}

impl Scenario {
    /// Percentile en millièmes (500 = médiane), par rang le plus proche vers le haut.
    fn percentile(&self, permille: usize) -> f64 {
        let mut v = self.ms.clone();
        v.sort_by(f64::total_cmp);
        let rank = (permille * v.len()).div_ceil(1000).max(1);
        v[rank.min(v.len()) - 1]
    }
}

struct Bench<'a> {
    service: &'a FrameService,
    full: Size,
    view: View,
    vp: Viewport,
    modulo: u32,
    dwell: Duration,
    total: u32,
}

impl Bench<'_> {
    /// Demande l'image `pres`, vérifie son numéro et renvoie (latence, octets).
    fn fetch(&self, pres: u32) -> Result<(Duration, usize), String> {
        let t = Instant::now();
        let frame = self
            .service
            .get(pres, self.vp)
            .map_err(|e| format!("image {pres} : {e}"))?;
        let dt = t.elapsed();
        let region = self.vp.region;
        let (code, mask) = read_frame_code_bits(&frame.image, region, self.full)
            .ok_or_else(|| format!("image {pres} : numéro incrusté illisible"))?;
        let expected = (pres % self.modulo) & mask;
        if code != expected {
            return Err(format!(
                "image {pres} : numéro lu {code:#x}, attendu {expected:#x} (masque {mask:#x})"
            ));
        }
        if frame.corrupt {
            return Err(format!("image {pres} : marquée corrompue"));
        }
        Ok((dt, frame.image.byte_len()))
    }

    /// Mesure les demandes `positions`, dans l'ordre. Si `split_first`, la première est publiée à
    /// part (`first_ms`) et exclue des percentiles.
    fn run(
        &self,
        name: &'static str,
        positions: &[u32],
        split_first: bool,
    ) -> Result<Scenario, String> {
        let before = self.service.stats().decoder.full_res_transfers;
        let (mut ms, mut bytes) = (Vec::with_capacity(positions.len()), 0u64);
        for &pres in positions {
            let (dt, len) = self.fetch(pres)?;
            ms.push(dt.as_secs_f64() * 1000.0);
            bytes += len as u64;
            std::thread::sleep(self.dwell);
        }
        let count = positions.len().max(1) as u64;
        let first_ms = split_first.then(|| ms.remove(0));
        Ok(Scenario {
            name,
            ms,
            first_ms,
            bytes_per_frame: bytes / count,
            full_res_transfers: self.service.stats().decoder.full_res_transfers - before,
        })
    }

    /// Place le décodeur à `pres` sans mesure, puis attend `dwell` comme après toute image.
    fn seek(&self, pres: u32) -> Result<(), String> {
        self.fetch(pres)?;
        std::thread::sleep(self.dwell);
        Ok(())
    }
}

/// Plus grande taille résidente atteinte (ko), lue dans `/proc/self/status` (`VmHWM`).
fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

fn mode_name(m: DecodeMode) -> &'static str {
    match m {
        DecodeMode::Hardware => "VA-API",
        DecodeMode::Software => "logiciel",
        DecodeMode::Unknown => "inconnu",
    }
}

fn path_name(p: ConvertPath) -> &'static str {
    match p {
        ConvertPath::Unknown => "inconnu",
        ConvertPath::Software => "logiciel",
        ConvertPath::VaapiVpp => "VPP",
        ConvertPath::VaapiVppRefused => "VPP refusée (projection)",
        ConvertPath::VaapiMapped => "projection (sans VPP)",
        ConvertPath::VaapiTransfer => "transfert",
    }
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Ouverture progressive : mesures de `--progressive`.
struct Progressive {
    /// Ouverture comprise, jusqu'à la réception de l'image 0.
    first_frame_ms: f64,
    /// Ouverture comprise, jusqu'à la fin de l'indexation.
    index_ms: f64,
    file_bytes: u64,
    /// Octets lus par l'indexeur (lecture directe) et par la navigation (cache), à la fin de
    /// l'indexation.
    indexer_bytes: u64,
    nav_bytes: u64,
}

impl Progressive {
    #[allow(clippy::cast_precision_loss)]
    fn ratio(&self, bytes: u64) -> f64 {
        bytes as f64 / self.file_bytes.max(1) as f64
    }
}

fn navigator(src: &Arc<TsAuSource>, pref: ModePref) -> Result<Navigator, String> {
    let mut nav_cfg = NavConfig::default();
    nav_cfg.cache_frames = nav_cfg.cache_frames.max(48);
    Navigator::new(
        Arc::clone(src) as Arc<dyn AuSource>,
        Arc::new(FfmpegFactory::default()),
        pref,
        nav_cfg,
    )
    .map_err(|e| format!("navigateur : {e}"))
}

/// Ouvre `args.file` en progressif, mesure la 1re image puis attend la fin de l'indexation.
fn open_progressive(
    args: &Args,
) -> Result<(Arc<TsAuSource>, FrameService, Size, Progressive), String> {
    let t0 = Instant::now();
    let (src, handle) =
        TsAuSource::open_progressive(&args.file).map_err(|e| format!("ouverture : {e}"))?;
    let src = Arc::new(src);
    let service = FrameService::start(navigator(&src, args.pref)?, PrefetchConfig::default());
    let full = service.probe_size().map_err(|e| format!("taille : {e}"))?;
    service
        .get(0, Viewport::fit(full, Size { w: 320, h: 180 }))
        .map_err(|e| format!("image 0 : {e}"))?;
    let first_frame_ms = t0.elapsed().as_secs_f64() * 1000.0;
    handle.join().map_err(|e| format!("indexation : {e}"))?;
    let p = Progressive {
        first_frame_ms,
        index_ms: t0.elapsed().as_secs_f64() * 1000.0,
        file_bytes: src.index().progress().bytes_total,
        indexer_bytes: src.indexer_io_stats().bytes_read,
        nav_bytes: src.io_stats().bytes_read,
    };
    Ok((src, service, full, p))
}

fn real_main() -> Result<(), String> {
    let args = parse_args()?;
    let (src, service, full, progressive) = if args.progressive {
        let (src, service, full, p) = open_progressive(&args)?;
        (src, service, full, Some(p))
    } else {
        let src = Arc::new(TsAuSource::open(&args.file).map_err(|e| format!("ouverture : {e}"))?);
        let service = FrameService::start(navigator(&src, args.pref)?, PrefetchConfig::default());
        let full = service.probe_size().map_err(|e| format!("taille : {e}"))?;
        (src, service, full, None)
    };
    let total = src.len();
    if total < 5 {
        return Err(format!("{total} image(s) : le banc en demande au moins 5"));
    }
    let name = stem_of(&args.file);
    let modulo = manifest(&name).code_modulo;
    let vp = match args.view {
        View::Fit(area) => Viewport::fit(full, area),
        View::OneToOne(area) => Viewport::region_1to1(
            full,
            Rect {
                x: 0,
                y: 0,
                w: area.w.min(full.w),
                h: area.h.min(full.h),
            },
        ),
    };
    let bench = Bench {
        service: &service,
        full,
        view: args.view,
        vp,
        modulo,
        dwell: args.dwell,
        total,
    };
    eprintln!(
        "{name} : {total} images {}x{}, vue {:?} -> viewport {:?}, dwell {} ms",
        full.w,
        full.h,
        bench.view,
        vp,
        args.dwell.as_millis()
    );

    // Échauffement (ouverture du décodeur, graphes GPU) hors mesure.
    bench.seek(0)?;

    let mut rng = Rng(SEED);
    let jumps: Vec<u32> = (0..JUMPS).map(|_| rng.below(bench.total)).collect();
    let steps = STEPS.min(total.saturating_sub(1) / 2);
    let fwd_from = total / 4;
    let fwd_from = fwd_from.min(total - 1 - steps);
    let back_from = (total / 2).max(steps).min(total - 1);

    let mut scenarios = vec![bench.run("jump", &jumps, false)?];
    bench.seek(fwd_from)?;
    let fwd: Vec<u32> = (1..=steps).map(|i| fwd_from + i).collect();
    scenarios.push(bench.run("step_forward", &fwd, true)?);
    bench.seek(back_from)?;
    let bwd: Vec<u32> = (1..=steps).map(|i| back_from - i).collect();
    scenarios.push(bench.run("step_backward", &bwd, true)?);

    std::thread::sleep(Duration::from_secs(1));
    let ready = service.readiness();

    let stats = service.stats();
    let mode = service.mode();
    let path = stats.decoder.path;
    let fell_back = service
        .take_events()
        .iter()
        .any(|e| matches!(e, DecodeEvent::FellBackToSoftware { .. }));
    let report = Report {
        args: &args,
        name,
        total,
        full,
        scenarios,
        ready,
        stats,
        mode_text: format!(
            "{} / {}{}",
            mode_name(mode),
            path_name(path),
            if fell_back { " (repli logiciel)" } else { "" }
        ),
        decode_mode: mode_name(mode),
        convert_path: path_name(path),
        fell_back,
        rss_kib: peak_rss_kib(),
        progressive,
    };
    report.print();
    if let Some(out) = &args.json {
        report.write_json(out)?;
    }
    Ok(())
}

/// Résultats complets d'une exécution.
struct Report<'a> {
    args: &'a Args,
    name: String,
    total: u32,
    full: Size,
    scenarios: Vec<Scenario>,
    ready: Readiness,
    stats: NavStats,
    mode_text: String,
    decode_mode: &'static str,
    convert_path: &'static str,
    fell_back: bool,
    rss_kib: u64,
    progressive: Option<Progressive>,
}

impl Report<'_> {
    fn print(&self) {
        println!(
            "fichier {}, vue {:?}, dwell {} ms",
            self.name,
            self.args.view,
            self.args.dwell.as_millis()
        );
        println!(
            "mode réel : {} (VA-API présent : {}, VPP : {})",
            self.mode_text,
            vaapi_available(None),
            vaapi_has_vpp(None)
        );
        println!(
            "{:<14} {:>5} {:>9} {:>9} {:>9} {:>10} {:>10} {:>12}",
            "scénario",
            "n",
            "p50 ms",
            "p95 ms",
            "max ms",
            "1er pas ms",
            "transferts",
            "octets/image"
        );
        for s in &self.scenarios {
            println!(
                "{:<14} {:>5} {:>9.1} {:>9.1} {:>9.1} {:>10} {:>10} {:>12}",
                s.name,
                s.ms.len(),
                s.percentile(500),
                s.percentile(950),
                s.percentile(1000),
                s.first_ms.map_or("-".into(), |v| format!("{v:.1}")),
                s.full_res_transfers,
                s.bytes_per_frame
            );
        }
        let r = self.ready;
        println!(
            "readiness après 1 s : centre {}, suivantes prêtes {}, précédentes prêtes {}",
            r.center, r.ahead_ready, r.behind_ready
        );
        println!(
            "transferts pleine résolution (total) : {}",
            self.stats.decoder.full_res_transfers
        );
        println!("RSS max : {} Mo", self.rss_kib / 1024);
        if let Some(p) = &self.progressive {
            println!(
                "progressif : 1re image {:.1} ms, indexation {:.1} ms, octets lus / fichier : \
                 indexeur {:.3}, navigation {:.3}",
                p.first_frame_ms,
                p.index_ms,
                p.ratio(p.indexer_bytes),
                p.ratio(p.nav_bytes)
            );
        }
    }

    fn write_json(&self, out: &Path) -> Result<(), String> {
        let mut j = String::new();
        let _ = write!(
            j,
            "{{\n  \"file\": \"{}\",\n  \"view\": \"{}\",\n  \"dwell_ms\": {},\n  \"frames\": {},\n  \"frame_size\": [{}, {}],\n  \"seed\": {SEED},\n  \"mode\": \"{}\",\n  \"decode_mode\": \"{}\",\n  \"convert_path\": \"{}\",\n  \"fell_back\": {},\n  \"full_res_transfers\": {},\n  \"rss_max_kib\": {},\n  \"readiness\": {{\"center\": {}, \"ahead_ready\": {}, \"behind_ready\": {}}},\n  \"scenarios\": [",
            json_escape(&self.args.file.display().to_string()),
            json_escape(&format!("{:?}", self.args.view)),
            self.args.dwell.as_millis(),
            self.total,
            self.full.w,
            self.full.h,
            json_escape(&self.mode_text),
            self.decode_mode,
            self.convert_path,
            self.fell_back,
            self.stats.decoder.full_res_transfers,
            self.rss_kib,
            self.ready.center,
            self.ready.ahead_ready,
            self.ready.behind_ready,
        );
        for (i, s) in self.scenarios.iter().enumerate() {
            let _ = write!(
                j,
                "{}\n    {{\"name\": \"{}\", \"n\": {}, \"p50_ms\": {:.3}, \"p95_ms\": {:.3}, \"max_ms\": {:.3}, \"first_ms\": {}, \"full_res_transfers\": {}, \"bytes_per_frame\": {}}}",
                if i == 0 { "" } else { "," },
                s.name,
                s.ms.len(),
                s.percentile(500),
                s.percentile(950),
                s.percentile(1000),
                s.first_ms.map_or("null".into(), |v| format!("{v:.3}")),
                s.full_res_transfers,
                s.bytes_per_frame
            );
        }
        j.push_str("\n  ]");
        if let Some(p) = &self.progressive {
            let _ = write!(
                j,
                ",\n  \"progressive\": {{\"first_frame_ms\": {:.3}, \"index_ms\": {:.3}, \"file_bytes\": {}, \"indexer_bytes_read\": {}, \"nav_bytes_read\": {}, \"indexer_read_ratio\": {:.4}, \"nav_read_ratio\": {:.4}, \"read_ratio\": {:.4}}}",
                p.first_frame_ms,
                p.index_ms,
                p.file_bytes,
                p.indexer_bytes,
                p.nav_bytes,
                p.ratio(p.indexer_bytes),
                p.ratio(p.nav_bytes),
                p.ratio(p.indexer_bytes + p.nav_bytes)
            );
        }
        j.push_str("\n}\n");
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(out, j).map_err(|e| format!("{}: {e}", out.display()))
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engine_bench : {e}");
            ExitCode::FAILURE
        }
    }
}
