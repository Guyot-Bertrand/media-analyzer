# MVP — M2a Socle moteur et démultiplexage TS : plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remplacer la source d'AU de test (« 1 PES = 1 AU ») par un vrai démultiplexeur TS maison. Il lit le fichier par lectures positionnelles avec un cache de blocs. Il découpe les AU dans l'ES, gère les GOP ouverts et les points de reprise, et indexe progressivement à mémoire bornée. Le décodeur de M1 et les bancs tournent dessus, avec les corrections de `ma-ffmpeg` reportées de M1 et les mesures NFS et broadcast réel.

**Architecture:** Cinq crates moteur nouvelles, toutes sans `unsafe` :
- `ma-core` : types partagés (`AuSource`, `FrameEntry`, `RapKind`… déplacés de `ma-decode`, qui les réexporte), horodatages, messages identifiés.
- `ma-bits` : lecteur de bits instrumenté (mode rapide / arbre) et écrivain de bits pour les tests.
- `ma-io` : `ReadAt`, fichier en `pread`, cache de blocs, lecture anticipée selon le système de fichiers, détection de troncature.
- `ma-es` : découpage des AU et classification (H.264, HEVC, MPEG-2) sur un flux d'octets ES, sans connaissance du conteneur. Il servira aussi aux ES bruts (M2b).
- `ma-ts` : paquets, PSI/SI, PES, PCR, indexeur progressif et `TsAuSource` (`impl AuSource`).

`ma-ts` dépend de `ma-core`, `ma-bits`, `ma-io` et `ma-es`, jamais de `ma-decode` ni de FFmpeg. Un module `testutil` (feature) fabrique des TS synthétiques ; `xtask` s'en sert pour dériver les fichiers de corpus.

**Tech Stack:** Rust stable (édition 2024), `thiserror`, `proptest` (dev), `fluent-syntax` (dans `ma-testkit` seulement), `tempfile` (dev). Oracles de test : `ffprobe`, `ffmpeg` (`-c copy`, `framemd5`) et TSDuck (`tsanalyze --json`). Fuzzing : `cargo-fuzz` sur la chaîne nightly (D5). Mesures : Docker (serveur NFS) et `tc netem`.

**Spec:** `docs/05-feuille-de-route-mvp.md` (ligne M2) ; `docs/02-stack-technique.md` (§2.1, §3.1, §3.2, §3.3, §3.5, §4.3, §5, §6.1) ; `docs/04-resultats-spike.md` §4 ; `docs/06-mesures-m1.md` §4.4 et §6 ; exigences `requirements/mvp/structure.yaml`, `performance.yaml` (PERF.INDEX.02, PERF.JOB.01, PERF.NET.01, PERF.OPEN.01, PERF.HW.01, PERF.XFER.01), `navigation.yaml` (NAV.ORACLE.01–04, NAV.THUMB.01–02), `donnees.yaml` (DATA.TS.01, DATA.PIC.01), `syntaxe.yaml` (SYN.ROBUST.01, SYN.TREE.02), `i18n.yaml` (I18N.KEYS.01) ; revue finale de M1 (`.superpowers/sdd/2026-10-31-mvp-m1-transport-image/final-review.md`, `progress.md`).

## Découpage proposé : M2a / M2b

La ligne M2 fait environ 18 tâches. C'est au-delà de la taille d'un plan (Scope Check). Proposition :

- **M2a (ce plan, 14 tâches)** — *démultiplexage et source d'AU réelle* : `ma-core` (types, messages), `ma-bits`, `ma-io`, `ma-es`, `ma-ts` (PSI/SI, PES, PCR, découpage des AU, GOP ouverts, points de reprise, indexation progressive **en mémoire**), remplacement de la source de test, corrections de `ma-ffmpeg` reportées de M1, mesures NFS et broadcast.
- **M2b (plan suivant, environ 5 tâches)** — *persistance et rapport* : `ma-store` (index persistant SQLite, PERF.INDEX.01, SET.LOG.01 ; messages du rapport, RPT.MUTE.01, RPT.SCALE.01, RPT.LOG.01, RPT.GROUP.01, RPT.TABLE.01), registre `ma-core` (`ContainerProbe`, détection du format, STRUCT.OPEN.03 ES bruts via `ma-es`, STRUCT.OPEN.04 complet), ouverture avec cache.
- **Raison de la frontière** : M3 (TR 101 290) et M4 (syntaxe) n'ont besoin que de M2a : paquets, PSI, PCR, ES, `ma-bits` et des messages en mémoire (`MessageSink`). M2b peut donc avancer en parallèle de M3 et M4. M2a se livre seul : un TS réel est navigable de bout en bout, sans cache disque.

## Décisions à valider par le porteur

Le plan est écrit en supposant chaque recommandation acceptée.

