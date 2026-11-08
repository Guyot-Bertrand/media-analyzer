# Mesures de M2a : socle moteur et démultiplexage TS

> Plan : `docs/superpowers/plans/2026-11-03-mvp-m2-socle-ts.md` (Tâche 14). Branche `mvp/m2-socle-ts`.
> Date : 2026-11-08. Suite de `06-mesures-m1.md`.
> Machine de référence : Intel NUC, i7-10710U, GPU Intel Comet Lake UHD, pilote iHD **non libre** (VPP présente), Ubuntu 24.04, FFmpeg 6.1, TSDuck, Docker.
> Fichier de mesure : `uhd_big`, TS HEVC Main 10, 3840×2160, 50 i/s, GOP fermé de 50 images, 4 056 183 096 octets, **62 000 images**. Le numéro incrusté de chaque image est relu et comparé à l'attendu : aucun écart.
> Source d'AU : `ma_ts::TsAuSource` (démultiplexeur maison, découpage des AU dans l'ES), et non plus la source de test « 1 PES = 1 AU » de M1.
> Toutes les mesures sont faites sur **disque local, cache disque chaud**.

## 1. Verdict

**M2a est implémenté. Les mesures locales sont faites ; la mesure NFS et les mesures sur flux réels restent à faire (§7).**

- Un vrai TS se navigue de bout en bout avec le démultiplexeur maison. Sur `uhd_big`, ouvert en progressif, la 1re image est servie par le moteur en **197–206 ms** (VA-API) et **535–546 ms** (logiciel). L'indexation complète prend **4,05–4,32 s**. Le saut est à **276–282 ms** au p95 en VA-API (moteur seul). Ce sont des mesures moteur, cache chaud : elles ne jugent pas PERF.OPEN.01 ni PERF.SEEK.01, dont l'acceptation est e2e (§2.2).
- L'indexeur lit le fichier **une fois** (ratio 1,0000). La navigation, à travers le cache de blocs, lit **1,02 fois** les octets d'AU en lecture séquentielle et **1,95 fois** au saut. Le spike lisait 770 Ko par AU de 65 Ko, soit environ 11,8 fois (§3).
- Les 5 parseurs ont tourné 10 min chacun sous `cargo-fuzz`, sans plantage (§5).
- **Mesure NFS non faite** : le banc `tools/nfs-bench/run.sh` est écrit et vérifié (`bash -n`), mais il demande `sudo`. C'est au porteur de le lancer (§7).
- **Corpus privé vide** : aucun vrai flux broadcast n'a été mesuré. PAFF n'est donc prouvé que sur des flux synthétiques, pas par le décodeur (§4, §6.2).
- Par rapport à M1, à protocole égal, le saut et les pas sont **15 à 30 % plus lents** (§2.3). La cause n'est pas établie.

## 2. Mesures locales

### 2.1 Moteur seul (`engine_bench --progressive`)

```
cargo build --release -p ma-ffmpeg --features ma-ffmpeg/ffmpeg6 --example engine_bench
target/release/examples/engine_bench --file ~/media-analyzer-corpus/generated/uhd_big.ts \
    --mode auto|sw --view fit:1920x1080 --dwell-ms 0 --progressive --json <out>
```

Protocole (celui de 06 §2.1, graine fixe) :
- `open_progressive`, puis 1re image (vignette 320×180), mesurée depuis l'ouverture ;
- attente de la fin de l'indexation (durée mesurée depuis l'ouverture) ;
- 100 sauts aléatoires, 200 pas avant depuis total/4, 200 pas arrière depuis total/2 (31 000, une IDR : pire cas voulu).

Vue Ajusté 1920×1080, intervalle 0. 3 exécutions par mode. Durées en ms.

