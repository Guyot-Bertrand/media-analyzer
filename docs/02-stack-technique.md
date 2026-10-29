# Media Analyzer : stack technique

> Prérequis : `docs/01-analyse-fonctionnelle.md` (périmètre, décisions D1 à D8, lots MVP à V5).
> Étape suivante : maquettes de l'interface avec Claude Design, puis plan de développement du MVP.
> Légende : **[décidé]** = choix du porteur du projet ; **[proposé]** = choix technique de l'assistant, modifiable.

---

## 1. Intention et contraintes

Application desktop Linux, open source, pour un usage personnel et celui de quelques collègues. Elle sert à **inspecter image par image les métadonnées d'un fichier vidéo** : structure, syntaxe, conformité, et liens avec l'audio, les sous-titres et les pistes de données. Ce n'est pas un lecteur.

| Contrainte | Origine |
|---|---|
| Langage principal **Rust**. Le C++ n'est admis qu'à travers des bibliothèques externes (FFmpeg, libvmaf…). | [décidé] |
| Outils externes bienvenus (FFmpeg et ses bibliothèques). | [décidé] |
| Le porteur ne lit pas le code. La qualité est donc garantie par les **tests**, une **matrice de traçabilité exigences ↔ tests** vérifiée automatiquement et l'analyse statique. | [décidé] |
| Tests unitaires avec une très bonne couverture, plus des **tests fonctionnels qui pilotent l'interface**. | [décidé] |
| Interface **fiable, efficace et pratique**, sans préférence technologique. Les maquettes seront faites avec Claude Design. | [décidé] |
| Fichiers de **plusieurs Go**. | [décidé] (D6) |
| Extensibilité : codecs et conteneurs ajoutés plus tard (AV2, VC-1, MXF…) sans refonte. | [décidé] (D1, D3) |
| Séparation moteur / interface, pour préparer un futur mode ligne de commande. | [décidé] (D5) |
| Interface **internationalisée dès le MVP** : français principal, anglais en alternative (voir §4.3). | [décidé] |
| Licence **GPL-3.0-or-later**, choisie pour pouvoir utiliser n'importe quelle build de FFmpeg (LGPL ou GPL). | [proposé], validé sur le principe « ne pas se fermer de portes » |

**Non-objectifs** : lecture fluide temps réel, Windows/macOS, mode ligne de commande en V1, chargement dynamique de plugins tiers.

**Anti-objectifs** (ce qui ferait échouer le projet même si « ça marche ») :
- une interface qui se fige pendant une analyse ;
- un index d'image qui ne correspond pas à l'image affichée ;
- une fonctionnalité livrée sans test qui la couvre ;
- un crash sur un flux corrompu, alors que les flux corrompus sont précisément la matière première d'un analyseur.

---

## 2. Vue d'ensemble de l'architecture

```
┌───────────────────────────── Application Tauri 2 ─────────────────────────────┐
│  UI (WebView / WebKitGTK)                                                     │
│  React + TypeScript + Vite                                                    │
│  ├─ couche `api` (interface TS) ──► impl. Tauri  │  impl. mock (tests)        │
│  ├─ rendu image + overlays : WebGL2 (planes YUV 8/10 bits)                    │
│  └─ listes/arbres virtualisés, graphes temporels                              │
│                     ▲ commandes / événements           ▲ protocole `frame://` │
├─────────────────────┼──────────────────────────────────┼──────────────────────┤
│  src-tauri : adaptateur mince (commandes ↔ moteur), aucune logique métier     │
├───────────────────────────────────────────────────────────────────────────────┤
│  ma-engine : session, index, orchestration des jobs, API de requêtes          │
│  ├─ conteneurs : ma-ts (MVP) · ma-isobmff, ma-mkv, ma-ott (V4)                │
│  ├─ codecs (parsing + conformité) : ma-h264 · ma-h265 · ma-mpeg2v (MVP)       │
│  │                                   ma-av1 · ma-vp9 · ma-vvc (V4) · ma-av2   │
│  ├─ audio : ma-audio (AAC, AC-3/E-AC-3, MPEG audio, loudness)                 │
│  ├─ données : ma-data (CC, DVB subs, Teletext, VPS/WSS, TTML, SCTE-35)        │
│  ├─ analyses : ma-tr101290 · ma-ptsdts · ma-buffer · ma-pq                    │
│  ├─ décodage : ma-decode (trait Decoder ; impl. FFmpeg ; V3 : instrumentés)   │
│  ├─ socle : ma-core (types, rapport, registre) · ma-bits · ma-io · ma-store   │
└───────────────────────────────────────────────────────────────────────────────┘
       ▼ bibliothèques natives : FFmpeg (libavcodec, libavutil, libswscale), libvmaf, SQLite
