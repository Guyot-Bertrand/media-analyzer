# Media Analyzer : analyse fonctionnelle

> Source : *CMA 1820 Compressed Media Analyzer, User Manual rev. 05.30* (Sencore, janvier 2021, 253 p.).
> Objectif : extraire et organiser le périmètre fonctionnel de référence pour une application desktop Linux d'analyse détaillée de fichiers vidéo.
> Étapes suivantes : (2) stack technique, (3) maquettes frontend via Claude Design, (4) développement.

---

## 0. Décisions de cadrage (2026-10-29)

| # | Sujet | Décision |
|---|---|---|
| D1 | Codecs et formats récents | **Inclus** : AV1, VP9, VVC/H.266, métadonnées HDR (HDR10, HDR10+, HLG, Dolby Vision), conteneurs MKV/WebM. **AV2 anticipé** : l'emplacement du module est prévu, à remplir quand la spécification et un décodeur de référence seront stables. |
| D2 | Usage cible | **Broadcast et OTT**, avec le **broadcast en priorité** (TS, DVB, TR 101 290, SCTE-35, Teletext). |
| D3 | Formats anciens | **Teletext conservé.** VC-1, ASF, FLV et MXF sont **écartés pour l'instant**. L'architecture doit permettre de les ajouter plus tard sans refonte (conteneurs et codecs sous forme de modules). |
| D4 | Qualité d'image | **Deux familles** : métriques sans référence (reprises du CMA) et métriques avec référence par comparaison de deux fichiers (PSNR, SSIM, VMAF). |
| D5 | Mode ligne de commande / batch | **Pas en V1.** L'IHM seule suffit pour commencer. Il reste souhaitable de séparer le moteur d'analyse de l'IHM pour pouvoir l'ajouter ensuite. |
| D6 | Volumétrie | Fichiers de **plusieurs Go**. Il faut donc une indexation progressive, un accès aléatoire sans tout charger en mémoire et des analyses longues non bloquantes (progression, abandon). |
| D7 | Décodeurs instrumentés | **Pas en V1.** Ils font l'objet d'une version dédiée. Jusque-là, on utilise un décodage standard (image reconstruite) et des parseurs de syntaxe maison. Les overlays fins (type, QP, vecteurs, partitions, coefficients, étapes du décodage) arrivent avec cette version. |
| D8 | Audio nouvelle génération | **Opus** est traité avec le WebM (coût faible). **AC-4 et MPEG-H 3D Audio** font l'objet d'une version dédiée (voir §7). |

---

## 1. Vue d'ensemble

Le CMA 1820 est un **analyseur hors ligne, purement logiciel**, de flux audio/vidéo compressés. Il ouvre un fichier, en reconstruit la structure (conteneur → programmes → flux élémentaires), décode les flux supportés, puis fournit :

