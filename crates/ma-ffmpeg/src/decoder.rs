//! [`FfmpegDecoder`] : implémentation de [`Decoder`] sur libavcodec, et sa fabrique.

use std::os::raw::c_int;
use std::path::PathBuf;

use ma_decode::{
    Codec, ConvertPath, DecodeError, DecodeErrorKind, DecodeMode, Decoder, DecoderFactory,
    DecoderStats, ModePref, Planes, Received, Size, Viewport,
};
use rsmpeg::avcodec::{AVCodec, AVCodecContext};
use rsmpeg::avutil::{AVFrame, AVHWDeviceContext};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;

use crate::cpu_scale::CpuScaler;
use crate::gpu_scale::GpuScaler;
use crate::{hw, packet};

/// Fabrique de [`FfmpegDecoder`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FfmpegFactory {
    /// Nœud DRM VA-API (`/dev/dri/renderD128`…) ; `None` : choix par défaut de `FFmpeg`.
    pub render_node: Option<PathBuf>,
}

impl FfmpegFactory {
    /// Ouvre un décodeur, sous son type concret (pour lire [`FfmpegDecoder::hw_unavailable`]).
    ///
    /// En [`ModePref::Auto`], essaie VA-API (un seul thread de décodage) ; si le périphérique ou
    /// le décodeur matériel ne s'ouvre pas, décode en logiciel et garde la cause de l'échec.
    pub fn open_decoder(&self, codec: Codec, pref: ModePref) -> Result<FfmpegDecoder, DecodeError> {
        let id = match codec {
            Codec::H264 => ffi::AV_CODEC_ID_H264,
            Codec::Hevc => ffi::AV_CODEC_ID_HEVC,
            Codec::Mpeg2 => ffi::AV_CODEC_ID_MPEG2VIDEO,
        };
        let dec = AVCodec::find_decoder(id).ok_or_else(|| DecodeError {
            kind: DecodeErrorKind::Unsupported,
            detail: format!("décodeur {codec:?} absent de libavcodec"),
        })?;
        if pref == ModePref::SoftwareOnly {
            return FfmpegDecoder::open(&dec, None);
        }
        let cause = match hw::vaapi_device(self.render_node.as_deref()) {
            Ok(dev) => match FfmpegDecoder::open(&dec, Some(dev)) {
                Ok(decoder) => return Ok(decoder),
                Err(e) => e.detail,
            },
            Err(cause) => cause,
        };
        let mut decoder = FfmpegDecoder::open(&dec, None)?;
        decoder.hw_unavailable = Some(cause);
        Ok(decoder)
    }
}

impl DecoderFactory for FfmpegFactory {
    /// Voir [`FfmpegFactory::open_decoder`].
    fn open(&self, codec: Codec, pref: ModePref) -> Result<Box<dyn Decoder>, DecodeError> {
        Ok(Box::new(self.open_decoder(codec, pref)?))
    }
}

/// Catégorie d'une erreur libavcodec de code `code`. `AVERROR_INVALIDDATA` : données invalides ;
/// toute autre erreur est imputée au GPU en mode matériel, sinon interne.
fn classify(code: Option<c_int>, hardware: bool) -> DecodeErrorKind {
    match code {
        Some(ffi::AVERROR_INVALIDDATA) => DecodeErrorKind::InvalidData,
        _ if hardware => DecodeErrorKind::Gpu,
        _ => DecodeErrorKind::Internal,
    }
}

/// Vrai si libavcodec signale l'image comme corrompue ou dissimulée.
fn is_corrupt(decode_error_flags: c_int, flags: c_int) -> bool {
    #[allow(clippy::cast_possible_wrap)]
    let corrupt = ffi::AV_FRAME_FLAG_CORRUPT as c_int;
    decode_error_flags != 0 || flags & corrupt != 0
}

