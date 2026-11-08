//! Banc interface du transport d'image : une fenêtre, un canvas WebGL2, et le moteur
//! (`FrameService` sur libavcodec) servi par le protocole `frame://`.
//!
//! - `frame://localhost/<pres>?x=&y=&w=&h=&ow=&oh=` : image `pres` pour le viewport donné. Réponse
//!   binaire : en-tête de 32 octets petit-boutiste (`MAF1`, layout, largeur, hauteur, nombre de
//!   plans, 3 strides en octets) puis les plans, compactés et concaténés. Une erreur est renvoyée
//!   en code 200 avec une enveloppe (`MAE1` + JSON `{"error": …}`) : `WebKitGTK` transforme les
//!   autres codes en `TypeError` côté `fetch` (spike, 04 §3.3).
//! - Commandes : `initial_file`, `open`, `viewport`, `engine_stats`, `rss`.
//!
//! L'interface n'a qu'une demande explicite en vol (R8) : la suivante part après réception.
//! Source d'AU : `ma_ts::TsAuSource` (démultiplexeur du moteur, indexation complète à l'ouverture).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ma_decode::{
    AuSource, ConvertPath, DecodeEvent, DecodeMode, FrameService, ModePref, NavConfig, Navigator,
    PixelLayout, Planes, PrefetchConfig, Rect, Size, Viewport,
};
use ma_ffmpeg::{FfmpegFactory, vaapi_available, vaapi_has_vpp};
use ma_testkit::manifest;
use ma_ts::TsAuSource;
use serde::Serialize;
use tauri::Manager;
use tauri::http::Response;

/// Fichier ouvert et son service d'images.
struct Session {
    service: FrameService,
    name: String,
    frames: u32,
    full: Size,
    code_modulo: u32,
    /// Un repli logiciel a été signalé depuis l'ouverture.
    fell_back: AtomicBool,
}

#[derive(Default)]
struct AppState {
    session: Mutex<Option<Arc<Session>>>,
}