- une **inspection visuelle** au niveau de l'image (overlays de macroblocs/CTB, vecteurs de mouvement, QP…) ;
- une **inspection syntaxique** bit à bit (NAL / BDU / DU, playlists, MPD) ;
- des **analyses de conformité** (TR 101 290, profils/niveaux, buffers HRD/T-STD, PTS/DTS, sous-titres, SCTE-35, HLS/DASH) ;
- des **mesures** (bitrate, loudness, qualité d'image) ;
- des **outils d'édition légère** (découpe TS, extraction ES, export audio).

Le produit commercial comprend aussi deux outils annexes, **hors périmètre** de cette analyse :
- *Media Extractor* : extraction de flux depuis des captures réseau (pcap, NetMonitor…), RTP/UDP, SAP/RTSP/SIP ;
- *Broadband Recorder* : capture de trafic IP.

---

## 2. Formats d'entrée

> Les tableaux 2.1 à 2.3 décrivent le périmètre du **CMA 1820**. Le périmètre retenu pour l'application est donné en **2.4**.

### 2.1 Conteneurs
| Conteneur | Remarques |
|---|---|
| MPEG-2 TS / TTS | paquets de 188/204/208 octets, mono ou multi-programme (MPTS) ; extensions `.ts .trp .mpeg .mpg` |
| MP4, MP4 fragmenté, 3GP, MOV (QuickTime) | ISO/IEC 14496-14/-15, 3GPP TS 26.244 |
| ASF | Microsoft |
| FLV | Adobe |
| MXF | SMPTE 377M / 379M |
| HLS | playlists Master/Media `.m3u8` + segments TS |
| MPEG-DASH | MPD (ISO/IEC 23009-1), profils Full / ISO BMFF (OnDemand, Live, Main) / MPEG-2 TS (Main, Simple) |
| Raw byte (ES nu) | voir ci-dessous |

Une **extension inconnue** peut être associée manuellement à un format supporté.

### 2.2 Flux élémentaires
| Type | Formats |
|---|---|
| Vidéo | H.265/HEVC (`.h265 .265 .hevc`), H.264/AVC (`.h264 .264`), MPEG-2 Video (`.m2v`), VC-1 (`.vc1 .v9e .rcv`) |
| Audio | AAC / HE-AAC v1/v2 (ADTS, LATM/LOAS ; `.aac`), Dolby AC-3 / E-AC-3 (`.ac3`), MPEG-1/2 Audio couches I/II/III (`.mp1 .mp2 .mp3 .mpa`), PCM ; AES-3 détecté mais non analysé |
| Données | Closed Captions CEA-608/708/SCTE-20, DVB Teletext, DVB Subtitles, VPS/WSS, DVB-TTML, WebVTT, SMPTE-TT, SCTE-35 |

### 2.3 Matrice conteneur × contenu (simplifiée)
| | Raw | TS | MXF | MP4/3GP/MOV/FLV | HLS | DASH | ASF |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| MPEG-2 Video | ✓ | ✓ | ✓ | | | | |
| H.264/AVC | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | |
| H.265/HEVC | ✓ | ✓ | | ✓ | ✓ | ✓ | |
| VC-1 | ✓ | ✓ | | | | | ✓ |
| Audio (AAC, AC-3, MPEG) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Teletext / DVB Subs / VPS-WSS / SCTE-35 / DVB-TTML | | ✓ | | | | | |
| WebVTT | | | | | ✓ | | |
| SMPTE-TT | | | | | | ✓ | |

Restriction : sur un fichier **TTS**, les analyses buffer, PTS/DTS et TR 101 290 ne sont pas disponibles.

### 2.4 Périmètre retenu pour l'application
| Catégorie | Retenu | Ajouté (absent du CMA) | Reporté |
|---|---|---|---|
| Conteneurs | MPEG-2 TS/TTS, MP4/fMP4, MOV, 3GP, HLS, MPEG-DASH, ES bruts | **MKV, WebM** ; TS sur 204/208 octets | ASF, FLV, MXF |
| Vidéo | H.264/AVC, H.265/HEVC, MPEG-2 Video | **AV1, VP9, VVC/H.266** ; métadonnées **HDR** (SEI mastering display / content light level, HDR10+, HLG, Dolby Vision RPU) ; **AV2** (anticipé) | VC-1 |
| Audio | AAC/HE-AAC, AC-3/E-AC-3, MPEG Audio, PCM | **Opus** (WebM) ; **AC-4, MPEG-H 3D Audio** (version dédiée) | AES-3 |
| Données | CEA-608/708, SCTE-20, DVB Subtitles, **Teletext**, VPS/WSS, DVB-TTML, SCTE-35, WebVTT, SMPTE-TT/IMSC | | |

**Point d'attention.** Pour AV1, VP9 et VVC, les overlays de blocs, vecteurs et QP exigent aussi un décodeur instrumenté. Les structures diffèrent de celles d'AVC/HEVC : superblocs et partitions AV1, arbre de codage VVC. Il faut donc prévoir dès le départ un **modèle de données de blocs générique**, qui ne soit pas limité au macrobloc.

---

## 3. Structure de l'interface (référence pour Claude Design)

```
┌───────────────────────────────────────────────────────────────────────┐
│ Barre de menus  (File · View · Analysis · Player · Tools · Help)      │
├──────────────┬───────────────────────────────────────┬────────────────┤
│ Vue          │  Onglets d'analyse (contextuels)      │  Data View     │
│ hiérarchique │  ┌─────────────────────────────────┐  │  (onglets      │
│ (arbre de    │  │                                 │  │   contextuels :│
│ structure du │  │          MAIN VIEW              │  │   Seq/Pic,     │
│ fichier)     │  │                                 │  │   Slice/MB,    │
│              │  └─────────────────────────────────┘  │   Coeff, CC…)  │
│              │  Barre d'outils (overlays, zoom, YUV…)│                │
├──────────────┴───────────────────────────────────────┴────────────────┤
│ Thumbnail view : 7 à 11 vignettes + navigation + sélection découpe    │
├───────────────────────────────────────────────────────────────────────┤
│ Report view : Type · Famille · Sous-famille · Index · Position · Desc │
└───────────────────────────────────────────────────────────────────────┘
```

**Principes transverses de l'IHM**
- **Code couleur unique** : vert = OK, orange = warning, rouge = erreur (LED, lignes de syntaxe, courbes, timelines).
- **Tooltips exhaustifs** sur chaque champ, LED, bouton, message.
- **Onglets et boutons contextuels** : seuls ceux pertinents pour le flux sélectionné sont affichés.
- **Vues synchronisées** : la sélection d'une image se propage (vignettes ↔ vue principale ↔ PTS/DTS ↔ audio ↔ loudness ↔ qualité ↔ SCTE-35), avec mode *Lock* optionnel.
- **Navigation d'erreur** : boutons « erreur précédente / suivante » dans les vues d'analyse.
- **Export texte/CSV** du contenu de la vue courante, partout.
- Vues hiérarchique et rapport masquables ; zoom de l'image sans interpolation.
- Écran d'accueil vide affichant les modules disponibles.

---

## 4. Domaines fonctionnels

Chaque domaine est décrit par : *objectif · fonctions · vues · paramètres*.

### F1. Ouverture et structure du fichier
- Ouvrir un fichier (conteneur, ES brut, playlist HLS, MPD DASH).
- Détection automatique du format, association manuelle des extensions inconnues.
- **Arbre hiérarchique** :
  - TS : Transport Stream → PAT → CAT → PMT (n° programme) → ES (vidéo, audio, sous-titres, teletext, SCTE-35, VBI, tables privées) ; colonnes *PID* et *Info* (TS ID, n° programme, PCR porté par la vidéo ou par un PID dédié, nom de service via SDT si DVB) ;
  - conteneurs fichiers : racine nommée selon le format, puis pistes ;
  - HLS : fichier → Master playlist → Media playlists (colonne URI) ;
  - DASH : MPD → Adaptation Sets → Representations (colonne @id) ;
  - ES brut : « Raw Byte file ».
- Sélection automatique de la première vidéo, puis sélection manuelle d'un ES.
- Flux non supportés signalés comme « Audio » / « Video » sans détail.
- Calcul des **bitrates** min/moy/max par ES.

### F2. Navigation vidéo (Thumbnail view)
- Décodage et affichage de 7 à 11 vignettes autour de la zone d'intérêt.
- Pour chaque vignette : type d'image (couleur : rouge = I/IDR/CRA/BLA/SI, vert = P/SP/Skip, bleu = B/BI), index de décodage, index de présentation, paramètres présents (SPS/PPS/VPS, SH/EH, SH/GH).
- Navigation : début/fin, I précédente/suivante, image précédente/suivante, segment précédent/suivant (OTT), *Go to* par index de présentation, barre de défilement (re-décodage depuis l'I précédente).
- **Mode RAP** (IDR ; IDR/CRA/BLA ; I + entry point ; I closed GOP) et **mode I seulement**.
- Tooltip vignette : position paquet / position dans le paquet / offset fichier.
- **Lecture vidéo seule** en mode raw (~25 i/s, overlays figés pendant la lecture).
- Paramètre : nombre de GOP décodés lors des sauts.

### F3. Analyse visuelle de l'image (onglet Video Analysis)
- Affichage de l'image décodée courante.
- **Grilles** : tiles (HEVC), slices, MB/CTB, partitions MB, CB / PB / TB (HEVC) ; couleur de grille blanc/noir.
- **Overlays** : type MB/CB (intra, inter, skip, direct/merge, 1-MV/4-MV, mixte, no MC), QP (palette absolue, nuances relatives ou valeur numérique), taille en bits, frame/field (AVC, MPEG-2), overlay personnalisable, transparence réglable.
- **Vecteurs de mouvement** : forward/backward (MPEG-2, VC-1) ou L0/L1 (AVC, HEVC), couleurs distinctes.
- **Focus macrobloc** : survol → infos ; clic → verrouillage pour suivre un MB/CTB/CB à travers la séquence.
- **Composantes Y / U / V** activables individuellement (chroma ré-échantillonnée à la taille luma).
- **États intermédiaires du décodage** (AVC, MPEG-2, si l'option coefficients est active) : image avant filtre de boucle, image prédite, résidu.
- **Zoom** sur 5 niveaux (×0,5 à ×4), sans interpolation.
- Options d'affichage : étendre les champs à la taille d'une image, affichage au rapport d'aspect réel.
- Affichage incrusté des sous-titres / CC / teletext sur l'image.

### F4. Données vidéo (Data View)
Contenu variable selon le codec ; exportable en texte.

| Onglet | Contenu |
|---|---|
| **Seq/Picture** | Séquence : profil, niveau, tier (HEVC), fréquence d'images, nombre d'images, durée, SAR/DAR, format chroma, profondeur de bits, taille CTB, bitrate, GOP (closed GOP, broken link). Image : type, index décodage/présentation, POC / référence temporelle, tailles (pixels, MB/CTB, tiles, octets), AFD, timecode (et SMPTE 328), scan (progressif/entrelacé/MBAFF), structure (frame/champ), pic_struct, statistiques de prédiction (% intra/inter/skip/direct/merge/field). |
| **Slice/MB** (AVC, MPEG-2, VC-1) | Slice : type, numéro, entropie CABAC/CAVLC, QP / Q scale, taille. MB : index/ligne/colonne, mode de prédiction et partition, direction, taille en bits, QP, CBP luma/chroma, listes de référence L0/L1, vecteurs de mouvement par bloc 4x4/8x8, modes intra, type de transformée (VC-1), mode MV. |
| **Coding Details** (HEVC) | Tile (index, taille), slice (type, QP, SAO, deblocking), CTB (index, taille), CB (mode, QP, taille) ; prédiction (MV par PB, modes intra luma/chroma, index de référence) ou transformée (CBF luma/chroma par bloc). |
| **MB Coeff** (AVC, MPEG-2) | Coefficients décodés, avant filtre de boucle, prédits, résiduels, déquantifiés, quantifiés ; sélection du bloc luma/chroma, aperçu du bloc. Option gourmande en mémoire. |
| **Transport** | Selon le conteneur : TS (PID, PTS, DTS, index et position du paquet, PTS−PTS0), MP4/MOV/FLV (track ID, timestamp, position, sample ID), ASF (stream ID, objet), MXF (track, PTS/DTS, sample), segment HLS/DASH/EBP (index, 1re image, début, durée, URI). |
| **Color** | Colorimétrie. |

### F5. Syntaxe et conformité du flux vidéo (onglet Syntax Analysis)
- **Deux arbres** : paramètres actifs (SPS/PPS, VPS/SPS/PPS, SH/EH, SH/SE/GH) à gauche, unités de l'access unit courante (NAL, BDU, DU) à droite.
- Colonnes : élément, valeur, longueur en bits, offset en bits, interprétation (avec OK / erreur en rouge) ; affichage décimal ou hexadécimal.
- Interprétation des user data / SEI T.35 (CEA-608/708).
- **Journal de parsing** : une ligne par NAL/BDU/DU, avec position, type, longueur et attributs clés.
- **Conformité générale** :
  - ordre des unités (SPS avant PPS, SEI avant l'image, EOS/EOB, ASO…) ;
  - **profils** (Baseline/Main/Extended/High ; Main/Main10/MainStill ; Simple/Main/Advanced ; profils MPEG-2) ;
  - **niveaux** (MaxFS, dimensions, contraintes de tiles HEVC) ;
  - **MPEG-2 Systems Amd3** (AUD, HRD/VUI) ;
  - **DVB guidelines** (intervalle RAP ≤ 5 s, signalisation RAP dans le TS, contraintes SDTV/HDTV : profil, niveau, aspect, fréquence d'images, VUI) ;
  - catalogue d'erreurs VC-1 (stream, sequence, entry point, picture, frame, user data).
- **Conformité par élément de syntaxe** : valeur unique, plage, liste de valeurs, dépendance à d'autres valeurs.
- **Ordre de sortie** (AVC) : POC = 0 sur IDR, incréments de POC, monotonie et écarts de PTS.
- Paramètres : activation/désactivation de chaque test.

### F6. Analyse du transport MPEG-2 TS
- Lancement automatique à l'ouverture.
- **TR 101 290 priorité 1** : TS sync loss (hystérésis), sync byte, PAT (fréquence, PID 0, scrambling), continuity counter, PMT (fréquence, scrambling), PID absent (délai configurable).
- **TR 101 290 priorité 2** : transport error, CRC (CAT/PAT/PMT/NIT/EIT/BAT/SDT/TOT), répétition PCR (> 40 ms), discontinuité PCR, précision PCR (±500 ns), répétition PTS (> 700 ms).
- Paramètres : activation globale ou test par test, seuils, standard DVB (lecture de la SDT), prise en compte ou non des paquets avec `transport_error_indicator`.

### F7. Analyse PTS/DTS
- Pour un ES vidéo ou audio d'un TS, analyse en une passe (attente de PAT → PMT → SPS/PPS).
- **Graphe par access unit** sur l'axe temps PCR : arrivée de l'en-tête PES, fenêtre d'arrivée de l'AU, DTS, PTS, PCR de référence ; type et index de l'image, LED d'état.
- Ordre d'affichage : présentation ou décodage ; navigation synchronisée avec les vignettes (vidéo) ou par trame/temps (audio).
- **Règles** : DTS/PTS > arrivée du dernier octet, PTS ≥ DTS, monotonie, DTS sans PTS interdit ; warnings : PTS manquant, cadence irrégulière.
- Données : PID, type d'ES, PTS/DTS (valeur, temps, intervalle), n° PES, temps et delta d'arrivée, PCR, début/fin/taille de l'AU, index.
- Navigation erreur précédente/suivante, export texte, abandon de l'analyse en cours.
- Paramètres : règles optionnelles (warnings) activables.

### F8. Analyse dynamique des buffers (AVC, HEVC)
- **Raw byte → modèle HRD** : graphes d'occupation **CPB** et **DPB** dans le temps.
- **TS → modèle T-STD (leak method)** : graphes **TB**, **MB**, **EB**.
- Affichage : pourcentage ou bits absolus, fenêtre de 10 ms / 100 ms / 1 s, défilement synchronisé des graphes, échelle Y automatique, ligne de taille max (la courbe passe au rouge en débordement), marqueurs d'entrée/sortie d'image avec index.
- Panneaux de chiffres clés par buffer (taille, débit…).
- **Messages** catégorisés avec **solution proposée** : paramètres manquants, paramètres incorrects (cohérence des temps d'arrivée, retrait CPB, cadence irrégulière, débits CPB/DPB max, taille DPB/CPB max, nombre de slices, octets par AU), erreurs de buffer (overflow/underflow CPB, overflow DPB).
- Export texte.

### F9. Analyse audio
- **Forme d'onde** par canal (mono → 5.1 : L, C, R, Ls, Rs, LFE) ; zoom temporel (boutons + glisser), échelle verticale automatique.
- **Navigateur de trames** (par index ou par temps), surlignage de la trame courante.
- **Lecture** et **sauvegarde** des canaux sélectionnés, sur une plage : WAV PCM ou format compressé d'origine.
- **Timestamps** audio (vert) et vidéo (rouge) superposés ; **mesure d'intervalle** en ms par glisser.
- **Synchronisation A/V** (mode Lock, « aller à l'image vidéo »).
- **Données** :
  - AC-3 : version, fréquence, type de service, mode de codage, bitrate, LFE, copyright, CRC, mix levels, mode surround, timecode, dialnorm, compression, DRC… ;
  - E-AC-3 : en plus, substreams, downmix Lt/Rt et Lo/Ro, facteurs d'échelle, mixage, Dolby modes, type de convertisseur A/N… ;
  - MPEG Audio : version, couche, bitrate, fréquence, mode, CRC… ;
  - AAC : en-tête complet ADTS/LOAS, détection de SBR/PS (HE-AAC v1/v2).
- **Conformité** : valeurs réservées/interdites dans les en-têtes, vérification des CRC, pertes et reprises de synchronisation, trames non décodables (remplacées par du silence ; abandon après 20 trames consécutives).
- Paramètre : alarme après N trames audio identiques consécutives.

### F10. Loudness audio
- Mesure selon **ITU-R BS.1770-3**, avec cibles **EBU R128**, **ATSC A/85**, **ARIB TR-B32** ou valeurs personnalisées.
- Plage analysée : tout le flux, plage de trames audio ou **plage d'images vidéo** (pour isoler un programme).
- **Graphes** : loudness momentary, short-term, integrated, true peak ; échelles EBU +9 / +18 ou absolue LUFS.
- **Valeurs** : Program Loudness, Loudness Range (LRA), Max True Peak avec statut ; min/max et nombre de dépassements momentary/short-term ; valeurs au point sélectionné.
- **Dialnorm** (AC-3/E-AC-3) : cohérence avec la Program Loudness ou avec une valeur cible.
- Synchronisation avec la vidéo (Lock).
- Paramètres : seuils par mesure, désactivation du true peak, désactivation des graphes.

### F11. Qualité d'image (H.264 uniquement dans le CMA)
- Analyse à la demande sur une plage d'index de présentation (*From/To*), avec barre de progression et abandon possible.
- **Métriques par image (graphes)** : blockiness, blurriness, noise, jerkiness, saturation, sur l'axe des images, avec seuil (rouge au-dessus).
- **Détections (rapport uniquement)** : séquences noires, séquences figées.
- LED par graphe (dépassement), tooltip min/max/nombre au-dessus du seuil.
- Navigation : image, page de 61 images, dépassement précédent/suivant, raccourcis +/−, Lock vers les vignettes.
- Vue détaillée : PID, index, valeurs par métrique, appartenance à une séquence noire ou figée.
- Paramètres : métriques actives, seuils, longueur minimale des séquences noires/figées, fenêtre de la jerkiness, bornes de saturation Y/C (16–235/240 ou 0–255), seuil de noir.
- Export de tous les résultats.

**Extensions retenues (D4)**
- Métriques sans référence étendues à **tous les codecs décodés**, et plus seulement à H.264.
- **Métriques avec référence** : PSNR (Y/U/V), SSIM / MS-SSIM, **VMAF**, calculées en comparant un fichier encodé à sa source :
  - sélection des deux fichiers et des deux flux, alignement temporel automatique (détection du décalage) ou manuel, mise à l'échelle si les résolutions diffèrent ;
  - graphe par image et par métrique, valeurs agrégées (moyenne, min, percentiles), seuils et navigation vers les images les plus dégradées ;
  - **vue de comparaison** : côte à côte, rideau (split) ou carte de différence, synchronisée avec les vignettes.

### F12. Sous-titres et données associées
| Format | Fonctions |
|---|---|
| **CEA-608 / CEA-708 / SCTE-20** | Détection automatique, jusqu'à 2 formats affichés en parallèle ; buffers front/back, styles pop-on/paint-on/roll-up, attributs (couleurs, italique, etc.) ; services et fenêtres 708 ; mode « texte seul » ou avec commandes ; XDS ; incrustation sur la vidéo ; export par service. Conformité : parité, doublement des codes de contrôle, 32 colonnes, ordre des commandes par style, XDS, CEB-10A, `cc_type` selon la structure d'image. |
| **DVB Subtitles** | Arbre page → régions → objets, état (acquisition / normal / changement de mode), timeout, CLUT, aperçu bitmap des objets, PTS du segment ; bordures fenêtre/région/objet ; simulation de la capacité couleur du décodeur. Conformité dédiée. |
| **DVB / EBU Teletext** | Service (page, langue, type), page (jeu de caractères, sous-code), bits de contrôle, paquets d'amélioration, simulation de l'affichage ou syntaxe détaillée, grille de caractères. Conformité dédiée. |
| **VPS / WSS** | Présence par image, image précédente/suivante, PTS. |
| **DVB-TTML** | Service (but, langue, description), cues (début/fin → index et PTS), texte ; bordures de région. |
| **WebVTT** (HLS) / **SMPTE-TT** (DASH) | Service (nom, langue, URI, encodage), cue courante (index et PTS de début/fin), texte ou identifiant d'image. |

Commun à tous : indication du premier point d'accès contenant des données, navigation depuis les vignettes, vue syntaxe octet par octet.

### F13. SCTE-35 / DPI (insertion publicitaire)
- **Onglet Avails** : timeline globale des coupures, histogramme (position et durée), détail d'une coupure (trois timelines : splice_out / avail / splice_in), splice immediate en pointillés, commandes marquées S/E/!, zoom et défilement.
- **Onglet Commands** : une timeline par type (insert/schedule out et in, time signal par type de segmentation, splice null, private, bandwidth reservation), navigation par type.
- **Données** : nombre d'avails, cue_stream_type, mode program/component, event IDs (et interprétation SCTE-67), PTS et type des images Out/In, durée en images et en secondes, temps de réception (PCR), splice time, pts_adjustment, durée, arm time.
- Positionnement des vignettes sur la première ou la dernière image d'une avail, vignettes étiquetées.
- **Analyses configurables** : arm time min/max, interdiction des immediate, type d'image Out/In, nombre minimal de splice out, intervalle de heartbeat, règle de chiffrement, durée de coupure par défaut, appariement Out/In par event ID.
- Export (résumé, détail, commandes) et vue syntaxe.

### F14. OTT : HLS et MPEG-DASH
- Parsing de la Master/Media playlist (HLS) ou du MPD (DASH), détection et analyse des segments.
- **Vue Analysis** : LED d'état (master, media, segments), **une timeline par média/representation** avec le segment courant, statut de conformité, résumé (conteneur, type, résolution, langue, bande passante, groupe), vignette de la première image I du segment.
- Navigation par index de segment ou par temps ; contrôle visuel de l'alignement entre profils.
- Actions : **ouvrir un segment ou un média dans une nouvelle instance** (segments concaténés traités comme un seul TS), enregistrer un média en fichier unique.
- **Vue Syntax** : contenu des playlists ou du MPD, lignes en erreur en rouge.
- **Données** : master (version, nombre de médias), media (URI, version, type, nombre de segments, durées, chiffrement AES-128/SAMPLE-AES, BANDWIDTH, CODECS, RESOLUTION, GROUP-ID, NAME, DEFAULT, AUTOSELECT), segment (numéro, début, durée, URI, byterange) ; DASH : profil, static/dynamic, adaptation sets, durée, codecs, fréquence d'images, scan, PAR/SAR.
- **Conformité** :
  - master : syntaxe, résolution des URI, statut agrégé ;
  - media : syntaxe, timing (EXTINF ≤ TARGETDURATION), résolution des URI ;
  - segment : structure TS (programme unique, PAT/PMT en tête), RAP (présence d'une image I, I en première position), continuité (timestamps, compteurs de continuité) ;
  - analyses additionnelles : alignement temporel entre médias, durée de segment min/max/constante, cohérence entre durée déclarée et durée réelle.

### F15. Édition : découpe et extraction
- Sélection d'une plage par clic droit sur les vignettes, recalée sur l'image I/RAP précédente.
- **TS trimming** : TS/TTS → TS propre (ajout de PAT/PMT et des paramètres requis encapsulés, compteurs de continuité corrigés).
- **ES extraction** : TS, TTS, MP4, MOV, FLV, 3GP, ASF, MXF ou raw → ES brut (H.264, HEVC, VC-1, MPEG-2) avec insertion des paramètres requis (SPS/PPS, VPS, SH/EH, SH/SE/GH).
- Sélection directe de séquences complètes en mode RAP ou I.
- Export audio : voir F9.

### F16. Lecteur TS intégré
- Lecture A/V d'un programme TS (vidéo MPEG-1/2, H.264 4:2:0 8 bits ; audio MPEG-1/2, AC-3/E-AC-3, AAC).
- Choix de la piste audio dans l'arbre ; pause, reprise, stop, volume, plein écran.
- Pas d'analyse ni d'overlay pendant la lecture.

### F17. Rapport, journaux et export
- **Report view** : messages info/warning/erreur, avec colonnes Type, Famille (TS, NAL, VCL, DBA, Syntax, Audio, CC, DPI, HLS, MXF, MP4, Timed Text…), Sous-famille (TR 101 290-1/-2, Parsing, Buffer, Profiles, Levels, DVB guidelines, PTS/DTS…), index de décodage, position (octet de sync), description.
- Mise en sourdine d'un type d'erreur après N occurrences.
- Fichiers journaux persistants : dossier configurable, écrasement ou versionnement à la réouverture, ouverture depuis l'application.
- **Export du contenu de la vue courante** en texte ou CSV.

### F18. Paramètres
Panneau unique, ouvert sur la section de l'analyse courante, avec une case « tout cocher/décocher » par section :
affichage (cadence de lecture, mode de l'overlay QP, extension des champs, rapport d'aspect, hexadécimal) · journaux · TS / TR 101 290 · PTS/DTS · vidéo (coefficients, GOP par saut, tests AVC) · qualité d'image · audio · loudness · closed captions · sous-titres/teletext/TTML · SCTE-35 · HLS.

---

## 5. Matrice codec × fonctions (référence CMA)

| Fonction | HEVC | AVC | MPEG-2 | VC-1 | Audio |
|---|:-:|:-:|:-:|:-:|:-:|
| Décodage + overlays | ✓ | ✓ | ✓ | ✓ | forme d'onde |
| Syntaxe + conformité | ✓ | ✓ | ✓ | ✓ | en-têtes |
| Coefficients / étapes du décodage | | ✓ | ✓ | | |
| Buffers HRD / T-STD | ✓ | ✓ | | | |
| PTS/DTS | ✓ | ✓ | ✓ | ✓ | ✓ |
| Qualité d'image | | ✓ | | | |
| Synchronisation A/V | ✓ | ✓ | ✓ | | ✓ |
| Lecteur TS | | ✓ | ✓ | | ✓ |
| Closed Captions | 608/708 | 608/708 | 608/708/SCTE-20 | 608/708 | |
| Loudness | | | | | ✓ |

---

## 6. Découpage en lots (broadcast d'abord)

Principe : livrer d'abord le cœur d'un analyseur broadcast autour du **MPEG-2 TS**, puis les analyses de conformité temporelle, puis les services de données broadcast. L'OTT et les codecs récents viennent ensuite.

| Lot | Contenu | Domaines |
|---|---|---|
| **MVP : cœur TS** | Ouverture de TS (SPTS/MPTS) et d'ES bruts H.264/HEVC/MPEG-2 ; arbre PAT/PMT/SDT ; bitrates par PID ; **TR 101 290 P1/P2** ; vignettes et navigation ; image décodée (décodeur standard), zoom, composantes Y/U/V ; grille de slices et de tiles issue du parsing ; Data view Seq/Pic (séquence, image, transport) ; arbre syntaxique ; Report view ; export texte/CSV ; indexation progressive des fichiers de plusieurs Go | F1, F2, F3 (partiel), F4 (partiel), F5 (syntaxe), F6, F17 |
| **V1 : timing et audio** | PTS/DTS ; buffers T-STD et HRD (calculés depuis la syntaxe, sans décodeur instrumenté) ; conformité (profils, niveaux, DVB guidelines, ordre de sortie) ; audio (forme d'onde, données, lecture, export) ; **loudness** (EBU R128 / ATSC A/85) ; synchronisation A/V ; métadonnées HDR (HEVC) ; découpe TS et extraction ES ; panneau de paramètres | F5 (complet), F7, F8, F9, F10, F15, F18 |
| **V2 : données broadcast et qualité** | CEA-608/708, SCTE-20, DVB Subtitles, **Teletext**, VPS/WSS, DVB-TTML ; **SCTE-35** ; lecteur TS ; qualité d'image sans référence et **avec référence (PSNR, SSIM, VMAF)**, qui ne nécessitent que l'image décodée | F11, F12, F13, F16 |
| **V3 : décodeurs instrumentés** | H.264, HEVC et MPEG-2 instrumentés : overlays (type de bloc, QP, taille, frame/field, vecteurs L0/L1, partitions MB/CB/PB/TB), focus et verrouillage d'un bloc, Data view Slice/MB et Coding Details, coefficients et étapes du décodage (avant filtre, prédit, résidu) ; **modèle de blocs générique** pour les codecs suivants | F3, F4 (complets) |
| **V4 : OTT et codecs récents** | MP4/fMP4, MOV, MKV/WebM, Opus ; **HLS, MPEG-DASH** ; WebVTT, SMPTE-TT/IMSC ; **AV1, VP9, VVC** : syntaxe et conformité, puis instrumentation sur le socle de la V3 | F14, extension de F1 à F5 |
| **V5 : audio nouvelle génération** | **MPEG-H 3D Audio** et **AC-4** : parsing, métadonnées, conformité, puis décodage et loudness selon la faisabilité (voir §7) | F9, F10 (extension) |
| Plus tard | **AV2** (dès que la spécification est stable) ; mode ligne de commande / batch ; VC-1, ASF, FLV, MXF | (D1, D3, D5) |

L'ordre de V3 à V5 reste ajustable. La V3 peut passer avant la V2 si l'analyse fine de l'encodage devient prioritaire par rapport aux services de données broadcast.

Hors périmètre : Media Extractor (pcap/RTP), Broadband Recorder, gestion de licences, spécificités Windows.

### Exigences non fonctionnelles issues du cadrage
- **Extensibilité** : conteneurs, codecs et analyses sous forme de modules enregistrés, pour pouvoir ajouter VC-1, MXF, etc. sans toucher au cœur (D3).
- **Séparation moteur / IHM** : moteur d'analyse sans dépendance graphique, pour préparer le futur mode ligne de commande (D5).
- **Gros fichiers** : lecture en flux, index persistant (positions des paquets et des images), décodage à la demande autour de la position courante, analyses en tâches de fond annulables avec progression (D6).
- **Plateforme** : Linux desktop, à x86_64 au minimum.

---

## 7. Points restant ouverts

Les questions de cadrage sont tranchées (voir §0). Points à garder en vue :

1. **Décodeurs instrumentés (V3)** : il faut les obtenir codec par codec, en forkant et instrumentant un décodeur existant ou en écrivant un décodeur dédié. La stack technique doit dès maintenant prévoir l'interface qu'ils implémenteront, pour que le MVP n'ait pas à être refondu.
2. **Audio nouvelle génération (V5)** : impact estimé (à vérifier au moment de la V5) :
   | Codec | Parsing / métadonnées / conformité | Décodage (forme d'onde, écoute, loudness) |
   |---|---|---|
   | **Opus** | faible | faible : libopus, libre |
   | **MPEG-H 3D Audio** | moyen : spécification ISO payante | moyen : Fraunhofer publie un décodeur open source (licence à examiner, brevets) ; le rendu objets/scènes ajoute de la complexité |
   | **AC-4** | moyen : spécification ETSI TS 103 190 publique | **élevé** : à ma connaissance, aucun décodeur libre abouti ; il faudrait sans doute une licence Dolby. Il est possible qu'on se limite à l'analyse syntaxique et aux métadonnées (dialnorm, loudness déclarée). |
3. **AV2** : prévoir l'emplacement du module dès l'architecture. AOMedia publie un code de référence de recherche (AVM). L'intégration effective attendra une spécification figée.
