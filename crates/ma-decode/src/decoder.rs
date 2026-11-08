//! Traits `Decoder` et `DecoderFactory`, et types associés.

use crate::Codec;
use crate::types::{Planes, Size, Viewport};

/// Mode de décodage effectif, déduit des images produites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodeMode {
    /// Matériel attaché, pas encore confirmé (aucune image sortie). Un décodeur ouvert sans
    /// périphérique est [`DecodeMode::Software`] dès l'ouverture.
    Unknown,
    Hardware,
    Software,
}

/// Préférence de mode demandée à l'ouverture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModePref {
    Auto,
    SoftwareOnly,
}

/// Catégorie d'erreur de décodage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodeErrorKind {
    InvalidData,
    Gpu,
    Unsupported,
    Device,
    Internal,
}

/// Erreur de décodage.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("erreur de décodage ({kind:?}) : {detail}")]
pub struct DecodeError {
    pub kind: DecodeErrorKind,
    pub detail: String,
}

/// Résultat d'un appel à [`Decoder::receive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// Le décodeur attend davantage de données.
    NeedInput,
    /// Le vidage (après `send_eof`) est terminé.
    Drained,
    /// Image décodée puis jetée (`want` a renvoyé `None`) : aucune conversion ni transfert.
    Discarded { pts: i64 },
    /// Image décodée et convertie ; `full` est la taille d'affichage réelle de l'image décodée.
    Frame {
        pts: i64,
        image: Planes,
        corrupt: bool,
        full: Size,
    },
}

/// Chemin de conversion (réduction, recadrage, transfert en RAM) d'une image décodée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConvertPath {
    /// Aucune image convertie.
    #[default]
    Unknown,
    /// Image logicielle, convertie en RAM.
    Software,
    /// VA-API avec traitement vidéo (VPP) : réduction et recadrage sur le GPU, seule l'image
    /// affichée est transférée en RAM.
    VaapiVpp,
    /// VA-API avec VPP, mais la VPP a refusé ce viewport : surface entière projetée en lecture
    /// (ou transférée), puis réduite ou recadrée par le CPU.
    VaapiVppRefused,
    /// VA-API sans VPP (pilote qui ne l'offre pas) : surface entière projetée en lecture
    /// (`av_hwframe_map`), réduite ou recadrée par le CPU. Mode dégradé : pas de repli logiciel.
    VaapiMapped,
    /// VA-API sans VPP ni projection : transfert complet de la surface, puis conversion CPU.
    VaapiTransfer,
}

/// Compteurs et diagnostic d'un décodeur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecoderStats {
    /// Images converties vers la sortie demandée.
    pub converted: u64,
    /// Transferts (ou projections) GPU vers RAM d'une image en pleine résolution.
    pub full_res_transfers: u64,
    /// Chemin de conversion de la dernière image convertie.
    pub path: ConvertPath,
}

/// Décodeur d'un flux d'unités d'accès.
pub trait Decoder: Send {
    /// Mode effectif, déduit des images produites.
    fn mode(&self) -> DecodeMode;
    /// Nombre d'images de retard du pipeline.
    fn latency(&self) -> u32;
    /// Repli silencieux : le décodeur a été ouvert avec un périphérique matériel mais sort des
    /// images logicielles (`FFmpeg` repasse seul en logiciel, sur un seul thread, pour un profil
    /// que le GPU refuse). Faux par défaut.
    fn silent_software(&self) -> bool {
        false
    }
    /// Soumet une unité d'accès.
    fn send(&mut self, pts: i64, au: &[u8]) -> Result<(), DecodeError>;
    /// Signale la fin du flux pour vider le pipeline.
    fn send_eof(&mut self) -> Result<(), DecodeError>;
    /// Récupère la prochaine image.
    ///
    /// `want(pts)` renvoie `Some(viewport)` pour convertir l'image (réduite ou recadrée) et `None`
    /// pour la jeter sans conversion ni transfert.
    ///
    /// Les implémentations appellent [`Viewport::normalized`] avec la taille réelle de l'image
    /// décodée, avant la conversion ; le viewport normalisé est celui qui est appliqué.
    fn receive(&mut self, want: &dyn Fn(i64) -> Option<Viewport>) -> Result<Received, DecodeError>;
    /// Vide l'état du décodeur (saut, changement de position).
    fn reset(&mut self);
    /// Compteurs courants.
    fn stats(&self) -> DecoderStats;
}

/// Fabrique de décodeurs.
pub trait DecoderFactory: Send + Sync {
    fn open(&self, codec: Codec, pref: ModePref) -> Result<Box<dyn Decoder>, DecodeError>;
}

/// Événement notable émis pendant le décodage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeEvent {
    FellBackToSoftware {
        reason: String,
        at_pres: Option<u32>,
    },
    CorruptAu {
        decode_idx: u32,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_error_displays_kind_and_detail() {
        let e = DecodeError {
            kind: DecodeErrorKind::Gpu,
            detail: "échec".into(),
        };
        assert_eq!(e.to_string(), "erreur de décodage (Gpu) : échec");
    }

    #[test]
    fn stats_default_is_zero() {
        assert_eq!(DecoderStats::default().converted, 0);
        assert_eq!(DecoderStats::default().full_res_transfers, 0);
        assert_eq!(DecoderStats::default().path, ConvertPath::Unknown);
    }
}
