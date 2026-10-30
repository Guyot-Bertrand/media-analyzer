# Media Analyzer — passation des maquettes MVP vers le code

Ce dossier contient les maquettes conçues avec Claude Design à partir de `docs/03-brief-design.md`.
Elles sont la **référence visuelle et comportementale** de l'IHM du MVP (Tauri + React + TypeScript).
Le fonctionnel reste défini par `docs/01-analyse-fonctionnelle.md`, la stack par `docs/02-stack-technique.md`.

> Les fichiers `maquettes/*.dc.html` ne sont **pas** du code à copier tel quel : c'est un format de maquette
> (gabarit HTML + petite classe JS). Il faut les **réécrire en composants React/TS**, en reprenant fidèlement
> mise en page, tailles, couleurs, libellés, infobulles et données d'exemple.

---

## 1. Lire le format `.dc.html`

Chaque fichier = un composant.

- `<x-dc> … </x-dc>` : le gabarit (JSX-like). Styles **inline** = valeurs de référence (px exacts).
- `{{chemin}}` : valeur renvoyée par `renderVals()` (simple lecture, jamais d'expression).
- `<sc-for list="{{items}}" as="it">` = `items.map(it => …)` · `<sc-if value="{{cond}}">` = `cond && …`.
- `<dc-import name="Led" state="ok">` = `<Led state="ok" />` (attributs kebab-case → props camelCase).
- `<script type="text/x-dc" data-props='…'>` : `data-props` = **props du composant** (types/valeurs par défaut) ;
  `class Component extends DCLogic` : `this.props`, `this.state` / `setState`, `renderVals()` = logique de rendu.
- Chaque fichier contient en tête de script les **tokens de thème** (`TH.dark` / `TH.light`) et les **libellés FR/EN** (`L.fr` / `L.en`).

## 2. Inventaire

| Fichier | Rôle | Cible React suggérée |
|---|---|---|
| `Main.dc.html` | Coque de l'écran principal : menus, 3 panneaux, onglets, vignettes, rapport, barre d'état ; état partagé (onglet, image courante) | `AppShell.tsx` |
| `TitleBar.dc.html` | Menus + barre d'outils globale | `TitleBar.tsx` |
| `StructureTree.dc.html` | Arbre TS → PAT/CAT/SDT → PMT → ES ; mode replié (rail d'icônes) | `StructureTree.tsx` |
| `VideoView.dc.html` | Onglet Vidéo : zoom, Y/U/V, grilles slices/tiles, overlays « bientôt », états chargement / non décodable | `VideoView.tsx` (+ canvas WebGL) |
| `SyntaxView.dc.html` | Onglet Syntaxe : 2 arbres (paramètres actifs / AU), déc/hex, vue hexadécimale | `SyntaxView.tsx` |
| `TransportView.dc.html` | Onglet Transport : KPI + 12 indicateurs TR 101 290 avec histogrammes ; état partiel | `TransportView.tsx` |
| `BitrateView.dc.html` | Onglet Débits : graphe empilé par PID + tableau | `BitrateView.tsx` |
| `DataPanel.dc.html` | Panneau Données : onglets Séquence/Image et Transport, clé/valeur copiables | `DataPanel.tsx` |
| `Thumbnails.dc.html` | Vignettes + navigation + Aller à + mode RAP/I + barre de position (repères, indexation, zone corrompue, sélection de plage) | `ThumbnailBar.tsx` |
| `Report.dc.html` | Rapport : compteurs-filtres, groupes dépliables, colonnes | `ReportTable.tsx` (virtualisé) |
| `StatusBar.dc.html` | Fichier, taille, indexation + annuler, langue | `StatusBar.tsx` |
| `Led.dc.html` | LED d'état (ok, warn, err, info, run, off) — **forme + couleur** | `Led.tsx` |
| `Accueil.dc.html` | Écran d'accueil (aucun fichier ouvert) | `WelcomeScreen.tsx` |
| `Reglages.dc.html` | Fenêtre de réglages (4 sections, « Tout cocher ») | `SettingsDialog.tsx` |
| `Etats.dc.html` | Planche des états transverses (chargement, non décodable, zone corrompue, dialogues d'erreur, sélection de plage) | référence |
| `Systeme.dc.html` | Système visuel : palettes, LED, badges, typo, composants de base | référence → `tokens.css` |
| `Syntaxe`, `Transport`, `Indexation`, `MainEN`, `Portable` `.dc.html` | Variantes de `Main` (onglet, indexation 64 %, anglais, 1366×768) | captures de recette |
| `canvas.json` | Disposition du canevas (ignorer pour le code) | — |

## 3. Tokens

Voir `tokens.css` (variables CSS sombre/claire, extraites des maquettes). À utiliser partout, aucun hex en dur.
Polices : **IBM Plex Sans** (UI, 12 px / 16 px) et **IBM Plex Mono** (valeurs, PID, offsets, hex, syntaxe ; `font-variant-numeric: tabular-nums`).
À embarquer localement dans l'app (pas de Google Fonts au runtime : application hors ligne).

Règles transverses :
- État = **LED à forme distincte** (disque+coche / triangle+! / octogone+× / cercle i / anneau / pointillés) ; type d'image = **badge plein** (I/IDR `#C8323A`, P `#237A35`, B `#2563D6`, lettre blanche).
- Accent violet réservé à la **sélection et au focus** (focus : contour 2 px `#7C5CFF`, offset 2 px).
- Lignes de tableau 21–22 px, arbres 34–36 px (2 lignes), cibles ≥ 24 px, grille de 4 px.

## 4. Comportements à reproduire

- **Synchronisation** : l'image courante est un état global (store) ; vignettes, vue Vidéo, Données, Syntaxe, position et rapport s'y abonnent.
- Clavier : ←/→, Ctrl+←/→ (I préc./suiv.), Début/Fin, Ctrl+G, F8 / Maj+F8 (erreur suiv./préc.), Ctrl+1/2/3 (panneaux), Alt+1…4 (onglets). Raccourcis rappelés dans les infobulles (`title` dans les maquettes → composant Tooltip).
- Panneaux **redimensionnables et masquables** (les maquettes montrent des séparateurs de 1 px).
- Rapport et syntaxe : listes **virtualisées** (millions de lignes), regroupement des messages répétés.
- Indexation / analyse : non bloquantes, progression + annulation ; la barre de position hachure la partie non indexée.
- i18n FR/EN à chaud : extraire les dictionnaires `L.fr` / `L.en` des maquettes vers des fichiers de traduction ; les noms d'éléments de syntaxe restent en anglais.

## 5. Données d'exemple

Les maquettes contiennent un jeu cohérent (TS 4,02 Go, HEVC Main 10 2160p50 PID 0x0100, AAC 0x0101, E-AC-3 0x0102,
DVB Subtitles 0x0103, SCTE-35 0x01F4, 3 erreurs CC sur 0x0101, 1 PCR accuracy, image courante déc. 12 450 / prés. 12 452).
À reprendre comme **fixtures** (`src/mocks/`) pour développer l'IHM avant que le moteur soit branché.

## 6. Hors périmètre de ces maquettes

Overlays fins (type de bloc, QP, vecteurs) : boutons présents mais désactivés « bientôt ». Découpe : état visuel de sélection seulement.
Onglets futurs (PTS/DTS, buffers, audio, loudness, sous-titres, SCTE-35, qualité, HLS/DASH) : la barre d'onglets doit accepter des onglets contextuels supplémentaires.

## 7. Démarrage suggéré pour Claude Code

1. Lire ce fichier, puis `maquettes/Systeme.dc.html` et `maquettes/Main.dc.html`.
2. Créer `tokens.css`, `Led`, `Tooltip`, puis la coque `AppShell` avec panneaux redimensionnables.
3. Implémenter les composants dans l'ordre : Thumbnails → DataPanel → VideoView → Report → StructureTree → SyntaxView → TransportView → BitrateView → StatusBar → Welcome → Settings.
4. Comparer chaque composant à sa maquette (ouvrir le `.dc.html` comme référence de cotes et de libellés).
