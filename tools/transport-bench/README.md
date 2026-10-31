# Banc interface du transport d'image

Application Tauri d'une seule vue : un canvas WebGL2 de 1920×1080 qui affiche les images servies
par le moteur (`FrameService` de `ma-decode`, décodeur libavcodec de `ma-ffmpeg`, source TS de
`ma-testkit`). Un test WebdriverIO la pilote pour mesurer le transport d'image **côté interface**
(PERF.SEEK.01, PERF.STEP.01, PERF.STEP.02). Ce banc **mesure** : il n'est lié à aucune exigence
par `covers`, et il n'est pas lancé par `cargo xtask ci`. La couverture e2e des exigences PERF se
fera en M7, sur l'application réelle.

Le dossier est **hors du workspace** Cargo (`exclude` du `Cargo.toml` racine) : il a son propre
`Cargo.lock` et son propre `target/`.

## Prérequis

Ceux du dépôt (`BUILD.md` §1–2), plus Node 24 et npm, et les outils de test d'interface
(`BUILD.md` §4) : `WebKitWebDriver`, `xvfb-run`, `cargo install tauri-driver --locked`. Le corpus
généré (`cargo xtask corpus gen`, `--big` pour `uhd_big`) doit être présent.

## Construire et lancer à la main

```bash
cd tools/transport-bench
npm ci
npm run tauri build -- --debug        # ou sans --debug : build release (mesures)
MA_BENCH_OPEN=~/media-analyzer-corpus/generated/hevc_720p_main10.ts \
  src-tauri/target/debug/transport-bench
```

La fenêtre s'ouvre en plein écran sur l'image 0. Flèches gauche/droite : image précédente/suivante ;
`v` : bascule entre la vue Ajustée et la vue 1:1 ; Alt+F4 pour fermer.

## Test e2e

```bash
npm run e2e -- --spec e2e/transport.e2e.ts --smoke             # fumée, hevc_720p_main10, Xvfb
npm run e2e -- --spec e2e/transport.e2e.ts --release           # mesure, uhd_big, Xvfb
npm run e2e -- --spec e2e/transport.e2e.ts --release --native  # mesure sur l'écran de la session
```

Options de `e2e/run.mjs` : `--smoke` (10 sauts, 10 pas de chaque côté ; fichier par défaut
`hevc_720p_main10`), `--native` (écran de la session au lieu de Xvfb 1920×1080), `--release`
(binaire `target/release`), `--file <ts>`, `--mode auto|sw`, `MA_BENCH_SHOT=<png>` (capture
WebDriver après la première image ; sous Xvfb seulement : sur l'écran Wayland, la capture de
WebKitWebDriver a déjà bloqué le test (délai dépassé, sans fin)). Sous Xvfb, `WAYLAND_DISPLAY` est retiré et `GDK_BACKEND=x11`
fixé : sinon GTK ouvre la fenêtre sur la session Wayland.

Protocole (voir l'en-tête de `e2e/transport.e2e.ts`) : 100 sauts à graine fixe (les positions de
`engine_bench`), 200 pas avant en Ajusté et en 1:1, 200 pas arrière en Ajusté ; chaque série avec
un intervalle de 0 puis 200 ms, appliqué aussi après le placement ; le premier pas après le
placement est publié à part (`first_ms`). Chaque image est vérifiée par son numéro incrusté relu
sur le canvas (en 1:1 sur une image UHD, seuls les 8 bits de poids faible sont dans la région).
Résultats : `target/bench/ui-<affichage>-<fichier>-<mode>-<horodatage>.json` à la racine du dépôt,
avec le mode réel (décodage et chemin de conversion, avant et après chaque série), le layout des
images par série, la somme des RSS et des PSS (`smaps_rollup`) des processus de l'application
(processus WebKit compris), le moteur WebGL (masqué par WebKitGTK : « Apple GPU ») avec, par
processus, la présence d'un nœud `/dev/dri/renderD*` ouvert, et `complete` (toutes les séries
passées).

## Interface

- `frame://localhost/<pres>?x=&y=&w=&h=&ow=&oh=` : en-tête de 32 octets petit-boutiste (`MAF1`,
  layout, largeur, hauteur, nombre de plans, 3 strides en octets) puis les plans ; une erreur
  revient en code 200 avec l'enveloppe `MAE1` + JSON (WebKitGTK change les autres codes en
  `TypeError`). En-têtes `x-backend-ms`, `x-pres`, `x-corrupt`.
- `window.bench` : `open(path, mode)`, `goto(n)`, `step(±1)`, `setView('fit'|'1to1')`,
  `lastTiming()` (`total` = demande → `gl.finish` après `drawArrays`, `fetch`, `draw`, `backend`,
  `bytes`), `stats()`, `rss()`, `renderer()`. Une panique côté Rust en servant une image revient
  en enveloppe `MAE1`. Une seule demande en vol : une commande pendant une autre échoue.
- Les durées viennent de `performance.now()`, arrondi à la milliseconde par WebKitGTK.
