# Messages du moteur (identifiants Fluent) — catalogue en.
# Aucune clé tant qu'aucune crate ne déclare de messages ; chaque crate ajoute ses clés avec
# les mêmes variables dans fr et en (vérifié par check_catalog de ma-testkit).

# TS packet layer (ma-ts)
ts-resync = TS synchronization lost: { $skipped } bytes skipped before packets were found again.
ts-truncated-packet = Incomplete last TS packet: { $bytes } bytes present.
ts-adaptation-length = Invalid adaptation field (length { $length }): packet skipped.
