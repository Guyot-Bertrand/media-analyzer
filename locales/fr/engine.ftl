# Messages du moteur (identifiants Fluent) — catalogue fr.
# Aucune clé tant qu'aucune crate ne déclare de messages ; chaque crate ajoute ses clés avec
# les mêmes variables dans fr et en (vérifié par check_catalog de ma-testkit).

# Couche paquets TS (ma-ts)
ts-resync = Synchronisation TS perdue : { $skipped } octets ignorés avant de retrouver les paquets.
ts-truncated-packet = Dernier paquet TS incomplet : { $bytes } octets présents.
ts-adaptation-length = Champ d'adaptation invalide (longueur { $length }) : paquet ignoré.