/// Décodeur libavcodec, logiciel ou VA-API.
pub struct FfmpegDecoder {
    ctx: AVCodecContext,
    /// Un périphérique VA-API est attaché au contexte.
    hw_device: bool,
    mode: DecodeMode,
    threads: u32,
    stats: DecoderStats,
    /// Cause de l'échec d'ouverture VA-API en [`ModePref::Auto`].
    hw_unavailable: Option<String>,
    /// Chemin de conversion des images VA-API : [`ConvertPath::VaapiVpp`] si le périphérique
    /// offre la VPP (détecté à l'ouverture), sinon [`ConvertPath::VaapiMapped`].
    vaapi_path: ConvertPath,
    /// Réduction et recadrage des images VA-API (VPP).
    gpu: GpuScaler,
    /// Réduction et recadrage des images logicielles (libswscale).
    cpu: CpuScaler,
}

impl FfmpegDecoder {
    /// Contexte ouvert, avec le périphérique VA-API `device` s'il est donné (un seul thread :
    /// les threads d'images recyclent les surfaces pendant le transfert et n'apportent rien en
    /// VA-API), sinon en logiciel avec un nombre de threads automatique.
    fn open(dec: &AVCodec, device: Option<AVHWDeviceContext>) -> Result<Self, DecodeError> {
        let mut ctx = AVCodecContext::new(dec);
        let hw_device = device.is_some();
        let vaapi_path = match &device {
            Some(dev) if hw::has_vpp(dev) => ConvertPath::VaapiVpp,
            _ => ConvertPath::VaapiMapped,
        };
        if let Some(dev) = device {
            ctx.set_hw_device_ctx(dev);
        }
        // SAFETY: contexte alloué, pas encore ouvert ; `thread_count` est un simple entier lu à
        // l'ouverture (0 : nombre automatique).
        unsafe { (*ctx.as_mut_ptr()).thread_count = c_int::from(hw_device) };
        ctx.open(None).map_err(|e| DecodeError {
            kind: if hw_device {
                DecodeErrorKind::Device
            } else {
                DecodeErrorKind::Internal
            },
            detail: format!("ouverture du décodeur : {e}"),
        })?;
        // Après l'ouverture, `thread_count` vaut le nombre de threads retenu : la sortie est
        // retardée d'autant d'images (défaut n° 1 du spike).
        let threads = u32::try_from(ctx.thread_count).unwrap_or(1).max(1);
        Ok(FfmpegDecoder {
            ctx,
            hw_device,
            mode: DecodeMode::Unknown,
            threads,
            stats: DecoderStats::default(),
            hw_unavailable: None,
            vaapi_path,
            gpu: GpuScaler::default(),
            cpu: CpuScaler::default(),
        })
    }

    /// Diagnostic : pourquoi VA-API n'a pas pu être ouvert alors que [`ModePref::Auto`] était
    /// demandé (périphérique absent, décodeur matériel refusé). `None` si VA-API est ouvert ou
    /// n'a pas été demandé. Un profil non pris en charge n'apparaît pas ici : il se lit sur
    /// [`Decoder::mode`].
    #[must_use]
    pub fn hw_unavailable(&self) -> Option<&str> {
        self.hw_unavailable.as_deref()
    }

    /// Diagnostic : nombre de graphes de filtres VA-API (VPP) en cache, 4 au plus.
    #[must_use]
    pub fn gpu_graphs(&self) -> usize {
        self.gpu.graphs()
    }

    /// Force le chemin de conversion des images VA-API : [`ConvertPath::VaapiMapped`] (projection
    /// et conversion CPU, comme sans VPP) ou [`ConvertPath::VaapiTransfer`] (transfert complet
    /// puis conversion CPU). Réservé aux tests et aux bancs : par défaut, le chemin est détecté à
    /// l'ouverture. Tout autre chemin est ignoré.
    pub fn force_vaapi_path(&mut self, path: ConvertPath) {
        if matches!(path, ConvertPath::VaapiMapped | ConvertPath::VaapiTransfer) {
            self.vaapi_path = path;
        }
    }

    /// Chemin de conversion des images VA-API : [`ConvertPath::VaapiVpp`] si le périphérique
    /// offre la VPP (détecté à l'ouverture), sinon [`ConvertPath::VaapiMapped`], ou le chemin
    /// forcé par [`FfmpegDecoder::force_vaapi_path`].
    #[must_use]
    pub fn vaapi_path(&self) -> ConvertPath {
        self.vaapi_path
    }

