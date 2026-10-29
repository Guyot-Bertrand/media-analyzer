# Brief de design : Media Analyzer (MVP)

> Ce brief est autonome. Il sert à concevoir avec Claude Design les maquettes de l'interface du **MVP** d'une application desktop Linux d'analyse de fichiers vidéo broadcast.
> Projet source : `docs/01-analyse-fonctionnelle.md` (fonctionnel) et `docs/02-stack-technique.md` (stack).

---

## 1. Le produit en bref

**Media Analyzer** est un analyseur **hors ligne** de fichiers vidéo compressés, inspiré des analyseurs broadcast professionnels (type Sencore CMA 1820). Il n'est **pas un lecteur vidéo**. On s'en sert pour **inspecter un fichier image par image** :
- sa structure (programmes, flux, PID) ;
- la syntaxe des en-têtes, bit à bit ;
- la conformité aux normes broadcast (TR 101 290…) ;
- les métadonnées de chaque image, et leur lien avec l'audio, les sous-titres et les autres pistes de données.

**Utilisateurs** : ingénieurs et techniciens vidéo/broadcast, experts du domaine. Ils veulent de la **densité d'information**, de la **précision** et de la **vitesse de navigation**, pas de la pédagogie grand public.

**Contexte d'usage** :
- poste de travail Linux, souvent un **petit PC (Intel NUC) ou un portable** avec un GPU intégré modeste ; écrans de 1366×768 (portable) à 2560×1440 ;
- fichiers MPEG-2 TS de **plusieurs Go**, locaux ou sur un serveur de fichiers ;
- sessions longues d'investigation : on cherche une erreur, on y navigue, on compare des images.

