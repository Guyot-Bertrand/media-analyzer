//! Outils de test et de banc de Media Analyzer.
//!
//! - [`TsAuSource`] : source d'AU sur Transport Stream, **réservée aux tests et aux bancs** (le
//!   moteur ne démultiplexe pas avec `FFmpeg`). Hypothèse « 1 PES = 1 AU », valable pour le corpus
//!   généré ; M2 la remplace par `ma-ts` derrière la même interface `AuSource`.
//! - [`classify`] : type d'image, point d'accès aléatoire et image de tête d'après les NAL.
//! - [`read_frame_code`] : lecture du numéro d'image incrusté dans le corpus.
//! - [`corpus_file`], [`manifest`], [`require_vaapi`] : accès au corpus et garde des tests GPU.
//! - [`check_catalog`] : cohérence des catalogues Fluent avec les messages déclarés.
//! - [`CorruptingSource`] : source qui corrompt des AU choisies.

mod catalog;
mod corpus;
mod corrupt;
mod frame_code;
mod nal;
mod oracle;
mod ts_source;

pub use catalog::check_catalog;
pub use corpus::{
    CorpusEntry, corpus_dir, corpus_file, corpus_file_if_present, manifest, require_vaapi,
};
pub use corrupt::CorruptingSource;
pub use frame_code::{read_frame_code, read_frame_code_bits};
pub use nal::{NalLeading, classify, first_slice_payload_offset};
pub use oracle::{
    ProbeFrame, ProbePacket, ffmpeg_es, ffmpeg_framemd5, ffprobe_frames, ffprobe_packets,
    private_corpus, tsduck_analyze,
};
pub use ts_source::{MAX_AU_BYTES, TsAuSource};
