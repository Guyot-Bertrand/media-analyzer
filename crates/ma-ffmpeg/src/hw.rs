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
    // `AVHWFramesContext` (alloué par `av_mallocz`, donc aligné). Son `device_ctx` est
    // l'`AVHWDeviceContext` VA-API, dont `hwctx` pointe sur un `AVVAAPIDeviceContext`. Les deux
    // pointeurs sont vérifiés non nuls avant d'être déréférencés.
    #[allow(clippy::cast_ptr_alignment)]
    let hwctx = unsafe {
        let frames = (*frame.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>();
        let device = if frames.is_null() {
            std::ptr::null_mut()
        } else {
            (*frames).device_ctx
        };
        if device.is_null() {
            std::ptr::null_mut()
        } else {
            (*device).hwctx
        }
    };
    if hwctx.is_null() {
        return Err("contexte de périphérique VA-API absent".into());
    }
    #[allow(clippy::cast_possible_truncation)]
    let surface = frame.data[3] as usize as u32;
    // SAFETY: `hwctx` (non nul, vérifié) pointe sur un `AVVAAPIDeviceContext` vivant, dont le
    // premier champ est le `VADisplay` (hwcontext_vaapi.h, stable de FFmpeg 4 à 8). Pour une image
    // VA-API, `data[3]` contient le `VASurfaceID` (entier 32 bits stocké dans un pointeur) ;
    // `vaSyncSurface` ne fait qu'attendre la fin du décodage de cette surface.
    let status = unsafe { vaSyncSurface(*hwctx.cast::<*mut c_void>(), surface) };
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
