//! Outils de test et de banc de Media Analyzer.
//!
//! - [`read_frame_code`] : lecture du numéro d'image incrusté dans le corpus.
//! - [`corpus_file`], [`manifest`], [`require_vaapi`] : accès au corpus et garde des tests GPU.
//! - [`check_catalog`] : cohérence des catalogues Fluent avec les messages déclarés.
//! - [`CorruptingSource`] : source qui corrompt des AU choisies.
//! - Oracles `ffprobe`, `ffmpeg` et `TSDuck` ([`ffprobe_packets`], [`ffmpeg_es`]…).
//!
//! La source d'AU sur TS est celle du moteur, dans `ma-ts` ; la source de test « 1 PES = 1 AU »
//! de M1 est retirée.

mod catalog;
mod corpus;
mod corrupt;
mod frame_code;
mod oracle;

pub use catalog::check_catalog;
pub use corpus::{
    CorpusEntry, corpus_dir, corpus_file, corpus_file_if_present, manifest, require_vaapi,
};
pub use corrupt::CorruptingSource;
pub use frame_code::{read_frame_code, read_frame_code_bits};
pub use oracle::{
    ProbeFrame, ProbePacket, ffmpeg_es, ffmpeg_framemd5, ffprobe_frames, ffprobe_packets,
    private_corpus, tsduck_analyze,
};
