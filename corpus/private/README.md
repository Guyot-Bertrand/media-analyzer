# Corpus privé

Dossier pour les vrais flux broadcast (TNT, satellite, IPTV) qui ne peuvent pas être publiés.
Tout son contenu est ignoré par git, sauf ce fichier.

- Déposer ici des fichiers `.ts` : au minimum un HEVC UHD en GOP ouvert et un H.264 1080i (PAFF si possible).
- La variable `MA_CORPUS_PRIVATE` désigne un autre dossier ; elle prend alors la place de celui-ci.
- Sans fichier, les tests qui en dépendent affichent `corpus privé absent : test non exécuté` et les mesures correspondantes sont publiées comme restant à faire.