```

### 2.1 Organisation du code [proposé]
- Un **workspace Cargo** avec une crate par domaine (liste ci-dessus). Ce découpage limite le temps de recompilation et rend les dépendances explicites.
- Le **moteur** (`ma-engine` et tout ce qui est en dessous) **ne dépend ni de Tauri ni d'une quelconque bibliothèque graphique**. Un futur binaire CLI le réutilisera tel quel.
- **Extensibilité** : chaque conteneur, codec ou analyse implémente des *traits* communs (`ContainerProbe`, `Demuxer`, `EsParser`, `Analysis`, `Decoder`) et s'enregistre dans un registre. L'enregistrement est **statique** (features Cargo) : pas de chargement dynamique. Ajouter AV2 ou VC-1 revient à ajouter une crate et une ligne d'enregistrement.
- `src-tauri` est un **adaptateur mince** : il traduit les commandes de l'interface en appels au moteur et relaie les événements (progression, nouveaux messages).
- `xtask` regroupe l'outillage du dépôt : matrice de traçabilité, génération et téléchargement du corpus, scripts de CI.

### 2.2 Versions [proposé]
Rust stable (édition 2024), Tauri 2.x, Node LTS, React 18+, TypeScript strict, FFmpeg 7.x (version épinglée), SQLite via `rusqlite` (build embarquée).

---

## 3. Choix techniques clés du moteur

### 3.1 Accès aux fichiers et index (D6)
- Le fichier est lu par **lectures positionnelles** (`pread`), avec un cache de blocs ; il n'est jamais chargé entièrement. [proposé] La projection mémoire (`mmap`) est écartée comme voie principale : si le fichier est tronqué ou modifié pendant la lecture (partage réseau, fichier en cours d'enregistrement), elle provoque un crash (SIGBUS). Une lecture positionnelle renvoie simplement une erreur, que l'on sait traiter.
- **Indexation progressive** à l'ouverture, en tâche de fond : positions des paquets TS par PID, unités PES, access units, type d'image, index de décodage et de présentation, PTS/DTS/PCR, points d'accès aléatoire. L'interface devient utilisable dès les premières images indexées.
- **Index persistant** dans un cache SQLite (`~/.cache/media-analyzer/`), identifié par chemin, taille, date de modification et empreinte partielle. Rouvrir un fichier déjà analysé est alors instantané.
- **Pas de syntaxe stockée pour tout le fichier.** L'arbre syntaxique d'une access unit est **reconstruit à la demande** en relisant ses octets via l'index. On évite ainsi des gigaoctets de données intermédiaires.

### 3.2 Parsing de syntaxe instrumenté
- **Parseurs maison** pour le TS (PSI/SI, PES, adaptation field) et pour chaque codec. Raisons :
  1. chaque élément doit être noté avec son nom, sa valeur, son **offset et sa longueur au bit près** et son interprétation ;
  2. il faut **tolérer les erreurs** : continuer après une corruption et la signaler au lieu d'échouer ;
  3. les conformités (F5) se branchent élément par élément.
- La crate `ma-bits` fournit un lecteur de bits qui alimente un **enregistreur de syntaxe**. Le même code de parsing sert à deux modes : un mode « rapide » qui extrait seulement les valeurs, pour l'indexation, et un mode « détaillé » qui construit l'arbre, pour la vue Syntax. Le parseur est écrit une seule fois.
- Les crates existantes (`h264-reader`, `mpeg2ts-reader`, `scte35-reader`…) servent de **référence**, pas de dépendances.
- Les règles de conformité sont **déclaratives quand c'est possible** : valeur unique, plage, liste, dépendance. Chacune porte un identifiant d'exigence (voir §5).

### 3.3 Décodage
- Le trait `Decoder` est **défini dès le MVP** ; son implémentation MVP est `FfmpegDecoder`, construit sur libavcodec. Le trait prévoit dès le départ un canal optionnel de **données de blocs** (vide en MVP), rempli en V3 par les décodeurs instrumentés : type, QP, vecteurs, partitions, coefficients. Ces données suivent un **modèle de blocs générique** (arbre de partitions rectangulaires avec attributs), valable pour le macrobloc, le CTU/CU, le superbloc AV1 et l'arbre VVC.
- **Le démultiplexage n'est pas confié à FFmpeg.** C'est notre démultiplexeur qui fournit les paquets à libavcodec. La correspondance entre index d'image, octets, syntaxe et image décodée est ainsi garantie par construction. Ce point protège contre l'anti-objectif « index ≠ image affichée ».
- **Accès aléatoire** : on décode depuis le point d'accès aléatoire précédent grâce à l'index, et on garde un cache LRU des images décodées autour de la position courante.
- Liaison FFmpeg : crate `rsmpeg` ou `ffmpeg-next`. Le choix se fera lors du spike (§8). FFmpeg est compilé à une version épinglée, **décodeurs seulement**, pour la CI et les paquets ; en développement, la version système est acceptée.
- V3 : les décodeurs instrumentés sont choisis codec par codec (export natif de FFmpeg pour les MV et le QP quand il existe, fork d'un décodeur de référence, ou décodeur Rust comme `rav1d` pour AV1). Ce choix relève de la V3 et ne touche pas au trait.

### 3.4 Audio, loudness, qualité
- Décodage audio via libavcodec. Les parseurs d'en-têtes (ADTS/LOAS, AC-3/E-AC-3, MPEG audio) sont maison, pour la syntaxe et la conformité.
- **Loudness** : crate `ebur128` (ITU-R BS.1770-4 : momentary, short-term, integrated, LRA, true peak). Les profils EBU R128, ATSC A/85 et ARIB sont des jeux de seuils.
- **Qualité d'image** : les métriques sans référence sont implémentées en Rust sur les planes décodées. PSNR et SSIM sont aussi en Rust ; **VMAF passe par libvmaf** (BSD).

### 3.5 Concurrence et jobs
- Un **ordonnanceur de jobs** gère l'indexation, TR 101 290, PTS/DTS, buffers, loudness, qualité… Chaque job a un jeton d'**annulation**, une **progression** et une priorité. La navigation interactive passe toujours avant les analyses de fond.
- Les jobs calculatoires tournent sur `rayon` ; le dialogue avec l'interface passe par des canaux. Aucune opération longue ne s'exécute dans le thread de l'interface.
- Les événements vers l'interface sont **regroupés et limités en fréquence** (par exemple 10 Hz) pour ne pas saturer la WebView.

### 3.6 Rapport et persistance
- Les messages du rapport (type, famille, sous-famille, index, position, description, identifiant de règle) sont stockés en **SQLite**, car ils peuvent se compter en millions sur un gros TS. L'interface les interroge par pages, avec filtres et tri.
- La mise en sourdine après N occurrences et l'export texte/CSV se font côté moteur.
- Les réglages sont un fichier **TOML** dans `~/.config/media-analyzer/`.

---

## 4. Interface

### 4.1 Choix : Tauri 2 + React/TypeScript [proposé]
| Critère | Tauri + React | Rust natif (egui, Slint) | Electron |
|---|---|---|---|
| Passerelle depuis Claude Design (maquettes web) | directe | à retraduire entièrement | directe |
| Composants riches (arbres, tableaux virtualisés, graphes, docking) | très large écosystème | plus limité | très large |
| Tests fonctionnels qui pilotent l'interface | Playwright, WebDriver | jeunes (egui_kittest) | Playwright natif |
| Intégration du cœur Rust | native | native | pont Node à écrire |
| Poids | léger (WebKitGTK système) | léger | lourd (Chromium embarqué) |

Le principal risque de Tauri sous Linux vient de **WebKitGTK** : performances WebGL selon les pilotes, petites différences de rendu. Le spike (§8) le vérifiera.

### 4.2 Organisation [proposé]
- **Couche `api`** : une interface TypeScript unique vers le moteur, avec deux implémentations. L'une passe par les commandes Tauri, l'autre est un **mock** alimenté par des jeux de données enregistrés. C'est ce qui permet de tester l'interface vite et sans backend (§5.3).
- **Transport des images** : un protocole personnalisé (`frame://<session>/<index>?plane=…`) renvoie les **planes YUV brutes** (8 ou 10 bits) sous forme de binaire, sans JSON ni base64. Les vignettes passent par le même protocole en résolution réduite, avec un cache.
- **Rendu image** : WebGL2. Un shader convertit le YUV en RGB, gère l'affichage sélectif Y/U/V, le zoom au plus proche voisin (sans interpolation, comme le CMA), la 10 bits et un tone mapping simple pour la HDR. Les overlays (grilles, blocs, vecteurs) sont des couches dessinées par-dessus.
- **Données volumineuses** : arbres et tableaux virtualisés (TanStack Virtual). Graphes temporels (PTS/DTS, buffers, loudness, qualité) avec **uPlot**, conçu pour des centaines de milliers de points.
- **État** : Zustand. **Mise en page** : panneaux redimensionnables et masquables (`react-resizable-panels`) ; le docking libre est reporté.
- Le style (CSS, éventuelle bibliothèque de composants) sera fixé à l'étape Claude Design.

