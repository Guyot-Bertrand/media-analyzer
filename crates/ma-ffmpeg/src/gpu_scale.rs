//! Réduction et recadrage sur le GPU : VPP VA-API par libavfilter (`buffer` → `scale_vaapi` →
//! `buffersink`). Seule l'image de sortie (taille affichée) est transférée en RAM.
//!
//! La région passe par les champs `crop_*` de l'image d'entrée : la VPP de `FFmpeg` en fait sa
//! `input_region`. `FFmpeg` n'applique aux images matérielles que le rognage droit et bas du flux
//! (fenêtre de conformité) ; le rognage gauche et haut reste dans `crop_left` / `crop_top`, et
//! s'ajoute à l'origine de la région ([`crop_origin`], [`display_size`]). Les graphes sont gardés en cache (au plus [`MAX_GRAPHS`], le moins récemment
//! utilisé est libéré), indexés par le contexte de surfaces d'entrée, son format, la taille
//! d'entrée, la taille et le format de sortie : un changement de résolution en cours de flux
//! (nouveau contexte de surfaces, nouvelle taille) construit un nouveau graphe.

use std::ffi::CString;

use ma_decode::{DecodeError, DecodeErrorKind, Planes, Rect, Size, Viewport};
use rsmpeg::avfilter::{AVFilter, AVFilterGraph};
use rsmpeg::avutil::AVFrame;
use rsmpeg::ffi;

use crate::planes;

/// Nombre maximal de graphes gardés en cache.
pub(crate) const MAX_GRAPHS: usize = 4;

/// Clé d'un graphe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GraphKey {
    /// Adresse du contexte de surfaces d'entrée (`AVHWFramesContext`) : le graphe en garde une
    /// référence, l'adresse ne peut donc pas être réutilisée par un autre contexte tant que le
    /// graphe vit.
    frames_ctx: usize,
    sw_format: i32,
    input: Size,
    out: Size,
    out_format: i32,
    /// Réduction (`mode=hq`) ou région 1:1 (`mode=fast`, simple copie) : une réduction et une
    /// région 1:1 de même sortie ne partagent jamais un graphe.
    reduced: bool,
}

fn gpu(detail: String) -> DecodeError {
    DecodeError {
        kind: DecodeErrorKind::Gpu,
        detail,
    }
}

fn to_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn fmt_name(format: i32) -> &'static str {
    match format {
        ffi::AV_PIX_FMT_NV12 => "nv12",
        ffi::AV_PIX_FMT_P010LE => "p010",
        _ => "?",
    }
}

/// Mode de `scale_vaapi` : `hq` pour une réduction, `fast` en 1:1 (copie de la région, exacte
/// octet pour octet).
fn scaling_mode(reduced: bool) -> &'static str {
    if reduced { "hq" } else { "fast" }
}

/// Construit le graphe `buffer` → `scale_vaapi` → `buffersink` pour `key`, sur le contexte de
/// surfaces `frames_ctx` de l'image d'entrée.
fn build(key: GraphKey, frames_ctx: *mut ffi::AVBufferRef) -> Result<AVFilterGraph, String> {
    let filter = |name: &std::ffi::CStr| {
        AVFilter::get_by_name(name).ok_or_else(|| format!("filtre {name:?} absent de libavfilter"))
    };
    let (buffer, scale, sink) = (
        filter(c"buffer")?,
        filter(c"scale_vaapi")?,
        filter(c"buffersink")?,
    );
    let graph = AVFilterGraph::new();
    {
        let mut src = graph
            .alloc_filter_context(&buffer, c"in")
            .ok_or("allocation du filtre buffer")?;
        // SAFETY: `av_buffersrc_parameters_alloc` rend une structure initialisée (ou nul, vérifié)
        // que l'on remplit puis libère avec `av_free`. `av_buffersrc_parameters_set` prend sa
        // propre référence sur `hw_frames_ctx` (référence vivante de l'image d'entrée, empruntée
        // pendant l'appel) et ne garde pas `par`.
        let status = unsafe {
            let par = ffi::av_buffersrc_parameters_alloc();
            if par.is_null() {
                return Err("allocation des paramètres de buffer".into());
            }
            (*par).format = ffi::AV_PIX_FMT_VAAPI;
            (*par).width = to_i32(key.input.w);
            (*par).height = to_i32(key.input.h);
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: 90_000,
            };
            (*par).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = frames_ctx;
            let status = ffi::av_buffersrc_parameters_set(src.as_mut_ptr(), par);
            ffi::av_free(par.cast());
            status
        };
        if status < 0 {
            return Err(format!("paramètres de buffer : code {status}"));
        }
        src.init_str(None).map_err(|e| format!("buffer : {e}"))?;
        let args = CString::new(format!(
            "w={}:h={}:format={}:mode={}",
            key.out.w,
            key.out.h,
            fmt_name(key.out_format),
            scaling_mode(key.reduced)
        ))
        .expect("arguments sans NUL");
        let mut vpp = graph
            .create_filter_context(&scale, c"vpp", Some(&args))
            .map_err(|e| format!("scale_vaapi : {e}"))?;
        let mut out = graph
            .create_filter_context(&sink, c"out", None)
            .map_err(|e| format!("buffersink : {e}"))?;
        src.link(0, &mut vpp, 0)
            .and_then(|()| vpp.link(0, &mut out, 0))
            .map_err(|e| format!("liaison des filtres : {e}"))?;
    }
    graph
        .config()
        .map_err(|e| format!("configuration du graphe : {e}"))?;
    Ok(graph)
}

