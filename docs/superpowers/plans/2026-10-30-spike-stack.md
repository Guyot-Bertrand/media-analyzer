# Spike de validation de la stack : plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Vérifier sur le NUC de référence que la chaîne « démultiplexeur TS maison → FFmpeg → planes YUV → protocole `frame://` → WebGL2 dans Tauri » tient les seuils du §8 de la stack, et choisir entre `rsmpeg` et `ffmpeg-next`.

**Architecture:** Code **jetable**, dans `spike/`, sur la branche `spike/stack-validation`, jamais fusionnée dans `main`. Seul le rapport `docs/04-resultats-spike.md` est commité sur `main`. Un crate `spike-core` (index TS, décodage, cache) est utilisé par un binaire de mesure et par une application Tauri minimale (React + WebGL2). Un test WebdriverIO pilote l'application réelle.

**Tech Stack:** Rust stable (≥ 1.85), Tauri 2, React + TypeScript + Vite, WebGL2, FFmpeg 6.1 système (libavcodec, VA-API), WebdriverIO + tauri-driver (WebKitWebDriver), Xvfb.

**Spec:** `docs/02-stack-technique.md` (§3.1, §3.3, §4.2, §6.1, §8), `docs/01-analyse-fonctionnelle.md` (contexte).

## Global Constraints

- Machine de référence : Intel NUC, i7-10710U, GPU Intel Comet Lake UHD, 31 Go RAM, Ubuntu 24.04.5.
- Fichier de mesure : TS HEVC Main 10, 3840×2160, 50 i/s, GOP d'environ 1 s, plusieurs Go.
- Seuils : affichage après saut arbitraire ≤ **500 ms** ; image suivante ou précédente ≤ **100 ms** ; première image à l'ouverture ≤ **2 s** ; mémoire résidente ≤ **2 Go** ; test bout en bout stable sur **20** exécutions consécutives.
- Lecture du fichier par lectures positionnelles (`FileExt::read_at`) et **jamais par mmap**.
- **FFmpeg ne démultiplexe pas** : il reçoit des paquets produits par notre démultiplexeur.
- Les décodeurs sont alimentés depuis le point d'accès aléatoire (RAP) précédent.
- Le décodage matériel (VA-API) sert uniquement à l'affichage. Il doit se replier en logiciel s'il est indisponible.
- Pas de tests exhaustifs, pas d'i18n, pas de design : c'est un spike. Les seules exigences sont les mesures et la correspondance index ↔ image.

## Review Focus