### 4.3 Internationalisation [décidé : FR principal, EN dès le MVP]
- **Un seul format de traduction, partagé par le moteur et l'interface : Project Fluent (`.ftl`)**. Côté Rust : `fluent-bundle` ; côté TS : `@fluent/bundle` et `@fluent/react`. Fichiers : `locales/fr/*.ftl`, `locales/en/*.ftl`. [proposé]
- **Le moteur ne produit jamais de texte en dur.** Messages du rapport, interprétations de syntaxe et explications de conformité sortent sous forme d'un **identifiant de message accompagné de paramètres typés**. La traduction est faite à l'affichage par l'interface, ou côté Rust pour les exports et le futur mode ligne de commande. Changer de langue ne nécessite donc pas de relancer l'analyse.
- **Ne sont pas traduits** : les noms d'éléments de syntaxe tels qu'ils figurent dans les normes (`pic_order_cnt_lsb`, `PCR_PID`…) et les noms de normes et d'indicateurs (TR 101 290 « Continuity_count_error »…). Leurs tooltips explicatifs, eux, sont traduits.
- **Formats** : nombres, durées et dates sont formatés selon la langue dans l'interface (virgule décimale en français). Les **exports CSV restent dans un format machine stable** (point décimal, ISO 8601), quelle que soit la langue, avec une option pour l'export lisible.
- Changement de langue à chaud depuis les réglages ; par défaut, la langue du système, sinon le français.
- **Tests** :
  - `cargo xtask i18n check` (bloquant en CI) vérifie que toute clé existe en FR et en EN, qu'aucune clé n'est orpheline et que les paramètres sont cohérents entre langues ;
  - les tests Playwright tournent par défaut en FR, avec une passe de fumée en EN ;
  - une **pseudo-localisation** (textes allongés et accentués) détecte les textes en dur et les débordements de mise en page.
