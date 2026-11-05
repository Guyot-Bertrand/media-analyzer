# Messages du moteur (identifiants Fluent) — catalogue en.
# Aucune clé tant qu'aucune crate ne déclare de messages ; chaque crate ajoute ses clés avec
# les mêmes variables dans fr et en (vérifié par check_catalog de ma-testkit).

# TS packet layer (ma-ts)
ts-resync = TS synchronization lost: { $skipped } bytes skipped before packets were found again.
ts-truncated-packet = Incomplete last TS packet: { $bytes } bytes present.
ts-adaptation-length = Invalid adaptation field (length { $length }): packet skipped.

# PSI/SI sections (ma-ts)
ts-psi-incomplete = PSI/SI section dropped before its end ({ $bytes } bytes received).
ts-psi-crc = PSI/SI section ignored: wrong CRC-32 (table_id { $table_id }).
ts-pmt-changed = PMT of program { $program } changed (version { $version }).
ts-psi-too-long = PSI/SI section dropped: section_length { $length } too large.
ts-psi-invalid = Invalid PSI/SI section ignored (table_id { $table_id }).

# Continuity and PES (ma-ts)
ts-cc-gap = Continuity counter break on PID { $pid }: expected { $expected }, got { $got }.
ts-scrambled = PID { $pid } is scrambled: its payload is ignored.
ts-pes-invalid = Invalid or incomplete PES header on PID { $pid }: PES ignored.

# Video index (ma-ts)
ts-au-no-pts = Picture without PTS on PID { $pid }: PTS estimated.
ts-au-too-large = Picture ignored on PID { $pid }: { $size } bytes, more than the maximum picture size.