| Mode réel | Exéc. | 1re image | Indexation | Saut p50 / **p95** / max | Avant p50 / **p95** / max (1er) | Arrière p50 / **p95** / max (1er) | RSS max |
|---|---|---|---|---|---|---|---|
| VA-API / VPP | 1 | 197,2 | 4 051 | 159,7 / **281,8** / 301,7 | 10,7 / **15,0** / 18,0 (29,9) | 0,0 / **17,5** / 415,8 (421,8) | 389 Mo |
| VA-API / VPP | 2 | 202,1 | 4 094 | 162,0 / **276,3** / 297,5 | 10,7 / **13,9** / 15,7 (30,0) | 0,0 / **20,1** / 491,8 (547,7) | 392 Mo |
| VA-API / VPP | 3 | 206,1 | 4 272 | 161,5 / **277,8** / 296,1 | 10,5 / **13,4** / 16,0 (29,2) | 0,0 / **10,4** / 427,1 (421,1) | 391 Mo |
| logiciel | 1 | 535,1 | 4 265 | 388,9 / **614,1** / 680,2 | 11,5 / **15,6** / 19,1 (155,6) | 0,0 / **29,2** / 651,1 (666,5) | 959 Mo |
| logiciel | 2 | 545,6 | 4 269 | 395,6 / **591,8** / 640,4 | 11,6 / **16,5** / 19,3 (160,6) | 0,0 / **26,6** / 654,8 (679,4) | 962 Mo |
| logiciel | 3 | 542,5 | 4 318 | 413,1 / **614,0** / 652,0 | 11,5 / **15,7** / 24,4 (164,3) | 0,0 / **24,5** / 650,5 (661,8) | 962 Mo |

Dans les 6 exécutions :
- aucun repli logiciel imprévu ;
- **0 transfert pleine résolution** ;
- 8 images suivantes et 8 précédentes prêtes 1 s après la dernière demande.

Octets lus, à la fin de l'indexation (identiques dans les 6 exécutions) :

| Lecteur | Octets | Rapporté à la taille du fichier |
|---|---|---|
| Indexeur (lecture directe, blocs de 4 Mio) | 4 056 206 304 | 1,0000 |
| Navigation (cache de blocs), pendant l'indexation | 2 097 152 | 0,0005 |
| Total | 4 058 303 456 | **1,0005** |

L'indexeur lit 23 208 octets de plus que le fichier : ce sont les lectures de la détection du format, faites avant l'indexation. Le banc ne relève pas les octets lus par la navigation après l'indexation (pendant les scénarios). L'amplification de lecture de la navigation est mesurée à part (§3).

**Exécutions écartées.** Une machine de bureau partagée : un processus `java` occupait en permanence environ un cœur pendant toutes les mesures. Deux séries ont été perturbées par une charge supplémentaire. Elles sont écartées des tableaux ci-dessus, mais leurs chiffres sont donnés ici :
- 1re série VA-API, charge moyenne d'environ 12 : 1re image 240–246 ms, indexation 5 177–10 822 ms, saut p95 350–414 ms ;
- 2e série logiciel, exécutions 2 et 3 (charge de 8 et 13 au lancement) : saut p95 826 et 1 067 ms.

Deux exécutions de contrôle, machine calme, donnent les mêmes valeurs que le tableau :
- VA-API sans `--progressive` (indexation complète avant les mesures) : saut p95 277,6 ms ;
- une 4e exécution VA-API progressive : 1re image 203,5 ms, indexation 4 044 ms, saut p95 276,2 ms.

Elles concordent aussi avec la mesure de la Tâche 11 (`f956fc9`) : 1re image 216,8 ms, indexation 4 017 ms, saut p95 284 ms.

### 2.2 Face aux seuils de PERF.OPEN.01 et PERF.SEEK.01

Ces exigences se jugent **côté interface** (niveau e2e), et PERF.OPEN.01 **cache disque froid**. Les chiffres ci-dessous sont ceux du moteur seul, cache chaud. Ils sont publiés sans jugement e2e, comme le demande PERF.NET.01.

| Mesure | Seuil (exigence) | Moteur, disque local, VA-API | Moteur, disque local, logiciel | NFS 5 ms |
|---|---|---|---|---|
| 1re image après l'ouverture | < 2 s, cache froid (PERF.OPEN.01) | 197–206 ms (cache chaud) | 535–546 ms (cache chaud) | à mesurer (§7) |
| Indexation complète, en fond | — | 4,05–4,27 s | 4,27–4,32 s | à mesurer |
| Saut p95 (100 sauts) | < 500 ms en matériel (PERF.SEEK.01) | 276–282 ms | 592–614 ms (pas de seuil, publié) | à mesurer |
| Pas suivant p95 | < 100 ms (PERF.STEP.01) | 13,4–15,0 ms | 15,6–16,5 ms | à mesurer |
| Pas précédent p95 | < 100 ms (PERF.STEP.02) | 10,4–20,1 ms | 24,5–29,2 ms | à mesurer |
| Octets lus / taille du fichier, fin d'indexation | — | 1,0005 | 1,0005 | à mesurer |