- Les exigences d'internationalisation entrent dans le référentiel (domaine transverse, lot MVP).

---

## 5. Qualité, tests et traçabilité

### 5.1 Référentiel d'exigences
- Fichiers `requirements/*.yaml` versionnés, **un identifiant stable par exigence**, par exemple `TS.TR1.CC.01` pour « TR 101 290 1.4 : continuity counter, paquet en double toléré une fois ».
- Champs : `id`, `titre`, `description`, `domaine` (F1–F18), `lot` (MVP…V5), `critères d'acceptation`, `niveaux de test attendus` (unitaire / fonctionnel UI / bout en bout), `statut`.
- Le référentiel est **dérivé de `01-analyse-fonctionnelle.md` et affiné lot par lot**. Celui d'un lot est rédigé et validé **avant** son développement ; c'est la première tâche de chaque plan.

### 5.2 Liaison tests ↔ exigences
- **Rust** : un attribut `#[covers("TS.TR1.CC.01")]` fourni par une petite crate `ma-trace`. Il n'a aucun effet à l'exécution ; il sert à l'analyse.
- **TypeScript** : un helper `covers("…")` appelé dans les tests Vitest et Playwright.
- `cargo xtask trace` lit les attributs et les helpers, croise avec les résultats d'exécution (JUnit de nextest, JSON de Playwright) et produit la **matrice** en Markdown et en HTML. Elle indique, pour chaque exigence, les tests qui la couvrent, leur statut et le niveau couvert.
- **Garde CI** : la CI échoue si une exigence du lot en cours n'a aucun test, ou si un test référence un identifiant inconnu.