impl AppState {
    fn current(&self) -> Option<Arc<Session>> {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[derive(Serialize)]
struct OpenInfo {
    name: String,
    frames: u32,
    width: u32,
    height: u32,
    code_modulo: u32,
}

#[derive(Serialize)]
struct ViewportDto {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    ow: u32,
    oh: u32,
}

#[derive(Serialize)]
struct EngineStats {
    decode_mode: &'static str,
    convert_path: &'static str,
    fell_back: bool,
    mode: String,
    vaapi_available: bool,
    vpp_available: bool,
    full_res_transfers: u64,
}

#[derive(Serialize)]
struct ProcessRss {
    pid: u32,
    name: String,
    rss_kib: u64,
    /// Taille proportionnelle (pages partagées divisées entre les processus), `smaps_rollup`.
    pss_kib: u64,
    /// Le processus a un nœud de rendu GPU ouvert (`/dev/dri/renderD*`) : indice de rendu
    /// matériel, `WebKitGTK` masquant le nom du moteur WebGL.
    gpu_render_node: bool,
}

#[derive(Serialize)]
struct RssReport {
    /// Somme des `VmRSS` (les pages partagées sont comptées dans chaque processus).
    total_kib: u64,
    /// Somme des `Pss` : empreinte sans double compte des pages partagées.
    pss_total_kib: u64,
    processes: Vec<ProcessRss>,
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

fn open_session(path: &Path, mode: &str) -> Result<Session, String> {
    let pref = match mode {
        "auto" => ModePref::Auto,
        "sw" => ModePref::SoftwareOnly,
        other => return Err(format!("mode inconnu : {other}")),
    };
    let src = Arc::new(TsAuSource::open(path).map_err(|e| format!("ouverture : {e}"))?);
    let frames = src.len();
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let code_modulo = manifest(&name).code_modulo;
    // Même configuration que le banc moteur (engine_bench).
    let mut nav_cfg = NavConfig::default();
    nav_cfg.cache_frames = nav_cfg.cache_frames.max(48);
    let nav = Navigator::new(
        src as Arc<dyn AuSource>,
        Arc::new(FfmpegFactory::default()),
        pref,
        nav_cfg,
    )
    .map_err(|e| format!("navigateur : {e}"))?;
    let service = FrameService::start(nav, PrefetchConfig::default());
    let full = service.probe_size().map_err(|e| format!("taille : {e}"))?;
    Ok(Session {
        service,
        name,
        frames,
        full,
        code_modulo,
        fell_back: AtomicBool::new(false),
    })
}

/// Fichier à ouvrir au démarrage (`MA_BENCH_OPEN`), pour le lancement à la main ; le test e2e
/// ouvre lui-même son fichier (`MA_BENCH_FILE` côté `WebdriverIO`).
#[tauri::command]
fn initial_file() -> Option<String> {
    std::env::var("MA_BENCH_OPEN").ok()
}

/// Ouvre `path` (`mode` : `auto` ou `sw`) et remplace la session courante.
#[tauri::command]
async fn open(
    state: tauri::State<'_, AppState>,
    path: String,
    mode: String,
) -> Result<OpenInfo, String> {
    // L'ancienne session est arrêtée avant d'ouvrir la nouvelle (un seul décodeur à la fois).
    let old = state
        .session
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    drop(old);
    let s = tauri::async_runtime::spawn_blocking(move || open_session(&PathBuf::from(path), &mode))
        .await
        .map_err(|e| format!("ouverture : {e}"))??;
    let info = OpenInfo {
        name: s.name.clone(),
        frames: s.frames,
        width: s.full.w,
        height: s.full.h,
        code_modulo: s.code_modulo,
    };
    *state.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(s));
    Ok(info)
}

/// Viewport du moteur pour la vue `fit` (image entière réduite pour tenir dans la zone) ou
/// `1to1` (région en haut à gauche, à l'échelle 1:1, bornée à l'image).
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passe `State` par valeur.
fn viewport(
    state: tauri::State<'_, AppState>,
    view: &str,
    area_w: u32,
    area_h: u32,
) -> Result<ViewportDto, String> {
    let s = state.current().ok_or("aucun fichier ouvert")?;
    let full = s.full;
    let vp = match view {
        "fit" => Viewport::fit(
            full,
            Size {
                w: area_w,
                h: area_h,
            },
        ),
        "1to1" => Viewport::region_1to1(
            full,
            Rect {
                x: 0,
                y: 0,
                w: area_w.min(full.w),
                h: area_h.min(full.h),
            },
        ),
        other => return Err(format!("vue inconnue : {other}")),
    };
    Ok(ViewportDto {
        x: vp.region.x,
        y: vp.region.y,
        w: vp.region.w,
        h: vp.region.h,
        ow: vp.out.w,
        oh: vp.out.h,
    })
}

/// Mode réel du décodeur et chemin de conversion de la dernière image.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)] // Tauri passe `State` par valeur.
fn engine_stats(state: tauri::State<'_, AppState>) -> Result<EngineStats, String> {
    let s = state.current().ok_or("aucun fichier ouvert")?;
    if s.service
        .take_events()
        .iter()
        .any(|e| matches!(e, DecodeEvent::FellBackToSoftware { .. }))
    {
        s.fell_back.store(true, Ordering::Relaxed);
    }
    let nav = s.service.stats();
    let (mode, path) = (s.service.mode(), nav.decoder.path);
    let fell_back = s.fell_back.load(Ordering::Relaxed);
    Ok(EngineStats {
        decode_mode: mode_name(mode),
        convert_path: path_name(path),
        fell_back,
        mode: format!(
            "{} / {}{}",
            mode_name(mode),
            path_name(path),
            if fell_back { " (repli logiciel)" } else { "" }
        ),
        vaapi_available: vaapi_available(None),
        vpp_available: vaapi_has_vpp(None),
        full_res_transfers: nav.decoder.full_res_transfers,
    })
}

/// Champ numérique (ko) d'un `/proc/<pid>/status`.
fn status_kib(status: &str, key: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Vrai si `pid` a un descripteur ouvert sur un nœud de rendu DRM.
fn has_render_node(pid: u32) -> bool {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .any(|t| t.to_string_lossy().starts_with("/dev/dri/renderD"))
}

/// Mémoire résidente de ce processus et de tous ses descendants (processus `WebKit` compris).
#[tauri::command]
fn rss() -> RssReport {
    let mut parent = HashMap::new();
    let mut status = HashMap::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(st) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        if let Some(ppid) = status_kib(&st, "PPid:") {
            parent.insert(pid, u32::try_from(ppid).unwrap_or(0));
        }
        status.insert(pid, st);
    }
    let me = std::process::id();
    let is_ours = |mut pid: u32| loop {
        if pid == me {
            return true;
        }
        match parent.get(&pid) {
            Some(&p) if p != 0 && p != pid => pid = p,
            _ => return false,
        }
    };
    let mut processes: Vec<ProcessRss> = status
        .iter()
        .filter(|&(&pid, _)| is_ours(pid))
        .map(|(&pid, st)| ProcessRss {
            pid,
            name: st
                .lines()
                .find_map(|l| l.strip_prefix("Name:"))
                .unwrap_or("")
                .trim()
                .to_owned(),
            rss_kib: status_kib(st, "VmRSS:").unwrap_or(0),
            pss_kib: std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
                .ok()
                .and_then(|r| status_kib(&r, "Pss:"))
                .unwrap_or(0),
            gpu_render_node: has_render_node(pid),
        })
        .collect();
    processes.sort_by_key(|p| p.pid);
    RssReport {
        total_kib: processes.iter().map(|p| p.rss_kib).sum(),
        pss_total_kib: processes.iter().map(|p| p.pss_kib).sum(),
        processes,
    }
}

fn layout_code(l: PixelLayout) -> u32 {
    match l {
        PixelLayout::Planar8 => 0,
        PixelLayout::Planar10 => 1,
        PixelLayout::SemiPlanar8 => 2,
        PixelLayout::SemiPlanar10 => 3,
    }
}

/// En-tête de 32 octets puis les plans.
fn encode(img: &Planes) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + img.byte_len());
    out.extend_from_slice(b"MAF1");
    let n = u32::try_from(img.planes.len()).unwrap_or(0);
    for v in [layout_code(img.layout), img.size.w, img.size.h, n] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for i in 0..3 {
        out.extend_from_slice(&img.strides.get(i).copied().unwrap_or(0).to_le_bytes());
    }
    for p in &img.planes {
        out.extend_from_slice(p);
    }
    out
}

