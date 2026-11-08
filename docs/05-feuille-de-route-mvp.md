# Feuille de route du MVP

> Entrées : `01-analyse-fonctionnelle.md` (lot MVP, §6), `02-stack-technique.md`, `03-brief-design.md` + `design/`, `04-resultats-spike.md`.
> Le MVP est découpé en sous-plans. Chacun produit un logiciel testable et fait l'objet de son propre plan détaillé (`docs/superpowers/plans/`), rédigé quand vient son tour, à partir de ce que les précédents ont réellement produit.

## Rappel du périmètre MVP (« cœur TS »)

- Ouverture de TS (SPTS/MPTS) et d'ES bruts H.264/HEVC/MPEG-2.
- Arbre PAT/PMT/SDT, débits par PID, **TR 101 290 P1/P2**.
- Vignettes et navigation, image décodée (zoom, Y/U/V), grilles de slices et tiles.
- Data view Séquence/Image et Transport, arbre syntaxique, rapport, export texte/CSV.
- Fichiers de plusieurs Go (indexation progressive et persistante), interface FR/EN.

## Sous-plans, dans l'ordre

| # | Sous-plan | Produit | Dépend de |
|---|---|---|---|
| **M0** | **Fondations** | Workspace Cargo, règles de qualité (lints, `cargo deny`, licence GPL-3), `BUILD.md` Ubuntu/Fedora, crate `ma-trace`, outil `cargo xtask trace` (matrice + garde), **référentiel d'exigences MVP** validé, CI locale + workflow | — |
| **M1** | **Transport d'image** (conditions du spike) | `ma-decode` : trait `Decoder`, FFmpeg/rsmpeg, VA-API avec détection réelle du mode et repli, **réduction sur GPU (VPP)**, région visible, **préchargement**, modèle de blocs générique (vide). Mesure : image suivante côté interface ≤ 100 ms p95, sans arrêt en arrière | M0 |
| **M2a** | **Socle moteur et démultiplexage TS** | `ma-core` (types partagés, horodatages, messages identifiés), `ma-bits` (lecteur de bits instrumenté), `ma-io` (lectures positionnelles, cache de blocs, lecture anticipée locale ou distante, troncature), `ma-es` (découpage des AU H.264/HEVC/MPEG-2), `ma-ts` (PSI/SI, PES, PCR, GOP ouverts / points de reprise, indexation progressive **en mémoire**, `TsAuSource`), corrections de `ma-ffmpeg` reportées de M1, fuzzing, mesures | M0 |
| **M2b** | **Persistance et rapport** | `ma-store` (index persistant SQLite, messages du rapport), registre `ma-core` (détection du format, ES bruts via `ma-es`), ouverture avec cache | M2a |
| **M3** | **TR 101 290 et débits** | `ma-tr101290` (P1/P2, seuils), débits par PID, oracle TSDuck | M2a |
| **M4** | **Syntaxe vidéo** | `ma-h264`, `ma-h265`, `ma-mpeg2v` : arbres syntaxiques (mode rapide / détaillé), paramètres actifs, données Séquence/Image, grilles de slices/tiles | M2a |
| **M5** | **Moteur applicatif** | `ma-engine` : session, jobs annulables avec progression, API de requêtes (structure, images, syntaxe, rapport paginé, débits, TR 101 290), exports, intégration de M1 à M4 | M1, M3, M4 |
| **M6** | **Interface** | Application Tauri : couche `api` (Tauri + mock), i18n Fluent FR/EN, composants des maquettes (AppShell, TitleBar, StructureTree, ThumbnailBar, VideoView, SyntaxView, TransportView, BitrateView, DataPanel, ReportTable, StatusBar, Welcome, Settings), tests Vitest + Playwright (mock) | M0 (contrat `api`), M5 pour le branchement réel |
| **M7** | **Intégration et livraison** | Tests bout en bout WebdriverIO sur corpus, comparaison visuelle aux maquettes, paquets AppImage / .deb, garde de livraison (toutes les exigences MVP implémentées et couvertes) | tout |

**Parallélisation possible** : après M0, M1 et M2a sont indépendants. Après M2a, M2b, M3 et M4 aussi. M6 peut démarrer sur le mock dès que le contrat `api` est fixé, au début de M5.

## État

| # | État |
|---|---|
| **M0** | Terminé, fusionné ; référentiel d'exigences MVP validé (point de passage n° 1). |
| **M1** | Implémenté (branche `mvp/m1-transport-image`) ; mesures dans `06-mesures-m1.md`. Point de passage n° 2 franchi le 2026-10-31 : *go*, avec PERF.STEP.02 précisée (options 1 et 2 de `06` §5). |
| **M2a** | Implémenté (branche `mvp/m2-socle-ts`) ; mesures dans `07-mesures-m2a.md`. Mesure NFS et corpus privé restant à faire (07 §7). |
| M2b à M7 | À faire. M3 et M4 ne dépendent que de M2a. |

## Reports de M2a vers les sous-plans suivants

Décidés à la revue finale de M2a (détail dans `07-mesures-m2a.md` §5, §6.5 et §6.6) :

- **M2b** : index persistant (SQLite) pour borner la mémoire de l'index, qui croît avec la durée et le nombre de programmes (entrées d'images et PCR de tous les programmes, environ 320 Mo par 24 h pour un SPTS à 25 i/s ; 07 §6.5).
- **M2b** : ouvrir les fichiers hors du fil d'interface, avec un indicateur d'attente : un `open()` sur NFS peut bloquer environ 100 s (période de grâce d'un serveur qui redémarre) et ne s'interrompt pas (07 §6.5, §7).
- **M3 et M6** : afficher le PTS du flux (`AuInfo::pts_raw`), jamais `FrameEntry::pts`, qui est le PTS de l'index, unique et décalé après une discontinuité (R20).
- **M5** (API du Navigator) :
  - `FrameError::Io` typé (`Truncated`, `Modified`) au lieu d'un texte, pour que l'interface distingue un fichier tronqué d'un fichier remplacé ;
  - `TsAuSource` d'une autre vidéo du même fichier (`other_video`) qui partage le cache de blocs et l'identité du fichier contrôlée, au lieu de `for_video` avec un cache fourni par l'appelant.
- **Outillage** : les cibles de fuzzing (`fuzz/`, hors workspace) ne sont compilées que si nightly est installé ; une casse d'API ne se voit pas en CI stable.

## Points de passage avec le porteur du projet

1. **Fin de M0** : validation du **référentiel d'exigences MVP** (liste, critères d'acceptation, niveaux de test).
2. **Fin de M1** : mesures de transport d'image. Les conditions du *go* sont tenues, ou bien il faut décider de la suite.
3. **Fin de M6** : revue de l'interface sur données simulées.
4. **Fin de M7** : recette du MVP.