### 5.3 Pyramide de tests
| Niveau | Outils | Rôle |
|---|---|---|
| Unitaires Rust | `cargo nextest` | parseurs, règles de conformité, calculs (PCR, buffers, loudness…) |
| Snapshots | `insta` | arbres syntaxiques, rapports, index : la sortie de référence est figée et relue une fois |
| Propriétés | `proptest` | invariants (ex. : un parseur ne panique jamais, round-trip des positions) |
| Fuzzing | `cargo-fuzz` | robustesse des parseurs face aux flux corrompus ; courtes sessions en CI, longues en nightly |
| Différentiels (oracles) | TSDuck, ffprobe, MediaInfo | comparaison automatique avec des outils de référence (TR 101 290, PTS/DTS, paramètres de séquence) |
| Composants UI | Vitest + Testing Library | comportement des composants isolés |
| Fonctionnels UI | **Playwright** sur l'interface servie en navigateur avec l'`api` mockée | la majorité des tests d'interface : rapides et déterministes |
| Bout en bout | **WebdriverIO + tauri-driver** (WebKitWebDriver), sous Xvfb | l'application réelle sur le corpus réel ; parcours critiques |

**Couverture** : `cargo-llvm-cov` côté Rust, avec un seuil minimal par crate (**décidé : 90 % des lignes minimum pour le moteur**) ; couverture Vitest/Playwright côté interface.

### 5.4 Analyse statique et hygiène
- `cargo fmt --check`, **`cargo clippy -D warnings`** (lints pédantes choisies), `cargo deny` (vulnérabilités, **licences compatibles GPL-3**, doublons), **Miri** sur les tests des portions `unsafe` (liaisons FFmpeg).
- `unsafe` est interdit hors des crates de liaison (`#![forbid(unsafe_code)]` partout ailleurs).
- Côté TS : `tsc --strict`, ESLint, Prettier.

### 5.5 Corpus de test
- **Généré** (`cargo xtask corpus gen`) : flux synthétiques reproductibles produits par FFmpeg, x264/x265, GPAC et **TSDuck**, avec injection d'erreurs (perte de paquets, CC cassé, PCR faux, PAT absente, PTS non monotones…). Les scripts sont versionnés ; les petits fichiers peuvent l'être aussi. Ces outils sont des dépendances de développement uniquement.
- **Public** (`cargo xtask corpus fetch`) : suites de conformité ITU-T (H.264, HEVC, VVC), vecteurs DASH-IF, exemples HLS, flux DVB de test. Un manifeste contient URL et SHA-256 ; les fichiers sont téléchargés dans un cache local hors Git.
- **Privé** : les fichiers réels du porteur sont placés dans un répertoire désigné par `MA_CORPUS_PRIVATE`. Les tests qui en dépendent sont **ignorés proprement** s'il est absent.
- Chaque fichier du corpus est décrit dans un manifeste : contenu, défauts attendus, exigences qu'il illustre.

