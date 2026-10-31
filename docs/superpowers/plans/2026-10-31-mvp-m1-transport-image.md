# MVP — M1 Transport d'image : plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Lever les deux conditions du *go* du spike. D'abord, n'envoyer à l'interface que ce qui est affiché, réduit sur le GPU. Ensuite, précharger les images voisines pour supprimer l'arrêt en arrière. Le tout avec un repli logiciel fiable, et des mesures côté interface présentées au porteur (point de passage n° 2).

**Architecture:** Deux crates moteur. `ma-decode` est sans `unsafe` et sans FFmpeg. Elle porte le contrat (trait `Decoder`, entrée `AuSource`, `Viewport`, modèle de blocs vide), le plan d'accès aléatoire, le `Navigator` (décodage continu, fenêtres, cache, tolérance, repli) et le `FrameService` (fil de préchargement prioritaire). Elle se teste entièrement avec un décodeur simulé. `ma-ffmpeg` est la crate de liaison, la seule où `unsafe` est autorisé. Elle implémente `Decoder` sur libavcodec (logiciel et VA-API), avec une réduction et un recadrage VA-API VPP (libavfilter `scale_vaapi`) ou libswscale. Une crate de test `ma-testkit` fournit une source d'AU minimale sur TS, le lecteur du numéro incrusté et l'accès au corpus. Un banc Tauri hors workspace (`tools/transport-bench`) mesure côté interface.

**Tech Stack:** Rust stable (édition 2024), rsmpeg 0.18 (`link_system_ffmpeg`, features `ffmpeg6`/`ffmpeg7`/`ffmpeg8`), libavcodec / libavfilter / libswscale / libva, `crossbeam-channel`, `thiserror`. Corpus : FFmpeg + libx264/libx265. Banc : Tauri 2, TypeScript, WebGL2, WebdriverIO + tauri-driver, Xvfb.

**Spec:** `docs/04-resultats-spike.md` (§1, §3, §4), `docs/02-stack-technique.md` (§3.3, §4.2, §5, §6.1), `docs/05-feuille-de-route-mvp.md` (ligne M1), `requirements/mvp/performance.yaml` (PERF.XFER.01, PERF.PREFETCH.01, PERF.HW.01, PERF.STEP.01/02, PERF.SEEK.01), `requirements/mvp/navigation.yaml` (NAV.ORACLE.01–04). Code de référence (principes seulement) : branche `spike/stack-validation`, `spike/spike-core/src/{decode,cache}.rs`.

## Global Constraints

- Les règles de M0 s'appliquent : GPL-3.0-or-later, édition 2024, `clippy -D warnings` (pedantic), `cargo deny`, **90 % des lignes** pour `crates/ma-*`, `cargo xtask ci` vert, textes et documents en français, identifiants en anglais.
- `unsafe` **uniquement dans `ma-ffmpeg`**. Cette crate surcharge le lint du workspace (`unsafe_code = "allow"`), porte `#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]`, et chaque bloc a un commentaire `// SAFETY:`. Miri ne s'applique pas à du FFI : la sûreté repose sur ce confinement et sur les tests.
- **Le moteur ne démultiplexe pas avec FFmpeg** (02 §3.3). La source d'AU sur TS de `ma-testkit` sert **uniquement aux tests et aux bancs**. Elle fait l'hypothèse « 1 PES = 1 AU », valable pour le corpus généré par FFmpeg. M2 la remplace par `ma-ts`, qui fournira la même interface `AuSource`.
- Seuils (performance.yaml) : image suivante p95 < 100 ms (Ajusté et 100 %) ; image précédente p95 < 100 ms et max < 250 ms ; saut p95 < 500 ms en VA-API ; < 4 Mo par image au zoom Ajusté sur 1920×1080 ; 8 images suivantes et 8 précédentes prêtes après 1 s ; aucun transfert GPU → RAM pleine résolution au zoom Ajusté.
- **Format de sortie** : si la taille de sortie est inférieure à la région, l'image est en **8 bits** (réduction : l'image n'est qu'approchée). Si elle est égale (1:1, zoom ≥ 100 %), l'image est à la **profondeur native**, octet pour octet identique au décodage. Pas d'agrandissement côté moteur : l'interface agrandit au plus proche voisin.
- Machine de référence : NUC i7-10710U, Comet Lake UHD, pilote iHD, Ubuntu 24.04, FFmpeg 6.1.
- **Tests dépendant du GPU** : sans périphérique VA-API (`/dev/dri/renderD*`), ils s'arrêtent en affichant `VA-API absent : test non exécuté`. Si `MA_REQUIRE_GPU=1`, ils échouent à la place. `cargo xtask ci` fixe `MA_REQUIRE_GPU=1` dès qu'un périphérique existe. **Tests dépendant du corpus** : ils échouent avec `corpus absent : lancer cargo xtask corpus gen`. Jamais d'abandon silencieux.
- Statuts en fin de M1 : `PERF.PREFETCH.01` passe en `implemented` (seul niveau `unit`). Les autres exigences touchées restent `validated` : leurs tests `#[covers]` existent, mais leurs autres niveaux (`ui`, `e2e`) ou leur source réelle (`ma-ts`) arrivent en M2, M6 et M7.

## Review Focus

