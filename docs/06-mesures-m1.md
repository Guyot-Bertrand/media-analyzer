# Mesures de M1 : transport d'image

> Plan : `docs/superpowers/plans/2026-10-31-mvp-m1-transport-image.md` (Tâches 9 à 12). Branche `mvp/m1-transport-image`.
> Date : 2026-10-31. Point de passage n° 2 de la feuille de route (`05-feuille-de-route-mvp.md`).
> Machine de référence : Intel NUC, i7-10710U, GPU Intel Comet Lake UHD, pilote iHD 24.1 **non libre** (`intel-media-va-driver-non-free`, VPP présente), Ubuntu 24.04, FFmpeg 6.1.
> Fichier de mesure : `uhd_big`, TS HEVC Main 10, 3840×2160, 50 i/s, GOP fermé de 50 images (1 s), 4,06 Go, **62 000 images** (le spike en avait 61 000). Le numéro incrusté de chaque image est relu et comparé à l'attendu ; le banc s'arrête au premier écart. Aucun écart n'a été rencontré.
> Toutes les mesures sont faites **cache disque chaud**.

## 1. Verdict

**Condition 1 du *go* tenue ; condition 2 tenue en régime établi à intervalle 200 ms seulement. La décision revient au porteur (§5).**

L'image suivante côté interface est à 19–33 ms au p95 au zoom Ajusté et à 34–44 ms en 1:1 (seuil 100 ms ; spike : 106 ms). La réduction sur GPU (VPP) supprime tout transfert pleine résolution au zoom Ajusté. Le saut reste sous 500 ms au p95 (258–273 ms côté interface). Pour l'image précédente, le p95 est sous 100 ms partout. Le maximum passe de 1 357 ms (spike) à 95–114 ms avec un intervalle de 200 ms entre les pas. Le seuil « max < 250 ms » n'est **pas** tenu dans deux cas : à intervalle nul, au franchissement d'un GOP qui n'a pas été préchargé (270 et 278 ms dans 2 exécutions Xvfb sur 3, 336–426 ms au banc moteur), et au premier pas arrière juste après un saut sur une IDR (219–382 ms côté interface), publié à part (R18). Cause estimée (§4.1) : reculer d'une image depuis une IDR oblige à décoder le GOP précédent entier, 50 images 4K Main 10. Au coût mesuré du décodage seul (5,2 à 7,7 ms par image), cela donne environ 250 à 385 ms sur ce GPU, conversion non comptée. Le coût d'un GOP n'a pas été mesuré directement. Le seuil n'est tenu qu'avec la VPP, donc avec le pilote Intel non libre. Le pilote libre installé par défaut sur Ubuntu n'a pas de VPP : il impose un chemin dégradé (§4.2), ce qui concerne les portables des collègues.

## 2. Mesures

### 2.1 Moteur seul (`engine_bench`, sans interface)

