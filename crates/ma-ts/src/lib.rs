//! Démultiplexeur MPEG-2 TS maison (le moteur n'utilise jamais `FFmpeg` pour démultiplexer) :
//! paquets de 188, 192 (TTS), 204 et 208 octets, détection du format, lecture séquentielle par
//! blocs avec resynchronisation, messages identifiés par une clé Fluent.

mod messages;
mod packet;
mod scan;
#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

pub use messages::MESSAGES;
pub use packet::{
    AdaptationField, Packet, PacketError, PacketFormat, PacketHeader, detect_format, parse_packet,
};
pub use scan::{OpenError, PacketScanner, ScannedPacket, probe};
