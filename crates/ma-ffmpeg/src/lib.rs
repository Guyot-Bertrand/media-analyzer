//! Liaison `FFmpeg` de Media Analyzer : implémentation du trait [`ma_decode::Decoder`] sur
//! libavcodec (via rsmpeg), en logiciel et en VA-API.
//!
//! C'est la **seule** crate du projet où `unsafe` est permis (voir `Cargo.toml`) : chaque bloc
//! porte un commentaire `// SAFETY:`. `FFmpeg` ne démultiplexe pas : il reçoit les unités d'accès
//! (AU) de la source, une par paquet.
//!
//! - [`FfmpegFactory`] ouvre un [`FfmpegDecoder`] ; en [`ma_decode::ModePref::Auto`], il essaie
//!   VA-API et laisse `FFmpeg` repasser silencieusement en logiciel si le profil n'est pas pris en
//!   charge : le mode réel est déduit du format des images produites.
//! - [`vaapi_available`] indique si un périphérique VA-API peut être ouvert, [`vaapi_has_vpp`]
//!   s'il offre aussi le traitement vidéo (VPP).

#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

mod cpu_scale;
mod decoder;
mod gpu_scale;
mod hw;
mod packet;
mod planes;
mod refusal;

pub use decoder::{FfmpegDecoder, FfmpegFactory};
pub use hw::{vaapi_available, vaapi_has_vpp};
pub use packet::MAX_AU_BYTES;