`cargo run --release -p ma-ffmpeg --example engine_bench`, 8 exécutions (mode × vue × intervalle). Protocole : 100 sauts aléatoires (graine fixe), puis 200 pas avant depuis total/4, puis 200 pas arrière depuis total/2 (31 000, une IDR : pire cas voulu). Les percentiles des pas portent sur les 199 pas qui suivent le premier. Le premier pas après le placement (« 1er ») est publié à part. Durées en ms (`Instant`, résolution de l'ordre de la µs). La vue fait 1920×1080 dans les deux cas : image réduite en Ajusté, région en haut à gauche en 1:1.

| Mode réel | Vue | Intervalle | Saut p50 / p95 / max | Avant p50 / p95 / max (1er) | Arrière p50 / p95 / max (1er) | RSS max |
|---|---|---|---|---|---|---|
| VA-API / VPP | Ajusté | 0 | 142,6 / **241,7** / 260,5 | 9,3 / **10,2** / 12,1 (22,3) | 0,0 / 15,3 / **335,7** (362,1) | 296 Mo |
| VA-API / VPP | Ajusté | 200 | 137,8 / **239,3** / 252,9 | 0,1 / **0,2** / 5,9 (24,0) | 0,1 / 4,2 / **56,5** (224,0) | 299 Mo |
| VA-API / VPP | 1:1 | 0 | 144,8 / 247,1 / 266,4 | 11,7 / **13,8** / 17,2 (28,0) | 0,0 / 17,8 / **425,9** (457,9) | 448 Mo |
| VA-API / VPP | 1:1 | 200 | 144,3 / 243,3 / 256,9 | 0,1 / **0,2** / 7,5 (34,0) | 0,1 / 11,3 / **84,3** (300,8) | 445 Mo |
| logiciel | Ajusté | 0 | 314,1 / 473,5 / 528,1 | 9,5 / 12,4 / 15,1 (136,3) | 0,0 / 25,0 / 519,4 (550,5) | 881 Mo |
| logiciel | Ajusté | 200 | 330,5 / 503,8 / 602,8 | 0,1 / 0,1 / 31,7 (150,6) | 0,1 / 3,8 / 50,9 (429,0) | 883 Mo |
| logiciel | 1:1 | 0 | 325,0 / 503,0 / 534,7 | 7,4 / 21,2 / 35,0 (126,3) | 0,0 / 48,1 / 493,9 (536,0) | 1 019 Mo |
| logiciel | 1:1 | 200 | 325,5 / 490,9 / 552,8 | 0,1 / 0,1 / 54,4 (136,5) | 0,1 / 9,0 / 25,0 (374,9) | 1 019 Mo |

Dans les 8 exécutions : aucun repli logiciel imprévu, **0 transfert pleine résolution**, et 8 images suivantes et 8 précédentes prêtes 1 s après la dernière demande.

Comparaison au spike (moteur, VA-API) :

| Mesure | Spike | M1 (VA-API / VPP, Ajusté) | Seuil |
|---|---|---|---|
| Saut p95 | 321 ms | 239–247 ms ✓ | < 500 ms |
| Image suivante p50 / p95 | 46 / 54 ms | 9,3 / 10,2 ms (intervalle 0) ✓ | < 100 ms |
| Image précédente max | 1 357 ms | 56–84 ms (intervalle 200, régime établi) ✓ ; 336–426 ms (intervalle 0) ✗ ; 1er pas 224–301 ms (intervalle 200) ✗ | < 250 ms |
| Mémoire résidente max | 0,96 Go | 0,30 Go (Ajusté), 0,45 Go (1:1) ✓ (processus du banc moteur, hors WebKit ; dans l'application réelle, le processus moteur pèse 490–651 Mo et l'application entière 0,77–0,92 Go, §2.4) | < 2 Go |
| Saut p95 en logiciel | 625 ms | 473–504 ms (pas de seuil) | — |

### 2.2 Interface (banc `tools/transport-bench`, Tauri + WebGL2, piloté par WebdriverIO)

Application de banc en build release : `frame://` → `FrameService` → texture WebGL2. Elle reprend le protocole et la graine du banc moteur. Une seule demande est en vol à la fois (R8). La durée va de la demande à `gl.finish` après le dessin. Elle est mesurée par `performance.now()`, à **1 ms de résolution** dans WebKitGTK. Chaque image affichée est vérifiée par son numéro, relu sur les pixels du canvas : 16 bits en Ajusté, 8 bits de poids faible en 1:1 UHD. Canvas de 1920×1080. Les plages donnent le min et le max sur 3 exécutions par affichage (JSON `target/bench/ui-*-uhd_big-*.json`).

**VA-API / VPP**, sur l'écran réel (Wayland, 2560×1440, canvas agrandi par CSS) puis sous Xvfb (1920×1080) :

| Affichage | Série | Vue | Intervalle | p50 | p95 | max | 1er pas |
|---|---|---|---|---|---|---|---|
| Wayland | saut (100) | Ajusté | 0 | 156–159 | **263–266** | 277–282 | – |
| Wayland | saut | Ajusté | 200 | 157–161 | **258–268** | 274–288 | – |
| Wayland | avant (199) | Ajusté | 0 | 15–16 | **20–21** | 29–35 | 47–50 |
| Wayland | avant | Ajusté | 200 | 15–16 | **19** | 23–29 | 46–49 |
| Wayland | avant | 1:1 | 0 | 27–29 | **35–37** | 38–46 | 57–64 |
| Wayland | avant | 1:1 | 200 | 27–28 | **34–36** | 41–53 | 68–82 |
| Wayland | arrière (199) | Ajusté | 0 | 14–18 | 29–32 | **172–220** | 372–382 |
| Wayland | arrière | Ajusté | 200 | 14–15 | 22–24 | **95–114** | 219–220 |
| Xvfb | saut | Ajusté | 0 | 162–169 | **270–273** | 283–291 | – |
| Xvfb | saut | Ajusté | 200 | 158–162 | **258–266** | 272–287 | – |
| Xvfb | avant | Ajusté | 0 | 19–20 | **30–33** | 42–59 | 48–55 |
| Xvfb | avant | Ajusté | 200 | 16–17 | **19–20** | 21–26 | 44–49 |
| Xvfb | avant | 1:1 | 0 | 30–31 | **40–44** | 51–67 | 62–72 |
| Xvfb | avant | 1:1 | 200 | 27–29 | **34–36** | 39–42 | 65–72 |
| Xvfb | arrière | Ajusté | 0 | 16–18 | 46–54 | **86, 270, 278** | 377–382 |
| Xvfb | arrière | Ajusté | 200 | 14–15 | 23–24 | **98–102** | 219–234 |

Décomposition au p50 :
- un saut se compose du moteur (environ 138–148 ms) et du transport et du dessin (15–20 ms) ;
- un pas avant en Ajusté se compose du fetch (14–17 ms, dont 0,06 ms de moteur : l'image est servie par le préchargement) et du dessin (2 ms) ;
- en 1:1, le fetch coûte 24–28 ms pour 6,2 Mo (environ 4 ms par Mo par le protocole) et le dessin 3 ms.

Ouverture du fichier : 713–752 ms.

**Logiciel** (`--mode sw`, Xvfb, 3 exécutions) :

| Série | Vue | Intervalle | p50 | p95 | max | 1er pas |
|---|---|---|---|---|---|---|
| saut (100) | Ajusté | 0 | 346–352 | **504–518** | 552–574 | – |
| saut | Ajusté | 200 | 348–354 | **488–495** | 515–594 | – |
| avant (199) | Ajusté | 0 | 20–21 | 31–32 | 71–119 | 144–230 |
| avant | Ajusté | 200 | 16–17 | 20 | 64–94 | 183–195 |
| avant | 1:1 | 0 | 32–33 | 38–45 | 124–175 | 196–226 |
| avant | 1:1 | 200 | 27–30 | 34–41 | 116–128 | 216–238 |
| arrière (199) | Ajusté | 0 | 17–18 | 74–83 | 115–135 | 558–574 |
| arrière | Ajusté | 200 | 14–15 | 42–47 | 84–106 | 363–435 |

Le p95 des sauts en logiciel côté interface, **488–518 ms**, est publié comme le demande PERF.SEEK.01, qui ne fixe pas de seuil en logiciel. Il est proche du banc moteur (473–504 ms) et inférieur au spike (625 ms au moteur). L'ouverture prend 846–856 ms.

Les quatre formats de pixels du shader ont été exécutés et vérifiés par leur numéro :
- Planar8 : H.264, MPEG-2, et UHD logiciel en Ajusté ;
- Planar10 : HEVC logiciel et UHD logiciel en 1:1 ;
- NV12 : UHD VA-API en Ajusté ;
- P010 : HEVC VA-API et UHD VA-API en 1:1.

Comparaison au spike (interface) :

| Mesure | Spike | M1 (VA-API / VPP) | Seuil |
|---|---|---|---|
| Image suivante p95, Ajusté | 106 ms (¼ de résolution, réduite sur CPU) | 19–33 ms ✓ | < 100 ms |
| Image suivante p95, pleine résolution / 1:1 | 247 ms (image entière) | 34–44 ms (région 1920×1080) ✓ | < 100 ms |
| Saut p95 | non mesuré (estimation 321 ms + transport) | 258–273 ms ✓ | < 500 ms |
| Image précédente p95 | non mesurée | 22–54 ms ✓ | < 100 ms |
| Image précédente max | non mesurée (1 357 ms au moteur) | 95–114 ms (intervalle 200) ✓ ; 86–278 ms (intervalle 0) ✗ ; 1er pas 219–382 ms ✗ | < 250 ms |
| Ouverture → 1re image | 743–779 ms | 713–752 ms | — |

### 2.3 Effet du préchargement (intervalle de 0 ms ou de 200 ms)

L'intervalle est le temps entre la réception d'une image et la demande suivante. Il s'applique aussi après chaque placement. 0 ms correspond à un appui maintenu, 200 ms à une navigation pas à pas réaliste.

- **Avant, moteur** : le p50 passe de 9–12 ms (une image décodée et convertie à chaque pas) à 0,1 ms (servie par le cache). Côté interface, le pas reste à 15–20 ms dans les deux cas : le transport domine. Même à intervalle nul, le moteur sert l'image du cache (0,06 ms), car les 14–17 ms du transport laissent au préchargement le temps de décoder la suivante.
- **Arrière, moteur, VA-API, Ajusté** : à 200 ms, le max en régime établi tombe de 336 ms à 56 ms ; en 1:1, de 426 ms à 84 ms.
- **Arrière, interface** : à 200 ms, le max vaut 95–114 ms contre 86–278 ms à intervalle nul.
- **Disponibilité** : 1 s après la dernière demande, les 8 suivantes et les 8 précédentes sont prêtes dans les 8 exécutions du banc moteur. C'est le compteur `readiness` de PERF.PREFETCH.01, aussi vérifié par 3 tests unitaires (`ma-decode::service`).
- **Premier pas arrière après un saut sur une IDR** : à 200 ms, il coûte encore 224–301 ms au moteur et 219–234 ms côté interface. 200 ms ne suffisent pas pour décoder les 50 images du GOP précédent.

### 2.4 Transport, transferts et mémoire

| Mesure | Ajusté (1920×1080) | 1:1 (région 1920×1080) | Seuil |
|---|---|---|---|
| Octets par image (moteur) | 3 110 400 (NV12 ou Planar8, 8 bits) ✓ | 6 220 800 (P010 ou Planar10, natif) | < 4 Mo en Ajusté |
| Octets par réponse `frame://` (avec en-tête de 32 octets) | 3 110 432 | 6 220 832 | — |
| `full_res_transfers`, VA-API / VPP, uhd_big | **0** (toutes les séries, moteur et interface) ✓ | 0 | 0 en Ajusté |
| `full_res_transfers`, logiciel, uhd_big | 0 (moteur et interface, valeur globale et par série dans chaque JSON) | 0 | — |

En logiciel, le compteur reste à 0 : il ne compte que les copies GPU → RAM d'une image pleine résolution, et ce mode n'en fait aucune. Seule la fumée VA-API sur `hevc_720p_main10` affiche un compteur non nul (451 au total ; 64, 16, 28, 255 et 10 selon les séries). C'est attendu : à 720p, la vue de 1280×720 est la taille native, donc chaque conversion est un transfert de l'image entière, sans réduction.

Mémoire de l'application entière, en fin de parcours d'interface (somme des processus Tauri/moteur, WebKitWebProcess et WebKitNetworkProcess) :

| Affichage | Mode | Somme des RSS | PSS | Seuil |
|---|---|---|---|---|
| Wayland | VA-API, 3 exécutions | 766 / 872 / 892 Mo | non relevé | < 2 Go ✓ |
| Xvfb | VA-API, 3 exécutions | 832 / 860 / 919 Mo | non relevé | < 2 Go ✓ |
| Xvfb | logiciel, 3 exécutions | 1 337 / 1 447 / 1 466 Mo | **1 104 / 1 214 / 1 234 Mo** | < 2 Go ✓ |

La somme des RSS compte trois fois les bibliothèques partagées. Elle surévalue l'empreinte d'environ 230 Mo par rapport au PSS. Le processus moteur pèse 490–651 Mo en VA-API, WebKitWebProcess 206–227 Mo et le processus réseau 48 Mo.

### 2.5 Coût de la conversion sur GPU (Tâche 9)

Mesure jetable, `uhd_big`, VA-API, build release, 200 images, 2 passes. Le décodage seul coûte 5,2 à 7,7 ms par image. Le surcoût est compté au-delà de ce décodage.

| Conversion | Surcoût par image | `full_res_transfers` |
|---|---|---|
| Ajusté 1920×1080, VPP + transfert NV12 | **5,4–7,0 ms** | 0 |
| Région 1:1 1920×1080, VPP + transfert P010 | 10,1–11,4 ms | 0 |
| Image entière 1:1 (transfert complet P010) | 32,4–42,6 ms | 1 par image |
| Ajusté, **sans VPP** (projection `av_hwframe_map` + SWS_AREA) | **21,7–23,8 ms** | 1 par image (R15) |
| Région 1:1, sans VPP (projection + copie) | 11,2–16,9 ms | 1 par image (R15) |
| Ajusté, transfert complet + SWS_AREA | 34,9–45,7 ms | 1 par image |

## 3. Écarts au plan

- **Exactitude de la VPP en 1:1** : la région 1:1 obtenue par la VPP est **octet pour octet identique** au recadrage du transfert complet (`vpp_region_1to1_is_bit_exact`, 5 images, NV12 et P010). Le repli `av_hwframe_map` prévu à l'étape 3 de la Tâche 9 n'a pas été nécessaire sur la machine de référence.
- **Sondage 16×16** : la VPP refuse une sortie de moins de 16 px. Le sondage de taille 2×2 faisait basculer tout le mode `auto` en logiciel. Il est passé à 16×16. Un viewport refusé par la VPP dégrade la conversion vers la projection et le CPU, pour ce viewport seulement, sans repli de décodage (R17).
- **Corpus `uhd_big` régénéré** par le démultiplexeur `concat` : `-stream_loop` laissait des raccords sans IDR. Le fichier a 62 000 images, contre 61 000 pour le fichier du spike.
- **Premier pas publié à part** (R18) : le max de l'image précédente est jugé en régime établi. Le premier pas arrière après un saut figure à part dans les tableaux. Le porteur tranche (§5).

## 4. Limites

### 4.1 Image précédente : le seuil « max < 250 ms »

| Situation | Moteur (VA-API) | Interface (VA-API) | Seuil |
|---|---|---|---|
| Régime établi, intervalle 200 ms | 56–84 ms ✓ | 95–114 ms ✓ | < 250 ms |
| Régime établi, intervalle 0 (franchissement d'un GOP non préchargé) | 336–426 ms ✗ | Wayland 172–220 ms ✓ ; Xvfb 86 ms ✓, puis **270 et 278 ms** ✗ | < 250 ms |
| 1er pas arrière après un saut sur une IDR, intervalle 200 ms | 224–301 ms ✗ | 219–234 ms ✗ | (publié à part, R18) |
| 1er pas arrière après un saut sur une IDR, intervalle 0 | 362–458 ms ✗ | 372–382 ms ✗ | (publié à part, R18) |

**Pourquoi.** Le fichier de mesure a des GOP fermés de 50 images. Pour l'image qui précède une IDR, il faut repartir de l'IDR précédente et décoder 49 images 4K Main 10 avant de pouvoir afficher la cible. Sur ce GPU, le décodage seul coûte 5,2 à 7,7 ms par image, soit environ 250 à 385 ms pour un GOP, conversion non comptée. Ce coût correspond aux premiers pas mesurés. À 200 ms d'intervalle, le préchargement arrière a le temps de couvrir un franchissement de GOP en régime établi, mais pas le recul d'un GOP entier juste après un saut. À intervalle nul, il n'a pas le temps de préparer le GOP précédent avant que l'utilisateur l'atteigne.

### 4.2 PERF.XFER.01 et pilote VA-API

PERF.XFER.01 (aucun transfert pleine résolution au zoom Ajusté) n'est tenue **qu'avec la VPP**. Sur Intel, la VPP exige le pilote **non libre** `intel-media-va-driver-non-free`. Le pilote libre installé par défaut sur Ubuntu (`intel-media-va-driver`) n'expose pas `VAEntrypointVideoProc`. Sans VPP, le moteur reste en décodage matériel et prend un chemin dégradé (R13) :
- il projette la surface en RAM (`av_hwframe_map`) ;
- il recadre et réduit l'image sur le CPU, avec une sortie Planar8 ;
- il compte chaque conversion comme un transfert pleine résolution (R15) : le pilote iHD détuile toute la surface, même pour une région.

Ce chemin coûte 21,7–23,8 ms par image en Ajusté, contre 5,4–7,0 ms avec la VPP et 32–43 ms pour un transfert complet (§2.5). PERF.XFER.01 n'y est **pas** tenue. Ce cas concerne les portables des collègues, s'ils gardent le pilote par défaut. Ce chemin n'a été mesuré que conversion par conversion, en forçant `VaapiMapped`. Aucun banc de bout en bout n'a tourné sur une machine réellement privée de VPP.

### 4.3 Mode logiciel

Sans VA-API :
- le saut vaut **488–518 ms** au p95 côté interface (473–504 ms au moteur) ;
- l'image suivante reste sous 100 ms au p95 (20–45 ms), avec un premier pas après saut de 144–238 ms ;
- l'image précédente a un p95 de 42–83 ms et un max de 84–135 ms en régime établi, mais son premier pas après un saut sur une IDR coûte 363–574 ms ;
- la mémoire de l'application atteint **1,1–1,2 Go en PSS** (1,34–1,47 Go en somme des RSS), sous le seuil de 2 Go mais avec moins de marge qu'en VA-API.

### 4.4 Autres limites connues

- **Drapeau `corrupt`** (R11) : avec FFmpeg 6.1, les images qui dépendent d'une AU corrompue sortent sans drapeau. Seule l'AU rejetée émet `CorruptAu`. Une image faussée peut donc s'afficher sans marque. À revoir en M2 : la tolérance côté analyse viendra de `ma-ts` et `ma-h26x`.
- **Moteur WebGL non identifié** : WebKitGTK masque le nom (`WebKit WebGL`, « Apple GPU » une fois démasqué). Ce nom n'a été relevé que dans les exécutions Xvfb (passe logicielle et fumées) ; les exécutions VA-API, Wayland comme Xvfb, ne l'enregistraient pas encore. WebKitWebProcess a un nœud `/dev/dri/renderD*` ouvert, même sous Xvfb. Le dessin 1080p prend 2–5 ms. Ce sont des indices de rendu GPU, pas une preuve : llvmpipe n'est pas exclu.
- **Résolution de 1 ms** de `performance.now()` dans WebKitGTK : les petites durées (dessin de 2–5 ms) sont grossières.
- **Captures WebDriver sur Wayland** : `saveScreenshot` ne répond jamais quand la fenêtre est en plein écran. Les exécutions Wayland ont donc été faites sans capture. Sur l'écran réel, la mesure s'arrête à `gl.finish`, pas à la composition à l'écran : le canvas 1920×1080 est agrandi par CSS en 2560×1440.
- **PSS absent des exécutions VA-API** : le PSS a été ajouté au banc après ces exécutions. Seule la somme des RSS (766–919 Mo) est relevée en VA-API.
- **Numéro en 1:1 UHD** : une région de 1920×1080 ne couvre que la moitié de la bande du numéro. Seuls ses 8 bits de poids faible sont vérifiés (16 bits en Ajusté).
- **Banc hors CI et hors `cargo deny`** : `tools/transport-bench` est exclu du workspace. Ses dépendances Tauri ne sont pas auditées par le dépôt.
- **Source d'AU de test** : les bancs lisent le TS avec la source de `ma-testkit` (« 1 PES = 1 AU »). Elle est valable pour ce corpus généré par FFmpeg et sera remplacée par `ma-ts` en M2.
- **Corpus synthétique** : mire, x265 `ultrafast`, GOP fermé. Les temps de décodage sont probablement optimistes face à un vrai flux broadcast.

## 5. Options mesurées pour l'image précédente (à décider par le porteur)

Aucune option n'est retenue ici. Les chiffres viennent des mesures ci-dessus. Ce qui n'a pas été mesuré est signalé.

1. **Juger le seuil en régime établi avec un intervalle réaliste** (lecture R18). Ce cas est mesuré : max de 56–84 ms au moteur et de 95–114 ms à l'interface à 200 ms. Il reste un arrêt de 219–382 ms au premier pas arrière après un saut sur une IDR, et ponctuellement de 270–278 ms en appui maintenu.
2. **Accepter le premier pas arrière après un saut comme une attente signalée**, avec un indicateur d'attente comme pour un saut. Son coût mesuré, 219–382 ms côté interface, reste sous le seuil de saut de 500 ms. Rien n'est à mesurer de plus. C'est une décision de spécification : elle revient à modifier l'acceptation de PERF.STEP.02.
3. **Précharger d'abord vers l'arrière après un saut** (le GOP précédent avant les 8 suivantes). Le coût de ce GOP est connu (environ 250–385 ms de décodage), mais le gain n'est **pas mesuré**. Le premier pas arrière resterait lent si l'utilisateur recule avant la fin de ce décodage. Le premier pas avant, aujourd'hui de 22–34 ms au moteur, risque d'en pâtir.
4. **Élargir la fenêtre arrière** pour garder le GOP précédent entier, réduit, en cache. La mémoire se calcule à partir des octets mesurés : 50 × 3,11 Mo, soit environ 156 Mo en Ajusté, et 50 × 6,22 Mo, soit environ 311 Mo en 1:1. Le budget de 2 Go le permet en VA-API (766–919 Mo mesurés), mais la marge est faible en logiciel (1,1–1,2 Go en PSS). Le gain en appui maintenu n'est **pas mesuré**.
5. **Limiter la cadence de l'appui maintenu** (une image toutes les N ms, sans intervalle nul). À 200 ms, le seuil est tenu en régime établi. Les intervalles intermédiaires ne sont **pas mesurés**.

Quelle que soit l'option, le résultat dépend de la longueur du GOP. Il faut mesurer de vrais flux broadcast (GOP ouverts, autres longueurs) en M2 et en M7.

## 6. Mesures restant à faire

- **Cache disque froid** : `drop_caches` demande les droits administrateur. Exigé par PERF.OPEN.01. En M7.
- **Fichier sur partage réseau** (NFS en conteneur local, 5 ms de latence) : PERF.NET.01, en M2 (lecture positionnelle de `ma-io`) et en M7.
- **Vrai flux broadcast UHD**, avec GOP ouverts et longueurs de GOP réelles : en M2 (`ma-ts` remplace la source de test) et en M7.
- **VA-API sans VPP de bout en bout**, sur une vraie machine au pilote libre (moteur et interface). Seules des conversions forcées ont été mesurées ici.
- **PSS en VA-API** sur le parcours d'interface complet.
- **Options du §5** retenues par le porteur, à mesurer avec le même protocole.