1. **Image affichée ≠ image demandée** (B-frames, réordonnancement, saut au milieu d'un GOP) → le motif binaire incrusté dans chaque image doit égaler l'index de présentation demandé modulo 1000 (tests des Tâches 3 et 6).
2. **VA-API absent ou en échec** (pilote, format 10 bits non supporté) → repli logiciel automatique, sans crash (test de la Tâche 3).
3. **Saut vers la toute dernière image ou au-delà de la fin** → dernière image affichée, ou erreur propre, pas de panique (test de la Tâche 3).
4. **Fichier tronqué au milieu d'un paquet** (copie interrompue) → l'index s'arrête au dernier paquet complet, sans panique (test de la Tâche 2).
5. **Saut avant le premier RAP** (premières images non décodables) → erreur « non décodable » explicite (test de la Tâche 3).

---

## Structure des fichiers (branche `spike/stack-validation`)

```
spike/
├── Cargo.toml                    # workspace : spike-core, spike-bench, app/src-tauri
├── scripts/gen-corpus.sh         # génération des fichiers de test
├── spike-core/src/
│   ├── lib.rs
│   ├── ts.rs                     # PAT/PMT, PES, PTS/DTS, détection RAP HEVC/H.264
│   ├── index.rs                  # FrameIndex (ordres décodage/présentation)
│   ├── decode.rs                 # trait FrameDecoder + impl FFmpeg (sw / vaapi)
│   └── cache.rs                  # Session : index + décodeur + LRU
├── spike-core/tests/corpus.rs    # tests sur fichiers générés
├── spike-bench/src/main.rs       # mesures sans interface
└── app/
    ├── src-tauri/src/main.rs     # commandes + protocole frame://
    ├── src/App.tsx, src/gl.ts    # UI minimale + rendu WebGL2
    └── e2e/                      # WebdriverIO + tauri-driver
docs/04-resultats-spike.md        # (sur main) rapport final
```

---

### Task 0 : Prérequis machine et branche

**Files:**
- Create: `spike/README.md` (commandes de prérequis, pour mémoire)

- [ ] **Step 1 : Demander au porteur d'exécuter les commandes nécessitant sudo** (à lui faire taper avec `!`) :

```bash
sudo apt install -y libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
  build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libclang-dev clang mold vainfo webkit2gtk-driver xvfb \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libva-dev
```

- [ ] **Step 2 : Mettre à jour Rust et installer les outils**

Run: `rustup update stable && rustup default stable && cargo install tauri-cli --version '^2' --locked && cargo install tauri-driver --locked`
Expected: `rustc --version` ≥ 1.85 ; `cargo tauri --version` affiche 2.x ; `tauri-driver --help` s'exécute.

- [ ] **Step 3 : Vérifier le décodage matériel HEVC 10 bits**

Run: `vainfo 2>/dev/null | grep -i 'HEVCMain10'`
Expected: une ligne `VAProfileHEVCMain10 : VAEntrypointVLD`. Si elle est absente, le noter pour le rapport ; les mesures VA-API seront alors « non disponibles ».

- [ ] **Step 4 : Installer TSDuck** (oracle, utile dès la Tâche 2)

Run: télécharger le `.deb` Ubuntu 24.04 de la dernière release depuis `https://github.com/tsduck/tsduck/releases`, puis `sudo apt install ./tsduck_*.deb` (demander au porteur).
Expected: `tsp --version` répond.

- [ ] **Step 5 : Créer la branche et le README, commit**

```bash
git switch -c spike/stack-validation
git add spike/README.md && git commit -m "spike: prérequis"
```

---

### Task 1 : Corpus de mesure avec numéro d'image incrusté

**Files:**
- Create: `spike/scripts/gen-corpus.sh`

Le script génère dans `${MA_SPIKE_DIR:-$HOME/media-analyzer-corpus/spike}` (hors dépôt) :
- `seg_uhd.ts` : 20 s de `testsrc2` en 3840×2160, 50 i/s ;
- `uhd_big.ts` : `seg_uhd.ts` mis en boucle (`-stream_loop`, `-c copy`) jusqu'à environ 4 Go ;
- `small_h264.ts` : 10 s en 1280×720, 25 i/s, H.264 8 bits, GOP de 25 images avec des B-frames.

**Code binaire du numéro d'image** (sert d'oracle à la correspondance index ↔ image) : une bande de 64 px en haut de l'image contient le numéro d'image *n* de la boucle, sur 10 bits, de gauche à droite (bit 0 à gauche), chaque bit étant un bloc de 128 px de large : blanc pour 1, noir pour 0. Filtre FFmpeg appliqué avant l'encodage :

```
geq=lum='if(lt(Y,64)*lt(X,1280), 255*mod(floor(N/pow(2,floor(X/128))),2), lum(X,Y))':cb='if(lt(Y,64),128,cb(X,Y))':cr='if(lt(Y,64),128,cr(X,Y))'
```

(Pour le 720p : même formule, blocs de 64 px de large, bande de 32 px de haut, 10 bits.)

- [ ] **Step 1 : Écrire `gen-corpus.sh`**

Paramètres d'encodage UHD : `-c:v libx265 -pix_fmt yuv420p10le -x265-params "keyint=50:min-keyint=50:open-gop=0:bframes=3" -b:v 25M`, audio `-c:a aac -b:a 192k`, sortie `-f mpegts`. Le filtre `geq` est appliqué en `yuv420p` **avant** `format=yuv420p10le`. Paramètres H.264 : `-c:v libx264 -g 25 -bf 2 -pix_fmt yuv420p`.

- [ ] **Step 2 : Lancer et vérifier**

Run: `bash spike/scripts/gen-corpus.sh && ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,profile,width,height,pix_fmt,r_frame_rate -of compact $HOME/media-analyzer-corpus/spike/uhd_big.ts && ls -l $HOME/media-analyzer-corpus/spike/`
Expected: `codec_name=hevc|profile=Main 10|width=3840|height=2160|pix_fmt=yuv420p10le|r_frame_rate=50/1` ; `uhd_big.ts` ≥ 3,5 Go.

- [ ] **Step 3 : Contrôle de continuité par l'oracle**