Le 2e critère de PERF.OPEN.01 (un saut au-delà de la partie indexée attend au lieu d'échouer, avec indicateur) n'est pas mesuré par ce banc : il attend la fin de l'indexation avant les scénarios.

### 2.3 Comparaison avec M1 (même fichier, même protocole, VA-API Ajusté, intervalle 0)

| Mesure | M1 (06 §2.1, source de test) | M2a (`TsAuSource`) | Écart |
|---|---|---|---|
| Saut p50 / p95 | 142,6 / 241,7 ms | 159,7–162,0 / 276–282 ms | +12 % / +15 % |
| Avant p50 / p95 | 9,3 / 10,2 ms | 10,5–10,7 / 13,4–15,0 ms | +15 % / +30–45 % |
| Arrière max (1er pas) | 335,7 (362,1) ms | 416–492 (421–548) ms | +25–45 % |
| RSS max | 296 Mo | 389–392 Mo | +93–96 Mo |
| Logiciel : saut p95 | 473,5 ms | 592–614 ms | +25–30 % |
| Logiciel : avant, 1er pas | 136,3 ms | 156–164 ms | +15–20 % |

Les écarts dépassent la dispersion des exécutions. **La cause n'est pas établie** : rien n'a été mesuré pour la trouver. Pistes, sans mesure :
- la charge permanente de la machine (environ un cœur pris par `java` pendant ces mesures ; elle n'a pas été relevée en M1) ;
- le découpage réel des AU et la lecture par le cache de blocs, au lieu de la lecture directe d'un PES ;
- la mémoire de l'index (62 000 entrées, environ 4,5 Mio) n'explique qu'une petite part des 93 Mo de RSS en plus.

Le saut reste sous le seuil de 500 ms avec une marge de 218 ms au moteur. Une analyse (profil du saut, M1 contre M2a, machine calme) est à faire (§7).

### 2.4 Mémoire de l'indexation (PERF.INDEX.02)

Test `indexing_uhd_big_memory_under_200_mib` (`crates/ma-ts/tests/corpus_index.rs`), en processus dédié sous nextest, profil `dev` (Tâche 10) : `uhd_big : 62000 images, VmHWM 4 → 14 Mio (+10 Mio)`, pour un seuil de 200 Mo. L'écart comprend le tampon du scanner (4 Mio), la table des chunks et 62 000 entrées d'index d'environ 76 octets.

## 3. Amplification de lecture face au spike

Le spike lisait **770 Ko pour une AU de 65 Ko** en moyenne (04 §4, point 5), soit environ 11,8 fois les octets utiles.

En M2a, la navigation lit par le cache de blocs de `ma-io`. Sur disque local : blocs de 1 Mio, 64 blocs en cache, 1 bloc anticipé. Mesure jetable : un programme hors dépôt lit les AU de `uhd_big` par `TsAuSource::read_au` selon trois motifs, puis rapporte `io_stats()` aux octets d'AU rendus. Il ne décode rien : c'est le motif de lecture du navigateur, sans son préchargement.

| Motif | AU lues | Octets d'AU | Octets lus | Lectures | Lus par AU | Amplification |
|---|---|---|---|---|---|---|
| Spike (04 §4) | — | 65 Ko en moyenne | 770 Ko par AU | 1 par AU | 770 Ko | ≈ 11,8 |
| Séquentiel (2 000 AU depuis total/4) | 2 000 | 127 675 558 | 130 023 424 | 62 | 65 Ko | **1,02** |
| 100 sauts (de l'IDR qui précède la cible jusqu'à la cible) | 2 302 | 157 741 270 | 308 281 344 | 147 | 134 Ko | **1,95** |
| 200 AU isolées au hasard (pire cas, aucun voisin lu) | 200 | 12 778 251 | 417 333 248 | 199 | 2 087 Ko | 32,7 |

- En lecture séquentielle (pas avant, préchargement), l'amplification disparaît : chaque octet est lu à peu près une fois, en lectures de 2 Mio (un bloc et un bloc anticipé).
- Au saut, chaque saut lit en moyenne 3,1 Mo pour 1,6 Mo d'AU : le GOP partiel plus le bloc anticipé, arrondis au bloc.
- Une AU isolée coûte un bloc et un bloc anticipé, soit 2 Mio. Ce motif n'arrive pas en navigation, qui décode toujours depuis un point d'accès. C'est la borne haute.

Sur NFS, les blocs passent à 4 Mio et 2 blocs anticipés : l'amplification au saut sera plus forte. Elle n'est pas mesurée (§7).

Le test `read_amplification_is_bounded` (`crates/ma-ts/tests/source.rs`) vérifie en CI que l'ouverture ne lit rien par le cache, puis que la lecture séquentielle de toutes les AU lit au plus 1,1 fois la taille du fichier plus un bloc.

## 4. Flux réels

**Non mesurés.** `corpus/private/` ne contient que son `README.md`, et `MA_CORPUS_PRIVATE` n'est pas défini. Les étapes suivantes de la fiche n'ont donc pas été faites :
- structure des GOP (longueurs, CRA, RASL, BLA, champs), en part des images ;
- `oracle_framemd5_private` : le test affiche `corpus privé absent : test non exécuté` ;
- `engine_bench` (saut, pas) en `auto` et `sw` ;
- fumée du banc interface (10 sauts, 10 pas de chaque côté).

Tout le reste est mesuré sur le corpus généré par FFmpeg (mire, x265 `ultrafast`, GOP fermé pour `uhd_big`). Les temps de décodage y sont probablement optimistes face à un vrai flux broadcast (06 §4.4).

## 5. Fuzzing (Tâche 13)

Session `cargo xtask fuzz --time 600`, NUC i7-10710U, `cargo-fuzz` 0.13.2 sur nightly, entrées de 64 Kio au plus. Commit `1fc1732`.

| Cible | Parseurs exercés | Durée | Exécutions | Exéc./s | Couverture (`cov`) | Amorces |
|---|---|---|---|---|---|---|
| `ts_scan` | `PacketScanner`, `StructureBuilder`, `PesAssembler` | 601 s | 728 227 | 1 212 | 1 576 | 21 |
| `ts_index` | `index` sur `MemSource`, relecture de l'index publié | 601 s | 324 111 | 539 | 3 245 | 21 |
| `es_h264` | `framer(H264)` | 601 s | 158 325 | 263 | 494 | 9 |
| `es_hevc` | `framer(Hevc)` | 601 s | 126 928 | 211 | 306 | 4 |
| `es_mpeg2` | `framer(Mpeg2)` | 601 s | 115 486 | 192 | 223 | 1 |

**Aucune panique, aucun artefact, ni dépassement de mémoire ni délai dépassé.** `cargo xtask ci` refait 20 s par cible si nightly est présent.

Réserves :
- **Amorces ES pauvres** : 1 seule amorce MPEG-2 (un seul fichier `mpeg2_*` au corpus) et 4 HEVC. La couverture ES vient surtout des mutations.
- **1er octet des amorces ES mangé par le sélecteur** : les cibles `es_*` lisent la taille de bloc dans le 1er octet de l'entrée. Celui d'une amorce est donc un octet d'ES, qui est perdu, et sa valeur fixe un bloc souvent minuscule (1 octet ⇒ environ 200 exécutions par seconde).
- `ts_scan` n'exerce que le format détecté. Les formats 192, 204 et 208 ne sont atteints que si l'entrée les fait détecter. `ts_scan` à format forcé n'est pas fuzzé.
- 10 min par cible, c'est peu : une campagne d'une nuit pourrait trouver plus.

## 6. Limites

### 6.1 AU sans PTS : PTS estimé (D2)

Quand un PES porte plusieurs AU (rare, non conforme en DVB), seule la 1re a un PTS. Les autres reçoivent un PTS **estimé** : PTS de l'AU précédente en décodage plus la durée d'image médiane, rendu unique. L'AU est marquée `pts_estimated`, avec un message `ts-au-no-pts`. L'ordre de présentation de ces AU est donc supposé égal à l'ordre de décodage. Avec des images B dans un tel PES, l'ordre serait faux. Le tri par POC (SPS, M4) remplacera l'estimation. Test : `au_without_pts_gets_estimated_pts` sur `h264_720p_pes_grouped` (50 AU estimées sur 100, sans B).

Un PTS estimé peut égaler le PTS réel d'une AU future : c'est une réserve de la revue de la Tâche 10.

### 6.2 H.264 PAFF et champs MPEG-2 (D3) : non prouvés par le décodeur

Ce qui est prouvé :
- `ma-es` détecte les champs H.264 (`field_pic_flag`, `bottom_field_flag`) sur un **flux synthétique** : `h264_paff_fields_are_detected` (`crates/ma-es/tests/synthetic.rs`). Les images MPEG-2 en champs (`picture_structure`) sont détectées de même : `mpeg2_field_pictures_are_detected`.
- L'indexeur apparie deux champs complémentaires et contigus en une entrée, par des **tests unitaires sur AU fabriquées** : `field_pairs_make_one_entry` et `h264_fields_pair_on_the_same_frame_num` (`crates/ma-ts/src/index.rs`).
- Le MBAFF (images entières entrelacées) est prouvé de bout en bout : une AU par image (`mbaff_is_one_au_per_frame`) et un numéro incrusté correct au décodeur (`oracle_on_split_pes_mbaff_and_pts_wrap`, `h264_1080i_mbaff`).

Ce qui ne l'est pas : qu'une paire de champs indexée donne bien **une** image de FFmpeg au bon numéro. Aucun outil du dépôt ne sait encoder du PAFF (x264 ne fait que MBAFF), et FFmpeg encode le MPEG-2 entrelacé en images entières. La preuve attend un fichier H.264 1080i PAFF dans le corpus privé (§7).

### 6.3 4:2:2 non supporté

Les profils 4:2:2, 4:4:4 et 12 bits sont rejetés par le décodeur (`DecodeErrorKind::Unsupported`, `crates/ma-ffmpeg/src/planes.rs`). C'est un choix du plan : ce sont des profils de contribution, sans exigence MVP ni fichier au corpus. L'indexation n'est pas concernée, mais l'aide de l'oracle MD5 (`frame_bytes`, `crates/ma-ffmpeg/tests/common/mod.rs`) suppose du 4:2:0 : `oracle_framemd5_private` ne saurait pas vérifier un fichier 4:2:2 du corpus privé. À revoir en M7 sur les vrais flux.

### 6.4 Décodeur : corrections reportées de M1, cas `EIO` en VA-API (Tâche 12)

Corrections faites en M2a :
- mode `Software` dès l'ouverture d'un décodeur sans périphérique (plus de `FellBackToSoftware` parasite) ;
- **réouverture multi-thread** après un repli silencieux de FFmpeg (profil refusé par VA-API) : une fois, à la 1re image, depuis le RAP de départ (06 §6) ;
- mémo « VPP refusée » tolérant, `ConvertPath` distinct ;
- rognage (`crop_*`) appliqué à tous les chemins : en VA-API, les images mesuraient 1920×1088.

**Cas `EIO` du pilote iHD.** Mesuré sur `hevc_720p_main10` (Comet Lake, FFmpeg 6.1), avec une image P corrompue, l'AU 13, dans un GOP de 50 :
- AU 13 : `send_packet` rend `AVERROR_INVALIDDATA` ;
- AU 14 à 49 : `send_packet` rend `EIO` (-5) à chaque AU, et aucune image ne sort ;
- AU 50 (IRAP) : le décodeur repart seul, sans `reset`.

Un décodeur matériel neuf reproduit exactement le même comportement : l'`EIO` est déterministe, pas passagère. Avant correction, cette `EIO`, classée « panne GPU », faisait passer toute la session en logiciel au premier flux broadcast abîmé.

**Décision R18.** Après un `InvalidData` en matériel, une `EIO` de `send_packet` est comptée comme une AU corrompue, sans repli. Cet état dure jusqu'à la 1re image sortie, au prochain `reset`, ou au 1er RAP envoyé. Une `EIO` sur un RAP, ou après, redevient une panne GPU et déclenche le repli. Résultat : pas de repli ; les images d'avant l'AU sont correctes ; le GOP suivant est correct en matériel.

Effets visibles de R18 :
- **Images non servies** : dans la fenêtre tolérée, les images échouent en `InvalidData` au lieu d'être servies marquées `corrupt`. En matériel, elles ne sortent pas du décodeur.
- **Événements répétés** : jusqu'à un GOP d'événements `CorruptAu`, un par AU envoyée dans la fenêtre.
- **Déclencheurs larges** : tout `InvalidData` à l'envoi arme l'état, y compris une AU trop grande (`MAX_AU_BYTES`) ou vide.
- **Détection retardée d'une vraie panne** : une panne du GPU juste après une AU corrompue est détectée au plus tard au 1er envoi de la session suivante. Ce n'est pas forcément un GOP plus tard : dans le dernier GOP du fichier, ou après un RAP non indexé (rafraîchissement intra sans SEI), l'état dure jusqu'à la fin de la session.

### 6.5 Dettes reportées à M2b

- **Messages d'indexation jetés** : `TsAuSource::open` et `open_progressive` passent un puits `Discard` à l'indexeur. Les messages (`ts-cc-gap`, `ts-au-no-pts`, `ts-pts-discontinuity`…) sont produits et testés, mais aucun consommateur ne les reçoit encore. RF1 reste invisible pour l'utilisateur tant que le rapport (M2b) ne les reçoit pas.
- **Fichier qui grossit** (enregistrement en cours) : vu comme `Modified`. `open` échoue, ou la source se bloque. À décider en M2b.
- **`sh.pcrs` croît avec la durée** : les échantillons de PCR sont gardés en entier, environ 11 Mo pour 2 h et par programme, et `pcrs()` les clone. C'est la seule part de l'index qui ne soit pas une entrée par image. À revoir avec M2b et M3.
- **GOP sain de plus de 2 048 AU (ou 32 Mio)** : au plafond de l'attente de l'indexeur (R15, R16), le lot est publié en **ordre de décodage** (message `ts-index-forced-publish`). L'ordre de présentation de ces images est alors approximatif. Les flux broadcast courants (GOP de 0,5 à 2 s) sont loin de ce plafond.

### 6.6 Autres limites connues

- **Troncature et cache par blocs** : le dernier bloc qui chevauche la coupure devient illisible en entier. Une AU qui finit juste avant la coupure, dans ce bloc, est refusée.
- **Contenu remplacé** : il peut être servi depuis le cache pendant au plus 31 lectures d'AU, jusqu'au contrôle suivant (`REVALIDATE_EVERY = 32`).
- **Troncature constatée pendant `open`** : l'ouverture échoue, au lieu de borner l'index.
- **`open_progressive`** attend la 1re vidéo de la PMT. Si elle n'a jamais d'image, il attend jusqu'à la fin du fichier. Sur un MPTS à PMT tardive, il peut choisir une autre vidéo qu'`open`.
- **Paquet perdu dans un code de début** (R12) : deux AU peuvent fusionner en une, marquée `damaged`, au lieu d'une AU perdue.
- **Discontinuité de PTS** (R14) : l'ordre de présentation est trié par segment, et les segments sont mis bout à bout en ordre de décodage.

## 7. Mesures restant à faire

- **NFS (PERF.NET.01)**, à lancer par le porteur (`sudo` demandé par le script ; Docker, `nfs-common` et le module `nfsd` requis, voir `tools/nfs-bench/README.md`) :

  ```bash
  tools/nfs-bench/run.sh --truncate
  ```

  Le script fait 3 exécutions en `auto` et 3 en `sw` de `engine_bench --progressive` sur `uhd_big` monté par NFSv4, avec 5 ms de latence (`tc netem`). Les JSON vont dans `target/bench/nfs/<horodatage>/`, à reporter dans la colonne « NFS 5 ms » du §2.2. Le script n'a pas été exécuté (décision R3) : seuls `bash -n` et l'analyse des options ont été vérifiés, shellcheck n'est pas installé sur la machine.
- **Troncature côté serveur pendant un parcours** (2e critère de PERF.NET.01) : même commande, option `--truncate`. Elle copie `uhd_big`, tronque la copie de moitié côté serveur pendant les sauts, et vérifie que le banc affiche l'erreur sans planter. Pas encore faite. Le niveau `unit` est couvert en CI par `truncation_during_navigation_is_an_error_not_a_crash`.
- **Amplification de lecture sur NFS** (blocs de 4 Mio, 2 anticipés) : à mesurer avec le même motif que le §3.
- **Corpus privé (D7)** : déposer dans `corpus/private/` au moins un HEVC UHD en GOP ouvert et un H.264 1080i (PAFF si possible), puis :
  - structure des GOP (longueurs, CRA, RASL, BLA, champs) en part des images ;
  - `MA_REQUIRE_GPU=1 cargo nextest run -p ma-ffmpeg --features ma-ffmpeg/ffmpeg6 oracle_framemd5_private` ;
  - `engine_bench` (saut, pas) en `auto` et `sw` ;
  - fumée du banc interface (`npm run e2e -- --spec e2e/transport.e2e.ts --smoke --file <ts>`).

  C'est aussi la preuve de PAFF par le décodeur (§6.2).
- **Écart de performance M1 → M2a** (§2.3) : profil du saut et des pas, sur une machine calme, source de test de M1 contre `TsAuSource`.
- **Côté interface (e2e) et cache disque froid** (PERF.OPEN.01, PERF.SEEK.01) : en M7, comme prévu par 06 §6.
- Restent de 06 §6, sans changement : VA-API sans VPP de bout en bout, PSS en VA-API, options du §5 de 06.
