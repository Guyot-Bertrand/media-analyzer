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
- Par rapport à M1, à protocole égal, le moteur est **12 à 51 % plus lent selon la mesure** (§2.3). La cause de cet écart de temps n'est pas établie. La RSS gagne **+93 à +96 Mo en VA-API** et **+78 à +81 Mo en logiciel** ; environ 70 Mo s'expliquent par le code (cache de blocs plein, tampon de l'indexeur, entrées d'index), le reste n'est pas établi (§2.3).

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

**Séries écartées.** Les mesures ont été faites sur la machine de bureau du porteur, en service.
- **Charge permanente.** Un processus `java` y prenait en permanence environ un cœur (106–111 % d'un cœur à chaque relevé), avec PyCharm à environ 34 %. Cette charge pèse sur **toutes** les séries, retenues comme écartées. Elle n'avait pas été relevée en M1.
- **Critère d'écartement**, fixé et appliqué **a posteriori**, après avoir vu les résultats : une série de 3 exécutions est écartée si, d'une exécution à l'autre, le saut p95 ou la durée d'indexation varie de plus de 10 % (max/min > 1,10). Une série retenue est donc homogène ; ce n'est pas une garantie que la machine était au repos.

Séries retenues (tableau ci-dessus) :

| Série | Saut p95 max/min | Indexation max/min | Charge relevée (moyenne sur 1 min, `/proc/loadavg`) |
|---|---|---|---|
| VA-API, 2e série | 1,02 | 1,05 | 4,12 / 3,24 / 2,38 au lancement des exécutions 1, 2 et 3. Ce chiffre inclut la fin de l'exécution précédente et le processus `java`. |
| Logiciel, 1re série | 1,04 | 1,01 | Non relevée au lancement. Un seul relevé, pendant l'exécution 1 : 12,01. Il inclut le banc lui-même (environ 494 % CPU, décodage logiciel multi-thread) et `java` (111 %). |

Séries écartées (chiffres publiés) :

| Série | Saut p95 | Indexation | max/min | Charge |
|---|---|---|---|---|
| VA-API, 1re série | 413,5 / 387,1 / 350,5 ms | 10 822 / 6 717 / 5 177 ms | 1,18 / 2,09 | Non relevée au lancement. |
| Logiciel, 2e série | 609,1 / 825,5 / 1 067,0 ms | 4 243 / 6 802 / 4 253 ms | 1,75 / 1,60 | 1,69 / 8,24 / 13,32 au lancement ; `java` à 140 % au 2e relevé. |

L'exécution 1 de la 2e série logicielle (609,1 ms, 4 243 ms) concorde avec la 1re série. Elle est écartée avec sa série, par application du critère.

Deux exécutions de contrôle VA-API, faites entre les deux séries, concordent avec la série retenue :
- sans `--progressive` (indexation complète avant les mesures) : saut p95 277,6 ms ; charge de 7,32 relevée juste après ;
- une 4e exécution progressive : 1re image 203,5 ms, indexation 4 044 ms, saut p95 276,2 ms ; charge de 4,78 juste après.

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
| Saut p50 | 142,6 ms | 159,7–162,0 ms | +12,0 à +13,6 % |
| Saut p95 | 241,7 ms | 276,3–281,8 ms | +14,3 à +16,6 % |
| Avant p50 | 9,3 ms | 10,5–10,7 ms | +12,9 à +15,1 % |
| Avant p95 | 10,2 ms | 13,4–15,0 ms | +31,4 à +47,1 % |
| Arrière max | 335,7 ms | 415,8–491,8 ms | +23,9 à +46,5 % |
| Arrière, 1er pas | 362,1 ms | 421,1–547,7 ms | +16,3 à +51,3 % |
| RSS max, VA-API | 296 Mo | 389–392 Mo | +93 à +96 Mo |
| Logiciel : saut p95 | 473,5 ms | 591,8–614,1 ms | +25,0 à +29,7 % |
| Logiciel : avant, 1er pas | 136,3 ms | 155,6–164,3 ms | +14,2 à +20,5 % |
| Logiciel : RSS max | 881 Mo | 959–962 Mo | +78 à +81 Mo |

Les écarts dépassent la dispersion des exécutions.

**RSS : la majeure partie s'explique par le code** (valeurs relues dans le code, non mesurées séparément) :

| Poste | M1 (source de test) | M2a (`TsAuSource`) | Écart |
|---|---|---|---|
| Cache de blocs de la navigation (`CacheConfig::for_locality(Local)`, `crates/ma-io/src/cache.rs`) | aucun : lecture directe de chaque AU (`read_exact_at`) | 64 blocs de 1 Mio, **plein** après les 100 sauts (chaque saut lit environ 3 Mio, §3) | **+64 Mio** |
| Tampon de lecture de l'indexation | `BufReader` de 1 Mio | bloc du scanner de 4 Mio (`IndexOptions::chunk_size`, `BLOCK` de `scan.rs`) | **+3 Mio** |
| Entrées d'index (62 000 images) | `FrameEntry` et position d'un PES | 80 octets par entrée (`Record`) + 4 octets (`by_pres`), soit environ 5 Mio | **+2 à +3 Mio** |
| Total expliqué | | | **≈ 70 Mio** |

Il reste environ **23 à 26 Mo en VA-API** et **8 à 11 Mo en logiciel** dont la cause **n'est pas établie** (fragmentation de l'allocateur, tampons de la table des chunks et de l'attente de l'indexeur, piste non mesurée).