1. **Viewport incohérent** (région hors image, vide, coordonnées impaires, sortie plus grande que la région) : il est normalisé (aligné sur 2, borné à l'image, sortie bornée à la région) ou refusé par `ViewportError`, jamais de panique. Test : Tâche 2, `viewport_normalization`.
2. **Changement de zoom pendant un préchargement** : les images préchargées pour l'ancien viewport ne sont jamais servies pour le nouveau. Le préchargement repart sur le nouveau viewport. Test : Tâche 6, `zoom_change_restarts_prefetch_on_new_viewport`.
3. **AU corrompue au milieu d'un GOP** : les autres images du GOP sont livrées. Celles qui dépendent de l'AU corrompue sont livrées marquées `corrupt` ou déclarées non décodables, sans échec de tout le GOP (leçon du spike). Tests : Tâche 4, `corrupt_au_does_not_fail_whole_gop` ; Tâche 8, `corrupted_au_real_decoder`.
4. **Bords du flux** : la dernière image (vidage du décodeur en fin de flux), les images avant le premier RAP et les RASL du premier CRA (déclarées non décodables, avec la raison). Tests : Tâche 3 et Tâche 4, `last_frame_is_served_after_drain`.
5. **Erreur GPU pendant un préchargement** (et non pendant une demande explicite) : le repli logiciel a lieu une seule fois. La demande explicite en attente est servie, et l'événement de repli est émis une seule fois. Test : Tâche 6, `gpu_failure_during_prefetch_falls_back_once`.

---

## Structure des fichiers

```
crates/ma-decode/                 # contrat + logique, sans unsafe ni FFmpeg
  src/lib.rs                      # réexports
  src/types.rs                    # Codec, FrameEntry, RapKind, Leading, AuSource, Rect, Size, Viewport, Planes, PixelLayout, DecodedFrame
  src/blocks.rs                   # modèle de blocs générique (vide en MVP)
  src/decoder.rs                  # trait Decoder, DecoderFactory, DecodeError, DecodeMode, Received, DecodeEvent
  src/plan.rs                     # choix du point de départ (RAP, RASL, recovery point)
  src/navigator.rs                # décodage continu, fenêtres, cache LRU, tolérance, repli
  src/service.rs                  # FrameService : fil de travail, priorités, préchargement
  src/fake.rs                     # FakeSource / FakeDecoder / FakeFactory (cfg(test) ou feature "fake")
crates/ma-ffmpeg/                 # liaison FFmpeg (seule crate avec unsafe)
  src/lib.rs  src/packet.rs  src/hw.rs  src/decoder.rs  src/gpu_scale.rs  src/cpu_scale.rs  src/planes.rs
  tests/{oracle,modes,scale}.rs
  examples/engine_bench.rs
crates/ma-testkit/                # aide aux tests (dev-dependency uniquement)
  src/lib.rs  src/corpus.rs  src/ts_source.rs  src/nal.rs  src/frame_code.rs  src/corrupt.rs
xtask/src/corpus.rs               # cargo xtask corpus gen [--big]
tools/transport-bench/            # banc Tauri hors workspace (exclu comme spike/)
docs/06-mesures-m1.md             # rapport de mesures (point de passage n° 2)
```

---

### Task 1 : Corpus généré (`cargo xtask corpus gen`)

**Files:**
- Create: `xtask/src/corpus.rs`, `corpus/manifest.toml`
- Modify: `xtask/src/main.rs` (sous-commande `corpus gen [--big] [--out <dir>]`), `xtask/src/ci.rs` (étape `corpus gen` avant les tests), `BUILD.md` (outils de test : `ffmpeg` avec libx264/libx265, `ffprobe` ; ajouter `libavfilter-dev` / `libavdevice-dev` / `libswresample-dev` aux dépendances apt, ils sont inclus dans `ffmpeg-devel` sous Fedora), `.github/workflows/ci.yml` (paquets correspondants + `ffmpeg`)

**Interfaces:**
- Produces : répertoire `$MA_CORPUS_DIR` (par défaut `~/media-analyzer-corpus/generated`). `corpus/manifest.toml` est versionné et décrit chaque fichier : `name`, `file`, `codec`, `profile`, `size` (`WxH`), `fps`, `frames`, `gop`, `code_bits = 16`, `code_modulo`, `features` (liste), `big` (bool). La génération est idempotente : un fichier présent et non vide n'est pas régénéré.
- **Numéro incrusté** : 16 bits sur toute la largeur. Le bloc i fait `W/16` de large, la bande fait `H/24` de haut, bit 0 à gauche, blanc = 1, chroma neutre dans la bande. La valeur incrustée est `n mod code_modulo` (n = index de présentation).

| name | contenu |
|---|---|
| `h264_720p_closed` | H.264 High 8 bits, 1280×720, 25 i/s, 250 images, GOP 25, 2 B, GOP fermé |
| `hevc_720p_main10` | HEVC Main 10, 1280×720, 50 i/s, 250 images, GOP 50, 3 B, GOP fermé |
| `hevc_720p_open_gop` | HEVC Main 10, `open-gop=1` (CRA + RASL), 250 images, GOP 50 |
| `h264_720p_recovery` | H.264 `open-gop=1` : I non IDR avec SEI recovery point après la première image, 250 images |
| `mpeg2_576i_closed` | MPEG-2 MP@ML, 720×576, 25 i/s, 125 images, GOP 12 |
| `h264_720p_high10` | H.264 High 10 (profil non supporté en VA-API sur Comet Lake), 50 images |
| `hevc_720p_cut` | `hevc_720p_main10` tronqué au paquet 188 près au milieu du 2e GOP, puis coupé au début pour commencer au milieu d'un GOP |
| `uhd_big` (`--big`) | Recette du spike : HEVC Main 10 3840×2160 50 i/s, GOP 50, 3 B, 25 Mb/s, ≥ 4 Go par bouclage, `code_modulo = 1000` |

- [ ] **Step 1 : Implémenter** `corpus.rs`. Commandes `ffmpeg`, filtre `geq` (gabarit : `gen-corpus.sh` du spike, porté à 16 bits), vérification après génération par `ffprobe` (codec, profil, nombre d'images), erreur claire si `ffmpeg` ou un encodeur manque.
- [ ] **Step 2 : Vérifier.**
  Run: `cargo xtask corpus gen && ffprobe -v error -show_entries stream=codec_name,profile -of csv $HOME/media-analyzer-corpus/generated/hevc_720p_open_gop.ts`
  Expected : 7 fichiers créés, profil `Main 10` ; une 2e exécution ne régénère rien. Vérifier aussi la présence de NAL CRA (21) et RASL (8/9) avec `ffprobe -show_frames`, ou le test de la Tâche 7.
- [ ] **Step 3 : Générer le gros fichier** (machine de référence seulement). Run: `cargo xtask corpus gen --big` ; Expected : `uhd_big.ts` ≥ 4 Go.
- [ ] **Step 4 : Commit** : `feat(xtask): corpus généré avec numéro d'image incrusté`.

---

### Task 2 : `ma-decode` — contrat et types

**Files:**
- Create: `crates/ma-decode/{Cargo.toml,src/lib.rs,src/types.rs,src/blocks.rs,src/decoder.rs}`
- Test: modules `tests` de `types.rs` et `blocks.rs`

**Interfaces:**
- Produces (tous `pub`, réexportés à la racine) :

```rust
pub enum Codec { H264, Hevc, Mpeg2 }
pub enum PictureType { I, P, B }
pub enum RapKind { None, Idr, Cra, Bla, Recovery { frame_cnt: u32 }, Intra { closed_gop: bool } }
pub enum Leading { None, Skippable /* RASL ou équivalent : dépend du RAP précédent */, Decodable /* RADL */ }
pub struct FrameEntry { pub decode_idx: u32, pub pres_idx: u32, pub pts: i64, pub picture: PictureType, pub rap: RapKind, pub leading: Leading }
pub trait AuSource: Send + Sync {
    fn codec(&self) -> Codec;
    fn len(&self) -> u32;                                   // nombre d'images (ordre de décodage)
    fn entry(&self, decode_idx: u32) -> Option<FrameEntry>;
    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32>;
    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> std::io::Result<()>;
}
pub struct Rect { pub x: u32, pub y: u32, pub w: u32, pub h: u32 }
pub struct Size { pub w: u32, pub h: u32 }
pub struct Viewport { pub region: Rect, pub out: Size }      // Eq + Hash (clé de cache)
impl Viewport {
    pub fn fit(frame: Size, area: Size) -> Viewport;        // image entière, réduite pour tenir dans `area`, ratio conservé
    pub fn region_1to1(frame: Size, region: Rect) -> Viewport;
    pub fn normalized(self, frame: Size) -> Result<Viewport, ViewportError>;
    pub fn is_reduced(&self) -> bool;                       // out < region
}
pub enum PixelLayout { Planar8, Planar10 /* u16 LE, bits de poids faible */, SemiPlanar8 /* NV12 */, SemiPlanar10 /* P010, bits de poids fort */ }
pub struct Planes { pub layout: PixelLayout, pub size: Size, pub planes: Vec<Vec<u8>>, pub strides: Vec<u32> }  // compactées
impl Planes { pub fn byte_len(&self) -> usize; }
pub struct DecodedFrame { pub pres_idx: u32, pub viewport: Viewport, pub image: Planes, pub corrupt: bool, pub blocks: Option<BlockTree> }
```

  - `blocks.rs` : `pub struct BlockTree { pub nodes: Vec<BlockNode> }` ; `pub struct BlockNode { pub rect: Rect, pub kind: Option<BlockKind>, pub attrs: Vec<(BlockAttr, i32)>, pub children: std::ops::Range<u32> }` ; `pub enum BlockKind { Intra, Inter, Skip }` ; `pub enum BlockAttr { Qp, MvX, MvY, RefIdx }`. Toujours `None` en MVP. `#[non_exhaustive]` sur les deux enums.
  - `decoder.rs` :

```rust
pub enum DecodeMode { Unknown, Hardware, Software }
pub enum ModePref { Auto, SoftwareOnly }
pub enum DecodeErrorKind { InvalidData, Gpu, Unsupported, Device, Internal }
pub struct DecodeError { pub kind: DecodeErrorKind, pub detail: String }          // thiserror
pub enum Received { NeedInput, Drained, Discarded { pts: i64 }, Frame { pts: i64, image: Planes, corrupt: bool } }
pub struct DecoderStats { pub converted: u64, pub full_res_transfers: u64 }
pub trait Decoder: Send {
    fn mode(&self) -> DecodeMode;                           // déduit des images produites
    fn latency(&self) -> u32;                               // images de retard du pipeline
    fn send(&mut self, pts: i64, au: &[u8]) -> Result<(), DecodeError>;
    fn send_eof(&mut self) -> Result<(), DecodeError>;
    fn receive(&mut self, want: &dyn Fn(i64) -> Option<Viewport>) -> Result<Received, DecodeError>;
    fn reset(&mut self);
    fn stats(&self) -> DecoderStats;
}
pub trait DecoderFactory: Send + Sync {
    fn open(&self, codec: Codec, pref: ModePref) -> Result<Box<dyn Decoder>, DecodeError>;
}
pub enum DecodeEvent { FellBackToSoftware { reason: String, at_pres: Option<u32> }, CorruptAu { decode_idx: u32, detail: String } }
```

  Dans `receive`, `want(pts)` renvoie `Some(viewport)` pour convertir l'image (réduite ou recadrée) et `None` pour la jeter sans conversion ni transfert.

- [ ] **Step 1 : Tests qui échouent** :
  - `viewport_fit_keeps_aspect_ratio` : `fit(3840×2160, 1920×1080)` ⇒ région 3840×2160, sortie 1920×1080 ; `fit(3840×2160, 1000×1000)` ⇒ sortie 1000×562 (dimensions arrondies au pair inférieur) ;
  - `viewport_normalization` (Review Focus 1) : région (1,1,9,9) sur 1280×720 ⇒ (0,0,10,10) ; région débordante ⇒ bornée ; région vide ⇒ `ViewportError::Empty` ; sortie > région ⇒ bornée à la région ;
  - `is_reduced` : vrai pour la réduction, faux en 1:1 ;
  - `planes_byte_len` : NV12 1920×1080 = 3 110 400 octets (< 4 Mo) ;
  - `block_tree_default_is_empty`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo nextest run -p ma-decode` ; Expected : FAIL (compilation).
- [ ] **Step 3 : Implémenter** les types (`Debug, Clone, PartialEq, Eq` partout ; `Hash` pour `Rect`, `Size`, `Viewport`).
- [ ] **Step 4 : Tout passe** — même commande ; Expected : PASS.
- [ ] **Step 5 : Commit** : `feat(ma-decode): contrat de décodage, viewport et modèle de blocs`.

---

### Task 3 : `ma-decode` — plan d'accès aléatoire

**Files:**
- Create: `crates/ma-decode/src/plan.rs`, `crates/ma-decode/src/fake.rs` (seulement `FakeSource` à ce stade)
- Test: module `tests` de `plan.rs`

**Interfaces:**
- Consumes : `AuSource`, `FrameEntry`, `RapKind`, `Leading` (Tâche 2).
- Produces :
  - `pub fn start_for(src: &dyn AuSource, decode_idx: u32) -> Result<u32 /* decode_idx de départ */, NotDecodable>` ;
  - `pub enum NotDecodable { BeforeFirstRap, LeadingOfFirstRap, OutOfRange }` ;
  - `fake::FakeSource` : `closed_gops(gops: u32, gop_len: u32, bframes: u32)`, `open_gops(...)` (RASL au format HEVC), `recovery(gops, gop_len, frame_cnt)`, `starting_mid_gop(...)`. Le contenu d'AU encode `decode_idx` sur 4 octets ; `with_corrupt(set: HashSet<u32>)` marque des AU comme corrompues (lues par `FakeDecoder`).
- **Règle** : on part du dernier RAP (`rap != None`) au plus égal à `decode_idx`. Si l'image est `Leading::Skippable` de ce RAP, ou si c'est un `Recovery { frame_cnt }` et que `pres_idx < pres(RAP) + frame_cnt`, on part du RAP précédent. Sans RAP précédent : `LeadingOfFirstRap`. Sans aucun RAP avant : `BeforeFirstRap`. Pour `Intra { closed_gop: false }` (MPEG-2), les B qui précèdent l'I en présentation sont `Skippable`.

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("NAV.ORACLE.02")] rasl_starts_from_previous_rap` ; `radl_starts_from_its_cra` ; `#[covers("NAV.ORACLE.02")] only_first_cra_rasl_not_decodable` ;
  - `#[covers("NAV.ORACLE.03")] recovery_point_bounds_decode_to_one_interval` (le départ est le recovery point le plus proche, jamais l'image 0, pour une cible au milieu) ;
  - `#[covers("NAV.ORACLE.04")] frames_before_first_rap_not_decodable` (source `starting_mid_gop`) ;
  - `idr_target_starts_at_itself`, `out_of_range`.
- [ ] **Step 2 : Vérifier l'échec** ; **Step 3 : Implémenter** (recherche arrière linéaire bornée par le GOP ; pas de structure d'index supplémentaire) ; **Step 4 : PASS** — Run: `cargo nextest run -p ma-decode plan`.
- [ ] **Step 5 : Commit** : `feat(ma-decode): plan d'accès aléatoire (RASL, recovery point)`.

---

### Task 4 : `ma-decode` — `Navigator` (décodage continu, fenêtres, cache, tolérance)

**Files:**
- Create: `crates/ma-decode/src/navigator.rs`
- Modify: `crates/ma-decode/src/fake.rs` (ajout de `FakeDecoder` et `FakeFactory`), `Cargo.toml` (feature `fake`)
- Test: module `tests` de `navigator.rs`

**Interfaces:**
- Consumes : Tâches 2 et 3.
- Produces :

```rust
pub struct NavConfig { pub cache_frames: usize /* 48 */, pub reorder_margin: u32 /* 16 */, pub keep_ahead: u32 /* 16 */, pub keep_behind: u32 /* 24 */ }
pub enum FrameError { OutOfRange { pres_idx: u32, len: u32 }, NotDecodable { pres_idx: u32, reason: NotDecodable }, Preempted, Io(String), Decoder(DecodeError), Viewport(ViewportError) }
pub struct NavStats { pub aus_sent: u64, pub cache_hits: u64, pub decoder: DecoderStats }
impl Navigator {
    pub fn new(src: Arc<dyn AuSource>, factory: Arc<dyn DecoderFactory>, pref: ModePref, cfg: NavConfig) -> Result<Self, DecodeError>;
    pub fn frame(&mut self, pres_idx: u32, vp: Viewport, cancel: &dyn Fn() -> bool) -> Result<Arc<DecodedFrame>, FrameError>;
    pub fn is_cached(&self, pres_idx: u32, vp: &Viewport) -> bool;
    pub fn mode(&self) -> DecodeMode;
    pub fn frame_size(&self) -> Option<Size>;                // connue après la 1re image
    pub fn take_events(&mut self) -> Vec<DecodeEvent>;
    pub fn stats(&self) -> NavStats;
}
```

- `FakeDecoder` modélise un vrai décodeur. Il sort les images par PTS croissant avec un retard de `reorder + latency`. Après un `reset`, il n'émet pas les `Skippable` tant qu'il n'a pas reçu le RAP précédent. Il renvoie `InvalidData` pour une AU marquée corrompue ; les images qui en dépendent dans le même GOP sortent `corrupt`. `FakeFactory { hw_available: bool, hw_fail_after_receives: Option<u32> }` permet d'injecter une erreur GPU. Ses `Planes` sont des NV12 à la taille du viewport, avec `pres_idx` écrit dans les 4 premiers octets (oracle du simulateur).
- **Comportement** (principes du spike §4.4, réécrits) :
  - cache LRU clé `(pres_idx, viewport)` ;
  - on poursuit le décodage en cours s'il a démarré à un RAP ≤ `start_for(cible)` et si la prochaine AU à envoyer est comprise entre `start_for(cible)` et `decode_idx(cible) + reorder_margin + latency` ; sinon `reset` et départ de `start_for(cible)` ;
  - on convertit les images de la fenêtre `[cible - keep_behind, cible]` si la demande est en arrière (cible = demande précédente - 1), sinon `[cible, cible + keep_ahead]` ;
  - si la limite est atteinte sans la cible, vidage (`send_eof`) ; si l'image manque encore, un seul nouvel essai depuis le RAP ;
  - `InvalidData` sur une AU : événement `CorruptAu`, on continue ;
  - `cancel()` est consulté entre deux AU : on renvoie `Preempted` en laissant l'état de décodage réutilisable.

- [ ] **Step 1 : Tests qui échouent** (sur `FakeSource` + `FakeFactory`) :
  - `#[covers("NAV.ORACLE.01")] every_requested_frame_is_the_one_returned` : 50 sauts pseudo-aléatoires (graine fixe), puis 20 suivantes et 20 précédentes. L'oracle des 4 octets est égal à `pres_idx` à chaque fois, en GOP fermé et en GOP ouvert ;
  - `sequential_step_continues_decoding` : 10 images suivantes ⇒ `aus_sent` ≤ 10 + marge (et non 10 × GOP) ;
  - `step_across_gop_does_not_restart` ;
  - `backward_step_converts_window_behind` : après une demande en arrière, les 24 images précédentes sont en cache ;
  - `discarded_frames_are_not_converted` : `stats().decoder.converted` ≤ taille de la fenêtre + 1 pour un saut ;
  - `cache_is_keyed_by_viewport` ;
  - `last_frame_is_served_after_drain` (Review Focus 4) ;
  - `corrupt_au_does_not_fail_whole_gop` (Review Focus 3) : les images qui précèdent l'AU corrompue dans le GOP sortent normalement, les suivantes sortent `corrupt`, un événement `CorruptAu` est émis ;
  - `cancel_returns_preempted_and_resumes` : `cancel` vrai après 3 AU ⇒ `Preempted` ; la demande suivante de la même image n'envoie pas à nouveau les 3 premières AU.
- [ ] **Step 2 : Vérifier l'échec** ; **Step 3 : Implémenter** `Navigator` et le simulateur ; **Step 4 : PASS** — Run: `cargo nextest run -p ma-decode`.
- [ ] **Step 5 : Commit** : `feat(ma-decode): navigateur d'images (décodage continu, fenêtres, cache)`.

---

### Task 5 : `ma-decode` — repli logiciel

**Files:**
- Modify: `crates/ma-decode/src/navigator.rs`
- Test: module `tests` de `navigator.rs`

**Interfaces:**
- Consumes : `Navigator`, `FakeFactory` (Tâche 4).
- Produces : comportement de `Navigator::frame`. Une erreur `Gpu`, `Device` ou `Internal` alors que `mode() == Hardware` déclenche, **une seule fois par session** : ouverture d'un décodeur `ModePref::SoftwareOnly`, événement `FellBackToSoftware { reason, at_pres }`, cache vidé, nouvel essai de la demande depuis `start_for`. Une erreur du décodeur logiciel remonte en `FrameError::Decoder`. `InvalidData` ne déclenche jamais de repli.

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("PERF.HW.01")] gpu_error_mid_navigation_falls_back_without_losing_frame` : `hw_fail_after_receives = Some(30)`, navigation de 60 images ⇒ toutes reçues avec le bon oracle, `mode() == Software`, exactement un événement `FellBackToSoftware` ;
  - `#[covers("PERF.HW.01")] unavailable_hw_opens_software` : `hw_available = false` ⇒ mode `Software` dès la 1re image, sans événement de repli (ce n'est pas un repli : le mode réel est simplement logiciel) ;
  - `invalid_data_never_triggers_fallback` ;
  - `software_error_is_reported` .
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo nextest run -p ma-decode navigator`.
- [ ] **Step 5 : Commit** : `feat(ma-decode): repli logiciel sur erreur GPU`.

---

### Task 6 : `ma-decode` — `FrameService` et préchargement

**Files:**
- Create: `crates/ma-decode/src/service.rs`
- Test: module `tests` de `service.rs`

**Interfaces:**
- Consumes : `Navigator` (Tâches 4 et 5).
- Produces :

```rust
pub struct PrefetchConfig { pub ahead: u32 /* 8 */, pub behind: u32 /* 8 */ }
pub struct Readiness { pub center: u32, pub ahead_ready: u32, pub behind_ready: u32 }
impl FrameService {
    pub fn start(nav: Navigator, cfg: PrefetchConfig) -> FrameService;       // lance un fil nommé "ma-decode"
    pub fn get(&self, pres_idx: u32, vp: Viewport) -> Result<Arc<DecodedFrame>, FrameError>;  // bloquant
    pub fn request(&self, pres_idx: u32, vp: Viewport) -> crossbeam_channel::Receiver<Result<Arc<DecodedFrame>, FrameError>>;
    pub fn readiness(&self) -> Readiness;
    pub fn mode(&self) -> DecodeMode;
    pub fn take_events(&self) -> Vec<DecodeEvent>;
    pub fn stats(&self) -> NavStats;
}   // Drop : arrêt du fil et attente de sa fin (join)
```

- **Comportement** :
  - un seul fil possède le `Navigator` ;
  - les demandes explicites passent par un canal. Une demande arrivée pendant un préchargement le préempte : le `cancel` du `Navigator` lit le compteur de demandes en attente ;
  - après chaque demande servie, le fil précharge pour le même viewport, du plus proche au plus lointain, d'abord dans le sens de la dernière navigation, jusqu'à `ahead` images après et `behind` images avant. Il ne refait pas une image déjà en cache ;
  - `readiness` compte les images prêtes autour de la dernière demande ;
  - les demandes en double ne sont décodées qu'une fois, et toutes les réponses sont envoyées.

- [ ] **Step 1 : Tests qui échouent** :
  - `#[covers("PERF.PREFETCH.01")] neighbours_ready_after_one_second` : `get(100)` puis attente d'au plus 1 s par sondage de `readiness` ⇒ 8 après et 8 avant (simulateur avec un délai de 2 ms par AU) ;
  - `#[covers("PERF.PREFETCH.01")] explicit_request_preempts_prefetch` : simulateur à 20 ms par AU, `get(100)` puis immédiatement `get(5000)` ⇒ la 2e réponse arrive avant la fin du préchargement de 100 (temps < temps d'un préchargement complet), et `aus_sent` le confirme ;
  - `zoom_change_restarts_prefetch_on_new_viewport` (Review Focus 2) ;
  - `gpu_failure_during_prefetch_falls_back_once` (Review Focus 5) : l'erreur est injectée pendant le préchargement ; la demande explicite suivante est servie, avec un seul événement ;
  - `drop_stops_worker` (pas de fil restant ; test borné à 1 s).
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo nextest run -p ma-decode service`.
- [ ] **Step 5 : Vérifier la couverture** — Run: `cargo llvm-cov nextest -p ma-decode --summary-only` ; Expected : ≥ 90 % des lignes.
- [ ] **Step 6 : Commit** : `feat(ma-decode): service d'images avec préchargement prioritaire`.

---

### Task 7 : `ma-testkit` — source TS, numéro incrusté, corpus

**Files:**
- Create: `crates/ma-testkit/{Cargo.toml,src/lib.rs,src/corpus.rs,src/ts_source.rs,src/nal.rs,src/frame_code.rs,src/corrupt.rs}`
- Test: modules `tests` + `crates/ma-testkit/tests/corpus_sources.rs`

**Interfaces:**
- Consumes : `AuSource`, `FrameEntry`, `Planes` (Tâche 2) ; `corpus/manifest.toml` (Tâche 1).
- Produces :
  - `pub fn corpus_file(name: &str) -> PathBuf` : panique avec `corpus absent : lancer cargo xtask corpus gen` si le fichier manque ; `pub fn manifest(name: &str) -> CorpusEntry` (champs du manifeste) ;
  - `pub fn require_vaapi() -> bool` : `false` (avec le message `VA-API absent : test non exécuté`) s'il n'y a pas de périphérique, sauf si `MA_REQUIRE_GPU=1` : dans ce cas, panique ;
  - `pub struct TsAuSource` (`impl AuSource`) et `TsAuSource::open(path: &Path) -> io::Result<Self>`. Il prend le premier PID vidéo de la PMT, garde la position de chaque PES et son PTS, et classe les images en présentation par tri des PTS. Les AU sont relues par `pread`. Il borne les AU à 32 Mio ;
  - `nal.rs` : `pub fn classify(codec: Codec, au: &[u8]) -> (PictureType, RapKind, Option<NalLeading>)`. HEVC : types NAL 16–21 (BLA/IDR/CRA), 6–9 (RADL/RASL). H.264 : 5 = IDR, SEI 6 de type 6 (`recovery_frame_cnt` en ue(v)), type de slice. MPEG-2 : en-tête de GOP (`closed_gop`) et `picture_coding_type`. Le statut `Leading` est déduit ensuite : présentation avant le RAP et décodage après lui ;
  - `pub fn read_frame_code(img: &Planes, region_of_full: Rect, full: Size) -> Option<u32>` : lit les 16 blocs au centre de la bande, sur la luma, avec un seuil à mi-plage, quel que soit le layout ; renvoie `None` si la bande n'est pas dans la région ;
  - `pub struct CorruptingSource<S> { pub inner: S, pub corrupt: HashSet<u32> }` (`impl AuSource`) : met à zéro les octets de l'AU après l'en-tête NAL.

- [ ] **Step 1 : Tests qui échouent** :
  - `ts_source_matches_ffprobe` : pour `h264_720p_closed` et `hevc_720p_main10`, le nombre d'images et les PTS en ordre de décodage sont égaux à ceux de `ffprobe -show_packets` (ffprobe sert d'oracle de test) ;
  - `open_gop_has_cra_and_rasl` ; `recovery_file_has_recovery_points_and_single_idr` ; `mpeg2_gop_flags` ;
  - `cut_file_starts_mid_gop` : les premières images précèdent tout RAP ;
  - `frame_code_roundtrip` : des planes synthétiques portant le code 0xA5C3, dans les 4 layouts, en pleine taille et réduites de moitié, donnent 0xA5C3.
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo xtask corpus gen && cargo nextest run -p ma-testkit`.
- [ ] **Step 5 : Commit** : `test(ma-testkit): source TS de test, lecture du numéro incrusté`.

---

### Task 8 : `ma-ffmpeg` — décodeur libavcodec (logiciel et VA-API)

**Files:**
- Create: `crates/ma-ffmpeg/{Cargo.toml,src/lib.rs,src/packet.rs,src/hw.rs,src/decoder.rs,src/planes.rs}`, `crates/ma-ffmpeg/tests/{oracle,modes}.rs`
- Modify: `xtask/src/ci.rs` (feature FFmpeg selon `pkg-config --modversion libavcodec` : 60 ⇒ `ffmpeg6`, 61 ⇒ `ffmpeg7`, 62 ⇒ `ffmpeg8`, transmise à toutes les commandes cargo par `--features ma-ffmpeg/<f>`, la feature par défaut de `ma-ffmpeg` étant `ffmpeg6` ; les features de rsmpeg doivent être cumulatives, ce qui est à vérifier dès l'étape 1, sinon s'arrêter et le signaler ; `MA_REQUIRE_GPU=1` si `/dev/dri/renderD*` existe ; sans GPU, couverture de `ma-ffmpeg` affichée mais non bloquante, avec l'avertissement `couverture de ma-ffmpeg non garantie : pas de VA-API`), `.github/workflows/ci.yml` (Fedora : idem), `BUILD.md`

**Interfaces:**
- Consumes : contrat de `ma-decode` (Tâche 2) ; `ma-testkit` en dev-dependency.
- Produces :
  - `pub struct FfmpegFactory { pub render_node: Option<PathBuf> }` (`impl DecoderFactory`) ; `pub fn vaapi_available(render_node: Option<&Path>) -> bool` ;
  - `pub struct FfmpegDecoder` (`impl Decoder`). En `ModePref::Auto`, il essaie un `AVHWDeviceContext` VA-API, avec un seul thread de décodage. En logiciel, le nombre de threads est automatique et la latence est égale au nombre de threads retenu. `mode()` vaut `Unknown` avant la 1re image, puis `Hardware` si son format est `AV_PIX_FMT_VAAPI`, sinon `Software` : c'est le repli silencieux de FFmpeg, détecté ici ;
  - chaque image VA-API sortie est synchronisée (`vaSyncSurface`), qu'elle soit gardée ou non (défaut n° 4 du spike) ;
  - classement des erreurs : `AVERROR_INVALIDDATA` ⇒ `InvalidData` ; toute autre erreur en mode matériel ⇒ `Gpu` ; drapeaux `decode_error_flags` ou `FF_DECODE_ERROR_*` ⇒ `corrupt: true` ;
  - à ce stade, la conversion copie l'image à pleine résolution puis prend la région sur le CPU (transfert GPU complet). La Tâche 9 remplace ce chemin.

- [ ] **Step 1 : Tests qui échouent** (`Navigator` + `FfmpegFactory` + `TsAuSource`) :
  - `#[covers("NAV.ORACLE.01")] oracle_h264_hevc_sw` : 50 sauts (graine fixe), 20 suivantes et 20 précédentes, sur `h264_720p_closed` et `hevc_720p_main10`, en `SoftwareOnly` ⇒ `read_frame_code` égal à `pres_idx mod code_modulo` ;
  - `#[covers("NAV.ORACLE.01")] oracle_hevc_vaapi` : idem en `Auto` (garde `require_vaapi`), mode `Hardware` vérifié ;
  - `#[covers("NAV.ORACLE.02")] open_gop_rasl_frames_correct` : toutes les RASL sauf celles du 1er CRA ;
  - `#[covers("NAV.ORACLE.03")] recovery_point_seek_mid_file` : saut au milieu ⇒ bon code, et `aus_sent` ≤ un intervalle de reprise + marge ;
  - `#[covers("NAV.ORACLE.04")] cut_file_frames_before_rap_not_decodable` ;
  - `mpeg2_oracle` : 20 sauts ;
  - `#[covers("PERF.HW.01")] unsupported_profile_reports_software` : `h264_720p_high10` en `Auto` (garde VA-API) ⇒ `mode() == Software` et bon code ;
  - `corrupted_au_real_decoder` (Review Focus 3) : `CorruptingSource` sur une P au milieu d'un GOP ⇒ le décodeur ne panique pas, les images avant elle ont le bon code, les autres sont livrées ou `NotDecodable`, et un événement `CorruptAu` est émis ;
  - `oversized_au_rejected` (32 Mio + 1).
- [ ] **Step 2–4 : échec, implémentation, PASS** — Run: `cargo nextest run -p ma-ffmpeg` (puis la même chose avec `MA_REQUIRE_GPU=1` sur la machine de référence).
- [ ] **Step 5 : Vérifier `cargo xtask ci`** ; Expected : vert, feature FFmpeg détectée affichée.
- [ ] **Step 6 : Commit** : `feat(ma-ffmpeg): décodeur libavcodec logiciel et VA-API, mode réel détecté`.

---

### Task 9 : `ma-ffmpeg` — réduction sur GPU (VPP) et région visible

**Files:**
- Create: `crates/ma-ffmpeg/src/gpu_scale.rs`, `crates/ma-ffmpeg/src/cpu_scale.rs`, `crates/ma-ffmpeg/tests/scale.rs`
- Modify: `crates/ma-ffmpeg/src/decoder.rs`

**Interfaces:**
- Consumes : `FfmpegDecoder` (Tâche 8), `Viewport::is_reduced` (Tâche 2).
- Produces :
  - `GpuScaler::scale(&mut self, frame: &AVFrame /* VAAPI */, vp: &Viewport) -> Result<Planes, DecodeError>`. Graphe libavfilter `buffer` (avec `hw_frames_ctx`) → `scale_vaapi=w:h:format=nv12|p010` → `buffersink`. La région passe par les champs `crop_*` de l'image d'entrée, que la VPP de FFmpeg utilise comme `input_region`. Les graphes sont mis en cache par `(format d'entrée, taille d'entrée, sortie, format)`, avec au plus 4 graphes. Le transfert vers la RAM ne porte que sur l'image de sortie ;
  - `CpuScaler::scale(&mut self, frame: &AVFrame, vp: &Viewport) -> Result<Planes, DecodeError>` : libswscale en `SWS_AREA` pour une réduction, copie de la région en 1:1 ;
  - formats : réduction ⇒ NV12 (GPU) / `Planar8` (CPU) ; 1:1 ⇒ P010 (GPU) ou format natif (CPU) ;
  - `DecoderStats::full_res_transfers` est incrémenté à chaque transfert GPU → RAM d'une image de la taille codée complète.

- [ ] **Step 1 : Tests qui échouent** (`hevc_720p_main10` et `uhd_big` si présent, sinon `hevc_720p_main10`) :
  - `#[covers("PERF.XFER.01")] fit_view_has_no_full_res_transfer` (garde VA-API) : 30 images au zoom Ajusté ⇒ `full_res_transfers == 0` ;
  - `#[covers("PERF.XFER.01")] fit_1920x1080_under_4_mib` : sur `uhd_big` (ou une image 3840×2160 générée à la volée si absent) ⇒ `byte_len() < 4 * 1024 * 1024` ;
  - `vpp_region_1to1_is_bit_exact` (garde VA-API) : pour 5 images, la région (256, 128, 640×360) en 1:1 est octet pour octet égale au même recadrage d'un transfert complet ;
  - `cpu_region_1to1_is_bit_exact` ;
  - `reduced_output_keeps_frame_code` : `read_frame_code` correct au zoom Ajusté, en GPU et en CPU ;
  - `viewport_change_reuses_or_rebuilds_graph` : alterner 3 viewports ⇒ images correctes, au plus 4 graphes.
- [ ] **Step 2 : Vérifier l'échec.**
- [ ] **Step 3 : Implémenter.** **Si `vpp_region_1to1_is_bit_exact` échoue sur la machine de référence**, le chemin 1:1 en VA-API passe par `av_hwframe_map` (lecture) et une copie des seules lignes de la région. Garder le test. Noter le cas et sa mesure dans le rapport de la Tâche 12.
- [ ] **Step 4 : PASS** — Run: `MA_REQUIRE_GPU=1 cargo nextest run -p ma-ffmpeg`.
- [ ] **Step 5 : Couverture** — Run: `cargo llvm-cov nextest -p ma-ffmpeg --summary-only` sur la machine de référence ; Expected : ≥ 90 %.
- [ ] **Step 6 : Commit** : `feat(ma-ffmpeg): réduction et recadrage VA-API (VPP), repli libswscale`.

---

### Task 10 : Banc moteur

**Files:**
- Create: `crates/ma-ffmpeg/examples/engine_bench.rs`

**Interfaces:**
- Consumes : `FrameService`, `Navigator`, `FfmpegFactory`, `TsAuSource`, `Viewport`.
- Produces : `cargo run --release -p ma-ffmpeg --example engine_bench -- --file <ts> --mode auto|sw --view fit:1920x1080|1to1:1920x1080 --dwell-ms <n> [--json <out>]`. Scénarios, à graine fixe : 100 sauts aléatoires, 200 pas avant, 200 pas arrière, et attente de 1 s puis `readiness`. Sortie : p50, p95 et max par scénario, `full_res_transfers`, octets par image, RSS max, mode réel. `--dwell-ms` est le temps entre la réception d'une image et la demande suivante.

- [ ] **Step 1 : Implémenter**, puis vérifier sur un petit fichier. Run: `cargo run --release -p ma-ffmpeg --example engine_bench -- --file $HOME/media-analyzer-corpus/generated/hevc_720p_main10.ts --mode sw --view fit:1280x720 --dwell-ms 0` ; Expected : les 4 scénarios s'exécutent et chaque numéro incrusté est vérifié (le banc s'arrête au premier écart).
- [ ] **Step 2 : Mesurer sur `uhd_big`**, cache disque chaud, en `auto` et en `sw`, en `fit:1920x1080` et en `1to1:1920x1080`, avec `--dwell-ms 0` et `--dwell-ms 200`. Garder les JSON dans `target/bench/`.
- [ ] **Step 3 : Commit** : `bench(ma-ffmpeg): banc moteur du transport d'image`.

