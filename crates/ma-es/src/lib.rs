//! Découpage d'un flux élémentaire vidéo en unités d'accès (AU) et classification.
//!
//! Le découpeur ([`framer`]) reçoit les octets d'un flux élémentaire (Annex B pour H.264 et
//! HEVC, codes de début MPEG-2) par morceaux de taille quelconque et rend les frontières des AU,
//! en offsets dans le flux poussé, avec leurs métadonnées : type d'image, point d'accès aléatoire,
//! image de tête, champ, paramètres présents. Il ne connaît pas le conteneur.
//!
//! La mémoire est bornée : le découpeur ne garde jamais l'AU entière, seulement l'état des codes
//! de début, les premiers octets de chaque NAL ([`MAX_HEADER_BYTES`] au plus, 64 pour une
//! tranche) et les jeux de paramètres déjà vus, rangés par identifiant.

mod framer;
mod h264;
mod hevc;
mod mpeg2;
mod startcode;

pub use ma_core::{Codec, PictureType};

/// Octets gardés au plus pour un NAL non VCL (ou un code de début MPEG-2 hors tranche).
pub const MAX_HEADER_BYTES: usize = 4096;

/// Octets gardés au plus pour une tranche : l'en-tête utile tient largement dedans.
pub(crate) const MAX_SLICE_HEADER_BYTES: usize = 64;

/// Unités de paramètres présentes dans une AU (ensemble de bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ParamSet(u8);

impl ParamSet {
    pub const EMPTY: ParamSet = ParamSet(0);
    /// HEVC : NAL 32.
    pub const VPS: ParamSet = ParamSet(1);
    /// H.264 : NAL 7 ; HEVC : NAL 33.
    pub const SPS: ParamSet = ParamSet(1 << 1);
    /// H.264 : NAL 8 ; HEVC : NAL 34.
    pub const PPS: ParamSet = ParamSet(1 << 2);
    /// H.264 : NAL 6 ; HEVC : NAL 39 et 40.
    pub const SEI: ParamSet = ParamSet(1 << 3);
    /// Délimiteur d'AU (H.264 : NAL 9 ; HEVC : NAL 35).
    pub const AUD: ParamSet = ParamSet(1 << 4);
    /// MPEG-2 : `sequence_header` (`B3`).
    pub const SEQ_HEADER: ParamSet = ParamSet(1 << 5);
    /// MPEG-2 : `sequence_extension` (`B5`, identifiant 1).
    pub const SEQ_EXT: ParamSet = ParamSet(1 << 6);
    /// MPEG-2 : `group_of_pictures_header` (`B8`).
    pub const GOP_HEADER: ParamSet = ParamSet(1 << 7);

    /// Vrai si toutes les unités de `p` sont présentes.
    #[must_use]
    pub fn contains(self, p: ParamSet) -> bool {
        self.0 & p.0 == p.0
    }

    /// Ajoute les unités de `p`.
    pub fn insert(&mut self, p: ParamSet) {
        self.0 |= p.0;
    }

    /// Étiquettes à afficher (NAV.THUMB.02) : `VPS`, `SPS`, `PPS`, `SEI` pour H.264 et HEVC,
    /// `SH`, `GH` pour MPEG-2, dans cet ordre. Le délimiteur d'AU et l'extension de séquence ne
    /// sont pas étiquetés.
    #[must_use]
    pub fn labels(self, codec: Codec) -> Vec<&'static str> {
        let table: &[(ParamSet, &'static str)] = match codec {
            Codec::H264 | Codec::Hevc => &[
                (Self::VPS, "VPS"),
                (Self::SPS, "SPS"),
                (Self::PPS, "PPS"),
                (Self::SEI, "SEI"),
            ],
            Codec::Mpeg2 => &[(Self::SEQ_HEADER, "SH"), (Self::GOP_HEADER, "GH")],
        };
        table
            .iter()
            .filter(|(p, _)| self.contains(*p))
            .map(|&(_, l)| l)
            .collect()
    }
}