/// Enveloppe d'erreur, en code 200 (voir l'en-tête du module).
fn error_response(msg: &str) -> Response<Vec<u8>> {
    let mut body = b"MAE1".to_vec();
    body.extend_from_slice(serde_json::json!({ "error": msg }).to_string().as_bytes());
    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .expect("réponse d'erreur")
}

/// `<pres>?x=&y=&w=&h=&ow=&oh=` → (pres, viewport).
fn parse_request(path: &str, query: Option<&str>) -> Result<(u32, Viewport), String> {
    let pres = path
        .trim_start_matches('/')
        .parse::<u32>()
        .map_err(|_| format!("numéro d'image invalide : {path}"))?;
    let mut q = HashMap::new();
    for kv in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("paramètre : {kv}"))?;
        let v = v
            .parse::<u32>()
            .map_err(|_| format!("paramètre {k} : {v}"))?;
        q.insert(k, v);
    }
    let get = |k: &str| {
        q.get(k)
            .copied()
            .ok_or_else(|| format!("paramètre {k} absent"))
    };
    let vp = Viewport {
        region: Rect {
            x: get("x")?,
            y: get("y")?,
            w: get("w")?,
            h: get("h")?,
        },
        out: Size {
            w: get("ow")?,
            h: get("oh")?,
        },
    };
    Ok((pres, vp))
}