---

### Task 11 : Banc interface (`tools/transport-bench`)

**Files:**
- Create: `tools/transport-bench/` : `src-tauri/` (Cargo.toml dépendant de `ma-decode` et `ma-ffmpeg` par chemin, de `ma-testkit` pour la source, `src/lib.rs`), `src/{main.ts,gl.ts}`, `index.html`, `package.json`, `vite.config.ts`, `e2e/{wdio.conf.ts,transport.e2e.ts}`, `README.md`
- Modify: `Cargo.toml` racine (`exclude = ["spike", "tools/transport-bench"]`), `BUILD.md` (§4)

**Interfaces:**
- Consumes : `FrameService::request`, `Viewport::{fit, region_1to1}`, `Planes`, `PixelLayout`.
- Produces : application Tauri d'une seule vue, avec un canvas WebGL2 plein écran de 1920×1080.
  - Protocole `frame://localhost/<pres>?x=&y=&w=&h=&ow=&oh=` : réponse binaire, avec un en-tête de 32 octets (magic `MAF1`, layout, w, h, strides) puis les planes. Une erreur est renvoyée en **code 200 avec une enveloppe** (magic `MAE1` + JSON), car WebKitGTK transforme les autres codes en `TypeError` (spike §3.3).
  - Le shader gère les 4 layouts (BT.709, plage limitée) et l'agrandissement au plus proche voisin.
  - Navigation exposée à WebDriver : `window.bench = { open(path), goto(n), step(±1), setView('fit'|'1to1'), lastTiming() }`. `lastTiming` donne les durées demande → `drawArrays` terminé (`gl.finish`), fetch et dessin.
  - Le numéro incrusté est relu sur les pixels du canvas, avec le même algorithme que `read_frame_code`.