Run: `tsp -I file $HOME/media-analyzer-corpus/spike/uhd_big.ts -P continuity -O drop 2>&1 | tail -3`
Expected: aucun message de discontinuité, sinon corriger la boucle (par exemple `-fflags +genpts`).

- [ ] **Step 4 : Commit** : `git add spike/scripts && git commit -m "spike: génération du corpus"`

---

### Task 2 : Index TS (démultiplexeur minimal)

**Files:**
- Create: `spike/Cargo.toml`, `spike/spike-core/Cargo.toml`, `spike/spike-core/src/{lib.rs,ts.rs,index.rs}`
- Test: `spike/spike-core/src/ts.rs` (tests unitaires), `spike/spike-core/tests/corpus.rs`

**Interfaces:**
- Produces:
  - `pub enum VideoCodec { H264, Hevc }`
  - `pub struct FrameEntry { pub decode_idx: u32, pub pres_idx: u32, pub pts: i64, pub dts: Option<i64>, pub pes_offset: u64, pub is_rap: bool }`
  - `pub struct FrameIndex { pub codec: VideoCodec, pub video_pid: u16, pub width: u32, pub height: u32, pub bit_depth: u8, pub frames: Vec<FrameEntry> /* ordre de décodage */ }`
  - `impl FrameIndex { pub fn build(path: &Path) -> anyhow::Result<FrameIndex>; pub fn by_pres(&self, pres_idx: u32) -> Option<&FrameEntry>; pub fn rap_before(&self, decode_idx: u32) -> Option<u32> }`
  - `pub fn parse_pts(b: &[u8]) -> u64` (5 octets d'un champ PTS ou DTS d'en-tête PES ; stocké en `i64` dans `FrameEntry`)
  - `pub fn read_pes_payload(file: &File, pid: u16, offset: u64) -> anyhow::Result<Vec<u8>>` (réassemble un PES à partir du paquet TS de début)

Approche : lecture par blocs de 188 × 4096 octets via `read_at` ; PAT → PMT → premier flux de `stream_type` 0x24 (HEVC) ou 0x1B (H.264) ; une entrée par PES vidéo (hypothèse du spike : 1 PES = 1 AU) ; `is_rap` si le PES contient une NAL IDR/CRA/BLA (HEVC : types 16 à 21) ou IDR (H.264 : type 5) ; `pres_idx` = rang du PTS une fois tous les PTS triés ; `width`, `height` et `bit_depth` lus dans le premier SPS (lecture exp-Golomb minimale).

- [ ] **Step 1 : Tests unitaires qui échouent** dans `ts.rs` :
  - `parses_pat_and_pmt_from_synthetic_packets` : 2 paquets construits à la main → `video_pid == 0x100` et `codec == Hevc` ;
  - `pts_decoding_33bit` : `parse_pts(&[0x21, 0x00, 0x05, 0xBF, 0x21]) == 90_000`, et `parse_pts(&[0x2F, 0xFF, 0xFF, 0xFF, 0xFF]) == (1u64 << 33) - 1` (valeur maximale sur 33 bits) ;
  - `truncated_file_stops_at_last_full_packet` : fichier temporaire de 188 × 10 + 100 octets → `build` réussit et ignore le paquet incomplet.
- [ ] **Step 2 : Vérifier l'échec** — Run: `cargo test -p spike-core` ; Expected: FAIL (fonctions absentes).
- [ ] **Step 3 : Implémenter** `ts.rs` et `index.rs` selon les Interfaces.
- [ ] **Step 4 : Test de corpus** `index_matches_ffprobe` dans `tests/corpus.rs` (ignoré si `MA_SPIKE_DIR` est absent) : sur `small_h264.ts` et `seg_uhd.ts`, le nombre d'entrées et le nombre de RAP sont égaux à ceux de `ffprobe -select_streams v:0 -show_entries packet=flags -of csv` (lignes et lignes contenant `K`), et `width`/`height`/`bit_depth` correspondent.
- [ ] **Step 5 : Tout passe** — Run: `MA_SPIKE_DIR=$HOME/media-analyzer-corpus/spike cargo test -p spike-core` ; Expected: PASS. Noter le temps d'indexation de `uhd_big.ts` (`cargo test --release -- --nocapture index_big_timing`), qui alimentera le rapport.
- [ ] **Step 6 : Commit** : `git commit -am "spike: index TS"`

---

### Task 3 : Décodage par FFmpeg et session avec cache