**Esprit visuel recherché** : un **outil professionnel dense et sobre**, dans la lignée des IDE ou des analyseurs de protocole (Wireshark, outils de debug GPU), mais moderne et soigné. Thème **sombre par défaut** (l'image vidéo doit ressortir), thème clair disponible.

---

## 2. Contraintes de conception

| Contrainte | Détail |
|---|---|
| Technologie cible | Application **Tauri** (fenêtre web WebKitGTK) en **React + TypeScript**. Les maquettes doivent être réalisables en HTML/CSS/React standard. L'image vidéo est rendue dans un **canvas WebGL** : dans la maquette, une image fixe ou un placeholder suffit. |
| Langues | **Français (langue principale) et anglais**, commutables à chaud. Prévoir des libellés ~30 % plus longs en anglais technique ou en pseudo-localisation, sans casser la mise en page. Les noms d'éléments de syntaxe des normes (`pic_order_cnt_lsb`, `PCR_PID`) restent en anglais, en police monospace. |
| Code couleur d'état (obligatoire, transverse) | **Vert = OK · Orange = avertissement · Rouge = erreur.** Utilisé partout : LED, lignes de syntaxe, messages, timelines. Les couleurs doivent rester distinguables par un daltonien : doubler la couleur d'une **icône ou d'une forme**. |
| Types d'image | Code couleur constant : **I/IDR = rouge, P = vert, B = bleu** (convention du domaine). Il doit rester lisible malgré le code couleur d'état : prévoir une distinction de style (badge plein pour le type, LED ou icône pour l'état). |
| Densité | Beaucoup de données numériques : police **monospace** pour les valeurs, les offsets, l'hexadécimal et les PID ; chiffres tabulaires ; tailles compactes. |
| Performance perçue | Les listes peuvent contenir des **millions de lignes** (messages, paquets) : tableaux et arbres **virtualisés**, sans pagination visible. Les analyses longues tournent en fond : indicateurs de **progression** non bloquants, et **annulation** possible. |
| Clavier | Navigation image par image au clavier (←/→ image, Ctrl+←/→ image I précédente/suivante, Début/Fin, Ctrl+G « aller à l'image n° »). Raccourcis visibles dans les tooltips. |
| Tooltips | **Chaque champ, LED, bouton ou message** a un tooltip explicatif (signification de l'élément, référence normative). |
| Accessibilité | Contraste AA minimum, focus clavier visible, tailles de clic raisonnables malgré la densité. |

---

## 3. Structure de l'écran principal

Disposition de référence, inspirée des analyseurs broadcast. Les panneaux sont **redimensionnables et masquables**.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ Barre de titre / menus : Fichier · Affichage · Analyse · Outils · Aide   │
│ Barre d'outils : ouvrir · zoom · Y/U/V · grilles · export · réglages      │
├───────────────┬─────────────────────────────────────────┬────────────────┤
│ STRUCTURE     │ [Vidéo] [Syntaxe] [Transport] [Débits]   │ DONNÉES        │
│ (arbre)       │ ┌─────────────────────────────────────┐ │ (onglets :     │
│               │ │                                     │ │  Séquence/     │
│ Transport     │ │           VUE PRINCIPALE            │ │  Image,        │
│ Stream        │ │     (selon l'onglet sélectionné)    │ │  Transport)    │
│  ├ PAT        │ │                                     │ │                │
│  ├ PMT 0x100  │ └─────────────────────────────────────┘ │                │
│  │ ├ H.264    │                                         │                │
│  │ └ AAC      │                                         │                │
├───────────────┴─────────────────────────────────────────┴────────────────┤
│ VIGNETTES : ◀◀ ◀I ◀ [img][img][IMG][img][img][img][img] ▶ I▶ ▶▶  Aller à│
│ ════════════════════ barre de position dans le fichier ═══════════════════│
├──────────────────────────────────────────────────────────────────────────┤
│ RAPPORT : filtres (types, familles) · recherche · compteurs ●12 ●48 ●3    │
│ Type │ Famille │ Sous-famille │ Image │ Position │ Description            │
├──────────────────────────────────────────────────────────────────────────┤
│ Barre d'état : fichier · taille · indexation 64 % ▓▓▓░ [annuler] · langue │
└──────────────────────────────────────────────────────────────────────────┘
```

**Synchronisation entre vues (essentielle)** : sélectionner une image (vignette, rapport, graphe, Go to) met à jour **toutes** les vues : image, données, syntaxe, position. Cliquer un message du rapport amène à l'image et à l'élément concernés.

---

## 4. Écrans et composants à maquetter (MVP)

### 4.1 Écran d'accueil (aucun fichier ouvert)
- Zone « ouvrir un fichier » (glisser-déposer ou bouton) et liste des **fichiers récents** (nom, taille, chemin, date, indicateur « index en cache » permettant une réouverture instantanée).
- Résumé des formats et analyses disponibles.

### 4.2 Arbre de structure (panneau gauche)
- Pour un TS : `Transport Stream` → `PAT` → `CAT` → `PMT (programme n, nom de service)` → flux élémentaires (vidéo, audio, sous-titres, teletext, SCTE-35, données privées).
- Colonnes : **Hiérarchie** (icône par type de flux), **PID** (hex et décimal), **Info** (codec, résolution, langue, porteur du PCR).
- LED d'état par nœud (agrégation des erreurs). Flux non supporté : libellé grisé « Vidéo » ou « Audio ».
- Sélectionner un flux change le contenu des onglets.

### 4.3 Barre de vignettes (navigation)
- 7 à 11 vignettes selon la largeur. Pour chacune : **type d'image** (badge coloré I/P/B/IDR), **index de décodage**, **index de présentation**, petites étiquettes des paramètres présents (SPS, PPS…).
- Vignette courante mise en évidence.
- Boutons : début, I précédente, image précédente, image suivante, I suivante, fin ; champ **« Aller à »** ; bascule **mode RAP / mode I seulement**.
- **Barre de position** sous les vignettes : position dans le fichier, avec repères d'erreurs (traits rouges et orange) cliquables.
- Tooltip vignette : position en octets, n° de paquet, PTS.
- Sélection d'une plage (clic droit début/fin) pour la future découpe : prévoir l'état visuel, la fonction viendra en V1.

### 4.4 Onglet Vidéo (vue principale)
- Image décodée, **zoom** (50 % à 400 %, sans lissage, pixels nets), déplacement à la souris.
- Bascules **Y / U / V** (afficher ou masquer chaque composante).
- Overlays disponibles au MVP : **grille de slices**, **grille de tiles** (HEVC). Prévoir l'emplacement des overlays futurs (type de bloc, QP, vecteurs de mouvement), désactivés et marqués « bientôt », et un **curseur de transparence**.
- États : chargement de l'image (squelette ou spinner discret), **image non décodable** (message explicite avec bouton « aller à l'image I suivante »).

### 4.5 Onglet Syntaxe
- **Deux arbres côte à côte** : à gauche, les **paramètres actifs** (SPS/PPS, VPS/SPS/PPS…) ; à droite, l'**access unit courante** (liste des unités NAL dépliables).
- Colonnes : **Élément** · **Valeur** · **Longueur (bits)** · **Offset (bits)** · **Interprétation**.
- Ligne en erreur : fond ou texte rouge, avec un tooltip qui cite la règle violée. Ligne OK : sobre ; le vert ne sert que pour les statuts explicites.
- Bascule décimal / hexadécimal, recherche dans l'arbre, tout déplier ou replier.
- Volet optionnel : **vue hexadécimale** des octets, avec la plage de l'élément sélectionné surlignée.

### 4.6 Onglet Transport (TR 101 290)
- **Tableau de bord** des indicateurs TR 101 290 :
  - **Priorité 1** : TS sync loss, Sync byte, PAT, Continuity count, PMT, PID manquant ;
  - **Priorité 2** : Transport error, CRC, PCR repetition, PCR discontinuity, PCR accuracy, PTS repetition.
- Pour chaque indicateur : LED d'état, nombre d'occurrences, première et dernière occurrence (cliquables), mini-histogramme de répartition sur la durée du fichier.
- Filtre par PID ou programme.

### 4.7 Onglet Débits
- Graphe du débit dans le temps, par PID (séries empilées ou superposées), avec débit total.
- Tableau : PID, type, débit min / moyen / max, part du multiplex.

### 4.8 Panneau Données (droite)
- **Séquence/Image** :
  - séquence : profil, niveau, résolution, fréquence d'images, SAR/DAR, chroma, profondeur de bits, nombre d'images, durée, débit ;
  - image : type, index de décodage et de présentation, POC, taille en octets, structure (frame/champ), scan ;
  - statistiques : % intra/inter/skip (affichées « bientôt » au MVP).
- **Transport** : PID, PTS, DTS, PTS − PTS0, n° et position du paquet.
- Présentation : listes clé/valeur compactes et regroupées. Les valeurs sont copiables. Tooltip d'explication sur chaque clé.

### 4.9 Rapport (panneau bas)
- Tableau virtualisé : **Type** (icône et couleur), **Famille** (TS, H.264, HEVC…), **Sous-famille** (TR 101 290-1, Syntax, Profiles…), **Image**, **Position** (octet), **Description** (traduite).
- Filtres par type et famille, recherche texte, compteurs info/warning/erreur cliquables (filtres rapides).
- Messages **groupés** quand ils se répètent (« Continuity count error × 1 254 », dépliable).
- Double-clic : navigation vers l'image et l'élément concernés.
- Bouton d'export (CSV / texte).

### 4.10 Réglages (fenêtre ou panneau)
- Sections : Affichage (langue, thème, hexadécimal), Journaux, TS / TR 101 290 (activer chaque test, seuils comme le délai de PID manquant), Vidéo (nombre de GOP décodés par saut, décodage matériel).
- Chaque section a une case **« tout cocher »**.

### 4.11 États transverses
- **Indexation en cours** sur un gros fichier : l'interface est utilisable immédiatement sur la partie indexée ; la barre de position montre la partie déjà indexée ; la progression est dans la barre d'état et peut être annulée.
- **Analyse en cours** (TR 101 290) : LED « en cours » ; les résultats arrivent progressivement.
- **Erreurs** : fichier illisible, format non supporté, fichier distant inaccessible. Messages clairs, sans jargon système.
- **Fichier corrompu** : l'outil ne doit **jamais** paraître « cassé ». Les zones corrompues sont signalées comme des données, pas comme une panne.

---

## 5. Évolutions à anticiper (ne pas maquetter en détail)

La mise en page doit pouvoir accueillir sans refonte les onglets et panneaux suivants :
- **PTS/DTS** (graphe chronologique par access unit), **analyse de buffers** (courbes d'occupation), **audio** (formes d'onde multicanal, synchronisation A/V), **loudness** (graphes LUFS) ;
- **sous-titres / CC / Teletext** incrustés sur l'image, avec un panneau de données dédié ;
- **SCTE-35** (timelines des coupures publicitaires) ;
- **qualité d'image**, y compris une **vue de comparaison** de deux fichiers (côte à côte, rideau, carte de différence) ;
- **overlays fins** des décodeurs (type de bloc, QP, vecteurs, partitions) ;
- **HLS/DASH** (une timeline par représentation).

---

## 6. Livrables attendus de Claude Design
1. **Système visuel** : palette sombre et claire (dont les couleurs d'état et de type d'image), typographies (sans-serif UI et monospace données), espacements, composants de base (LED, badges, boutons-icônes, onglets, tableaux denses, arbres).
2. **Maquettes** :
   - écran d'accueil ;
   - écran principal en onglet **Vidéo** ;
   - onglet **Syntaxe** ;
   - onglet **Transport** ;
   - état **indexation en cours** ;
   - **réglages**.
3. Une variante **portable 1366×768** de l'écran principal (panneaux repliés).
4. Une capture **en anglais** de l'écran principal pour valider la tenue des libellés.
5. Si possible, des **composants React** réutilisables (tableau virtualisé, arbre, barre de vignettes, LED), avec des **données d'exemple réalistes**, par exemple :
   - un TS de 4 Go, 1 programme ;
   - vidéo HEVC Main 10, 3840×2160, 50 i/s, PID 0x0100 ;
   - audio AAC-LC stéréo, PID 0x0101 (fra) ; E-AC-3 5.1, PID 0x0102 (eng) ;
   - DVB subtitles, PID 0x0103 ; SCTE-35, PID 0x01F4 ;
   - 3 erreurs de continuity counter sur 0x0101, 1 PCR accuracy en warning.
