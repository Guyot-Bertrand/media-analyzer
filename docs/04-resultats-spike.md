# Résultats du spike de validation de la stack

> Plan : `docs/superpowers/plans/2026-10-30-spike-stack.md`. Code jetable sur la branche `spike/stack-validation` (non fusionnée).
> Date : 2026-10-30.
> Machine de référence : Intel NUC, i7-10710U (6 cœurs / 12 threads), GPU Intel Comet Lake UHD (pilote iHD 24.1), 31 Go de RAM, Ubuntu 24.04.5, FFmpeg 6.1, Rust 1.99, Tauri 2.12.
> Fichier de mesure : TS HEVC Main 10, 3840×2160, 50 i/s, GOP de 1 s, 4,0 Go (61 000 images). Le numéro de chaque image est incrusté sous forme de code binaire, pour vérifier automatiquement que l'image affichée est bien celle demandée.

## 1. Verdict

**Go, avec la stack prévue (Tauri + WebGL2 + FFmpeg via rsmpeg), sans recourir aux plans B du §8.**

Le dessin WebGL n'est pas un goulot : 10 à 16 ms pour une image UHD 10 bits. WebKitGTK est stable (20/20 exécutions bout en bout). Le moteur tient tous les seuils en VA-API.

**Une condition pour le MVP** : ne pas transporter l'image pleine résolution à chaque affichage. Envoyée telle quelle (25 Mo), l'image coûte environ 125 ms de transport vers l'interface. L'interface doit donc demander l'image à la taille affichée, ou la seule région visible à fort zoom (§4).

## 2. Mesures

### 2.1 Moteur seul (binaire de mesure, sans interface)

| Mesure | Logiciel | VA-API | Seuil |
|---|---|---|---|
| Première image (index partiel de 64 Mo) | 312 ms ✓ | 147 ms ✓ | ≤ 2 s |
| Saut arbitraire, p50 / p95 / max | 399 / **625** / 670 ms ✗ | 196 / **321** / 355 ms ✓ | ≤ 500 ms |
| Image suivante, p50 / p95 | 17 / 22 ms ✓ | 46 / 54 ms ✓ | ≤ 100 ms |
| Image précédente, p50 / p95 / max | 0 / 0 / 813 ms ✓ | 0 / 0 / 1 357 ms ✓ | ≤ 100 ms |
| Mémoire résidente maximale | 1,49 Go ✓ | 0,96 Go ✓ | ≤ 2 Go |
| Index complet du fichier de 4 Go (en fond) | 2,8 s | 2,8 s | — |

Le max de l'image précédente correspond au re-décodage arrière, une fois toutes les 24 images.

Toutes ces mesures sont faites avec le **cache disque chaud** : le vidage du cache (`drop_caches`) demande les droits administrateur et n'a pas été fait. Le décodage domine le temps de saut, mais l'ouverture et l'indexation seront plus lentes à froid ou sur un partage réseau.

En logiciel, le seuil de saut n'est pas tenu. C'est une limite physique : jusqu'à 50 images à décoder depuis le RAP, plus la latence du pipeline multi-thread (12 images), à environ 100 i/s. Une machine sans VA-API restera donc autour de 0,6 s par saut sur de l'UHD HEVC 10 bits avec un GOP de 1 s.

### 2.2 Interface (Tauri, test WebdriverIO pilotant l'application réelle)

Image suivante, mesurée sur l'écran réel (session Wayland), VA-API, build release :

| Variante de transport | Total p50 / p95 | dont fetch `frame://` | dont moteur | dont dessin WebGL |
|---|---|---|---|---|
| 10 bits, pleine résolution (25 Mo) | 209 / 247 ms ✗ | 193 ms | ~75 ms | 16 ms |
| 8 bits, pleine résolution (12,5 Mo) | 151 / 188 ms ✗ | 141 ms | — | 10 ms |
| 8 bits, ¼ de résolution (0,8 Mo) | **95 / 106 ms** ≈ | 93 ms | 89 ms | 1 ms |
| 10 bits, ¼ de résolution | 101 / 121 ms ≈ | 98 ms | 91 ms | 2 ms |

Autres résultats :
- Ouverture jusqu'à la première image affichée : **743 à 779 ms** ✓.
- Stabilité : **20/20** exécutions consécutives réussies sous Xvfb ✓. Le test vérifie, en lisant les pixels du canvas, que l'image affichée porte le bon numéro après un saut et après 20 images suivantes.
- Le transport via le protocole personnalisé de WebKitGTK coûte environ **5 ms par Mo**. Une fois l'image réduite, le temps restant est celui du moteur, dominé en VA-API par le transfert GPU → RAM de l'image **pleine résolution** (environ 40 ms) avant réduction.
- Les sauts arbitraires n'ont pas été mesurés côté interface. Estimation : moteur (p95 321 ms) + transport. On reste sous 500 ms si l'image est envoyée à la taille affichée, mais pas en pleine résolution.

