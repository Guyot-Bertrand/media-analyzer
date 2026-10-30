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
| **M2** | **Socle moteur et démultiplexage TS** | `ma-core` (types, messages, registre), `ma-bits` (lecteur de bits instrumenté), `ma-io` (lectures positionnelles, cache de blocs), `ma-ts` (PSI/SI, PES, PCR, **découpage des AU**, GOP ouverts / points de reprise), `ma-store` (index persistant SQLite, rapport), indexation progressive | M0 |
| **M3** | **TR 101 290 et débits** | `ma-tr101290` (P1/P2, seuils), débits par PID, oracle TSDuck | M2 |
| **M4** | **Syntaxe vidéo** | `ma-h264`, `ma-h265`, `ma-mpeg2v` : arbres syntaxiques (mode rapide / détaillé), paramètres actifs, données Séquence/Image, grilles de slices/tiles | M2 |
| **M5** | **Moteur applicatif** | `ma-engine` : session, jobs annulables avec progression, API de requêtes (structure, images, syntaxe, rapport paginé, débits, TR 101 290), exports, intégration de M1 à M4 | M1, M3, M4 |
| **M6** | **Interface** | Application Tauri : couche `api` (Tauri + mock), i18n Fluent FR/EN, composants des maquettes (AppShell, TitleBar, StructureTree, ThumbnailBar, VideoView, SyntaxView, TransportView, BitrateView, DataPanel, ReportTable, StatusBar, Welcome, Settings), tests Vitest + Playwright (mock) | M0 (contrat `api`), M5 pour le branchement réel |
| **M7** | **Intégration et livraison** | Tests bout en bout WebdriverIO sur corpus, comparaison visuelle aux maquettes, paquets AppImage / .deb, garde de livraison (toutes les exigences MVP implémentées et couvertes) | tout |

**Parallélisation possible** : après M0, M1 et M2 sont indépendants. Après M2, M3 et M4 aussi. M6 peut démarrer sur le mock dès que le contrat `api` est fixé, au début de M5.

## Points de passage avec le porteur du projet

1. **Fin de M0** : validation du **référentiel d'exigences MVP** (liste, critères d'acceptation, niveaux de test).
2. **Fin de M1** : mesures de transport d'image. Les conditions du *go* sont tenues, ou bien il faut décider de la suite.
3. **Fin de M6** : revue de l'interface sur données simulées.
4. **Fin de M7** : recette du MVP.