### 5.6 Intégration continue
Ordre des étapes : formatage → clippy → `cargo deny` → tests Rust et couverture → fuzzing court → tests UI (Vitest, Playwright) → bout en bout Tauri (Xvfb) → **matrice de traçabilité (bloquante)** → build des paquets. L'hébergement CI est à choisir : GitHub Actions par défaut si le dépôt est sur GitHub.

**Temps de build** : workspace découpé, éditeur de liens **mold**, **sccache**, profil `dev` avec dépendances optimisées (`opt-level = 1` pour FFmpeg-sys et les crates de calcul), build release réservée à la CI et aux livrables.

---

## 6. Plateformes, distribution et licences

### 6.1 Plateformes cibles [décidé]
- **Exécution** : Ubuntu **24.04 LTS et suivantes**, x86_64.
- **Construction** : possible sur d'autres distributions, Fedora au minimum. La CI comporte un job de build et de tests dans un conteneur **Fedora** en plus d'Ubuntu 24.04. Les dépendances système (WebKitGTK 4.1, outils de build FFmpeg) sont documentées pour les deux familles (apt / dnf).
- **Matériel** : le **minimum possible**. La machine de référence est un **Intel NUC à GPU intégré** ; les portables de l'équipe ont un chipset inconnu, à supposer modeste. Conséquences :
  - **décodage matériel VA-API** (Intel Quick Sync via FFmpeg) utilisé quand il est disponible, avec **repli automatique en logiciel**. Il sert uniquement à l'affichage ; les analyses ne dépendent jamais du décodeur matériel ;
  - shaders WebGL2 simples. Le repli « rendu côté Rust » du §8 doit rester une option toujours disponible, activable dans les réglages ;
  - le spike est mesuré **sur le NUC**.
- **Usage** : en local sur le poste. Les fichiers sont **locaux ou sur un serveur de fichiers** (NFS/SMB). Les lectures positionnelles (§3.1) utilisent une lecture anticipée plus large pour les fichiers distants, et l'index persistant évite de relire un gros fichier distant à chaque ouverture.

### 6.2 Distribution et licences
- Paquets **AppImage** (FFmpeg épinglé embarqué, utilisable aussi sur Fedora) et **.deb** via le bundler Tauri ; **.rpm** possible par le même bundler. Flatpak éventuellement plus tard.
- Dépendance système : WebKitGTK 4.1.
- Licence du projet : **GPL-3.0-or-later** (§1). `cargo deny` vérifie la compatibilité de chaque dépendance.
- Points de vigilance reportés : licence du décodeur MPEG-H de Fraunhofer, potentiellement incompatible GPL (V5) ; AC-4 et Dolby Vision sont soumis à brevets (dernière version, cf. D8).

---

## 7. Risques principaux
| Risque | Impact | Parade |
|---|---|---|
| Performances et bugs WebGL/WebKitGTK selon les pilotes | rendu lent ou incorrect | spike dès le départ ; repli en rendu canvas 2D |
| Décodage UHD HEVC 10 bits trop lent sur le CPU d'un NUC (saut = décodage depuis le RAP) | seuils du spike non tenus | VA-API ; cache d'images décodées ; vignettes générées en fond |
| Débit IPC des images UHD 10 bits | navigation lente | protocole binaire, planes YUV brutes, cache, préchargement des voisines |
| Correspondance index ↔ image décodée | anti-objectif n° 2 | démultiplexage maison qui alimente le décodeur ; tests d'oracle avec ffprobe |
| Volume du travail de parsing et de conformité | délais | lots stricts ; règles déclaratives ; corpus généré tôt |
| Décodeurs instrumentés (V3) | coût élevé | trait et modèle de blocs posés dès le MVP ; choix codec par codec ensuite |
| Tests bout en bout Tauri sous Linux (WebKitWebDriver) moins mûrs | tests fragiles | majorité des tests UI en Playwright mocké ; bout en bout limités aux parcours critiques |