    /// Conversion d'une image VA-API sans VPP : projection en lecture (ou, si elle échoue, ou
    /// si [`ConvertPath::VaapiTransfer`] est forcé, transfert complet), puis réduction ou
    /// recadrage par le CPU. La surface entière passe en RAM : compté comme transfert pleine
    /// résolution.
    fn convert_without_vpp(
        &mut self,
        frame: &AVFrame,
        vp: &Viewport,
    ) -> Result<(Planes, ConvertPath), DecodeError> {
        let mapped = if self.vaapi_path == ConvertPath::VaapiMapped {
            let mapped = hw::map_read(frame).ok();
            if mapped.is_none() {
                // Projection refusée par le pilote : inutile de la retenter à chaque image.
                self.vaapi_path = ConvertPath::VaapiTransfer;
            }
            mapped
        } else {
            None
        };
        let (ram, path) = if let Some(m) = mapped {
            (m, ConvertPath::VaapiMapped)
        } else {
            let mut sw = AVFrame::new();
            sw.hwframe_transfer_data(frame)
                .map_err(|e| Self::gpu(format!("transfert GPU → RAM : {e}")))?;
            (sw, ConvertPath::VaapiTransfer)
        };
        self.stats.full_res_transfers += 1;
        Ok((self.cpu.scale(&ram, vp)?, path))
    }

    /// Le décodage est matériel, ou peut l'être (aucune image encore vue).
    fn hardware(&self) -> bool {
        self.hw_device && self.mode != DecodeMode::Software
    }

    fn error(&self, op: &str, e: &RsmpegError) -> DecodeError {
        DecodeError {
            kind: classify(e.raw_error(), self.hardware()),
            detail: format!("{op} : {e}"),
        }
    }

    fn gpu(detail: String) -> DecodeError {
        DecodeError {
            kind: DecodeErrorKind::Gpu,
            detail,
        }
    }

    /// Traite une image sortie du décodeur : synchronisation VA-API, puis conversion si `want`
    /// la demande.
    fn output(
        &mut self,
        frame: &AVFrame,
        want: &dyn Fn(i64) -> Option<Viewport>,
    ) -> Result<Received, DecodeError> {
        let pts = frame.best_effort_timestamp;
        let vaapi = frame.format == ffi::AV_PIX_FMT_VAAPI;
        self.mode = if vaapi {
            DecodeMode::Hardware
        } else {
            DecodeMode::Software
        };
        if vaapi {
            hw::sync(frame).map_err(Self::gpu)?;
        }
        let Some(vp) = want(pts) else {
            return Ok(Received::Discarded { pts });
        };
        let full = Size {
            w: u32::try_from(frame.width).unwrap_or(0),
            h: u32::try_from(frame.height).unwrap_or(0),
        };
        let vp = vp.normalized(full).map_err(|e| DecodeError {
            kind: DecodeErrorKind::Unsupported,
            detail: format!(
                "viewport : {e} ({vp:?} pour une image {}x{})",
                full.w, full.h
            ),
        })?;
        // Réduction (8 bits) ou région 1:1 (profondeur native) : seule l'image affichée est
        // transférée en RAM.
        let (image, path) = if !vaapi {
            (self.cpu.scale(frame, &vp)?, ConvertPath::Software)
        } else if self.vaapi_path == ConvertPath::VaapiVpp {
            let image = self.gpu.scale(frame, &vp);
            self.stats.full_res_transfers += std::mem::take(&mut self.gpu.full_res_transfers);
            (image?, ConvertPath::VaapiVpp)
        } else {
            self.convert_without_vpp(frame, &vp)?
        };
        self.stats.converted += 1;
        self.stats.path = path;
        Ok(Received::Frame {
            pts,
            image,
            corrupt: is_corrupt(frame.decode_error_flags, frame.flags),
            full,
        })
    }
}

impl Decoder for FfmpegDecoder {
    fn mode(&self) -> DecodeMode {
        self.mode
    }

    fn latency(&self) -> u32 {
        self.threads
    }

