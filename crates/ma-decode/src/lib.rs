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
    DecodeError, DecodeErrorKind, DecodeEvent, DecodeMode, Decoder, DecoderFactory, DecoderStats,
    ModePref, Received,
};
pub use navigator::{Direction, FrameError, NavConfig, NavStats, Navigator};
pub use plan::{NotDecodable, start_for};
pub use service::{FrameService, PrefetchConfig, Readiness};
pub use types::{
    AuSource, Codec, DecodedFrame, FrameEntry, Leading, PictureType, PixelLayout, Planes, RapKind,
    Rect, Size, Viewport, ViewportError,
};