/// Structure de l'image portée par l'AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Field {
    /// Image entière (y compris MBAFF), ou structure inconnue.
    #[default]
    Frame,
    Top,
    Bottom,
}

/// Classe de point d'accès aléatoire lue dans l'AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RapClass {
    #[default]
    None,
    /// H.264 : NAL 5 ; HEVC : NAL 19 et 20.
    Idr,
    /// HEVC : NAL 21.
    Cra,
    /// HEVC : NAL 16 à 18.
    Bla,
    /// H.264 : image I non IDR précédée d'un SEI « recovery point » (valeur brute de
    /// `recovery_frame_cnt`).
    Recovery { frame_cnt: u32 },
    /// MPEG-2 : image I précédée, dans la même AU, d'un en-tête de séquence ou de GOP.
    Intra { closed_gop: bool, broken_link: bool },
}

/// Image de tête lisible dans le type de NAL (HEVC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NalLeading {
    /// RASL (NAL 8 et 9).
    Skippable,
    /// RADL (NAL 6 et 7).
    Decodable,
}

/// Métadonnées d'une AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuMeta {
    /// Type de la 1re tranche (P si inconnu, I pour une IRAP HEVC dont le PPS est inconnu).
    pub picture: PictureType,
    pub rap: RapClass,
    /// HEVC seulement.
    pub leading: Option<NalLeading>,
    /// H.264 : `nal_ref_idc != 0` ; HEVC : pas une sous-couche non référence ; MPEG-2 : I ou P.
    ///
    /// HEVC : une image `_N` (`TRAIL_N`, `RASL_N`…) n'est non référence que pour sa propre
    /// sous-couche ; si son `TemporalId` est inférieur au maximum, elle peut servir de référence
    /// aux sous-couches supérieures. Ce champ est informatif : il ne doit pas servir à sauter des
    /// images au décodage.
    pub reference: bool,
    pub params: ParamSet,
    pub field: Field,
    /// H.264 seulement, si le SPS et le PPS de la 1re tranche sont connus.
    pub frame_num: Option<u32>,
    /// Type de NAL de la 1re tranche (H.264, HEVC) ; 0 pour MPEG-2 ou sans tranche.
    pub first_vcl_type: u8,
}

impl Default for AuMeta {
    fn default() -> Self {
        Self {
            picture: PictureType::P,
            rap: RapClass::None,
            leading: None,
            reference: false,
            params: ParamSet::EMPTY,
            field: Field::Frame,
            frame_num: None,
            first_vcl_type: 0,
        }
    }
}

/// Frontières d'une AU, en octets depuis le premier `push` ; `end` est exclusif. Une AU s'étend
/// jusqu'au début de la suivante (octets nuls de fin compris).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuBoundary {
    pub start: u64,
    pub end: u64,
    pub meta: AuMeta,
}

/// Découpeur d'AU en flux.
pub trait AuFramer: Send {
    /// Pousse les octets suivants du flux ; les AU terminées sont ajoutées à `out`. Une AU est
    /// émise quand le premier NAL de la suivante est complet (au code de début d'après).
    fn push(&mut self, data: &[u8], out: &mut Vec<AuBoundary>);
    /// Fin du flux : émet la dernière AU.
    fn finish(&mut self, out: &mut Vec<AuBoundary>);
}

/// Découpeur pour `codec`.
#[must_use]
pub fn framer(codec: Codec) -> Box<dyn AuFramer> {
    match codec {
        Codec::H264 => Box::new(framer::Framer::new(h264::H264::default())),
        Codec::Hevc => Box::new(framer::Framer::new(hevc::Hevc::default())),
        Codec::Mpeg2 => Box::new(framer::Framer::new(mpeg2::Mpeg2::default())),
    }
}

/// Position du premier octet après l'en-tête de NAL de la première tranche de l'AU (NAL H.264
/// 1 ou 5, NAL HEVC < 32, code de tranche MPEG-2 `01` à `AF`), ou `None` sans tranche.
#[must_use]
pub fn first_slice_payload_offset(codec: Codec, au: &[u8]) -> Option<usize> {
    startcode::first_slice_payload_offset(codec, au)
}
