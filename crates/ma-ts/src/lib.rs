//! Démultiplexeur MPEG-2 TS maison (le moteur n'utilise jamais `FFmpeg` pour démultiplexer) :
//! paquets de 188, 192 (TTS), 204 et 208 octets, détection du format, lecture séquentielle par
//! blocs avec resynchronisation, messages identifiés par une clé Fluent.

mod messages;
mod packet;
mod psi;
mod scan;
mod structure;
mod tables;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

pub use messages::MESSAGES;
pub use packet::{
    AdaptationField, Packet, PacketError, PacketFormat, PacketHeader, detect_format, parse_packet,
};
pub use psi::{Section, SectionAssembler, crc32_mpeg2};
pub use scan::{OpenError, PacketScanner, ScannedPacket, probe};
pub use structure::{EsInfo, Program, StructureBuilder, TsStructure};
pub use tables::{
    AudioKind, Descriptor, Pat, Pmt, PmtStream, Sdt, SdtService, StreamKind, dvb_string, parse_pat,
    parse_pmt, parse_sdt, stream_kind,
};