    fn send(&mut self, pts: i64, au: &[u8]) -> Result<(), DecodeError> {
        let pkt = packet::packet(pts, au)?;
        match self.ctx.send_packet(Some(&pkt)) {
            Ok(()) => Ok(()),
            // Contrat : `receive` jusqu'à `NeedInput` avant chaque `send`, et pas de `send` après
            // `send_eof` sans `reset`.
            Err(e @ (RsmpegError::DecoderFullError | RsmpegError::DecoderFlushedError)) => {
                Err(DecodeError {
                    kind: DecodeErrorKind::Internal,
                    detail: format!("send_packet hors contrat : {e}"),
                })
            }
            Err(e) => Err(self.error("send_packet", &e)),
        }
    }

    fn send_eof(&mut self) -> Result<(), DecodeError> {
        match self.ctx.send_packet(None) {
            Ok(()) | Err(RsmpegError::DecoderFlushedError) => Ok(()),
            Err(e) => Err(self.error("send_packet (fin de flux)", &e)),
        }
    }

    fn receive(&mut self, want: &dyn Fn(i64) -> Option<Viewport>) -> Result<Received, DecodeError> {
        match self.ctx.receive_frame() {
            Ok(frame) => self.output(&frame, want),
            Err(RsmpegError::DecoderDrainError) => Ok(Received::NeedInput),
            Err(RsmpegError::DecoderFlushedError) => Ok(Received::Drained),
            Err(e) => Err(self.error("receive_frame", &e)),
        }
    }

    fn reset(&mut self) {
        self.ctx.flush_buffers();
    }

    fn stats(&self) -> DecoderStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified() {
        let invalid = Some(ffi::AVERROR_INVALIDDATA);
        let other = Some(ffi::AVERROR(ffi::EIO));
        assert_eq!(classify(invalid, false), DecodeErrorKind::InvalidData);
        assert_eq!(classify(invalid, true), DecodeErrorKind::InvalidData);
        assert_eq!(classify(other, true), DecodeErrorKind::Gpu);
        assert_eq!(classify(None, true), DecodeErrorKind::Gpu);
        assert_eq!(classify(other, false), DecodeErrorKind::Internal);
        assert_eq!(classify(None, false), DecodeErrorKind::Internal);
    }

    #[test]
    fn corrupt_flags() {
        assert!(!is_corrupt(0, 0));
        #[allow(clippy::cast_possible_wrap)]
        let concealed = ffi::FF_DECODE_ERROR_CONCEALMENT_ACTIVE as c_int;
        assert!(is_corrupt(concealed, 0));
        #[allow(clippy::cast_possible_wrap)]
        let corrupt = ffi::AV_FRAME_FLAG_CORRUPT as c_int;
        assert!(is_corrupt(0, corrupt));
        // Un autre drapeau d'image (clé) ne compte pas.
        assert!(!is_corrupt(0, 2));
    }

    #[test]
    fn hardware_flag_follows_the_detected_mode() {
        let dec = AVCodec::find_decoder(ffi::AV_CODEC_ID_H264).unwrap();
        let mut d = FfmpegDecoder::open(&dec, None).unwrap();
        assert!(!d.hardware());
        d.hw_device = true;
        assert!(d.hardware(), "mode inconnu : peut-être matériel");
        d.mode = DecodeMode::Software;
        assert!(!d.hardware(), "repli silencieux de FFmpeg détecté");
        let e = d.error(
            "op",
            &RsmpegError::ReceiveFrameError(ffi::AVERROR(ffi::EIO)),
        );
        assert_eq!(e.kind, DecodeErrorKind::Internal);
        assert!(e.detail.starts_with("op : "));
    }

    #[test]
    fn send_after_eof_breaks_the_contract() {
        let dec = AVCodec::find_decoder(ffi::AV_CODEC_ID_H264).unwrap();
        let mut d = FfmpegDecoder::open(&dec, None).unwrap();
        d.send_eof().unwrap();
        let err = d.send(0, &[0, 0, 1, 0x65, 0x88]).unwrap_err();
        assert_eq!(err.kind, DecodeErrorKind::Internal);
        assert!(err.detail.contains("hors contrat"), "{err}");
    }
}