fn frame_response(session: Option<&Session>, path: &str, query: Option<&str>) -> Response<Vec<u8>> {
    let Some(s) = session else {
        return error_response("aucun fichier ouvert");
    };
    let (pres, vp) = match parse_request(path, query) {
        Ok(r) => r,
        Err(e) => return error_response(&e),
    };
    let t0 = Instant::now();
    let rx = s.service.request(pres, vp);
    let frame = match rx.recv() {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return error_response(&format!("image {pres} : {e}")),
        Err(_) => return error_response("service arrêté"),
    };
    let backend_ms = t0.elapsed().as_secs_f64() * 1000.0;
    Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Expose-Headers",
            "x-backend-ms, x-pres, x-corrupt",
        )
        .header("x-backend-ms", format!("{backend_ms:.3}"))
        .header("x-pres", frame.pres_idx.to_string())
        .header("x-corrupt", u8::from(frame.corrupt).to_string())
        .body(encode(&frame.image))
        .expect("réponse d'image")
}

/// Lance l'application.
///
/// # Panics
///
/// Si Tauri ne peut pas démarrer.
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .register_asynchronous_uri_scheme_protocol("frame", |ctx, request, responder| {
            let session = ctx.app_handle().state::<AppState>().current();
            let uri = request.uri();
            let (path, query) = (uri.path().to_owned(), uri.query().map(str::to_owned));
            // La demande est servie hors du fil principal : le décodage peut prendre du temps.
            std::thread::spawn(move || {
                // Une panique ne doit pas laisser le `fetch` sans réponse : enveloppe d'erreur.
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    frame_response(session.as_deref(), &path, query.as_deref())
                }))
                .unwrap_or_else(|e| {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|m| (*m).to_owned()))
                        .unwrap_or_default();
                    error_response(&format!("panique en servant {path} : {msg}"))
                });
                responder.respond(res);
            });
        })
        .invoke_handler(tauri::generate_handler![
            initial_file,
            open,
            viewport,
            engine_stats,
            rss
        ])
        .run(tauri::generate_context!())
        .expect("démarrage de l'application Tauri");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parsing() {
        let (pres, vp) = parse_request("/42", Some("x=0&y=2&w=1920&h=1080&ow=960&oh=540")).unwrap();
        assert_eq!(pres, 42);
        assert_eq!(
            vp,
            Viewport {
                region: Rect {
                    x: 0,
                    y: 2,
                    w: 1920,
                    h: 1080
                },
                out: Size { w: 960, h: 540 }
            }
        );
        assert!(parse_request("/x", Some("x=0")).is_err());
        assert!(parse_request("/1", Some("x=0&y=0&w=1&h=1&ow=1")).is_err());
    }

    #[test]
    fn header_is_32_bytes() {
        let img = Planes {
            layout: PixelLayout::SemiPlanar10,
            size: Size { w: 4, h: 2 },
            planes: vec![vec![1; 16], vec![2; 8]],
            strides: vec![8, 8],
        };
        let b = encode(&img);
        assert_eq!(&b[..4], b"MAF1");
        assert_eq!(b.len(), 32 + 24);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(b[20..24].try_into().unwrap()), 8);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 0);
        assert_eq!(b[32], 1);
        assert_eq!(b[48], 2);
    }

    #[test]
    fn rss_includes_this_process() {
        let r = rss();
        assert!(r.processes.iter().any(|p| p.pid == std::process::id()));
        assert!(r.total_kib > 0);
        assert!(r.pss_total_kib > 0 && r.pss_total_kib <= r.total_kib);
    }
}