## 3. Choix et constats

### 3.1 Liaison FFmpeg : **rsmpeg** (détails dans `spike/NOTES.md`)
- rsmpeg fournit l'accélération matérielle (`AVHWDeviceContext`, transfert GPU) et l'accès aux side data (vecteurs de mouvement, QP). Ces deux points sont indispensables : VA-API dès le MVP, side data en V3.
- ffmpeg-next n'expose ni l'un ni l'autre : il faudrait écrire notre propre FFI pour précisément ces deux besoins.
- Contrepartie : un peu plus de code `unsafe` (création de paquet, réglage des threads, accès aux planes), à confiner dans la crate de liaison.
- Les deux crates ne peuvent pas cohabiter dans un même graphe Cargo (`links = "ffmpeg"`).

### 3.2 Défauts réels trouvés et corrigés (tous couverts par un test qui échouait d'abord)
1. **Latence du pipeline multi-thread** : FFmpeg retarde la sortie d'environ autant d'images que de threads. Une marge fixe rendait des images « non décodables ». La marge est maintenant de réordonnancement + latence, avec vidage du décodeur en dernier recours.
2. **Navigation séquentielle** : chaque image suivante repartait du RAP (405 AU envoyées pour 10 images). On poursuit désormais le décodage en cours, y compris d'un GOP au suivant.
3. **Conversion de toutes les images intermédiaires** : chaque transfert GPU coûte environ 40 ms, et chaque copie 25 Mo. On ne convertit plus que les images voisines de la cible, avec une fenêtre qui dépend du sens de navigation.
4. **VA-API : « Failed to sync surface » intermittent** quand des images jetées n'étaient pas synchronisées. Corrigé par un `vaSyncSurface` sur chaque image sortie et un seul thread de décodage en VA-API (stable sur 5 et 3 exécutions répétées).
5. **Mémoire de l'indexation** : +3,3 Go, car le début de chaque PES était conservé jusqu'à la fin du balayage. Chaque PES est maintenant analysé dès qu'il est complet ; l'indexation est aussi plus rapide (4,2 s → 2,8 s).
6. **Sécurité** : taille d'AU non bornée sur un flux malformé, avec un risque de débordement à la création du paquet. Les AU sont maintenant bornées à 32 Mio.
7. **Saut au-delà de l'index partiel** pendant l'indexation en fond : la requête échouait. Elle attend maintenant la fin de l'indexation.

### 3.3 Particularités de WebKitGTK à retenir
- Le nom du GPU est masqué (`WEBGL_debug_renderer_info` renvoie « Apple GPU ») : impossible de vérifier ainsi que WebGL est accéléré. Le dessin est de toute façon négligeable.
- Une réponse non 200 du protocole personnalisé remonte en `TypeError: Load failed` côté `fetch` : il faut transmettre les erreurs autrement (code 200 avec une enveloppe d'erreur, ou une commande Tauri).
- Une capture X (`xwd`) d'une fenêtre WebKitGTK sous Xvfb est blanche : les captures de recette doivent passer par WebDriver (`saveScreenshot`).

## 4. Enseignements pour le plan du MVP

1. **Transport d'image adapté à l'affichage** (condition du *go*) :
   - en zoom « ajusté », envoyer l'image à la taille affichée ;
   - à fort zoom, envoyer la **région visible** seulement ;
   - la pleine résolution n'est envoyée que si elle est visible ;
   - faire la réduction **sur le GPU** en VA-API (VPP) pour éviter le transfert pleine résolution.
2. **Préchargement** des images voisines en tâche de fond, pendant que l'utilisateur regarde l'image courante.
3. **VA-API par défaut**, avec repli logiciel automatique. Ce repli est plus lent en saut et doit être signalé dans l'interface.
4. Reprendre tels quels les mécanismes validés :
   - index partiel puis index complet en fond ;
   - fenêtre de conversion selon le sens de navigation ;
   - poursuite du décodage à travers les GOP ;
   - synchronisation explicite des surfaces VA-API ;
   - indexation à mémoire bornée.
5. **Lever l'hypothèse « 1 PES = 1 AU »** du spike : le vrai démultiplexeur doit découper les AU dans l'ES.
6. **Index persistant** (prévu dans la stack) : il évite les 2,8 s d'indexation à chaque réouverture d'un fichier de 4 Go.
7. **Mesures restant à faire** : cache disque froid, fichier sur partage réseau (NFS/SMB), sauts mesurés côté interface.
8. **Tests bout en bout** : la combinaison WebdriverIO + tauri-driver + Xvfb est fiable (20/20). Les tests qui lisent les pixels du canvas pour vérifier l'image affichée sont précieux et à généraliser.
