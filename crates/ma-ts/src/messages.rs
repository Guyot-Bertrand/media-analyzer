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

/// Tous les messages de la crate (vérifiés contre `locales/{fr,en}/engine.ftl`).
pub const MESSAGES: &[MsgDef] = &[RESYNC, TRUNCATED_PACKET, ADAPTATION_LENGTH];

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
