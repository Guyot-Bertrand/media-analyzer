//! VA-API : ouverture du périphérique et synchronisation des surfaces décodées.

use std::ffi::{CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rsmpeg::avutil::{AVFrame, AVHWDeviceContext};
use rsmpeg::ffi;

#[link(name = "va")]
unsafe extern "C" {
    /// `VAStatus vaSyncSurface(VADisplay dpy, VASurfaceID render_target)` (libva).
    fn vaSyncSurface(dpy: *mut c_void, surface: u32) -> i32;
    /// `int vaMaxNumEntrypoints(VADisplay dpy)` (libva).
    fn vaMaxNumEntrypoints(dpy: *mut c_void) -> i32;
    /// `VAStatus vaQueryConfigEntrypoints(VADisplay dpy, VAProfile profile,
    /// VAEntrypoint *entrypoint_list, int *num_entrypoints)` (libva).
    fn vaQueryConfigEntrypoints(
        dpy: *mut c_void,
        profile: i32,
        entrypoints: *mut i32,
        num: *mut i32,
    ) -> i32;
}

/// `VAProfileNone` (va.h).
const VA_PROFILE_NONE: i32 = -1;
/// `VAEntrypointVideoProc` (va.h).
const VA_ENTRYPOINT_VIDEO_PROC: i32 = 10;

/// `VADisplay` du périphérique VA-API `device` (`AVHWDeviceContext`), ou nul.
///
/// # Safety
/// `device` est nul ou pointe sur un `AVHWDeviceContext` VA-API vivant.
unsafe fn display_of(device: *const ffi::AVHWDeviceContext) -> *mut c_void {
    if device.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `device` (non nul) est un `AVHWDeviceContext` VA-API vivant (contrat de la
    // fonction) : son `hwctx` est nul ou pointe sur un `AVVAAPIDeviceContext`, dont le premier
    // champ est le `VADisplay` (hwcontext_vaapi.h, stable de FFmpeg 4 à 8).
    unsafe {
        let hwctx = (*device).hwctx;
        if hwctx.is_null() {
            std::ptr::null_mut()
        } else {
            *hwctx.cast::<*mut c_void>()
        }
    }
}

/// Vrai si le périphérique VA-API `device` offre le traitement vidéo (VPP,
/// `VAEntrypointVideoProc`), nécessaire à `scale_vaapi`. Le pilote iHD libre d'Ubuntu
/// (`intel-media-va-driver`) ne l'a pas ; la version non libre l'a.
pub(crate) fn has_vpp(device: &AVHWDeviceContext) -> bool {
    // SAFETY: `device.data` d'un contexte de périphérique VA-API créé par
    // `av_hwdevice_ctx_create` est un `AVHWDeviceContext` (alloué par `av_mallocz`, donc aligné)
    // vivant tant que `device` l'est.
    #[allow(clippy::cast_ptr_alignment)]
    let dpy = unsafe { display_of(device.data.cast::<ffi::AVHWDeviceContext>()) };
    if dpy.is_null() {
        return false;
    }
    // SAFETY: `dpy` est le `VADisplay` initialisé du périphérique (vivant) ; la liste reçoit au
    // plus `vaMaxNumEntrypoints` éléments, sa taille.
    unsafe {
        let max = vaMaxNumEntrypoints(dpy).max(0);
        let mut list = vec![0i32; usize::try_from(max).unwrap_or(0)];
        let mut num = 0;
        let status =
            vaQueryConfigEntrypoints(dpy, VA_PROFILE_NONE, list.as_mut_ptr(), &raw mut num);
        status == 0
            && list
                .iter()
                .take(usize::try_from(num).unwrap_or(0))
                .any(|&e| e == VA_ENTRYPOINT_VIDEO_PROC)
    }
}

/// Projection en lecture de l'image VA-API `frame` en mémoire (`av_hwframe_map`), sans copie
/// par l'appelant : accès direct à la surface si le pilote le permet, sinon image intermédiaire du
/// pilote. Rend la cause en cas d'échec.
pub(crate) fn map_read(frame: &AVFrame) -> Result<AVFrame, String> {
    let mut last = 0;
    #[allow(clippy::cast_possible_wrap)]
    for flags in [
        ffi::AV_HWFRAME_MAP_READ | ffi::AV_HWFRAME_MAP_DIRECT,
        ffi::AV_HWFRAME_MAP_READ,
    ] {
        let mut dst = AVFrame::new();
        // SAFETY: `dst` est une image vide possédée ici, `frame` une image VA-API vivante ;
        // `av_hwframe_map` remplit `dst` d'une projection qui garde une référence sur `frame`.
        last = unsafe { ffi::av_hwframe_map(dst.as_mut_ptr(), frame.as_ptr(), flags as i32) };
        if last == 0 {
            return Ok(dst);
        }
    }
    Err(format!("av_hwframe_map : code {last}"))
}

/// Ouvre un périphérique VA-API : le nœud DRM `render_node`, ou celui que `FFmpeg` choisit par
/// défaut. En cas d'échec, rend la cause.
pub(crate) fn vaapi_device(render_node: Option<&Path>) -> Result<AVHWDeviceContext, String> {
    let node = match render_node {
        Some(p) => Some(
            CString::new(p.as_os_str().as_bytes())
                .map_err(|_| format!("nœud VA-API invalide : {}", p.display()))?,
        ),
        None => None,
    };
    AVHWDeviceContext::create(ffi::AV_HWDEVICE_TYPE_VAAPI, node.as_deref(), None, 0).map_err(|e| {
        let node = render_node.map_or("(par défaut)".into(), |p| p.display().to_string());
        format!("ouverture du périphérique VA-API {node} : {e}")
    })
}

/// Vrai si un périphérique VA-API peut être ouvert (`render_node`, ou le choix par défaut de
/// `FFmpeg`). Ne dit rien des profils pris en charge : le mode réel se lit sur les images.
#[must_use]
pub fn vaapi_available(render_node: Option<&Path>) -> bool {
    vaapi_device(render_node).is_ok()
}

/// Attend la fin du décodage GPU de l'image VA-API `frame`, sans la copier.
///
/// À faire pour **chaque** image sortie, gardée ou jetée : sans cela, la surface d'une image jetée
/// peut être réutilisée pendant que le GPU y décode encore, et un `vaSyncSurface` ultérieur échoue
/// (« Failed to sync surface », défaut n° 4 du spike).
pub(crate) fn sync(frame: &AVFrame) -> Result<(), String> {
    if frame.format != ffi::AV_PIX_FMT_VAAPI || frame.hw_frames_ctx.is_null() {
        return Err("image VA-API sans contexte de surfaces".into());
    }
    // SAFETY: `frame` est une image VA-API sortie du décodeur, donc `hw_frames_ctx` (non nul,
    // vérifié) référence un `AVBufferRef` vivant tant que l'image l'est, dont `data` est un
    // `AVHWFramesContext` (alloué par `av_mallocz`, donc aligné), vérifié non nul ; son
    // `device_ctx` est nul ou l'`AVHWDeviceContext` VA-API vivant.
    #[allow(clippy::cast_ptr_alignment)]
    let dpy = unsafe {
        let frames = (*frame.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>();
        if frames.is_null() {
            std::ptr::null_mut()
        } else {
            display_of((*frames).device_ctx)
        }
    };
    if dpy.is_null() {
        return Err("contexte de périphérique VA-API absent".into());
    }
    #[allow(clippy::cast_possible_truncation)]
    let surface = frame.data[3] as usize as u32;
    // SAFETY: `dpy` (non nul, vérifié) est le `VADisplay` vivant du périphérique. Pour une image
    // VA-API, `data[3]` contient le `VASurfaceID` (entier 32 bits stocké dans un pointeur) ;
    // `vaSyncSurface` ne fait qu'attendre la fin du décodage de cette surface.
    let status = unsafe { vaSyncSurface(dpy, surface) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("vaSyncSurface : statut {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_rejects_a_software_frame() {
        let frame = AVFrame::new();
        assert!(sync(&frame).is_err());
    }
}