| # | Décision | Recommandation |
|---|---|---|
| D1 | Découpage M2a / M2b ci-dessus. | Accepter. `05-feuille-de-route-mvp.md` est mis à jour en Tâche 14. |
| D2 | **AU sans PTS** (plusieurs AU par PES, rare et non conforme en DVB). La spec ne dit rien. L'ordre exact demande le POC, donc les SPS (M4). | En M2a, le PTS est **estimé** : PTS de l'AU précédente en décodage + durée d'image médiane, rendu unique. L'AU est marquée `pts_estimated`, avec un message `ts-au-no-pts`. L'ordre par POC remplacera l'estimation en M4. |
| D3 | **H.264 PAFF** (paires de champs, fréquent en 1080i broadcast) : FFmpeg sort une image pour deux AU de champ. Cela casse l'hypothèse « une AU par image ». Le détecter demande une lecture minimale du SPS, du PPS et de l'en-tête de tranche, donc un léger doublon avec M4. | Le faire en M2a dans `ma-es` (sous-ensemble documenté), avec l'appariement des champs MPEG-2 (`picture_structure`). Aucun outil du dépôt ne sait encoder du PAFF (x264 ne fait que MBAFF) : la preuve par le décodeur passe par le corpus privé (D7), sinon la limite est publiée. |
| D4 | **TSDuck devient requis par `cargo xtask ci`** (oracle de structure dès M2a, de TR 101 290 en M3). BUILD.md le liste déjà comme outil de test. | Accepter. Sans TSDuck, le test échoue avec `TSDuck absent : voir BUILD.md §4` (jamais d'abandon silencieux). La génération du corpus, elle, n'utilise que FFmpeg et `xtask`. |
| D5 | **Fuzzing** (SYN.ROBUST.01 : 10 min par parseur) : `cargo-fuzz` exige nightly, alors que `rust-toolchain.toml` épingle stable. | Nightly installé à côté de stable, pour `fuzz/` seulement (hors workspace). `cargo xtask fuzz` lance les sessions longues à la main. `cargo xtask ci` fait 20 s par cible si nightly est présent, sinon il affiche `nightly absent : fuzzing court non exécuté`. Les tests `proptest` « ne panique jamais » restent dans la CI stable. |
| D6 | **Mesure NFS** (PERF.NET.01) : serveur NFS en conteneur `--privileged`, montage et `tc netem` (5 ms) demandent `sudo`. | Mesure manuelle, scriptée (`tools/nfs-bench/`), hors `cargo xtask ci`. En CI, une source à latence simulée (`ma-io`) et un test de troncature couvrent le niveau `unit`. |
| D7 | **Vrais flux broadcast** : aucun n'est dans le dépôt (06 §6). | **Validée 2026-11-03** : emplacement `corpus/private/` dans le dépôt, ignoré par git (seul `corpus/private/README.md` est versionné) ; `$MA_CORPUS_PRIVATE` le remplace s'il est défini. Le porteur n'a pas encore de fichiers : il y déposera plus tard au moins un HEVC UHD en GOP ouvert et un H.264 1080i (PAFF si possible). Les tests correspondants affichent `corpus privé absent : test non exécuté` sans eux, et la mesure est alors publiée comme restant à faire. |
| D8 | (à trancher avant M2b) Index et rapport SQLite. | `rusqlite` en build embarquée (déjà proposé par 02 §2.2). Clé : chemin + taille + mtime + empreinte (premier et dernier Mio). Version de schéma différente ⇒ suppression et réindexation, sans migration. Pas d'éviction automatique en MVP : une purge manuelle dans les réglages. |

## Reports de M1 : triage

**Intégrés à ce plan** :
- Tâche 3 : `frame_cnt` H.264 lu en présentation ; `broken_link` MPEG-2 et RASL d'une BLA ; tests de repli trop faibles (Minor 8) ; test `navigator.rs:983` à valeur constante.
- Tâche 1 : valeur `gop` du manifeste MPEG-2 ; fichier 1080p avec rognage gauche/haut.
- Tâche 12 :
  - `mode = Software` sans périphérique (`FellBackToSoftware` parasite) ;
  - réouverture multi-thread après le repli silencieux de FFmpeg ;
  - `ConvertPath` distinct pour « VPP refusée » ;
  - mémo `vpp_refused` tolérant ;
  - classement `Gpu` sur un flux corrompu en VA-API ;
  - rognage `crop_*`.
- Tâches 8 et 10 : drapeau `corrupt` peu fiable (R11). Le transport marque désormais l'AU `damaged`.
- Tâche 11 : `SCRATCH` thread_local de 192 Kio, supprimé avec `ts_source.rs`.
- Tâche 9 : risques entrelacé, PAFF et MBAFF.

**Laissés de côté volontairement** :
- **Taille figée à la 1re image** (changement de résolution) : il faut le SPS complet (M4). Le Navigator borne sans rejeter.
- **4:2:2, 4:4:4 et 12 bits** (`Unsupported`) : aucune exigence MVP ni fichier du corpus. Ce sont des profils de contribution, pas de diffusion. À revoir en M7 sur les vrais flux.
- **HEVC entrelacé** (champs signalés par SEI `pic_timing`) : une AU donne une image, FFmpeg sort des champs. Sans risque pour la navigation, l'affichage relève de M6.
- **Viewport appliqué en bord d'image** et **événements non bornés** : dépendances de M6, selon la revue finale.
- **Diagnostic VA-API derrière `Box<dyn Decoder>`** : au-delà du `ConvertPath` distinct, il est affiché en M6.
- **Qualité de tests de M1 sans effet** (sondage, EOF non discriminant, `probe_candidates` en O(n), nom du fil), **outillage du corpus de T1** (`geq`, `pix_fmt`, commentaires) et **cosmétique** (commentaires, `# Errors`, paramètre `gpu`, `require_vpp` sur le nœud par défaut) : coût sans bénéfice pour l'utilisateur.
- **Banc Tauri** (`drop` hors `spawn_blocking`, pas de tests TS, `cargo deny`) : banc seulement. Il est juste migré vers `ma-ts`.
- **Cache disque froid, PSS en VA-API, options du §5 de 06** : M7, comme prévu par 06 §6.
- **VA-API sans VPP de bout en bout** : il faut une autre machine (pilote libre). La mesure reste publiée comme à faire.
- **Bits 8 à 15 du numéro incrusté** vérifiés par synthèse seulement : sans effet sur le moteur.

## Global Constraints

- Les règles de M0 et M1 s'appliquent : GPL-3.0-or-later, édition 2024, `clippy -D warnings` (pedantic), `cargo deny`, **90 % des lignes** par crate `crates/ma-*`, `cargo xtask ci` vert, textes et documents en français, identifiants en anglais. Chaque nouvelle crate a `[lints] workspace = true`.
- `unsafe` **uniquement dans `ma-ffmpeg`** (`#![forbid(unsafe_code)]` partout ailleurs, via le workspace).
- **Le moteur ne démultiplexe pas avec FFmpeg** (02 §3.3). `ffprobe`, `ffmpeg` et TSDuck servent uniquement d'oracles de test et d'outils de génération du corpus.
- **Pas de `mmap`** : lectures positionnelles seulement (`FileExt::read_at`, 02 §3.1). Un fichier tronqué ou modifié produit une erreur, jamais un plantage.
- **Aucun parseur ne panique**, quelle que soit l'entrée (anti-objectif n° 4). Une donnée invalide produit un message et l'analyse continue.
- **Le moteur n'émet aucun texte en langue naturelle** (02 §4.3). Un message, c'est un identifiant Fluent (`[a-z][a-z0-9-]*`, par exemple `ts-cc-gap`) et des paramètres typés. Chaque identifiant existe dans `locales/fr/engine.ftl` et `locales/en/engine.ftl`, avec les mêmes variables. Les noms normatifs (`PCR_PID`…) ne sont pas traduits.
- AU bornée à **32 Mio** (`MAX_AU_BYTES`, comme en M1). Index en mémoire : `AuInfo` ≤ 96 octets par image. Hors entrées d'index, la mémoire d'indexation ne dépend pas de la taille du fichier (< 200 Mo sur `uhd_big`, PERF.INDEX.02).
- Lecture anticipée : système de fichiers local ⇒ blocs de 1 Mio, 64 blocs en cache, 1 bloc anticipé ; distant (`nfs`, `nfs4`, `cifs`, `smb3`, `fuse.sshfs`) ⇒ 4 Mio, 32 blocs, 2 blocs anticipés.
- Tests dépendant d'une ressource absente, jamais d'abandon silencieux :
  - GPU et corpus : règles de M1 (`VA-API absent : test non exécuté`, `corpus absent : lancer cargo xtask corpus gen`) ;
  - `uhd_big` : `uhd_big absent : test non exécuté (cargo xtask corpus gen --big)` ;
  - corpus privé : `corpus privé absent : test non exécuté` ;
  - TSDuck : échec (D4).
- Machine de référence : NUC i7-10710U, Comet Lake UHD, pilote iHD non libre (VPP), Ubuntu 24.04, FFmpeg 6.1, TSDuck installé, Docker.
- Statuts en fin de M2a :
  - passent en `implemented` (niveau `unit` seul, tests qui passent) : STRUCT.OPEN.01, STRUCT.TREE.03, NAV.ORACLE.02, NAV.ORACLE.03, PERF.INDEX.02 ;
  - les autres exigences touchées restent `validated`, car leurs niveaux `ui`/`e2e` ou leurs parties M2b, M3 à M6 manquent.
  - `transport.yaml` (TR 101 290, débits) relève de M3 : M2a n'en fournit que les briques (CC, PCR, sections, CRC).
- `#[covers("ID")]` ne marque que les tests qui vérifient un critère d'acceptation de l'exigence citée.

## Review Focus

1. **Paquet perdu ou dupliqué au milieu d'une AU vidéo** (CC cassé, cas courant en broadcast) : seule l'AU touchée est marquée `damaged`, un message `ts-cc-gap` est émis, et l'indexation et les AU voisines sont intactes. Un doublon unique ou un `discontinuity_indicator` ne produit rien. Tests : Tâche 8, `lost_packet_marks_gap` ; Tâche 10, `lost_packet_damages_only_its_au`.
2. **Fichier tronqué ou remplacé pendant l'indexation ou la navigation** (NFS, enregistrement en cours) : `IoError::Truncated` / `Modified`, index partiel conservé et stable, `FrameError::Io` côté Navigator, application utilisable. Tests : Tâche 5, `truncated_after_open_is_reported` ; Tâche 10, `truncated_during_indexing_keeps_partial_index` ; Tâche 11, `truncation_during_navigation_is_an_error_not_a_crash`.
3. **Bouclage des PTS sur 33 bits au milieu du fichier** : l'ordre de présentation reste continu et croissant, et le PCR se déroule aussi. Tests : Tâche 10, `pts_wrap_keeps_presentation_order` et `pcr_samples_are_unwrapped` ; Tâche 11, oracle du numéro incrusté sur `h264_720p_pts_wrap`.
4. **Octets parasites avant la 1re synchro ou au milieu du fichier** : le format est détecté, la synchronisation est retrouvée, et les offsets et numéros de paquet restent exacts après la resynchronisation. Test : Tâche 6, `resync_after_garbage_keeps_offsets`.
5. **PMT qui change en cours de flux** (nouvelle version, nouveau PID vidéo) : la structure suit la dernière version, le nouveau PID vidéo a son propre index, et les AU de deux PID ne sont jamais mélangées. Tests : Tâche 7, `pmt_version_change_is_followed` ; Tâche 10, `pmt_change_starts_new_video_index`.

---

## Structure des fichiers

```
crates/ma-core/src/{lib.rs, media.rs, time.rs, message.rs}     # types partagés, horodatages, messages
crates/ma-bits/src/{lib.rs, reader.rs, rbsp.rs, syntax.rs, writer.rs}
crates/ma-io/src/{lib.rs, source.rs, cache.rs, locality.rs, testing.rs}
crates/ma-es/src/{lib.rs, framer.rs, startcode.rs, h264.rs, hevc.rs, mpeg2.rs}
crates/ma-ts/src/{lib.rs, packet.rs, scan.rs, psi.rs, tables.rs, structure.rs, pes.rs,
                  es_read.rs, index.rs, source.rs, messages.rs, testutil.rs}
crates/ma-ts/tests/{corpus_structure.rs, corpus_index.rs, source.rs}
crates/ma-testkit/src/{oracle.rs, catalog.rs}  (+ suppression de ts_source.rs et nal.rs)
locales/{fr,en}/engine.ftl
fuzz/                                    # cargo-fuzz, hors workspace (D5)
xtask/src/{corpus.rs, fuzz.rs, ci.rs, main.rs}
tools/nfs-bench/{run.sh, README.md}      # mesure NFS manuelle (D6)
docs/07-mesures-m2a.md
```

---

### Task 1 : Corpus M2a et oracles de test

**Files:**
- Modify: `corpus/manifest.toml`, `xtask/src/corpus.rs`, `crates/ma-testkit/{Cargo.toml,src/lib.rs,src/corpus.rs}`, `BUILD.md` (§4 : TSDuck requis par `cargo xtask ci`, corpus privé `corpus/private/` ou `MA_CORPUS_PRIVATE`). Déjà fait avant la Tâche 1 : `.gitignore` (`/corpus/private/*`, `!/corpus/private/README.md`) et `corpus/private/README.md`
- Create: `crates/ma-testkit/src/oracle.rs`
- Test: modules `tests` de `oracle.rs` et de `xtask/src/corpus.rs`

**Interfaces:**
- Produces (fichiers générés, mêmes règles qu'en M1 : numéro incrusté, idempotence, vérification `ffprobe`) :

| name | contenu |
|---|---|
| `h264_1080p_crop_top` | H.264 High, encodé en 1920×1088 puis `-bsf:v h264_metadata=crop_top=8:crop_bottom=0` ⇒ affiché en 1920×1080 avec un rognage en haut ; 25 i/s, 50 images, GOP 25, `features = ["crop_top"]` |
| `h264_1080i_mbaff` | H.264 High 1920×1080 entrelacé TFF (`-flags +ildct+ilme`, MBAFF), 25 i/s, 100 images, GOP 25, 2 B, `features = ["interlaced", "mbaff"]` |
| `h264_720p_pts_wrap` | recette de `h264_720p_closed` avec `-output_ts_offset 95440` : PTS et PCR franchissent 2^33 vers l'image 93, `features = ["pts_wrap"]` |
| `h264_720p_nob` | H.264 High 1280×720, 25 i/s, 100 images, GOP 25, **sans B** (`-bf 0`), base des fichiers dérivés de la Tâche 8 |

- Valeur `gop` de `mpeg2_576i_closed` : la remplacer par l'intervalle réel entre images I mesuré par `ffprobe` (le test du manifeste le vérifie).
- `ma-testkit/src/oracle.rs` (dev uniquement) :

```rust
pub struct ProbePacket { pub pts: Option<i64>, pub dts: Option<i64>, pub size: u32, pub pos: Option<u64>, pub key: bool }
pub fn ffprobe_packets(path: &Path) -> Vec<ProbePacket>;      // 1er flux vidéo, ordre du fichier
pub struct ProbeFrame { pub pts: i64, pub pict_type: char, pub key: bool }
pub fn ffprobe_frames(path: &Path) -> Vec<ProbeFrame>;        // ordre de présentation
pub fn ffmpeg_es(path: &Path) -> Vec<u8>;                     // `-map 0:v:0 -c copy -f h264|hevc|mpeg2video -`
pub fn ffmpeg_framemd5(path: &Path) -> Vec<(i64 /* pts */, String /* md5 */)>;
pub fn tsduck_analyze(path: &Path) -> serde_json::Value;      // `tsanalyze --json` ; panique « TSDuck absent : voir BUILD.md §4 »
pub fn private_corpus() -> Vec<PathBuf>;                      // *.ts de $MA_CORPUS_PRIVATE, sinon de <racine du dépôt>/corpus/private ; vide ⇒ message « corpus privé absent : test non exécuté »
```

- [ ] **Step 1 : Tests qui échouent** :
  - `manifest_in_repo_is_complete` (existant, étendu aux 4 entrées) ;
  - `ffprobe_packets_of_closed_h264` : 250 paquets, le 1er `key`, PTS présents ;
  - `ffprobe_frames_types` : `h264_720p_closed` ⇒ 10 `I` ;
  - `ffmpeg_es_starts_with_start_code` (`00 00 00 01` ou `00 00 01`) ;
  - `private_corpus_empty_without_env` (dossier vide ou ne contenant que le README ⇒ liste vide) ;
  - `private_corpus_reads_env_dir` (`MA_CORPUS_PRIVATE` vers un dossier temporaire contenant `a.ts` et `b.txt` ⇒ seul `a.ts`).
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-testkit -p xtask` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter** les recettes, les helpers (commandes avec `-v error`, erreur claire si un outil manque) et la correction du manifeste.
- [ ] **Step 4 : Vérifier** — Run: `cargo xtask corpus gen && cargo nextest run -p ma-testkit -p xtask`.
  - Expected : 4 fichiers créés, tests PASS.
  - `ffprobe -show_streams h264_1080p_crop_top.ts` donne `width=1920 height=1080`.
  - `ffprobe -show_packets h264_720p_pts_wrap.ts` montre un PTS qui revient sous 10 000.
- [ ] **Step 5 : Commit** : `test(corpus): fichiers M2a (rognage, MBAFF, bouclage des PTS, sans B) et oracles ffprobe/TSDuck`.

---

### Task 2 : `ma-core` — types partagés, horodatages, messages

**Files:**
- Create: `crates/ma-core/{Cargo.toml,src/lib.rs,src/media.rs,src/time.rs,src/message.rs}`, `locales/fr/engine.ftl`, `locales/en/engine.ftl` (en-tête de commentaire seulement), `crates/ma-testkit/src/catalog.rs`
- Modify: `crates/ma-decode/{Cargo.toml,src/types.rs,src/lib.rs}` (types déplacés, puis réexportés : `pub use ma_core::{AuSource, Codec, FrameEntry, Leading, PictureType, RapKind};`), `crates/ma-testkit/Cargo.toml` (`fluent-syntax`)

**Interfaces:**
- Produces :
  - `media.rs` : `Codec`, `PictureType`, `RapKind`, `Leading`, `FrameEntry` et le trait `AuSource`, **déplacés tels quels** depuis `ma-decode/src/types.rs` (doc comprise). Aucun appelant ne change.
  - `time.rs` :

```rust
pub const PTS_HZ: u64 = 90_000;
pub const PCR_HZ: u64 = 27_000_000;
pub struct TsUnwrapper { /* dernier déroulé */ }
impl TsUnwrapper {
    pub fn new(bits: u32) -> Self;                 // 33 pour PTS/DTS, 33 pour la base PCR
    pub fn unwrap(&mut self, raw: u64) -> i64;     // raw + k·2^bits, k tel que |résultat − dernier| soit minimal
}
pub struct Pcr { pub base: u64 /* 33 bits */, pub ext: u16 /* < 300 */ }
impl Pcr { pub fn ticks(&self) -> u64; }           // base × 300 + ext
```

  - `message.rs` :

```rust
pub enum Severity { Error, Warning, Info }
pub enum Family { File, Transport, Video, Decode }
pub enum Param { Int(i64), UInt(u64), Pid(u16), Text(String) }   // Text : noms normatifs ou chemins, jamais de phrase
pub struct MsgDef { pub id: &'static str, pub params: &'static [&'static str] }
pub struct Message { pub id: &'static str, pub severity: Severity, pub family: Family, pub sub_family: &'static str,
                     pub offset: Option<u64>, pub pid: Option<u16>, pub decode_idx: Option<u32>,
                     pub params: Vec<(&'static str, Param)> }
impl Message {
    pub fn new(def: &MsgDef, severity: Severity, family: Family, sub_family: &'static str) -> Message;
    pub fn param(self, name: &'static str, v: Param) -> Message;   // debug_assert : nom déclaré dans def.params
    pub fn at(self, offset: u64) -> Message; pub fn pid(self, pid: u16) -> Message; pub fn frame(self, decode_idx: u32) -> Message;
}
pub trait MessageSink: Send { fn push(&mut self, m: Message); }
#[derive(Default)] pub struct VecSink { pub messages: Vec<Message> }   // impl MessageSink
```

  - `ma-testkit/src/catalog.rs` : `pub fn check_catalog(defs: &[MsgDef], locales: &Path) -> Vec<String>`. Il renvoie les écarts : identifiant absent en `fr` ou en `en`, ou variables `{ $x }` différentes de `def.params`. Chaque crate qui déclare des messages a un test `catalog_is_complete` qui l'appelle avec son `MESSAGES` et `repo/locales`.

- [ ] **Step 1 : Tests qui échouent** :
  - `pts_unwrap_crosses_33_bits` : `unwrap(2^33 − 90)` puis `unwrap(10)` ⇒ `2^33 + 10` ;
  - `small_backward_jump_is_not_a_wrap` : `1000` puis `900` ⇒ `900` ;
  - `pcr_ticks` : base 1, ext 299 ⇒ 599 ;
  - `message_builder_keeps_params_and_position` ;
  - (testkit) `#[covers("I18N.KEYS.01")] catalog_detects_missing_and_mismatched_keys`, sur des `.ftl` temporaires : clé absente en `en`, variable `$pid` en trop en `fr`, cas conforme ⇒ liste vide ;
  - (ma-decode) `core_types_are_reexported` : `let _: ma_core::FrameEntry = ma_decode::FrameEntry { … };`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-core -p ma-decode -p ma-testkit` ; Expected : FAIL (compilation).
- [ ] **Step 3 : Implémenter**, déplacer les types et ajouter les réexports.
- [ ] **Step 4 : PASS** — même commande, puis `cargo nextest run --workspace` (aucune régression de M1).
- [ ] **Step 5 : Commit** : `feat(ma-core): types partagés, horodatages déroulés, messages identifiés`.

---

### Task 3 : `ma-decode` — points de reprise en présentation, lien rompu, tests de repli

**Files:**
- Modify: `crates/ma-core/src/media.rs` (`RapKind::Recovery { pres_span: u32 }`), `crates/ma-decode/src/{plan.rs,fake.rs,navigator.rs}`, `crates/ma-testkit/src/nal.rs` (renommage seulement : le fichier disparaît en Tâche 11)
- Test: modules `tests` de `plan.rs` et `navigator.rs`

**Interfaces:**
- Consumes : `ma-core` (Tâche 2).
- Produces :
  - `RapKind::Recovery { pres_span: u32 }` remplace `Recovery { frame_cnt }`. Doc : « nombre d'images, en ordre de présentation à partir du RAP, qui ne sont pas encore correctes ; la source le calcule à partir de `recovery_frame_cnt` (Tâche 10) ». La règle de `start_for` est inchangée (`target.pres_idx < rap.pres_idx + pres_span` ⇒ RAP précédent).
  - `NotDecodable::BrokenLink` (`#[error("l'image dépend d'images remplacées par un raccordement (lien rompu)")]`). Règle : une cible `Leading::Skippable` dont le RAP le plus proche est `Bla` ⇒ `Err(BrokenLink)`, sans remonter au RAP précédent. On l'utilise pour HEVC BLA et pour MPEG-2 `broken_link` (Tâche 10).
  - `FakeSource::broken_link_gops(gops, gop_len, bframes)` : comme `open_gops`, mais tous les RAP après le premier sont `Bla`. `FakeDecoder` ne sort pas les `Skippable` d'une `Bla`.

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("NAV.ORACLE.04")] bla_leading_frames_not_decodable` : `broken_link_gops(3, 8, 2)` ⇒ `start_for(9) == Err(BrokenLink)`, `start_for(12) == Ok(8)` ; via le Navigator, `FrameError::NotDecodable { reason: BrokenLink }` ;
  - `#[covers("NAV.ORACLE.03")] recovery_span_is_in_presentation_order` (renommage de l'existant, avec `pres_span`) ;
  - reprise des tests faibles (Minor 8 de la revue finale de M1) :
    - `fallback_happens_once_and_second_error_is_reported` provoque réellement une erreur du décodeur logiciel après le repli ⇒ `FrameError::Decoder` et exactement un `FellBackToSoftware` ;
    - `invalid_data_never_triggers_fallback` : sans code mort ni `let _ =` ; il vérifie un événement `CorruptAu`, l'absence de `FellBackToSoftware` et `mode() == Hardware` ;
    - `gpu_failure_during_prefetch_falls_back_once` : une demande explicite est en attente au moment de l'erreur, et elle est servie ;
    - le test de `navigator.rs:983` calcule `expected` depuis la source au lieu d'une constante.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-decode` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
- [ ] **Step 4 : PASS** — même commande, puis `cargo nextest run --workspace`.
- [ ] **Step 5 : Commit** : `feat(ma-decode): reprise en ordre de présentation, lien rompu (BLA), tests de repli renforcés`.

---

### Task 4 : `ma-bits` — lecteur de bits instrumenté

**Files:**
- Create: `crates/ma-bits/{Cargo.toml,src/lib.rs,src/reader.rs,src/rbsp.rs,src/syntax.rs,src/writer.rs}`
- Test: modules `tests` de chaque fichier

**Interfaces:**
- Produces :

```rust
pub enum BitError { Truncated { at_bit: u64, wanted: u32 }, ExpGolombTooLong { at_bit: u64 } }   // thiserror
pub struct BitReader<'a> { /* data, pos */ }
impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self;
    pub fn bit_pos(&self) -> u64; pub fn bits_left(&self) -> u64;
    pub fn u(&mut self, n: u32) -> Result<u64, BitError>;      // n ≤ 64
    pub fn flag(&mut self) -> Result<bool, BitError>;
    pub fn ue(&mut self) -> Result<u32, BitError>;              // au plus 31 zéros de tête
    pub fn se(&mut self) -> Result<i32, BitError>;
    pub fn skip(&mut self, n: u64) -> Result<(), BitError>;
    pub fn byte_align(&mut self); pub fn is_byte_aligned(&self) -> bool;
    pub fn more_rbsp_data(&self) -> bool;                       // il reste des bits avant le dernier 1 (rbsp_trailing_bits)
}
pub struct Rbsp { pub data: Vec<u8>, removed: Vec<u32> }        // octets 0x03 retirés (position dans le NAL)
pub fn rbsp(nal: &[u8]) -> Rbsp;
impl Rbsp { pub fn nal_bit(&self, rbsp_bit: u64) -> u64; }      // offset correspondant dans le NAL (vue hexadécimale)
pub enum Value { U(u64), S(i64), Flag(bool) }
pub trait Recorder {
    fn enter(&mut self, name: &'static str, bit: u64);
    fn exit(&mut self, bit: u64, error: Option<&'static str>);
    fn field(&mut self, name: &'static str, bit: u64, len: u32, value: Value);
}
pub struct Fast;                                                // ne mémorise rien (mode « rapide », indexation)
pub struct SyntaxNode { pub name: &'static str, pub bit_offset: u64, pub bit_len: u64, pub value: Option<Value>,
                        pub error: Option<&'static str>, pub children: std::ops::Range<u32> }
#[derive(Default)] pub struct TreeRecorder { pub nodes: Vec<SyntaxNode> }   // mode « détaillé », arbre aplati
pub struct Syntax<'a, 'r, R: Recorder> { pub bits: BitReader<'a>, pub rec: &'r mut R }
impl<R: Recorder> Syntax<'_, '_, R> {
    pub fn u(&mut self, name: &'static str, n: u32) -> Result<u64, BitError>;
    pub fn flag(&mut self, name: &'static str) -> Result<bool, BitError>;
    pub fn ue(&mut self, name: &'static str) -> Result<u32, BitError>;
    pub fn se(&mut self, name: &'static str) -> Result<i32, BitError>;
    pub fn group<T>(&mut self, name: &'static str, f: impl FnOnce(&mut Self) -> Result<T, BitError>) -> Result<T, BitError>;  // erreur : nœud fermé avec error = "truncated"
}
#[cfg(feature = "writer")] pub struct BitWriter;                // u(n, v), ue(v), se(v), finish() -> Vec<u8> (avec rbsp_trailing_bits)
#[cfg(feature = "writer")] pub fn escape(rbsp: &[u8]) -> Vec<u8>;   // ajoute les 0x03 d'échappement
```

- [ ] **Step 1 : Tests qui échouent** :
  - `u_reads_across_bytes` : `[0xAC, 0x50]` ⇒ `u(3) = 5`, `u(7) = 49`, `u(6) = 16` ;
  - `ue_se_values` : `[0xA6, 0x40]` ⇒ `ue` × 4 = 0, 1, 2, 3 ; les bits `010`, `011`, `00100` lus en `se` ⇒ 1, −1, 2 ;
  - `truncated_read_is_an_error_not_a_panic` : `[0xFF]`, `u(9)` ⇒ `Truncated { at_bit: 0, wanted: 9 }` ;
  - `exp_golomb_longer_than_31_is_rejected` (5 octets nuls) ;
  - `rbsp_removes_emulation_prevention_and_maps_offsets` : `[0,0,3,1,0xAA]` ⇒ `data == [0,0,1,0xAA]`, `nal_bit(24) == 32` ;
  - `more_rbsp_data_detects_trailing_bits` : `[0xA0]`, vrai après 1 bit lu, faux après 2 ;
  - `#[covers("SYN.TREE.02")] tree_recorder_records_offsets_and_lengths` : `[0x64, 0x00, 0x28]` dans `group("seq_parameter_set")` ⇒ `profile_idc` = 100, offset 0, longueur 8 ; `level_idc` = 40, offset 16, longueur 8 ; groupe (0, 24) ;
  - `fast_and_tree_give_same_values` ;
  - `error_inside_group_leaves_partial_tree` ;
  - `writer_roundtrip` : `BitWriter` puis `BitReader` sur des valeurs u/ue/se aléatoires (proptest) ;
  - `#[covers("SYN.ROBUST.01")] reader_never_panics` : proptest, octets et suite d'opérations aléatoires.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-bits --all-features` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
- [ ] **Step 4 : PASS** — même commande.
- [ ] **Step 5 : Commit** : `feat(ma-bits): lecteur de bits instrumenté (modes rapide et arbre), RBSP`.

---

### Task 5 : `ma-io` — lectures positionnelles, cache de blocs, troncature

**Files:**
- Create: `crates/ma-io/{Cargo.toml,src/lib.rs,src/source.rs,src/cache.rs,src/locality.rs,src/testing.rs}`
- Test: modules `tests` ; `tempfile` en dev

**Interfaces:**
- Produces :

```rust
pub enum IoError {                                   // thiserror ; impl From<IoError> for std::io::Error
    Truncated { expected_len: u64, actual_len: u64 },   // kind UnexpectedEof
    Modified,                                           // kind Other
    InvalidData(&'static str),                          // kind InvalidData
    Os(std::io::Error),
}
pub trait ReadAt: Send + Sync {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError>;   // lecture courte = fin de fichier
}
pub struct FileIdentity { pub len: u64, pub mtime_ns: i128, pub inode: u64 }
pub struct FileSource { /* File, chemin, identité à l'ouverture */ }
impl FileSource {
    pub fn open(path: &Path) -> Result<FileSource, IoError>;
    pub fn identity(&self) -> FileIdentity; pub fn locality(&self) -> Locality;
    pub fn check_unchanged(&self) -> Result<(), IoError>;   // fstat : taille réduite ⇒ Truncated ; mtime ou inode ⇒ Modified
}
// impl ReadAt for FileSource : `FileExt::read_at` ; une lecture plus courte que demandé avant `len()` ⇒ check_unchanged()
pub enum Locality { Local, Remote }
pub fn fs_locality(fs_type: &str) -> Locality;         // nfs, nfs4, cifs, smb3, fuse.sshfs ⇒ Remote
pub fn path_locality(path: &Path) -> Locality;         // /proc/self/mountinfo, point de montage le plus long qui préfixe le chemin canonique
pub struct CacheConfig { pub block_size: u32, pub max_blocks: usize, pub read_ahead: u32 }
impl CacheConfig { pub fn for_locality(l: Locality) -> CacheConfig; }   // valeurs : Global Constraints
pub struct IoStats { pub syscalls: u64, pub bytes_read: u64, pub hits: u64 }
pub struct BlockCache<S: ReadAt> { /* Mutex<LRU de blocs alignés> */ }
impl<S: ReadAt> BlockCache<S> {
    pub fn new(src: S, cfg: CacheConfig) -> Self;
    pub fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), IoError>;   // un défaut de cache lit le bloc + `read_ahead` blocs en un seul appel
    pub fn source(&self) -> &S; pub fn stats(&self) -> IoStats;
}
impl<S: ReadAt> ReadAt for BlockCache<S>
// testing.rs (feature "testing") : MemSource(Vec<u8>) comptant les appels ; LatencySource<S> { inner, latency: Duration } ;
// TruncatingSource<S> { inner, cut_at: u64 } : renvoie Truncated au-delà de cut_at
```

- [ ] **Step 1 : Tests qui échouent** :
  - `read_at_matches_file_content` ;
  - `overlapping_reads_share_one_block` : deux lectures dans le 1er Mio ⇒ `syscalls == 1` ;
  - `sequential_reads_amplification_bounded` : 8 Mio lus par morceaux de 64 Kio ⇒ `bytes_read ≤ len + block_size × read_ahead` ;
  - `lru_evicts_oldest_block` (`max_blocks = 2`) ;
  - `#[covers("PERF.NET.01")] truncated_after_open_is_reported` (Review Focus 2) : fichier de 3 Mio tronqué à 1 Mio après l'ouverture, lecture à 2 Mio ⇒ `Truncated { expected_len: 3 Mi, actual_len: 1 Mi }`, sans panique ;
  - `rewritten_file_is_modified` : contenu réécrit et mtime avancé ⇒ `check_unchanged() == Err(Modified)` ;
  - `fs_type_mapping` ; `tmp_is_local` ;
  - `#[covers("PERF.NET.01")] remote_read_ahead_reduces_round_trips` : `LatencySource` de 5 ms, 16 Mio lus séquentiellement par 64 Kio ⇒ `syscalls ≤ 2` en config `Remote` contre `≤ 8` en `Local`, sur le même parcours ;
  - `io_error_maps_to_std_kinds`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-io --all-features` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
- [ ] **Step 4 : PASS** — même commande.
- [ ] **Step 5 : Commit** : `feat(ma-io): lectures positionnelles, cache de blocs, lecture anticipée distante, troncature`.

---

### Task 6 : `ma-ts` — paquets, format, synchronisation

**Files:**
- Create: `crates/ma-ts/{Cargo.toml,src/lib.rs,src/packet.rs,src/scan.rs,src/messages.rs,src/testutil.rs}`, `crates/ma-ts/tests/corpus_structure.rs`
- Modify: `xtask/{Cargo.toml,src/corpus.rs}` (dépendance `ma-ts`, feature `testutil`), `corpus/manifest.toml`, `locales/{fr,en}/engine.ftl`

**Interfaces:**
- Consumes : `ma-core` (`Pcr`, messages), `ma-io` (`ReadAt`, `IoError`).
- Produces :

```rust
pub enum PacketFormat { Ts188, Tts192, Rs204, Ts208 }
impl PacketFormat { pub fn size(self) -> usize; pub fn sync_offset(self) -> usize /* 4 pour Tts192 */; pub fn timing_reliable(self) -> bool /* false pour Tts192 */ }
pub fn detect_format(head: &[u8]) -> Option<(PacketFormat, usize /* offset du 1er paquet */)>;
    // plus petit offset (≤ 64 Kio) où 5 octets 0x47 consécutifs tombent à la période ; périodes essayées : 188, 192, 204, 208
pub struct PacketHeader { pub tei: bool, pub pusi: bool, pub priority: bool, pub pid: u16, pub scrambling: u8, pub afc: u8, pub cc: u8 }
pub struct AdaptationField { pub discontinuity: bool, pub random_access: bool, pub pcr: Option<Pcr> }
pub struct Packet<'a> { pub header: PacketHeader, pub adaptation: Option<AdaptationField>, pub payload: &'a [u8] }
pub enum PacketError { SyncByte, AdaptationLength }
pub fn parse_packet(p: &[u8; 188]) -> Result<Packet<'_>, PacketError>;
pub struct ScannedPacket<'a> { pub offset: u64 /* octet 0x47 */, pub index: u64, pub packet: Packet<'a> }
pub struct PacketScanner<'s> { /* lit par blocs de 4 Mio */ }
impl<'s> PacketScanner<'s> {
    pub fn new(src: &'s dyn ReadAt, format: PacketFormat, start: u64, end: Option<u64>) -> Self;
    pub fn next(&mut self, sink: &mut dyn MessageSink) -> Result<Option<ScannedPacket<'_>>, IoError>;
}   // mauvais octet de synchro : recherche de 5 synchros consécutives plus loin, message `ts-resync` (param `skipped`) ; paquet final incomplet : `ts-truncated-packet` (`bytes`)
pub enum OpenError { Empty, AccessDenied, NotTs, NoVideo, Io(IoError) }   // thiserror
pub fn probe(path: &Path) -> Result<(FileSource, PacketFormat, u64), OpenError>;   // lit le 1er Mio
pub const MESSAGES: &[MsgDef];
// testutil (feature) : packet(pid, pusi, cc, af: Option<AfSpec>, payload) -> [u8; 188] ;
//   repacketize(ts: &[u8], f: PacketFormat) -> Vec<u8> (204/208 : 16/20 octets 0x00 ajoutés ; TTS : préfixe de 4 octets, compteur 27 MHz croissant)
```

- Corpus dérivés de `h264_720p_closed` par `repacketize` : `ts204_h264`, `ts208_h264`, `tts_h264` (`derived_from`, `features = ["packet_204"]`, etc.).

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("STRUCT.OPEN.01")] detects_188_204_208` (corpus) : format attendu et nombre de paquets = taille / taille de paquet ;
  - `#[covers("STRUCT.OPEN.02")] detects_tts_and_flags_timing` : `Tts192`, `timing_reliable() == false`, même nombre de paquets que la source ;
  - `#[covers("STRUCT.OPEN.04")] empty_text_and_unreadable_files_are_reported` : fichier vide ⇒ `Empty`, texte ⇒ `NotTs`, mode `000` ⇒ `AccessDenied` (si les tests tournent en root, le cas « accès refusé » affiche `exécuté en root : cas accès refusé non vérifiable`) ;
  - `parse_packet_fields` : PID 0x1FFE, CC 7, PCR base 0x1_2345_6789 et ext 299 ;
  - `adaptation_length_overflow_is_an_error` : longueur 190 ⇒ `AdaptationLength` ;
  - `resync_after_garbage_keeps_offsets` (Review Focus 4) : 1000 octets parasites, 10 paquets, 7 octets parasites, 10 paquets ⇒ 20 paquets, offsets exacts, index 0 à 19, un seul `ts-resync` ;
  - `last_partial_packet_is_reported` ;
  - `#[covers("SYN.ROBUST.01")] scanner_never_panics` (proptest) ;
  - `catalog_is_complete`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-ts --all-features` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter**, puis ajouter les 3 recettes à `xtask`.
- [ ] **Step 4 : PASS** — Run: `cargo xtask corpus gen && cargo nextest run -p ma-ts --all-features`.
- [ ] **Step 5 : Commit** : `feat(ma-ts): paquets TS 188/192/204/208, détection du format, resynchronisation`.

---

### Task 7 : `ma-ts` — PSI/SI et structure du flux

**Files:**
- Create: `crates/ma-ts/src/{psi.rs,tables.rs,structure.rs}`
- Modify: `crates/ma-ts/src/{testutil.rs,messages.rs}`, `crates/ma-ts/tests/corpus_structure.rs`, `xtask/src/corpus.rs`, `corpus/manifest.toml`, `locales/{fr,en}/engine.ftl`

**Interfaces:**
- Consumes : `PacketScanner`, `ScannedPacket` (Tâche 6).
- Produces :

```rust
pub fn crc32_mpeg2(data: &[u8]) -> u32;              // polynôme 0x04C11DB7, init 0xFFFFFFFF, sans réflexion
pub struct Section { pub table_id: u8, pub table_id_ext: u16, pub version: u8, pub current: bool,
                     pub section_number: u8, pub last_section_number: u8, pub body: Vec<u8>, pub crc_ok: bool, pub offset: u64 }
pub struct SectionAssembler { /* par PID */ }
impl SectionAssembler {
    pub fn new(pid: u16) -> Self;
    pub fn push(&mut self, pkt: &ScannedPacket, out: &mut Vec<Section>, sink: &mut dyn MessageSink);
}   // pointer_field, plusieurs sections par paquet, sections sur plusieurs paquets, bourrage 0xFF ; rupture de CC ⇒ section abandonnée (`ts-psi-incomplete`)
pub struct Descriptor { pub tag: u8, pub data: Vec<u8> }
pub struct Pat { pub ts_id: u16, pub programs: Vec<(u16 /* numéro */, u16 /* PID de PMT */)> }
pub struct PmtStream { pub stream_type: u8, pub pid: u16, pub descriptors: Vec<Descriptor> }
pub struct Pmt { pub program_number: u16, pub version: u8, pub pcr_pid: u16, pub descriptors: Vec<Descriptor>, pub streams: Vec<PmtStream> }
pub struct SdtService { pub service_id: u16, pub provider: String, pub name: String }
pub struct Sdt { pub ts_id: u16, pub services: Vec<SdtService> }
pub fn parse_pat(s: &Section) -> Option<Pat>; pub fn parse_pmt(s: &Section) -> Option<Pmt>; pub fn parse_sdt(s: &Section) -> Option<Sdt>;
pub fn dvb_string(b: &[u8]) -> String;               // 0x15 ⇒ UTF-8 ; sans préfixe ou 0x10 0x00 0x01 ⇒ Latin-1 ; autres jeux ⇒ remplacement U+FFFD
pub enum AudioKind { Aac, AacLatm, Ac3, Eac3, Ac4, Mpeg }
pub enum StreamKind { Video(Option<Codec>), Audio(Option<AudioKind>), Subtitles, Teletext, Scte35, Data }
pub fn stream_kind(stream_type: u8, descriptors: &[Descriptor]) -> StreamKind;
    // 0x1B / 0x24 / 0x02 ⇒ vidéo analysée ; 0xEA (VC-1) ⇒ Video(None) ; 0x0F AAC ; 0x11 LATM ; 0x03/0x04 MPEG ; 0x81 AC-3 ; 0x87 E-AC-3 ;
    // 0x06 + descripteur 0x6A AC-3 / 0x7A E-AC-3 / 0x59 Subtitles / 0x56 Teletext / 0x7F ext 0x15 AC-4 ; 0x86 Scte35 ; sinon Data
pub struct EsInfo { pub pid: u16, pub stream_type: u8, pub kind: StreamKind, pub carries_pcr: bool, pub analysed: bool }
pub struct Program { pub number: u16, pub pmt_pid: u16, pub pcr_pid: Option<u16>, pub service: Option<SdtService>, pub streams: Vec<EsInfo> }
impl Program { pub fn dedicated_pcr_pid(&self) -> Option<u16>; }   // PCR_PID qui n'est pas un PID d'ES
pub struct TsStructure { pub format: PacketFormat, pub ts_id: Option<u16>, pub has_cat: bool, pub programs: Vec<Program> }
pub struct StructureBuilder { /* assembleurs PAT, CAT, PMT, SDT */ }
impl StructureBuilder {
    pub fn new(format: PacketFormat) -> Self;
    pub fn push(&mut self, pkt: &ScannedPacket, sink: &mut dyn MessageSink);   // CRC faux ⇒ section ignorée (`ts-psi-crc`) ; nouvelle version de PMT ⇒ `ts-pmt-changed`
    pub fn structure(&self) -> TsStructure; pub fn complete(&self) -> bool /* PAT et toutes ses PMT vues */; pub fn generation(&self) -> u32;
}
// testutil : section(table_id, ext, version, body) -> Vec<u8> (avec CRC) ; psi_packets(pid, &section, cc) -> Vec<[u8; 188]> ;
//   pat(..), pmt(..), sdt(..) ; replace_psi(ts, pid, sections) et fill_nulls(ts, pid, packets) pour dériver le corpus
```

- Corpus (ffmpeg, puis réécriture `xtask` par `testutil` ; les nouveaux paquets **remplacent des paquets nuls**, grâce à `-muxrate`, pour garder le timing) :

| name | contenu |
|---|---|
| `ts_ref` | HEVC Main 10 1280×720 50 i/s 100 images (PID 0x0100), AAC 0x0101, E-AC-3 0x0102 (`-streamid`, `-muxrate 6M`), service « MA Référence », fournisseur « Media Analyzer ». PMT réécrite avec 0x0103 (0x06 + `subtitling_descriptor` « fra ») et 0x01F4 (0x86, `registration_descriptor` CUEI) ; sections SCTE-35 `splice_null` chaque seconde sur 0x01F4. PCR sur la vidéo. |
| `ts_pcr_pid` | `ts_ref` avec `PCR_PID = 0x01FF` et des paquets PCR seuls sur 0x01FF toutes les 30 ms au plus (PCR interpolé depuis ceux de la vidéo) |
| `ts_unsupported` | `ts_ref` dont la PMT ajoute 0x0104 (0xEA, VC-1) et 0x0105 (0x06 + descripteur d'extension AC-4) |
| `mpts_3prog` | 3 programmes H.264 640×360 (`-program` × 3), services « P1 », « P2 », « P3 » |

- [ ] **Step 1 : Tests qui échouent** :
  - `crc32_known_vector` : `crc32_mpeg2(b"123456789") == 0x0376E6E7` ;
  - `section_spanning_packets_and_several_per_packet` ;
  - `bad_crc_section_is_ignored_with_message` ;
  - `#[covers("STRUCT.TREE.01")] reference_tree_has_expected_pids` : `ts_ref`, 1 programme, flux `[(0x100, Video(Some(Hevc))), (0x101, Audio(Some(Aac))), (0x102, Audio(Some(Eac3))), (0x103, Subtitles), (0x1F4, Scte35)]` ;
  - `#[covers("STRUCT.TREE.02")] pcr_on_video_and_on_dedicated_pid` : `ts_ref` ⇒ `carries_pcr` sur 0x100, pas de PID dédié ; `ts_pcr_pid` ⇒ `dedicated_pcr_pid() == Some(0x1FF)` ;
  - `#[covers("STRUCT.TREE.03")] sdt_service_name_and_provider` : `ts_ref` ⇒ « MA Référence » / « Media Analyzer » ; flux synthétique sans SDT ⇒ `service == None` ;
  - `#[covers("STRUCT.TREE.04")] unsupported_streams_are_generic` : 0x104 ⇒ `Video(None)`, `analysed == false` ; 0x105 ⇒ `Audio(Some(Ac4))`, `analysed == false` ;
  - `#[covers("STRUCT.TREE.05")] mpts_has_three_programs` ;
  - `#[covers("STRUCT.OPEN.02")] tts_tree_equals_ts_tree` : `tts_h264` et `h264_720p_closed` ont la même structure, au champ `format` près ;
  - `structure_matches_tsduck` : PID, `stream_type`, noms de service et fournisseur de `ts_ref` et `mpts_3prog` égaux à ceux de `tsduck_analyze` ;
  - `pmt_version_change_is_followed` (Review Focus 5) : PMT v0 vidéo 0x100, puis v1 vidéo 0x200 ⇒ structure avec 0x200, `generation()` incrémentée, un `ts-pmt-changed` ;
  - `#[covers("SYN.ROBUST.01")] section_assembler_never_panics` (proptest).
- [ ] **Step 2 : Vérifier l'échec** ; **Step 3 : Implémenter**, avec les 4 recettes de corpus ; **Step 4 : PASS** — Run: `cargo xtask corpus gen && cargo nextest run -p ma-ts --all-features`.
- [ ] **Step 5 : Commit** : `feat(ma-ts): sections PSI/SI (PAT, CAT, PMT, SDT), CRC, structure du flux`.

---

### Task 8 : `ma-ts` — PES, positions dans l'ES, relecture

**Files:**
- Create: `crates/ma-ts/src/{pes.rs,es_read.rs}`
- Modify: `crates/ma-ts/src/{testutil.rs,messages.rs}`, `xtask/src/corpus.rs`, `corpus/manifest.toml`, `locales/{fr,en}/engine.ftl`

**Interfaces:**
- Consumes : Tâches 5 à 7.
- Produces :

```rust
pub struct EsPos { pub packet_offset: u64, pub skip: u16 }   // paquet TS qui porte l'octet, et octets d'ES déjà consommés dans ce paquet
pub struct PesHeader { pub stream_id: u8, pub packet_length: u16, pub pts: Option<u64>, pub dts: Option<u64>, pub header_len: usize }
pub enum PesError { NoStartCode, Truncated }
pub fn parse_pes_header(buf: &[u8]) -> Result<PesHeader, PesError>;
pub struct PesStart { pub pts: Option<u64>, pub dts: Option<u64>, pub packet_offset: u64, pub packet_index: u64 }
pub struct EsChunk<'a> { pub data: &'a [u8], pub pos: EsPos, pub pes_start: Option<PesStart>, pub gap_before: bool }
pub struct PesAssembler { /* par PID : CC attendu, en-tête PES en cours (peut couvrir 2 paquets) */ }
impl PesAssembler {
    pub fn new(pid: u16) -> Self;
    pub fn push<'a>(&mut self, pkt: &'a ScannedPacket<'a>, sink: &mut dyn MessageSink) -> Option<EsChunk<'a>>;
}   // CC : un doublon unique est ignoré, discontinuity_indicator ⇒ pas de trou, sinon gap_before + `ts-cc-gap` (pid, expected, got) ;
    // TEI ⇒ charge ignorée + gap ; scrambling ≠ 0 ⇒ charge ignorée + gap, `ts-scrambled` une fois par PID
pub const MAX_AU_BYTES: usize = 32 * 1024 * 1024;
pub fn read_es(src: &dyn ReadAt, format: PacketFormat, pid: u16, start: EsPos, end: EsPos, buf: &mut Vec<u8>) -> Result<(), IoError>;
    // rejoue PacketScanner + PesAssembler de start.packet_offset à end.packet_offset ; > MAX_AU_BYTES ⇒ IoError::InvalidData("au trop grande")
// testutil : rewrite_video_pes(ts: &[u8], pid: u16, f: impl FnOnce(Vec<OrigPes>) -> Vec<NewPes>) -> Vec<u8>
//   OrigPes / NewPes { pts, dts, payload } ; paquets vidéo réécrits aux positions d'origine, en gardant le champ d'adaptation (PCR) ;
//   paquets en surplus ⇒ nuls, paquets manquants ⇒ insérés après le dernier paquet vidéo
```

- Corpus dérivés :
  - `h264_720p_pes_split` : depuis `h264_720p_closed`, le PES n contient la 2e moitié de l'AU n et la 1re moitié de l'AU n+1, avec les PTS/DTS de l'AU n+1 ;
  - `h264_720p_pes_grouped` : depuis `h264_720p_nob`, 2 AU par PES, avec le PTS de la première seulement.

- [ ] **Step 1 : Tests qui échouent** :
  - `pes_header_pts_dts` ; `pes_header_split_over_two_packets` ;
  - `es_chunks_concatenate_to_ffmpeg_es` : `h264_720p_closed`, `hevc_720p_main10`, `mpeg2_576i_closed`, `h264_720p_pes_split` ⇒ la concaténation des `data` du PID vidéo égale `ffmpeg_es` ;
  - `read_es_roundtrip` : 50 couples `(EsPos, EsPos)` pris parmi les chunks ⇒ égal à la tranche correspondante de l'ES concaténé ;
  - `lost_packet_marks_gap` (Review Focus 1) : paquet vidéo retiré ⇒ `gap_before` sur le chunk suivant et `ts-cc-gap { expected: n, got: n+2 }` ; doublon unique ⇒ rien ; `discontinuity_indicator` ⇒ rien ;
  - `scrambled_payload_is_a_gap_not_data` ;
  - `read_es_rejects_oversized_au` ;
  - `#[covers("SYN.ROBUST.01")] pes_assembler_never_panics` (proptest).
- [ ] **Step 2–4 : échec, implémentation (et recettes), PASS** — Run: `cargo xtask corpus gen && cargo nextest run -p ma-ts --all-features`.
- [ ] **Step 5 : Commit** : `feat(ma-ts): assemblage PES, positions dans l'ES, relecture par plage, trous de continuité`.

---

### Task 9 : `ma-es` — découpage des AU et classification

**Files:**
- Create: `crates/ma-es/{Cargo.toml,src/lib.rs,src/framer.rs,src/startcode.rs,src/h264.rs,src/hevc.rs,src/mpeg2.rs}`, `crates/ma-es/tests/corpus.rs`

**Interfaces:**
- Consumes : `ma-bits` (lecture ; `writer` en dev), `ma-core` (`Codec`, `PictureType`) ; `ma-testkit::ffmpeg_es` (dev).
- Produces :

```rust
pub struct ParamSet(u8);                 // VPS, SPS, PPS, SEI, AUD, SEQ_HEADER, SEQ_EXT, GOP_HEADER
impl ParamSet { pub fn contains(self, p: ParamSet) -> bool; pub fn labels(self, codec: Codec) -> Vec<&'static str>; }   // ["VPS","SPS","PPS","SEI"] ; MPEG-2 ["SH","GH"]
pub enum Field { Frame, Top, Bottom }
pub enum RapClass { None, Idr, Cra, Bla, Recovery { frame_cnt: u32 }, Intra { closed_gop: bool, broken_link: bool } }
pub enum NalLeading { Skippable, Decodable }
pub struct AuMeta { pub picture: PictureType, pub rap: RapClass, pub leading: Option<NalLeading>, pub reference: bool,
                    pub params: ParamSet, pub field: Field, pub frame_num: Option<u32>, pub first_vcl_type: u8 }
pub struct AuBoundary { pub start: u64, pub end: u64, pub meta: AuMeta }   // offsets dans l'ES (octets poussés depuis le début)
pub trait AuFramer: Send {
    fn push(&mut self, data: &[u8], out: &mut Vec<AuBoundary>);
    fn gap(&mut self);                   // discontinuité : oublie l'état partiel, resynchronise au prochain code de début
    fn finish(&mut self, out: &mut Vec<AuBoundary>);
}
pub fn framer(codec: Codec) -> Box<dyn AuFramer>;
pub const MAX_HEADER_BYTES: usize = 4096;    // octets gardés par NAL (non-VCL) ; 64 pour une tranche
pub fn first_slice_payload_offset(codec: Codec, au: &[u8]) -> Option<usize>;   // repris de ma-testkit/nal.rs, utilisé par CorruptingSource
```

- **Règles** :
  - **début d'AU** : à l'offset du `00` qui précède `00 00 01` s'il existe (code de 4 octets), sinon à `00 00 01` ;
  - **H.264** : nouvelle AU sur NAL 9, 7, 8, 6, 14 à 18 après un NAL VCL de l'AU courante, ou sur un VCL (1, 5) dont `first_mb_in_slice == 0`. `reference = nal_ref_idc != 0`. `picture` vient du `slice_type` de la 1re tranche. SEI type 6 ⇒ `Recovery { frame_cnt: recovery_frame_cnt }` sur une I non IDR.
    - **Sous-ensemble SPS** (D3) : jusqu'à `frame_mbs_only_flag`, y compris `chroma_format_idc`, les profondeurs et les listes de scaling sautées, `log2_max_frame_num` et les branches `pic_order_cnt_type` 0 et 1.
    - **PPS** : `pps_id` et `sps_id`.
    - **Tranche** : `first_mb`, `slice_type`, `pps_id`, `colour_plane_id` si besoin, `frame_num`, puis `field_pic_flag` et `bottom_field_flag`.
    - SPS ou PPS manquant ⇒ `Field::Frame`, `frame_num: None`.
  - **HEVC** : nouvelle AU sur `first_slice_segment_in_pic_flag`, ou sur un NAL 32 à 35, 39, 41 à 44 ou 48 à 55 après un VCL.
    - 19 et 20 ⇒ `Idr` ; 21 ⇒ `Cra` ; 16 à 18 ⇒ `Bla` ; 8 et 9 ⇒ `Skippable` ; 6 et 7 ⇒ `Decodable`.
    - `reference = !(type ≤ 14 && type % 2 == 0)`.
    - `slice_type` lu avec le `num_extra_slice_header_bits` du PPS ; PPS inconnu ⇒ `I` pour une IRAP, sinon `P`.
  - **MPEG-2** : nouvelle AU sur `B3`, `B8` ou `00` après les tranches de l'image courante.
    - `picture_coding_type` donne le type ; `reference` = I ou P.
    - Une I précédée dans la même AU d'un SH ou d'un GH ⇒ `Intra { closed_gop, broken_link }`.
    - `picture_coding_extension` : `picture_structure` 1 ⇒ `Top`, 2 ⇒ `Bottom`, 3 ⇒ `Frame`.

- [ ] **Step 1 : Tests qui échouent** :
  - `framing_is_independent_of_chunking` : proptest, découpages aléatoires des 2 premiers Mo de l'ES H.264 et HEVC ⇒ mêmes frontières ;
  - `#[covers("DATA.PIC.01")] au_count_and_sizes_match_ffprobe` : `h264_720p_closed`, `hevc_720p_main10`, `mpeg2_576i_closed`, `h264_1080i_mbaff` ⇒ nombre d'AU = nombre de paquets ffprobe, taille de l'AU = `size` ffprobe, RAP ⇔ drapeau `key`. Si l'écart ne porte que sur des octets nuls de fin (`trailing_zero_8bits`), la comparaison les ignore, avec un commentaire ;
  - `hevc_open_gop_has_cra_and_rasl` ; `h264_recovery_points` (un seul `Idr`, puis des `Recovery`) ; `mpeg2_gop_flags` ;
  - `#[covers("NAV.THUMB.02")] params_present_in_au` : 1re AU de `hevc_720p_main10` ⇒ `labels` contient VPS, SPS, PPS ; MPEG-2 ⇒ SH, GH ;
  - `mbaff_is_one_au_per_frame` : `h264_1080i_mbaff`, 100 AU `Field::Frame` ;
  - `h264_paff_fields_are_detected` : SPS (`frame_mbs_only = 0`), PPS et deux tranches de champ synthétiques (`BitWriter`) ⇒ 2 AU `Top`/`Bottom`, même `frame_num` ;
  - `mpeg2_field_pictures_are_detected` ; `mpeg2_broken_link_is_reported` ; `hevc_bla_is_rap` (synthétiques) ;
  - `header_buffer_is_bounded` : un NAL de 10 Mo sans code de début ⇒ tampon interne ≤ `MAX_HEADER_BYTES` ;
  - `#[covers("SYN.ROBUST.01")] framers_never_panic` (proptest, 3 codecs).
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-es` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
- [ ] **Step 4 : PASS** — même commande.
- [ ] **Step 5 : Commit** : `feat(ma-es): découpage des AU H.264/HEVC/MPEG-2, RAP, images de tête, champs`.

---

### Task 10 : `ma-ts` — indexeur progressif

**Files:**
- Create: `crates/ma-ts/src/index.rs`, `crates/ma-ts/tests/corpus_index.rs`
- Modify: `crates/ma-ts/src/messages.rs`, `locales/{fr,en}/engine.ftl`

**Interfaces:**
- Consumes : Tâches 2, 3 (`RapKind::Recovery { pres_span }`, `Bla`), 5 à 9.
- Produces :

```rust
pub struct AuInfo { pub start: EsPos, pub end: EsPos, pub size: u32, pub pts: i64, pub dts: Option<i64>, pub pts_estimated: bool,
                    pub damaged: bool, pub params: ParamSet, pub pes_packet: u64, pub packet_index: u64, pub nal_type: u8, pub fields: u8 }   // ≤ 96 octets
pub struct PcrSample { pub packet_index: u64, pub offset: u64, pub pcr: i64 /* ticks 27 MHz déroulés */, pub discontinuity: bool }
pub struct Progress { pub bytes_done: u64, pub bytes_total: u64, pub complete: bool }
pub struct VideoIndex { /* entrées publiées, en ajout seul */ }
impl VideoIndex {
    pub fn pid(&self) -> u16; pub fn codec(&self) -> Codec; pub fn published(&self) -> u32;
    pub fn entry(&self, decode_idx: u32) -> Option<FrameEntry>; pub fn au(&self, decode_idx: u32) -> Option<AuInfo>;
    pub fn decode_idx_of(&self, pres_idx: u32) -> Option<u32>;
}
pub struct TsIndex { /* RwLock interne */ }
impl TsIndex {
    pub fn new(format: PacketFormat, bytes_total: u64) -> TsIndex;
    pub fn structure(&self) -> TsStructure; pub fn videos(&self) -> Vec<(u16, Codec)>;
    pub fn with_video<R>(&self, pid: u16, f: impl FnOnce(&VideoIndex) -> R) -> Option<R>;
    pub fn pcrs(&self, pcr_pid: u16) -> Vec<PcrSample>; pub fn progress(&self) -> Progress; pub fn format(&self) -> PacketFormat;
}
pub struct IndexOptions { pub chunk_size: usize /* 4 Mio */, pub publish_every: Duration /* 100 ms */ }
pub enum IndexError { Io(IoError), Cancelled }
pub fn index(src: &dyn ReadAt, start: u64, out: &TsIndex, opts: &IndexOptions, sink: &mut dyn MessageSink, cancel: &AtomicBool) -> Result<(), IndexError>;
pub struct IndexHandle { /* fil "ma-index" */ }
pub fn spawn_index(src: Arc<dyn ReadAt>, start: u64, out: Arc<TsIndex>, opts: IndexOptions, sink: Box<dyn MessageSink>) -> IndexHandle;
impl IndexHandle { pub fn cancel(&self); pub fn join(self) -> Result<(), IndexError>; }   // Drop : cancel puis join
```

- **Règles** :
  - **Passage et PID** : un seul passage. Tous les PID vidéo analysés de la structure courante sont indexés. Si `generation()` change, un nouveau PID vidéo ouvre un nouveau `VideoIndex`, et un PID retiré est figé. `cancel` est consulté à chaque bloc.
  - **PTS** : le PTS d'un PES s'applique à la 1re AU qui *commence* dans ce PES (H.222 §2.7.5). Une AU sans PTS (D2) reçoit le PTS de l'AU précédente en décodage + la médiane des 16 derniers écarts de PTS positifs (3600 par défaut), augmenté de 1 tant qu'il existe déjà. Elle est marquée `pts_estimated`, avec un message `ts-au-no-pts`. PTS et DTS sont déroulés par `TsUnwrapper`.
  - **Champs** : deux AU consécutives `Top`/`Bottom` complémentaires (même `frame_num` en H.264 ; MPEG-2 : la seconde suit immédiatement) forment une entrée. Alors `fields = 2`, `end` est celui de la seconde et le PTS celui de la première.
  - **RAP** : `RapClass` devient `RapKind`, avec `Intra { broken_link: true }` ⇒ `Bla`. Pour `Recovery { frame_cnt }`, R est la 1re AU, à partir du RAP, telle que le nombre d'AU `reference` dans [RAP, R) vaut `frame_cnt`. Alors `pres_span = pres(R) − pres(RAP)`, borné à 0. Un RAP dont R n'est pas encore vu n'est pas publiable.
  - **Images de tête** : HEVC d'après `leading`. H.264 et MPEG-2 : `pres < pres(RAP)` et `decode > decode(RAP)` ⇒ `Skippable`, sauf après `Idr` ou `Intra { closed_gop: true }` (`Decodable`).
  - **`damaged`** : toute AU qui chevauche un `gap_before`.
  - **PCR** : un `PcrSample` par PCR du PID de PCR de chaque programme.
  - **Préfixe publié** (à chaque `publish_every`) : L est le plus grand index de décodage d'un RAP r tel que :
    - un RAP postérieur à r a déjà été vu ;
    - max(PTS de [0, r)) < min(PTS de [r, vus)) ;
    - les `Recovery` de [0, r) sont résolus.

    Les entrées de [0, L) reçoivent `pres_idx` = rang du PTS dans [0, L). En fin de fichier, L couvre tout. **Une entrée publiée ne change plus.**

- [ ] **Step 1 : Tests qui échouent** :
  - `index_matches_ffprobe` : pour chaque fichier non `big` du corpus, `published()` égale `frames`, et les PTS en ordre de présentation égalent ceux de `ffprobe_frames` ;
  - `published_entries_never_change` : proptest, `chunk_size` aléatoire de 188 × [1, 64] et `publish_every = 0` ; chaque instantané est un préfixe exact de l'index final, `pres_idx` compris ;
  - `partial_index_ends_on_a_rap` ; `open_gop_cra_waits_for_next_rap` (`hevc_720p_open_gop`) ;
  - `#[covers("NAV.ORACLE.03")] recovery_span_from_reference_count` : `h264_720p_recovery`, chaque `Recovery` a `pres_span == pres(R) − pres(RAP)`, et `pres_span ≤ gop` ;
  - `pts_wrap_keeps_presentation_order` (Review Focus 3) : `h264_720p_pts_wrap`, 250 images, PTS strictement croissants en présentation, dernier > 2^33 ;
  - `pcr_samples_are_unwrapped` : même fichier, PCR croissants ;
  - `au_without_pts_gets_estimated_pts` : `h264_720p_pes_grouped`, PTS uniques, la moitié `pts_estimated`, ordre de présentation = ordre de décodage, 50 messages `ts-au-no-pts` ;
  - `field_pairs_make_one_entry` (MPEG-2 synthétique via `testutil`) ;
  - `lost_packet_damages_only_its_au` (Review Focus 1) : un paquet retiré au milieu de l'AU 100 de `h264_720p_closed` (en mémoire) ⇒ seule `au(100).damaged`, `published()` inchangé ;
  - `truncated_during_indexing_keeps_partial_index` (Review Focus 2) : `TruncatingSource` à 50 % ⇒ `Err(Io(Truncated))`, `published() > 0`, entrées égales au préfixe de l'index complet ;
  - `#[covers("PERF.JOB.01")] cancel_stops_within_500ms` : `hevc_720p_main10` derrière `LatencySource` (5 ms par lecture, `chunk_size` 64 Kio) ; `cancel` à 100 ms ⇒ `join` en < 500 ms, `Err(Cancelled)`, entrées publiées lisibles ;
  - `#[covers("PERF.INDEX.02")] indexing_uhd_big_memory_under_200_mib` : `VmHWM` (`/proc/self/status`) avant et après indexation de `uhd_big` ⇒ écart < 200 Mio (nextest exécute chaque test dans son propre processus) ;
  - `au_info_is_compact` : `size_of::<AuInfo>() <= 96` ;
  - `multiple_videos_indexed` (`mpts_3prog`) ; `pmt_change_starts_new_video_index` (Review Focus 5, synthétique).
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo nextest run -p ma-ts --all-features`.
- [ ] **Step 5 : Commit** : `feat(ma-ts): indexeur progressif (préfixe stable, reprise, champs, PCR, mémoire bornée)`.

---

### Task 11 : `TsAuSource` et remplacement de la source de test

**Files:**
- Create: `crates/ma-ts/src/source.rs`, `crates/ma-ts/tests/source.rs`
- Modify:
  - `crates/ma-ffmpeg/{Cargo.toml,tests/common/mod.rs,tests/modes.rs,tests/scale.rs,tests/oracle.rs,examples/engine_bench.rs}` ;
  - `tools/transport-bench/src-tauri/{Cargo.toml,src/lib.rs}` ;
  - `crates/ma-testkit/{src/lib.rs,src/corrupt.rs}` (utilise `ma_es::first_slice_payload_offset`).
- Delete: `crates/ma-testkit/src/ts_source.rs`, `crates/ma-testkit/src/nal.rs` (leurs tests de corpus passent dans `ma-ts`)

**Interfaces:**
- Consumes : Tâches 5, 8, 10 ; contrat `AuSource` (Tâche 2).
- Produces :

```rust
pub struct TsAuSource { /* Arc<TsIndex>, pid, codec, Arc<BlockCache<FileSource>> */ }
impl TsAuSource {
    pub fn open(path: &Path) -> Result<TsAuSource, OpenError>;     // probe + indexation complète, 1re vidéo analysée dans l'ordre de la PMT (STRUCT.TREE.06)
    pub fn open_progressive(path: &Path) -> Result<(TsAuSource, IndexHandle), OpenError>;   // rend la main dès qu'une image est publiée, ou à la fin
    pub fn for_video(index: Arc<TsIndex>, io: Arc<BlockCache<FileSource>>, pid: u16) -> Option<TsAuSource>;
    pub fn index(&self) -> &Arc<TsIndex>; pub fn pid(&self) -> u16; pub fn au_info(&self, decode_idx: u32) -> Option<AuInfo>;
    pub fn io_stats(&self) -> IoStats;
}
impl AuSource for TsAuSource   // len = published() ; read_au = read_es(start, end) ; une AU `damaged` est lue telle quelle
```

  - `engine_bench` gagne `--progressive` : il mesure le temps jusqu'à la 1re image, la durée d'indexation et les octets lus rapportés à la taille du fichier, dans le JSON. `ma_testkit::MAX_AU_BYTES` devient `ma_ts::MAX_AU_BYTES`.

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("DATA.TS.01")] pts_dts_pos_match_ffprobe` : `h264_720p_closed`, `hevc_720p_main10`, `mpeg2_576i_closed`, `h264_720p_pes_split` ⇒ PTS et DTS de chaque AU égaux à ceux de ffprobe ; `pes_packet` = `pos` ffprobe quand l'AU commence un PES ;
  - `#[covers("NAV.THUMB.01")] picture_types_and_indices_match_ffprobe` : appariement par PTS, `pict_type` et `key` égaux, `pres_idx` = rang dans `ffprobe_frames` ;
  - `au_bytes_match_ffmpeg_es` : concaténation de `read_au` sur toutes les AU = `ffmpeg_es` ;
  - `read_amplification_is_bounded` : `read_au` séquentiel sur `hevc_720p_main10` ⇒ `bytes_read ≤ 1,1 × taille + 1 bloc` ;
  - `progressive_open_returns_before_full_index` (`uhd_big` si présent) : `len() > 0` et `progress().complete == false` au retour ;
  - (`ma-ffmpeg`) tous les tests de M1 passent sur `ma_ts::TsAuSource` sans changer leurs assertions : NAV.ORACLE.01 à 04, PERF.HW.01, PERF.XFER.01 ;
  - (`ma-ffmpeg`) `#[covers("NAV.ORACLE.01")] oracle_on_split_pes_mbaff_and_pts_wrap` : 30 sauts, 10 pas avant et 10 arrière sur `h264_720p_pes_split`, `h264_1080i_mbaff` et `h264_720p_pts_wrap` en `SoftwareOnly` ⇒ numéro incrusté correct ;
  - (`ma-ffmpeg`) `#[covers("NAV.ORACLE.02")] open_gop_rasl_frames_correct` (repris) ;
  - (`ma-ffmpeg`) `#[covers("NAV.ORACLE.01")] oracle_framemd5_private` : pour chaque fichier de `private_corpus()`, 20 images au hasard (graine fixe), décodées en `SoftwareOnly` 1:1 à la profondeur native ⇒ MD5 des plans égal à `ffmpeg_framemd5` pour le même PTS ;
  - (`ma-ffmpeg`) `#[covers("PERF.NET.01")] truncation_during_navigation_is_an_error_not_a_crash` (Review Focus 2) : copie dans un répertoire temporaire, `FrameService`, troncature à 50 % ⇒ image au-delà = `FrameError::Io`, image avant = OK, le service répond encore.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-ts -p ma-ffmpeg -p ma-testkit` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter** `TsAuSource`, migrer les appelants, supprimer `ts_source.rs` et `nal.rs`.
- [ ] **Step 4 : PASS** :
  - Run: `cargo nextest run --workspace` puis `cd tools/transport-bench/src-tauri && cargo build` ; Expected : PASS et le banc compile ;
  - Run: `grep -rn "TsAuSource" crates/ma-testkit` ; Expected : rien.
- [ ] **Step 5 : Commit** : `feat(ma-ts): source d'AU réelle ; la source de test « 1 PES = 1 AU » est retirée`.

---

### Task 12 : `ma-ffmpeg` — corrections reportées de M1

**Files:**
- Modify: `crates/ma-ffmpeg/src/{decoder.rs,gpu_scale.rs}`, `crates/ma-decode/src/{decoder.rs,navigator.rs,fake.rs}`, `crates/ma-ffmpeg/tests/{modes.rs,scale.rs}`, `tools/transport-bench/src-tauri/src/lib.rs` (nom du nouveau chemin)
- Create: `crates/ma-ffmpeg/src/refusal.rs`

**Interfaces:**
- Consumes : `Navigator`, `FakeFactory` (M1) ; `TsAuSource` (Tâche 11).
- Produces :
  - `FfmpegDecoder` ouvert sans périphérique ⇒ `mode() == Software` dès l'ouverture. `Unknown` ne veut plus dire que « matériel attaché, pas encore confirmé ».
  - `trait Decoder { fn silent_software(&self) -> bool { false } }` : vrai quand le décodeur a été ouvert avec un périphérique et sort des images logicielles (repli silencieux de FFmpeg, un seul thread). Navigator : à la 1re image où c'est vrai, réouverture **une seule fois** en `SoftwareOnly`, `reset` et nouvel essai depuis `start_for`. Aucun `FellBackToSoftware`, car le mode réel était déjà logiciel. `NavStats::software_reopens: u32` est ajouté. `FakeFactory { silent_sw: bool, .. }`.
  - `ConvertPath::VaapiVppRefused` : la VPP a refusé ce viewport, d'où projection et CPU. `VaapiMapped` ne désigne plus que « pilote sans VPP ».
  - `refusal.rs` : `pub struct RefusalMemo` ; `fn refuses(&self, vp: &Viewport) -> bool` ; `fn failed(&mut self, vp: Viewport)` ; `fn succeeded(&mut self, vp: &Viewport)` ; `fn tick(&mut self)` (une conversion). Un viewport n'est refusé qu'après **2 échecs consécutifs**, et il est réessayé après **250 conversions**. Au plus 16 entrées.

- [ ] **Step 1 : Tests qui échouent** :
  - (`ma-decode`) `silent_software_fallback_reopens_once_without_event` : `silent_sw = true` ⇒ `software_reopens == 1`, aucun `FellBackToSoftware`, oracle correct sur 20 images ;
  - `#[covers("PERF.HW.01")] no_device_reports_software_from_open` : `FfmpegFactory { render_node: Some("/dev/dri/absent") }` ⇒ `mode() == Software` avant toute image ; Navigator sur `h264_720p_closed` : 0 `FellBackToSoftware` ;
  - `#[covers("PERF.HW.01")] unsupported_profile_reopens_multithread` (garde VA-API) : `h264_720p_high10` en `Auto` ⇒ `mode() == Software`, `software_reopens == 1`, aucun `FellBackToSoftware`, numéro correct ; `latency() > 1` si `available_parallelism() > 1` ;
  - `refusal_memo_needs_two_failures` ; `refusal_memo_retries_after_250_conversions` ; `success_resets_failure_count` ;
  - `vpp_refused_path_is_distinct` (garde VPP) : viewport de 8×8 ⇒ `stats().path == VaapiVppRefused` ;
  - `#[covers("PERF.XFER.01")] fit_view_has_no_full_res_transfer` (repris, inchangé) ;
  - `corrupted_au_vaapi_does_not_fall_back` (garde VA-API) : `CorruptingSource` sur une P au milieu d'un GOP de `hevc_720p_main10`, en `Auto` ⇒ aucun `FellBackToSoftware`, `mode() == Hardware`, images avant l'AU correctes ;
  - `crop_top_1080p_vaapi_matches_software` (garde VPP) : `h264_1080p_crop_top`, 5 images, 1:1 sur l'image entière ⇒ taille 1920×1080 dans les deux modes, plan de luma identique octet pour octet, numéro correct.
- [ ] **Step 2 : Vérifier l'échec** — Run: `MA_REQUIRE_GPU=1 MA_REQUIRE_VPP=1 cargo nextest run -p ma-decode -p ma-ffmpeg` ; Expected : FAIL.
- [ ] **Step 3 : Implémenter.**
  - **Si `corrupted_au_vaapi_does_not_fall_back` échoue** sur la machine de référence (pilote en `EIO` sur une donnée corrompue), la règle du Navigator devient : une erreur `Gpu` en matériel déclenche d'abord **un** nouvel essai avec un décodeur matériel neuf depuis `start_for`. Seule une 2e erreur `Gpu` consécutive provoque le repli. `gpu_error_mid_navigation_falls_back_without_losing_frame` doit rester vert. Consigner le cas dans 07.
  - **Si `crop_top_1080p_vaapi_matches_software` échoue** (rognage haut perdu par `crop_*`), `GpuScaler` ajoute le rognage du décodeur (`crop_top`, `crop_left`) à la région avant de l'écrire dans `crop_*`.
- [ ] **Step 4 : PASS** — même commande, puis `cargo xtask ci`.
- [ ] **Step 5 : Couverture** — Run: `cargo llvm-cov nextest -p ma-ffmpeg -p ma-decode --summary-only` ; Expected : ≥ 90 % chacune.
- [ ] **Step 6 : Commit** : `fix(ma-ffmpeg): mode logiciel sans périphérique, réouverture multi-thread, mémo VPP tolérant, rognage`.

---

### Task 13 : Fuzzing des parseurs TS et ES

**Files:**
- Create: `fuzz/{Cargo.toml,fuzz_targets/ts_scan.rs,fuzz_targets/ts_index.rs,fuzz_targets/es_h264.rs,fuzz_targets/es_hevc.rs,fuzz_targets/es_mpeg2.rs}`, `xtask/src/fuzz.rs`
- Modify: `Cargo.toml` racine (`exclude = ["spike", "tools/transport-bench", "fuzz"]`), `xtask/src/{main.rs,ci.rs}`, `BUILD.md` (§2 : `rustup toolchain install nightly`, `cargo install cargo-fuzz --locked`)

**Interfaces:**
- Consumes : `PacketScanner`, `StructureBuilder`, `PesAssembler` (Tâches 6 à 8), `index` sur `MemSource` (Tâche 10), `framer` (Tâche 9).
- Produces :
  - `cargo xtask fuzz [--time <s>] [--target <nom>]`, par défaut 600 s et toutes les cibles. Il lance `cargo +nightly fuzz run <cible> -- -max_total_time=<s>` avec un corpus d'amorce : les 64 premiers Kio des fichiers du corpus généré, ou l'ES pour les cibles `es_*`. Il échoue si un artefact de plantage apparaît.
  - Étape `fuzzing court` de `cargo xtask ci`, après les tests : 20 s par cible si `cargo +nightly --version` réussit, sinon `nightly absent : fuzzing court non exécuté` (avertissement, D5).

- [ ] **Step 1 : Test qui échoue** (`xtask`) : `fuzz_targets_match_directory` (la liste des cibles égale les fichiers de `fuzz/fuzz_targets/`) ; `nightly_detection_parses_version`.
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo nextest run -p xtask`.
- [ ] **Step 5 : Sessions longues** (machine de référence) — Run: `cargo xtask fuzz --time 600` ; Expected : aucune panique sur les 5 cibles. Durées et nombre d'exécutions notés pour 07. Toute panique trouvée est corrigée avec un test de non-régression dans la crate concernée.
- [ ] **Step 6 : Commit** : `test(fuzz): cibles cargo-fuzz TS et ES, cargo xtask fuzz, fuzzing court en CI`.

---

### Task 14 : Mesures, rapport et statuts

**Files:**
- Create: `tools/nfs-bench/{run.sh,README.md}`, `docs/07-mesures-m2a.md`
- Modify:
  - `requirements/mvp/structure.yaml` (STRUCT.OPEN.01, STRUCT.TREE.03), `navigation.yaml` (NAV.ORACLE.02, NAV.ORACLE.03), `performance.yaml` (PERF.INDEX.02) : `status: implemented` ;
  - `docs/05-feuille-de-route-mvp.md` (lignes M2a et M2b, état) ;
  - `docs/06-mesures-m1.md` §6 (renvois vers 07 pour les mesures faites).

**Interfaces:**
- Consumes : `engine_bench --progressive` (Tâche 11), `tools/transport-bench`, `cargo xtask fuzz`.
- `tools/nfs-bench/run.sh` (D6, `sudo` demandé explicitement par le script) :
  1. serveur NFSv4 en conteneur Docker `--privileged` qui exporte `$MA_CORPUS_DIR` ;
  2. `tc qdisc add dev <pont docker> root netem delay 5ms` ;
  3. montage dans `/mnt/ma-nfs` ;
  4. `engine_bench --progressive` sur `uhd_big` en `auto` et `sw` ;
  5. démontage et nettoyage, même en cas d'échec (`trap`).

- [ ] **Step 1 : Mesurer sur NFS** : 3 exécutions par mode.
  - Mesures publiées : 1re image, durée d'indexation, saut p95, pas suivant et précédent p95, octets lus rapportés à la taille du fichier.
  - Troncature manuelle côté serveur pendant un parcours : le banc affiche l'erreur et reste vivant.
- [ ] **Step 2 : Mesurer sur le corpus privé** (D7), s'il est présent :
  - structure des GOP (longueurs, CRA, RASL, BLA, champs), en part des images ;
  - résultat de `oracle_framemd5_private` ;
  - `engine_bench` (saut, pas) en `auto` et `sw` ;
  - fumée du banc interface (10 sauts, 10 pas de chaque côté).
- [ ] **Step 3 : Rédiger `07-mesures-m2a.md`** sur le modèle de 06 :
  - verdict ;
  - local face à NFS (comparaison avec les seuils de PERF.OPEN.01 et PERF.SEEK.01, publiés sans jugement e2e) ;
  - amplification de lecture face au spike (770 Ko par AU de 65 Ko) ;
  - flux réels ;
  - fuzzing (Tâche 13) ;
  - limites : AU sans PTS estimées (D2), PAFF prouvé ou non (D3), 4:2:2 non supporté, cas `EIO` en VA-API (Tâche 12) ;
  - mesures restant à faire.
- [ ] **Step 4 : Statuts et feuille de route** : passer les 5 exigences en `implemented`. Dans 05, remplacer la ligne M2 par M2a et M2b avec leur périmètre, et noter « M2a : implémenté, mesures dans 07 ».
- [ ] **Step 5 : Vérifier la garde** — Run: `cargo xtask ci` ; Expected : vert, et les 5 exigences ✓ dans `target/trace/matrix.md`.
- [ ] **Step 6 : Commit** : `docs: mesures de M2a (NFS, flux réels), statuts des exigences, feuille de route M2a/M2b`.
