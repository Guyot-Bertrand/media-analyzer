//! Types de base partagés : codec, entrées d'index et source d'unités d'accès.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Mpeg2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PictureType {
    I,
    P,
    B,
}

/// Point d'accès aléatoire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RapKind {
    None,
    Idr,
    Cra,
    Bla,
    Recovery { frame_cnt: u32 },
    Intra { closed_gop: bool },
}

/// Image précédant son RAP en ordre de présentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leading {
    None,
    /// RASL ou équivalent : dépend du RAP précédent.
    Skippable,
    /// RADL : décodable seule.
    Decodable,
}

/// Entrée d'index d'une image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameEntry {
    pub decode_idx: u32,
    pub pres_idx: u32,
    pub pts: i64,
    pub picture: PictureType,
    pub rap: RapKind,
    pub leading: Leading,
}

/// Source d'unités d'accès (une AU par image), indexée en ordre de décodage.
pub trait AuSource: Send + Sync {
    fn codec(&self) -> Codec;
    /// Nombre d'images (ordre de décodage).
    fn len(&self) -> u32;
    fn entry(&self, decode_idx: u32) -> Option<FrameEntry>;
    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32>;
    /// Remplace le contenu de `buf` par l'AU demandée.
    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> std::io::Result<()>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
