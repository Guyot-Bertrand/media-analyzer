//! Types partagés du moteur de Media Analyzer : codec et entrées d'index (`media`), horodatages
//! et déroulement des compteurs 33 bits (`time`), messages identifiés par une clé Fluent
//! (`message`). Aucune dépendance, aucun `unsafe`, aucun texte en langue naturelle.

mod media;
mod message;
mod time;

pub use media::{AuSource, Codec, FrameEntry, Leading, PictureType, RapKind};
pub use message::{Family, Message, MessageSink, MsgDef, Param, Severity, VecSink};
pub use time::{PCR_HZ, PTS_HZ, Pcr, TsUnwrapper};
