# Banc NFS (PERF.NET.01)

Mesure manuelle de M2a (décision D6 du plan `docs/superpowers/plans/2026-11-03-mvp-m2-socle-ts.md`) :
le banc moteur `engine_bench --progressive` lit `uhd_big` sur un partage NFSv4 servi par un
conteneur local, avec une latence simulée de 5 ms. Le script n'est pas lancé par
`cargo xtask ci` : il demande `sudo` (module `nfsd`, `tc`, `mount`). Les résultats vont dans
`docs/07-mesures-m2a.md`.

## Prérequis

- Ceux du dépôt (`BUILD.md` §1–2) et le corpus généré avec `uhd_big`
  (`cargo xtask corpus gen --big`).
- Docker, utilisable par l'utilisateur (groupe `docker`).
- `nfs-common` (`mount.nfs4`) et `iproute2` (`tc`, module `sch_netem`).
- Le module noyau `nfsd` : le serveur du conteneur utilise le noyau de l'hôte (d'où
  `--privileged`). Le script le charge par `sudo modprobe nfsd`.
- Environ 4,1 Go libres dans le dossier du corpus pour `--truncate` (copie de `uhd_big`).

## Lancer

**En utilisateur normal, jamais par `sudo tools/nfs-bench/run.sh`.** Le script demande `sudo`
lui-même, pour les seules commandes privilégiées (`modprobe`, `tc`, `mount`, `umount`). Le corpus,
`cargo` et `target/` doivent être ceux de l'utilisateur : sous `sudo`, `HOME` vaut `/root`. Lancé
en root, le script s'arrête (code 2).

```bash
tools/nfs-bench/run.sh               # 3 exécutions en auto, 3 en sw
tools/nfs-bench/run.sh --truncate    # idem, puis le test de troncature
tools/nfs-bench/run.sh --no-bench --truncate   # troncature seule
```

Options : `--runs N` (défaut 3), `--delay MS` (défaut 5). Variables : `MA_CORPUS_DIR` (défaut
`~/media-analyzer-corpus/generated`), `MA_FFMPEG_FEATURE` (défaut `ma-ffmpeg/ffmpeg6`),
`MA_NFS_IMAGE` (défaut `itsthenetwork/nfs-server-alpine:12`).

## Ce que fait le script

1. Construit `engine_bench` en release, puis demande `sudo -v`. L'horodatage de `sudo` expire
   par défaut au bout de 15 min, avant la fin des mesures : le script le rafraîchit en fond
   (`sudo -n -v` toutes les 60 s) et arrête ce rafraîchissement au nettoyage. Si `sudo` est
   configuré sans horodatage (`timestamp_timeout=0`), il redemandera le mot de passe.
2. Crée un réseau Docker dédié (pont `ma-nfs0`, pour ne pas toucher `docker0`) et lance le
   conteneur `ma-nfs-server` (`--privileged`) qui exporte `$MA_CORPUS_DIR` en NFSv4 (dossier
   monté en lecture seule dans le conteneur).
3. `tc qdisc add dev ma-nfs0 root netem delay 5ms` : 5 ms sur les paquets de l'hôte vers le
   serveur.
4. Monte le partage en lecture seule sur `/mnt/ma-nfs` (`vers=4.2`). `ma-io` le voit comme
   distant (`nfs4`) : blocs de 4 Mio, 32 blocs, 2 blocs anticipés.
5. Lance `engine_bench --progressive --view fit:1920x1080 --dwell-ms 0` sur
   `/mnt/ma-nfs/uhd_big.ts`, en `auto` puis en `sw`. Sorties texte et JSON dans
   `target/bench/nfs/<horodatage>/`.
6. Avec `--truncate` :
   - crée `$MA_CORPUS_DIR/nfs-trunc/` et y copie `uhd_big.ts`. Le script refuse si ce dossier
     existe déjà, et ne le retire au nettoyage que s'il l'a créé ;
   - monte une 2e fois le partage sur `/mnt/ma-nfs-noac`, **sans cache d'attributs**
     (`actimeo=0,nosharecache`) : la nouvelle taille est vue dès le contrôle suivant de `ma-io` ;
   - lance le banc sur la copie à travers ce montage et attend la fin de l'indexation (ligne
     `uhd_big : N images …`) ;
   - tronque la copie de moitié **côté serveur** pendant les sauts. Si l'indexation n'est pas
     finie au bout de 300 s, le script l'annonce (« indexation non finie en 300 s ») et tronque
     quand même, donc pendant l'indexation.

   Attendu : le banc s'arrête sur l'erreur de lecture (code 1, ligne
   `engine_bench : … lecture de la source : fichier tronqué : …`) et ne plante pas. Le script
   échoue dans trois cas :
   - le banc panique ou meurt d'un signal (code > 1) ;
   - le parcours finit sans erreur (code 0, « troncature non constatée ») ;
   - le code 1 vient d'une autre erreur d'E/S (EIO, ESTALE…) : le script cite la ligne
     d'erreur.

   Une erreur `fichier modifié` (`IoError::Modified`) est acceptée, car PERF.NET.01 n'exige
   qu'une erreur sans plantage. Le script l'annonce à part : « modification constatée,
   acceptée par PERF.NET.01 ».
7. Démonte, retire la règle `tc`, le conteneur, le réseau et la copie, même en cas d'échec ou
   d'interruption (`trap` ; code 130 sur Ctrl-C, 143 sur `TERM`).

## Résultats à publier

Pour chaque mode, sur les 3 exécutions (champs du JSON) : 1re image
(`progressive.first_frame_ms`), durée d'indexation (`progressive.index_ms`), saut p95
(`scenarios[jump].p95_ms`), pas suivant et précédent p95 (`step_forward`, `step_backward`),
octets lus rapportés à la taille du fichier (`progressive.read_ratio`).

## Limites

- Le serveur et le client sont sur la même machine : les fichiers servis viennent du cache de
  pages de l'hôte (cache chaud côté serveur). Seuls la latence et le protocole NFS sont simulés,
  pas un disque de serveur ni un débit réseau limité.
- La latence n'est ajoutée que dans un sens (hôte → serveur), donc 5 ms par aller-retour.
