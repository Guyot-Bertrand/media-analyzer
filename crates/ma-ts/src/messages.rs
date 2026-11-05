//! Messages émis par la couche paquets (identifiants Fluent, jamais de phrase).

use ma_core::MsgDef;

/// Octets parasites ignorés avant de retrouver la synchronisation.
pub(crate) const RESYNC: MsgDef = MsgDef {
    id: "ts-resync",
    params: &["skipped"],
};
/// Dernier paquet incomplet en fin de données.
pub(crate) const TRUNCATED_PACKET: MsgDef = MsgDef {
    id: "ts-truncated-packet",
    params: &["bytes"],
};
/// Longueur du champ d'adaptation supérieure à la place disponible (paquet ignoré).
pub(crate) const ADAPTATION_LENGTH: MsgDef = MsgDef {
    id: "ts-adaptation-length",
    params: &["length"],
};

/// Section PSI/SI abandonnée avant sa fin (rupture de compteur, nouveau début de section).
pub(crate) const PSI_INCOMPLETE: MsgDef = MsgDef {
    id: "ts-psi-incomplete",
    params: &["bytes"],
};
/// Section dont le CRC-32 est faux (ignorée).
pub(crate) const PSI_CRC: MsgDef = MsgDef {
    id: "ts-psi-crc",
    params: &["table_id"],
};
/// PMT d'un programme remplacée par un contenu différent (nouvelle version).
pub(crate) const PMT_CHANGED: MsgDef = MsgDef {
    id: "ts-pmt-changed",
    params: &["program", "version"],
};
/// `section_length` supérieure à la limite de la table : section abandonnée.
pub(crate) const PSI_TOO_LONG: MsgDef = MsgDef {
    id: "ts-psi-too-long",
    params: &["length"],
};
/// Section dont la structure est invalide (en-tête, `pointer_field` ou contenu de table).
pub(crate) const PSI_INVALID: MsgDef = MsgDef {
    id: "ts-psi-invalid",
    params: &["table_id"],
};

/// Rupture du compteur de continuité d'un PID : paquets perdus ou dupliqués plus d'une fois.
pub(crate) const CC_GAP: MsgDef = MsgDef {
    id: "ts-cc-gap",
    params: &["pid", "expected", "got"],
};
/// Paquets brouillés (`transport_scrambling_control` non nul) : charge ignorée, une fois par PID.
pub(crate) const SCRAMBLED: MsgDef = MsgDef {
    id: "ts-scrambled",
    params: &["pid"],
};
/// En-tête PES invalide ou abandonné : le PES est ignoré jusqu'au prochain début de PES.
pub(crate) const PES_INVALID: MsgDef = MsgDef {
    id: "ts-pes-invalid",
    params: &["pid"],
};

/// AU vidéo sans PTS (aucun début de PES ne lui en donne) : PTS estimé.
pub(crate) const AU_NO_PTS: MsgDef = MsgDef {
    id: "ts-au-no-pts",
    params: &["pid"],
};
/// AU vidéo plus grande que `MAX_AU_BYTES` : ignorée, absente de l'index.
pub(crate) const AU_TOO_LARGE: MsgDef = MsgDef {
    id: "ts-au-too-large",
    params: &["pid", "size"],
};

/// Discontinuité de PTS (recul d'un RAP ou `discontinuity_indicator` du PCR) : nouveau
/// segment de présentation.
pub(crate) const PTS_DISCONTINUITY: MsgDef = MsgDef {
    id: "ts-pts-discontinuity",
    params: &["pid"],
};
/// Trop d'images en attente de publication (pas de RAP, point de reprise jamais résolu) :
/// publication forcée de `count` images.
pub(crate) const FORCED_PUBLISH: MsgDef = MsgDef {
    id: "ts-index-forced-publish",
    params: &["pid", "count"],
};

/// Tous les messages de la crate (vérifiés contre `locales/{fr,en}/engine.ftl`).
pub const MESSAGES: &[MsgDef] = &[
    RESYNC,
    TRUNCATED_PACKET,
    ADAPTATION_LENGTH,
    PSI_INCOMPLETE,
    PSI_CRC,
    PMT_CHANGED,
    PSI_TOO_LONG,
    PSI_INVALID,
    CC_GAP,
    SCRAMBLED,
    PES_INVALID,
    AU_NO_PTS,
    AU_TOO_LARGE,
    PTS_DISCONTINUITY,
    FORCED_PUBLISH,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn catalog_is_complete() {
        let locales = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../locales");
        let issues = ma_testkit::check_catalog(MESSAGES, &locales);
        assert!(issues.is_empty(), "{issues:?}");
    }
}
