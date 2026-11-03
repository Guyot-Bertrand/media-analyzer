//! Contrat de décodage de Media Analyzer : types d'images, viewport, source d'unités d'accès
//! et traits `Decoder` / `DecoderFactory`. Aucune dépendance `FFmpeg`, aucun `unsafe`.

mod blocks;
mod decoder;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
mod navigator;
mod plan;
mod service;
mod types;

pub use blocks::{BlockAttr, BlockKind, BlockNode, BlockTree};
pub use decoder::{
    ConvertPath, DecodeError, DecodeErrorKind, DecodeEvent, DecodeMode, Decoder, DecoderFactory,
    DecoderStats, ModePref, Received,
};
pub use ma_core::{AuSource, Codec, FrameEntry, Leading, PictureType, RapKind};
pub use navigator::{Direction, FrameError, NavConfig, NavStats, Navigator};
pub use plan::{NotDecodable, start_for};
pub use service::{FrameService, PrefetchConfig, Readiness};
pub use types::{DecodedFrame, PixelLayout, Planes, Rect, Size, Viewport, ViewportError};

#[cfg(test)]
mod tests {
    #[test]
    fn core_types_are_reexported() {
        let e: ma_core::FrameEntry = crate::FrameEntry {
            decode_idx: 0,
            pres_idx: 0,
            pts: 0,
            picture: crate::PictureType::I,
            rap: crate::RapKind::Idr,
            leading: crate::Leading::None,
        };
        assert_eq!(e.picture, ma_core::PictureType::I);
        let _: ma_core::Codec = crate::Codec::H264;
    }
}