---

## 8. Spike de validation recommandé (jetable, avant le MVP)
Objectif : vérifier en quelques jours les hypothèses les plus risquées, avant d'engager le MVP.
1. Application Tauri minimale : ouvrir un TS H.264/HEVC UHD 10 bits de plusieurs Go.
2. Démultiplexage minimal maison → FFmpeg → planes YUV → protocole `frame://` → rendu WebGL2 avec bascule Y/U/V et zoom.
3. Mesurer : temps d'affichage d'une image après un saut, fluidité image par image, mémoire.
4. Un test WebdriverIO + tauri-driver qui ouvre le fichier et vérifie l'image affichée.
5. Trancher entre `rsmpeg` et `ffmpeg-next`.

**Critères de réussite** [proposé], mesurés sur un TS HEVC UHD 10 bits (GOP d'environ 1 s) sur la machine de référence :
| Mesure | Seuil |
|---|---|
| Affichage d'une image après un saut arbitraire | ≤ 500 ms |
| Passage à l'image suivante ou précédente | ≤ 100 ms |
| Ouverture : première image affichée | ≤ 2 s, indexation en fond |
| Mémoire résidente | ≤ 2 Go |
| Test bout en bout tauri-driver | stable sur 20 exécutions consécutives |

**En cas d'échec** [proposé] :
- si seul **WebGL** pose problème (pilotes, rendu logiciel) : on garde Tauri, mais la conversion YUV → RGB et le dessin des overlays passent côté Rust (libswscale). L'interface affiche alors une image RGBA dans un canvas 2D. Les seuils restent à respecter.
- si **WebKitGTK lui-même** est inutilisable (débit IPC, stabilité, tests bout en bout) : **retour vers le porteur du projet** avant tout engagement. L'alternative serait une interface Rust native (egui ou Slint), avec une autre stratégie de tests d'interface. Le moteur, lui, est inchangé grâce à la séparation moteur / interface.

---

## 9. Hypothèses de planification [proposé]
- **Séquence** : spike → maquettes Claude Design → plan du MVP → développement. **Les maquettes existent avant le début du développement de l'interface.** Le moteur, le contrat `api` TS et les jeux de données mock peuvent démarrer avant.
- **Portée du premier plan** : le spike et le MVP. Les lots suivants auront chacun leur propre plan, précédé de leur référentiel d'exigences.
- **Réalisation** : par des agents IA (Claude), avec une parallélisation possible entre crates indépendantes (par exemple les parseurs de différents codecs). Pas de date cible.
- **Corpus privé** : il n'est **jamais** versionné ni utilisé en CI publique ; il sert uniquement aux tests locaux. Le corpus généré (avec injection de défauts) doit donc couvrir à lui seul tous les critères d'acceptation exécutés en CI.
- **Validation de la justesse métier** [décidé] : **les outils de référence font foi**. Il n'y a pas de relecture experte des sorties. Autorités en cas de désaccord : TSDuck pour TR 101 290 et le transport TS, ffprobe pour les timestamps et les paramètres de séquence, MediaInfo en complément. Les vérifications sans oracle externe (buffers HRD, conformité de syntaxe fine) s'appuient sur les **flux de conformité officiels** (ITU-T, DASH-IF), dont le résultat attendu est connu. Un écart volontaire avec un outil de référence est justifié par la norme et consigné dans le manifeste du corpus.

## 10. Laissé au constructeur
- Noms exacts des crates, découpage fin des modules internes.
- Bibliothèques UI secondaires (icônes, composants), en cohérence avec les maquettes de Claude Design.
- Format exact des fichiers de cache et du manifeste de corpus.
- Liste précise des lints Clippy activées et seuils de couverture par crate (le minimum moteur est fixé à 90 %).
- Hébergement Git et CI.