/// Réduction et recadrage VA-API, avec cache de graphes.
#[derive(Default)]
pub(crate) struct GpuScaler {
    /// Du plus récemment utilisé au plus ancien.
    graphs: Vec<(GraphKey, AVFilterGraph)>,
    /// Transferts GPU → RAM d'une image de la taille complète de l'image décodée.
    pub(crate) full_res_transfers: u64,
}

impl GpuScaler {
    /// Nombre de graphes en cache.
    pub(crate) fn graphs(&self) -> usize {
        self.graphs.len()
    }

    /// Image VA-API `frame` (décodée et synchronisée) rendue pour `vp` (normalisé) : réduction en
    /// NV12, ou région 1:1 au format natif des surfaces (NV12 ou P010). Toute erreur est imputée
    /// au GPU.
    ///
    /// `vp` est exprimé dans l'image affichée ([`display_size`]) ; le rognage gauche et haut du
    /// décodeur ([`crop_origin`]) y est ajouté.
    pub(crate) fn scale(&mut self, frame: &AVFrame, vp: &Viewport) -> Result<Planes, DecodeError> {
        let input = Size {
            w: u32::try_from(frame.width).unwrap_or(0),
            h: u32::try_from(frame.height).unwrap_or(0),
        };
        let shown = display_size(frame);
        let (cx, cy) = crop_origin(frame);
        let r = Rect {
            x: vp.region.x + cx,
            y: vp.region.y + cy,
            ..vp.region
        };
        let whole = Rect {
            x: 0,
            y: 0,
            w: shown.w,
            h: shown.h,
        };
        if !vp.is_reduced() && vp.region == whole {
            // Image entière en 1:1 : la VPP n'apporterait rien.
            let sw = self.transfer(frame, shown)?;
            return planes::extract(&sw, r);
        }
        let sw_format = sw_format(frame)?;
        let out_format = if vp.is_reduced() {
            ffi::AV_PIX_FMT_NV12
        } else if matches!(sw_format, ffi::AV_PIX_FMT_NV12 | ffi::AV_PIX_FMT_P010LE) {
            sw_format
        } else {
            return Err(gpu(format!(
                "format de surface VA-API non géré : {sw_format}"
            )));
        };
        let key = GraphKey {
            frames_ctx: frames_ctx_addr(frame),
            sw_format,
            input,
            out: vp.out,
            out_format,
            reduced: vp.is_reduced(),
        };
        let mut cropped = frame.clone();
        // SAFETY: `cropped` est une nouvelle référence (`av_frame_clone`) possédée ici ; les champs
        // `crop_*` sont de simples entiers. La région est incluse dans l'image affichée (viewport
        // normalisé), elle-même incluse dans la surface décalée du rognage : les soustractions ne
        // débordent pas.
        unsafe {
            let f = cropped.as_mut_ptr();
            (*f).crop_left = r.x as usize;
            (*f).crop_top = r.y as usize;
            (*f).crop_right = (input.w - r.x - r.w) as usize;
            (*f).crop_bottom = (input.h - r.y - r.h) as usize;
        }
        let scaled = match run(self.graph(key, frame.hw_frames_ctx)?, cropped) {
            Ok(scaled) => scaled,
            Err(e) => {
                // Le graphe peut garder une image en attente : il n'est pas réutilisé.
                self.graphs.remove(0);
                return Err(e);
            }
        };
        let sw = self.transfer(&scaled, shown)?;
        planes::extract(
            &sw,
            Rect {
                x: 0,
                y: 0,
                w: vp.out.w,
                h: vp.out.h,
            },
        )
    }

    /// Graphe pour `key`, pris dans le cache (et remis en tête) ou construit.
    fn graph(
        &mut self,
        key: GraphKey,
        frames_ctx: *mut ffi::AVBufferRef,
    ) -> Result<&AVFilterGraph, DecodeError> {
        if let Some(i) = self.graphs.iter().position(|(k, _)| *k == key) {
            let entry = self.graphs.remove(i);
            self.graphs.insert(0, entry);
        } else {
            let graph = build(key, frames_ctx).map_err(|e| gpu(format!("VPP : {e}")))?;
            self.graphs.truncate(MAX_GRAPHS - 1);
            self.graphs.insert(0, (key, graph));
        }
        Ok(&self.graphs[0].1)
    }