**Files:**
- Create: `spike/spike-core/src/{decode.rs,cache.rs}`
- Test: `spike/spike-core/tests/corpus.rs`

**Interfaces:**
- Consumes: `FrameIndex`, `read_pes_payload` (Tâche 2).
- Produces:
  - `pub enum PixelLayout { Planar8, Planar10, SemiPlanar8 /* NV12 */, SemiPlanar10 /* P010 */ }`
  - `pub struct DecodedFrame { pub pres_idx: u32, pub width: u32, pub height: u32, pub layout: PixelLayout, pub planes: Vec<Vec<u8>>, pub strides: Vec<u32> }`
  - `pub enum HwMode { Software, Vaapi }`
  - `pub trait FrameDecoder { fn decode_range(&mut self, packets: &mut dyn Iterator<Item = (i64 /*pts*/, Vec<u8>)>, want_pts: i64) -> anyhow::Result<DecodedFrame>; fn hw_mode(&self) -> HwMode; }`
  - `pub struct Session { … }` avec `pub fn open(path: &Path, hw: HwMode) -> anyhow::Result<Session>`, `pub fn info(&self) -> &FrameIndex`, `pub fn frame(&mut self, pres_idx: u32) -> anyhow::Result<Arc<DecodedFrame>>`
  - `pub enum FrameError { NotDecodable { pres_idx: u32 }, OutOfRange { pres_idx: u32, last: u32 } }` (via `anyhow::Error::downcast_ref`)

Approche :
- Liaison **`rsmpeg`** (feature correspondant à FFmpeg 6.1) ; l'alternative `ffmpeg-next` est évaluée à l'étape 6.
- Pour `frame(p)` : trouver le RAP qui précède la position de décodage de la cible, alimenter le décodeur avec les PES depuis ce RAP jusqu'à obtenir la sortie de PTS voulu, puis vider le décodeur (flush).
- Les sorties intermédiaires sont mises en cache LRU (capacité 32 images).
- En mode `Vaapi`, `av_hwframe_transfer_data` produit du NV12 ou du P010. Si le contexte matériel ne peut pas être créé, on passe en `Software` et un avertissement est journalisé.