- `e2e/transport.e2e.ts` reprend les protocoles de performance.yaml. 100 sauts (PERF.SEEK.01), 200 pas avant en Ajusté et en 1:1 (PERF.STEP.01), 200 pas arrière (PERF.STEP.02). Chaque série est passée avec un intervalle de 0 ms puis de 200 ms. Un pas est compté de la commande jusqu'à l'image dessinée, et chaque image est vérifiée par son numéro incrusté. La somme des RSS des processus de l'application est relevée en fin de parcours. Les résultats JSON vont dans `target/bench/ui-*.json`. Ce test **mesure**, il n'est pas lié par `covers` : la couverture e2e des exigences PERF se fera en M7 sur l'application réelle.

- [ ] **Step 1 : Construire et lancer à la main** sur `hevc_720p_main10`. Run: `cd tools/transport-bench && npm ci && npm run tauri build -- --debug` ; Expected : l'image s'affiche, avec son numéro lisible.
- [ ] **Step 2 : Test e2e de fumée** sur `hevc_720p_main10` (10 sauts, 10 pas de chaque côté), sous Xvfb. Run: `npm run e2e -- --spec e2e/transport.e2e.ts --smoke` ; Expected : PASS, tous les numéros vérifiés.
- [ ] **Step 3 : Mesurer sur `uhd_big`** en build release, sur l'écran réel (session Wayland), puis sous Xvfb : 3 exécutions de chaque.
- [ ] **Step 4 : Commit** : `bench: banc interface du transport d'image (Tauri, WebGL2, WebdriverIO)`.