    /// Transfert GPU → RAM de l'image VA-API `frame` ; compté s'il porte sur une image de la
    /// taille complète `full`.
    fn transfer(&mut self, frame: &AVFrame, full: Size) -> Result<AVFrame, DecodeError> {
        let mut sw = AVFrame::new();
        sw.hwframe_transfer_data(frame)
            .map_err(|e| gpu(format!("transfert GPU → RAM : {e}")))?;
        if sw.width >= to_i32(full.w) && sw.height >= to_i32(full.h) {
            self.full_res_transfers += 1;
        }
        Ok(sw)
    }
}

/// Rognage gauche et haut (`crop_left`, `crop_top`) laissé par `FFmpeg` dans l'image `frame` :
/// origine de l'image affichée dans la surface. Nul pour une image logicielle (déjà rognée).
pub(crate) fn crop_origin(frame: &AVFrame) -> (u32, u32) {
    let at = |v: usize| u32::try_from(v).unwrap_or(0);
    (at(frame.crop_left), at(frame.crop_top))
}

/// Taille affichée de l'image `frame` : sa taille, moins les rognages encore présents dans
/// `crop_*`.
pub(crate) fn display_size(frame: &AVFrame) -> Size {
    let side = |v: i32, a: usize, b: usize| {
        u32::try_from(v)
            .unwrap_or(0)
            .saturating_sub(u32::try_from(a.saturating_add(b)).unwrap_or(u32::MAX))
    };
    Size {
        w: side(frame.width, frame.crop_left, frame.crop_right),
        h: side(frame.height, frame.crop_top, frame.crop_bottom),
    }
}

/// Passe l'image `frame` dans le graphe et rend l'image VA-API de sortie.
fn run(graph: &AVFilterGraph, frame: AVFrame) -> Result<AVFrame, DecodeError> {
    let mut src = graph.get_filter(c"in").expect("filtre « in » du graphe");
    src.buffersrc_add_frame(Some(frame), None)
        .map_err(|e| gpu(format!("VPP (entrée) : {e}")))?;
    let mut sink = graph.get_filter(c"out").expect("filtre « out » du graphe");
    sink.buffersink_get_frame(None)
        .map_err(|e| gpu(format!("VPP (sortie) : {e}")))
}

/// Adresse de l'`AVHWFramesContext` de l'image (et non de sa référence `AVBufferRef`, propre à
/// chaque image).
fn frames_ctx_addr(frame: &AVFrame) -> usize {
    if frame.hw_frames_ctx.is_null() {
        return 0;
    }
    // SAFETY: `hw_frames_ctx` (non nul, vérifié) référence un `AVBufferRef` vivant tant que
    // l'image l'est.
    unsafe { (*frame.hw_frames_ctx).data as usize }
}

/// Format logiciel des surfaces de l'image VA-API `frame`.
fn sw_format(frame: &AVFrame) -> Result<i32, DecodeError> {
    if frame.format != ffi::AV_PIX_FMT_VAAPI || frame.hw_frames_ctx.is_null() {
        return Err(gpu("image VA-API sans contexte de surfaces".into()));
    }
    // SAFETY: `hw_frames_ctx` (non nul, vérifié) référence un `AVBufferRef` vivant tant que
    // l'image l'est, dont `data` est un `AVHWFramesContext` alloué par `av_mallocz` (aligné),
    // vérifié non nul avant lecture.
    #[allow(clippy::cast_ptr_alignment)]
    let format = unsafe {
        let frames = (*frame.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>();
        if frames.is_null() {
            None
        } else {
            Some((*frames).sw_format)
        }
    };
    format.ok_or_else(|| gpu("contexte de surfaces VA-API vide".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_frames_are_rejected() {
        let mut s = GpuScaler::default();
        let mut f = AVFrame::new();
        f.set_format(ffi::AV_PIX_FMT_YUV420P);
        f.set_width(64);
        f.set_height(64);
        f.alloc_buffer().unwrap();
        let vp = Viewport {
            region: Rect {
                x: 0,
                y: 0,
                w: 64,
                h: 64,
            },
            out: Size { w: 32, h: 32 },
        };
        let err = s.scale(&f, &vp).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Gpu);
        assert!(err.detail.contains("sans contexte"), "{err}");
        // Image entière en 1:1 : transfert direct, refusé pour une image logicielle.
        let whole = Viewport {
            out: Size { w: 64, h: 64 },
            ..vp
        };
        assert_eq!(s.scale(&f, &whole).unwrap_err().kind, DecodeErrorKind::Gpu);
        assert_eq!((s.full_res_transfers, s.graphs()), (0, 0));
    }

    #[test]
    fn scaling_mode_follows_the_reduction() {
        assert_eq!(scaling_mode(true), "hq");
        assert_eq!(scaling_mode(false), "fast");
    }

    #[test]
    fn output_formats_have_names() {
        assert_eq!(fmt_name(ffi::AV_PIX_FMT_NV12), "nv12");
        assert_eq!(fmt_name(ffi::AV_PIX_FMT_P010LE), "p010");
        assert_eq!(fmt_name(ffi::AV_PIX_FMT_RGB24), "?");
    }
}