**Temps : la cause n'est pas établie** ; rien n'a été mesuré pour la trouver. Pistes, sans mesure :
- la charge permanente de la machine (environ un cœur pris par `java` pendant toutes ces mesures ; elle n'a pas été relevée en M1) ;
- le découpage réel des AU et la lecture par le cache de blocs, au lieu de la lecture directe d'un PES.

Saut p95 au moteur, VA-API : 276,3–281,8 ms. Seuil de PERF.SEEK.01 : 500 ms, mesuré côté interface. En M1, l'interface ajoutait au saut moteur :
- 15–20 ms au p50 ;
- au p95, à intervalle 0 (06 §2.1 et §2.2) : +21 à +24 ms sous Wayland (263–266 ms contre 241,7 ms) et +28 à +31 ms sous Xvfb (270–273 ms). Une analyse est à faire (§7) : profil du saut, M1 contre M2a, sans la charge permanente.

### 2.4 Mémoire de l'indexation (PERF.INDEX.02)

Test `indexing_uhd_big_memory_under_200_mib` (`crates/ma-ts/tests/corpus_index.rs`), en processus dédié sous nextest, profil `dev` (Tâche 10) : `uhd_big : 62000 images, VmHWM 4 → 14 Mio (+10 Mio)`, pour un seuil de 200 Mo. L'écart comprend le tampon du scanner (4 Mio), la table des chunks et 62 000 entrées d'index.

- **Date de la mesure.** Ce chiffre a été relevé **avant** les bornes R15 et R16 (attente bornée, coupe forcée) et avant R20 (PTS brut gardé : l'entrée stockée passe de 72 à 80 octets, plus 4 octets de `by_pres`). Le test a été relancé par la revue finale de branche, après R15 et R16 : il passe.
- **Ce que le test ne dit pas** : `uhd_big` dure 20 min (62 000 images à 50 i/s), en SPTS. La mémoire de l'index croît avec la durée et le nombre de programmes : voir le chiffrage au §6.5.

## 3. Amplification de lecture face au spike

Le spike lisait **770 Ko pour une AU de 65 Ko** en moyenne (04 §4, point 5), soit environ 11,8 fois les octets utiles.

En M2a, la navigation lit par le cache de blocs de `ma-io`. Sur disque local : blocs de 1 Mio, 64 blocs en cache, 1 bloc anticipé. Programme `crates/ma-ts/examples/read_amp.rs` (décision R19) : il lit les AU par `TsAuSource::read_au` selon trois motifs à graine fixe, puis rapporte `io_stats()` aux octets d'AU rendus. Il ne décode rien : c'est le motif de lecture du navigateur, sans son préchargement.

```
cargo run --release -p ma-ts --example read_amp -- ~/media-analyzer-corpus/generated/uhd_big.ts
```

Deux lancements, sur disque local, cache chaud, donnent des chiffres identiques à l'octet près (graine fixe) :
- la mesure d'origine ;
- la relance par le programme versionné.

Sur les petits fichiers du corpus, tout le fichier tient dans le cache de 64 Mio : l'amplification y est inférieure à 1 et ne dit rien.

| Motif | AU lues | Octets d'AU | Octets lus | Lectures | Lus par AU | Amplification |
|---|---|---|---|---|---|---|
| Spike (04 §4) | — | 65 Ko en moyenne | 770 Ko par AU | 1 par AU | 770 Ko | ≈ 11,8 |
| Séquentiel (2 000 AU depuis total/4) | 2 000 | 127 675 558 | 130 023 424 | 62 | 65 Ko | **1,02** |
| 100 sauts (de l'IDR qui précède la cible jusqu'à la cible) | 2 302 | 157 741 270 | 308 281 344 | 147 | 134 Ko | **1,95** |
| 200 AU isolées au hasard (pire cas, aucun voisin lu) | 200 | 12 778 251 | 417 333 248 | 199 | 2 087 Ko | 32,7 |

- En lecture séquentielle (pas avant, préchargement), l'amplification disparaît : chaque octet est lu à peu près une fois, en lectures de 2 Mio (un bloc et un bloc anticipé).
- Au saut, chaque saut lit en moyenne 3,1 Mo pour 1,6 Mo d'AU : le GOP partiel plus le bloc anticipé, arrondis au bloc.
- Une AU isolée coûte un bloc et un bloc anticipé, soit 2 Mio. Ce motif n'arrive pas en navigation, qui décode toujours depuis un point d'accès. C'est la borne haute.

Sur NFS, les blocs passent à 4 Mio et 2 blocs anticipés : l'amplification au saut sera plus forte. Elle n'est pas mesurée (§7). La même commande s'y applique, sur `/mnt/ma-nfs/uhd_big.ts`.

La lecture anticipée est **synchrone** : un bloc absent du cache est lu en **un seul appel** avec ses blocs anticipés (`BlockCache::fetch`). Sur disque local, cela fait 2 Mio par défaut de cache ; sur NFS, **12 Mio** (3 blocs de 4 Mio), lus avant que l'AU demandée soit rendue. À 5 ms de latence et au débit du lien, ce temps s'ajoute au saut. Le cache NFS occupe aussi jusqu'à 128 Mio (32 blocs de 4 Mio), contre 64 Mio en local (§2.3).

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
- **Ce qui n'est pas fuzzé.** Les entrées font 64 Kio au plus, soit environ 350 paquets : les bornes anti-fichier hostile de l'indexeur (R15 : 4 096 entrées ou 64 Mio d'ES en attente, coupe forcée ; R16 : coupe au plafond) ne sont **jamais atteintes** par le fuzzing ; elles sont couvertes par des tests unitaires (`stream_without_rap_is_force_published`, `empty_pes_and_gaps_are_bounded`…). `read_es` (relecture d'une AU par rejeu des paquets) n'est **pas** fuzzé non plus : la cible `ts_index` relit les entrées publiées (`entry`, `au`), pas leurs octets.
- **Cibles non compilées sans nightly** (M-9) : `fuzz/` est hors du workspace, et `cargo xtask ci` ne fait la session courte que si nightly est installé. Sans nightly, une modification d'API qui casse une cible ne se voit pas en CI stable.
- **Amorces ES pauvres** : 1 seule amorce MPEG-2 (un seul fichier `mpeg2_*` au corpus) et 4 HEVC. La couverture ES vient surtout des mutations.
- **1er octet des amorces ES mangé par le sélecteur** : les cibles `es_*` lisent la taille de bloc dans le 1er octet de l'entrée. Celui d'une amorce est donc un octet d'ES, qui est perdu, et sa valeur fixe un bloc souvent minuscule (1 octet ⇒ environ 200 exécutions par seconde).
- `ts_scan` n'exerce que le format détecté. Les formats 192, 204 et 208 ne sont atteints que si l'entrée les fait détecter. `ts_scan` à format forcé n'est pas fuzzé.
- 10 min par cible, c'est peu : une campagne d'une nuit pourrait trouver plus.

## 6. Limites

### 6.1 AU sans PTS : PTS estimé (D2)

Quand un PES porte plusieurs AU (rare, non conforme en DVB), seule la 1re a un PTS. Les autres reçoivent un PTS **estimé** : PTS de l'AU précédente en décodage plus la durée d'image médiane, augmenté de 1 tant qu'il égale un des 256 PTS récents. L'AU est marquée `pts_estimated`, avec un message `ts-au-no-pts`. L'ordre de présentation de ces AU est donc supposé égal à l'ordre de décodage. Avec des images B dans un tel PES, l'ordre serait faux. Le tri par POC (SPS, M4) remplacera l'estimation. Test : `au_without_pts_gets_estimated_pts` sur `h264_720p_pes_grouped` (50 AU estimées sur 100, sans B).

Un PTS estimé peut encore égaler un PTS réel plus ancien que les 256 récents, ou celui d'une AU future. Depuis R20, cela ne casse plus l'unicité : le PTS **de l'index** (`FrameEntry::pts`) est rendu unique et croissant à la publication (§6.6, « Unicité des PTS »). Seul l'ordre de présentation entre ces deux AU reste approximatif (égalité départagée par l'ordre de décodage).

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
- **La mémoire de l'index croît avec la durée et le nombre de programmes** (R21), à rattacher à l'index SQLite de M2b. Tailles relues dans le code (`size_of`) :
  - par image : entrée stockée `Record` de **80 octets** + 4 octets de `by_pres`, soit **84 octets** ;
  - par échantillon de PCR : `PcrSample` de **32 octets** ; un PCR toutes les 20 à 40 ms (ETSI TR 101 290 : 40 ms au plus), soit 25 à 50 par seconde ;
  - `follow_structure` indexe **toutes** les vidéos analysées de la structure, et garde les PCR de **tous** les programmes : un MPTS se paie par programme.

  | Cas | Par seconde | Par heure | Par 24 h |
  |---|---|---|---|
  | 1 vidéo à 25 i/s | 2,1 Ko | 7,6 Mo | 181 Mo |
  | 1 vidéo à 50 i/s | 4,2 Ko | 15,1 Mo | 363 Mo |
  | PCR d'un programme, 25 à 50 par seconde | 0,8 à 1,6 Ko | 2,9 à 5,8 Mo | 69 à 138 Mo |
  | SPTS, 1 programme à 25 i/s, PCR à 50/s | 3,7 Ko | 13,3 Mo | **320 Mo** |
  | MPTS de 6 programmes à 25 i/s, PCR à 50/s | 22,2 Ko | 80 Mo | **1,9 Go** |

  Les `Vec` doublent leur capacité en croissant : la mémoire réservée peut atteindre le double de ces chiffres au moment d'une réallocation. `pcrs()` clone en plus tout le vecteur d'un programme à chaque appel. Les PCR sont **conservés** (décision R21) : M3 en a besoin pour l'analyse de cadence. Sur un fichier de 20 min (`uhd_big`), c'est négligeable (§2.4) ; sur une capture de 24 h, c'est la limite de l'index en mémoire.
- **GOP sain de plus de 2 048 AU (ou 32 Mio)** : au plafond de l'attente de l'indexeur (R15, R16), le lot est publié sans attendre de point d'accès (message `ts-index-forced-publish`). Depuis R23, il est trié comme tout lot, par (segment, PTS du flux), puis reçoit ses PTS d'index dans cet ordre (R20). C'est l'ordre où un vrai décodeur sort les images (POC). Auparavant, le lot restait en ordre de décodage : dans un GOP IBBP, chaque I ou P suivie de B recevait un PTS d'index plus petit que ses B, le Navigator refusait l'image sortie après elles, et environ 1 image sur 3 n'était jamais servie. Seules les images de part et d'autre d'une coupe forcée (au plus la profondeur de réordonnancement) gardent un ordre approximatif ; elles peuvent ne pas être servies. Tests : `forced_batch_gets_index_pts_in_stream_order` et `forced_batch_is_navigable_by_a_poc_order_decoder` (décodeur factice qui sort par POC, `FakeFactory::output_key`). Les flux broadcast courants (GOP de 0,5 à 2 s) sont loin de ce plafond.

### 6.6 Autres limites connues

- **Troncature et cache par blocs** : le dernier bloc qui chevauche la coupure devient illisible en entier. Une AU qui finit juste avant la coupure, dans ce bloc, est refusée.
- **Contenu remplacé** : il peut être servi depuis le cache pendant au plus 31 lectures d'AU, jusqu'au contrôle suivant (`REVALIDATE_EVERY = 32`).
- **Troncature constatée pendant `open`** : l'ouverture échoue, au lieu de borner l'index.
- **`open_progressive`** attend la 1re vidéo de la PMT. Si elle n'a jamais d'image, il attend jusqu'à la fin du fichier. Sur un MPTS à PMT tardive, il peut choisir une autre vidéo qu'`open`.
- **Paquet perdu dans un code de début** (R12) : deux AU peuvent fusionner en une, marquée `damaged`, au lieu d'une AU perdue.
- **Discontinuité de PTS** (R14) : l'ordre de présentation est trié par segment, et les segments sont mis bout à bout en ordre de décodage.
- **Unicité des PTS de l'index** (R20) : le Navigator identifie l'image sortie du décodeur par son PTS. Avant R20, deux segments R14 (épissure, `discontinuity_indicator`) ou un PTS estimé pouvaient porter le même PTS, et le Navigator pouvait servir **une autre image** sans erreur. Désormais :
  - `ma-ts` garantit que `FrameEntry::pts` est **unique dans l'index d'un PID et strictement croissant avec `pres_idx`** (contrat écrit sur `ma_core::FrameEntry`). Chaque segment est décalé une durée d'image après le dernier PTS publié ; un PTS qui ne dépasse pas le précédent (PTS réel répété, PTS estimé, images autour d'une coupe forcée R15) devient ce précédent + 1. Les lots forcés sont triés par PTS du flux avant (R23, §6.5). Tests : `segments_get_unique_pts`, `published_pts_are_unique_and_increasing` (`crates/ma-ts/src/index.rs`).
  - Le **PTS du flux** reste dans `AuInfo::pts_raw` (DTS dans le même repère). Le PTS de l'index n'est pas celui du flux dès le 2e segment : **les PTS affichés (M3, M6) doivent venir de `pts_raw`**.
  - Le Navigator est défensif : une AU envoyée avec un PTS déjà en attente de sortie (ou déjà sorti) rend ce PTS ambigu ; l'image sortie sous un PTS ambigu n'est jamais attribuée, et la cible elle-même échoue en `FrameError::DuplicatePts`. Test : `duplicate_pts_never_serves_a_wrong_frame` (`crates/ma-decode/src/navigator.rs`).
  - `DuplicatePts` **ne relance pas** le décodage depuis le RAP, contrairement à une image absente d'un décodage poursuivi : la demande échoue. Avec un index qui respecte R20, ce cas ne vient que de flux pathologiques, où la profondeur de réordonnancement réelle dépasse celle que supposent les PTS (images autour d'une coupe forcée, ordre POC différent de l'ordre des PTS).
- **L'indexeur ne revalide pas le fichier pendant l'indexation** : ses lectures directes ne passent pas par les contrôles de `TsAuSource` (R17). Une lecture courte donne `Truncated` ; un contenu remplacé sans changement de taille n'est vu qu'**en fin d'indexation**, par `check_unchanged`, dans `open` comme dans `open_progressive`. Ce contrôle passe **avant** la publication de `progress().complete` : sur un échec, les entrées restent publiées mais l'index n'est jamais vu complet. `open_progressive` rend alors `Io(Modified)` si l'indexation est déjà finie, sinon le `join` du handle le rend.
- **Lectures après troncature non prouvées pour les images en cache** : les tests montrent qu'une AU au-delà de la coupure est refusée. Ils ne montrent pas qu'une image déjà décodée, servie depuis le cache d'images du Navigator, reste cohérente après la coupure.
- **Repli silencieux en cours de flux** : si FFmpeg écarte le hwaccel en cours de session (et non à la 1re image), le décodage passe en logiciel sans événement `FellBackToSoftware`. La réouverture multi-thread ne joue qu'à la 1re image.
- **`CorruptAu` réémis après une réouverture silencieuse** : la réouverture redécode depuis le RAP de départ. Une AU corrompue de cet intervalle émet donc son événement `CorruptAu` une 2e fois.

## 7. Mesures restant à faire

- **NFS (PERF.NET.01)**, à lancer par le porteur (`sudo` demandé par le script ; Docker, `nfs-common` et le module `nfsd` requis, voir `tools/nfs-bench/README.md`) :

  ```bash
  tools/nfs-bench/run.sh --truncate
  ```

  À lancer en utilisateur normal, **pas** par `sudo run.sh` : le script s'arrête s'il est lancé en root. Le script fait 3 exécutions en `auto` et 3 en `sw` de `engine_bench --progressive` sur `uhd_big` monté par NFSv4, avec 5 ms de latence (`tc netem`). Les JSON vont dans `target/bench/nfs/<horodatage>/`, à reporter dans la colonne « NFS 5 ms » du §2.2. Le script n'a pas été exécuté (décision R3) : seuls `bash -n` et l'analyse des options ont été vérifiés, shellcheck n'est pas installé sur la machine.
- **Troncature côté serveur pendant un parcours** (2e critère de PERF.NET.01) : même commande, option `--truncate`. Elle copie `uhd_big` et la lit par un 2e montage sans cache d'attributs (`actimeo=0`). Elle tronque la copie de moitié côté serveur pendant les sauts. Le script vérifie que le banc s'arrête sur l'erreur `fichier tronqué`, sans planter. Il accepte aussi `fichier modifié`, que PERF.NET.01 admet, et l'annonce par un message distinct. Il échoue si le parcours finit sans erreur (« troncature non constatée ») ou sur toute autre erreur d'E/S (EIO, ESTALE…), qu'il cite. Pas encore faite. Le niveau `unit` est couvert en CI par `truncation_during_navigation_is_an_error_not_a_crash`.
- **Amplification de lecture sur NFS** (blocs de 4 Mio, 2 anticipés, lus de façon synchrone : 12 Mio par défaut de cache, §3) : pendant que le partage est monté par `tools/nfs-bench/run.sh`, lancer la commande du §3 sur le fichier monté :

  ```bash
  cargo run --release -p ma-ts --example read_amp -- /mnt/ma-nfs/uhd_big.ts
  ```

  Le script démonte le partage en fin de parcours : il faut lancer cette commande pendant ses mesures, ou en ajoutant une étape au script.
- **Corpus privé (D7)** : déposer dans `corpus/private/` au moins un HEVC UHD en GOP ouvert et un H.264 1080i (PAFF si possible), puis :
  - structure des GOP (longueurs, CRA, RASL, BLA, champs) en part des images ;
  - `MA_REQUIRE_GPU=1 cargo nextest run -p ma-ffmpeg --features ma-ffmpeg/ffmpeg6 oracle_framemd5_private` ;
  - `engine_bench` (saut, pas) en `auto` et `sw` ;
  - fumée du banc interface (`npm run e2e -- --spec e2e/transport.e2e.ts --smoke --file <ts>`).

  C'est aussi la preuve de PAFF par le décodeur (§6.2).
- **Écart de performance M1 → M2a** (§2.3) : profil du saut et des pas, sur une machine calme, source de test de M1 contre `TsAuSource`.
- **Côté interface (e2e) et cache disque froid** (PERF.OPEN.01, PERF.SEEK.01) : en M7, comme prévu par 06 §6.
- Restent de 06 §6, sans changement : VA-API sans VPP de bout en bout, PSS en VA-API, options du §5 de 06.