- [ ] **Step 1 : Tests qui échouent** dans `tests/corpus.rs`, en définissant `fn read_code(f: &DecodedFrame) -> u32`, qui lit les 10 blocs de la bande en haut de la plane Y (moyenne du bloc > mi-échelle ⇒ bit à 1) :
  - `decoded_frame_matches_requested_index_sw` : pour 50 index de présentation pseudo-aléatoires (graine fixe 42) dans `seg_uhd.ts` et `small_h264.ts`, `read_code(frame(p)) == p % frames_per_loop` (1000 pour l'UHD, 250 pour le H.264) ;
  - `decoded_frame_matches_requested_index_vaapi` : idem en `HwMode::Vaapi` sur l'UHD ; le test est ignoré, avec un message, si `hw_mode()` est retombé en `Software` ;
  - `out_of_range_is_clean_error` : `frame(last + 10)` renvoie `OutOfRange` ;
  - `frames_before_first_rap_are_not_decodable` : construire un fichier commençant au milieu d'un GOP (`dd` avec `skip` aligné sur 188 octets, sans le premier RAP) ⇒ `frame(0)` renvoie `NotDecodable`.
- [ ] **Step 2 : Vérifier l'échec** — Run: `MA_SPIKE_DIR=... cargo test -p spike-core --test corpus` ; Expected: FAIL.
- [ ] **Step 3 : Implémenter** `decode.rs` et `cache.rs`.
- [ ] **Step 4 : Tout passe** — Run: `MA_SPIKE_DIR=... cargo test -p spike-core --release` ; Expected: PASS (ou test VA-API ignoré avec la raison affichée).
- [ ] **Step 5 : Commit** : `git commit -am "spike: décodage FFmpeg + cache"`
- [ ] **Step 6 : Évaluation de `ffmpeg-next`** (boîte de temps : 2 h) : réécrire `FrameDecoder` pour le mode logiciel avec `ffmpeg-next` derrière la feature `ffnext`, faire passer `decoded_frame_matches_requested_index_sw` avec `--features ffnext`, puis consigner dans `spike/NOTES.md` : versions de FFmpeg supportées (6.x, 7.x), accès au hwaccel, quantité de code `unsafe` nécessaire, qualité de la documentation, date de la dernière release. Commit.

---

### Task 4 : Binaire de mesure sans interface

**Files:**
- Create: `spike/spike-bench/{Cargo.toml,src/main.rs}`

**Interfaces:**
- Consumes: `Session` (Tâche 3).
- Produces: un fichier `spike/results/bench-<hw>.json` : `{ "hw": "...", "open_first_frame_ms": f64, "seek_ms": {"p50":..,"p95":..,"max":..}, "next_ms": {...}, "prev_ms": {...}, "peak_rss_mb": f64, "index_build_ms": f64 }`.

Protocole :
- **ouverture** : `Session::open` puis `frame(0)` ;
- **sauts** : 100 index de présentation tirés au hasard (graine 7) ;
- **suivante** : 200 appels `frame(p+1)` à partir d'un point aléatoire ;
- **précédente** : 200 appels `frame(p-1)` ;
- **mémoire** : RSS de pointe lue dans `/proc/self/status` (`VmHWM`).

On le lance pour `--hw sw` et `--hw vaapi`, en cache froid : `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches` est à faire taper par le porteur avant chaque série, sinon le rapport précise « cache chaud ».

- [ ] **Step 1 : Implémenter `main.rs`** (`clap` : `--file`, `--hw sw|vaapi`, `--out`).
- [ ] **Step 2 : Mesurer** — Run: `cargo run --release -p spike-bench -- --file $MA_SPIKE_DIR/uhd_big.ts --hw sw --out spike/results/bench-sw.json`, puis la même commande avec `vaapi`.
  Expected: fichiers JSON produits. Comparer `seek_ms.p95` à 500, `next_ms.p95` à 100, `open_first_frame_ms` à 2000 et `peak_rss_mb` à 2048.
- [ ] **Step 3 : Commit** : `git add spike/spike-bench spike/results && git commit -m "spike: mesures moteur"`

---

### Task 5 : Application Tauri minimale (protocole `frame://` et WebGL2)

**Files:**
- Create: `spike/app/` (via `cargo tauri init` puis Vite React TS), `spike/app/src-tauri/src/main.rs`, `spike/app/src/App.tsx`, `spike/app/src/gl.ts`

**Interfaces:**
- Consumes: `Session`, `DecodedFrame`, `PixelLayout` (Tâche 3).
- Produces (pour la Tâche 6) :
  - Commande `open_file(path: String, hw: String) -> FileInfo { frames: u32, width: u32, height: u32, bit_depth: u8, hw_mode: String }`.
  - Protocole asynchrone `frame://localhost/<pres_idx>` (`register_asynchronous_uri_scheme_protocol`), qui renvoie `application/octet-stream`. En-tête de 32 octets little-endian : `magic "MAF1"`, `width u32`, `height u32`, `layout u32 (0..3, ordre de PixelLayout)`, `n_planes u32`, `stride[0..3] u32`. Viennent ensuite les planes concaténées.
  - DOM : `#frame-label` affiche `pres_idx` de l'image **effectivement dessinée** ; `#stats` affiche les ms de la dernière requête (fetch → `drawArrays` terminé, mesuré par `performance.now()` après `gl.finish()`) ; `canvas#view` est créé avec `preserveDrawingBuffer: true`.
  - Au démarrage, si la variable d'environnement `MA_SPIKE_OPEN` est définie, le fichier qu'elle désigne s'ouvre automatiquement (pour la Tâche 6).

Rendu :
- WebGL2, une texture par plane : `R8` / `RG8` pour 8 bits, `R16UI` / `RG16UI` (avec `usampler2D`) pour 10 bits ;
- normalisation 10 bits : `/1023.0` pour le planaire, `/65535.0` pour le P010 ;
- conversion YUV → RGB en BT.709 limited range ;
- uniforms `showY/showU/showV` ;
- filtres `NEAREST`, zoom de 25 % à 400 %.

Interface : un bouton Ouvrir (dialogue de fichier), les boutons ◀ ▶, un champ « Aller à », les flèches ←/→, les cases Y/U/V et le zoom. Pas de design au-delà de cela.

- [ ] **Step 1 : Squelette** — Run: `cd spike/app && npm create vite@latest . -- --template react-ts && npm i && npm i -D @tauri-apps/cli@^2 && npx tauri init` (identifiant `dev.media-analyzer.spike`) ; Expected: `npx tauri dev` ouvre une fenêtre vide.
- [ ] **Step 2 : Implémenter** la commande, le protocole, `gl.ts` et `App.tsx`.
- [ ] **Step 3 : Vérification manuelle** — Run: `MA_SPIKE_OPEN=$MA_SPIKE_DIR/uhd_big.ts npx tauri dev` ; Expected: la mire s'affiche avec le code binaire en haut, ← et → changent d'image, la case Y seule donne une image en niveaux de gris, `#stats` affiche des temps. Faire une capture pour le rapport.
- [ ] **Step 4 : Commit** : `git commit -am "spike: app Tauri + WebGL2"`

---

### Task 6 : Test bout en bout et mesures côté interface

**Files:**
- Create: `spike/app/e2e/{wdio.conf.ts,spike.e2e.ts}`, `spike/app/e2e/run20.sh`

**Interfaces:**
- Consumes: le DOM et `MA_SPIKE_OPEN` de la Tâche 5 ; le build `npx tauri build --debug --no-bundle`.

`wdio.conf.ts` suit la configuration de référence de Tauri pour Linux : `tauri-driver` est lancé en `onPrepare`, avec la capability `tauri:options.application` pointant le binaire construit.

Le test `spike.e2e.ts` :
1. attend que `#frame-label` vaille `0` (délai max 10 s) et enregistre le temps d'ouverture ;
2. saisit `1234` dans « Aller à » ⇒ attend `#frame-label == 1234`. Il lit ensuite par `browser.execute` les pixels du canvas (bande du code binaire) et décode le code, qui doit valoir `1234 % 1000` ;
3. appuie 20 fois sur → et vérifie le label et le code à chaque fois ; les valeurs de `#stats` sont collectées ;
4. écrit les mesures dans `spike/results/ui-<timestamp>.json`.

`run20.sh` lance le test 20 fois sous `xvfb-run -a` et compte les succès.

- [ ] **Step 1 : Écrire la config et le test.**
- [ ] **Step 2 : Lancer une fois** — Run: `cd spike/app && npx tauri build --debug --no-bundle && MA_SPIKE_OPEN=$MA_SPIKE_DIR/uhd_big.ts npx wdio run e2e/wdio.conf.ts` ; Expected: PASS.
- [ ] **Step 3 : Stabilité** — Run: `bash e2e/run20.sh` ; Expected: `20/20 PASS`. Noter le résultat exact dans tous les cas.
- [ ] **Step 4 : Mesure sur l'écran réel** (hors Xvfb, sur la session graphique du NUC, pour que WebGL soit accéléré) : relancer une fois `npx wdio run e2e/wdio.conf.ts` avec `DISPLAY` de la session. Vérifier l'accélération via `browser.execute(() => gl.getParameter(ext.UNMASKED_RENDERER_WEBGL))` avec l'extension `WEBGL_debug_renderer_info` : le nom doit contenir « Intel » et non « llvmpipe ».
- [ ] **Step 5 : Commit** : `git add spike/app/e2e spike/results && git commit -m "spike: e2e + mesures UI"`

---

### Task 7 : Rapport et décision

**Files:**
- Create (sur `main`) : `docs/04-resultats-spike.md`

Contenu :
1. tableau mesure / seuil / logiciel / VA-API / interface (Xvfb et écran réel), avec ✓ ou ✗ ;
2. renderer WebGL constaté ;
3. stabilité du test bout en bout (x/20) ;
4. temps d'indexation de `uhd_big.ts` ;
5. **choix de la liaison FFmpeg** (`rsmpeg` ou `ffmpeg-next`) et sa justification (`spike/NOTES.md`) ;
6. **décision** :
   - *go* tel quel ;
   - *go* avec le repli « rendu côté Rust » (§8 de la stack) ;
   - *retour au porteur* si WebKitGTK est inutilisable ;
7. enseignements pour le plan du MVP (par exemple : hypothèse « 1 PES = 1 AU » à lever, taille du cache, préchargement des voisines).

- [ ] **Step 1 : Rédiger le rapport** à partir de `spike/results/*.json` et `spike/NOTES.md`.
- [ ] **Step 2 : Commit sur `main`** :

```bash
git switch main && git add docs/04-resultats-spike.md && git commit -m "docs: résultats du spike de stack"
```

- [ ] **Step 3 : Présenter le rapport au porteur.** La branche `spike/stack-validation` est conservée pour référence, mais n'est pas fusionnée.