---

### Task 12 : Rapport de mesures et point de passage n° 2

**Files:**
- Create: `docs/06-mesures-m1.md`
- Modify: `requirements/mvp/performance.yaml` (`PERF.PREFETCH.01` : `status: implemented`), `docs/05-feuille-de-route-mvp.md` (M1 : état)

- [ ] **Step 1 : Rédiger `06-mesures-m1.md`** sur le modèle de `04-resultats-spike.md` :
  - verdict ;
  - tableaux moteur et interface, comparés aux seuils et aux valeurs du spike (106 ms et 1 357 ms) ;
  - effet du préchargement (intervalle de 0 ms ou de 200 ms) ;
  - `full_res_transfers`, octets par image, RSS ;
  - écarts éventuels (exactitude de la VPP en 1:1) ;
  - mesures encore à faire (cache froid, NFS, vrai flux broadcast : M2 et M7).
- [ ] **Step 2 : Vérifier la garde.** Run: `cargo xtask ci` ; Expected : vert, `PERF.PREFETCH.01` ✓ dans `target/trace/matrix.md`.
- [ ] **Step 3 : Commit** : `docs: mesures de M1 (transport d'image)`.
- [ ] **Step 4 : Point de passage.** Présenter au porteur le verdict et les deux seuils du *go* : image suivante p95 < 100 ms côté interface, image précédente sans arrêt (max < 250 ms). En cas d'échec, présenter les options mesurées. Ne pas lancer M3 à M7 sans sa décision.
