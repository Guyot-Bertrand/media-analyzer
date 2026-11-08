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
- `python3` (bibliothèque standard seule) pour `cache.py` ; `strace` pour `--strace`
  (facultatif : absent, le script l'annonce et continue sans).
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
tools/nfs-bench/run.sh --strace --drop-caches   # 1re exécution tracée, drop_caches en secours
```

Options :

- `--runs N` (défaut 3), `--delay MS` (défaut 5) ;
- `--drop-caches` : secours du cache froid (voir « Protocole ») : si `uhd_big` reste résident à
  plus de 1 % après `POSIX_FADV_DONTNEED`, `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches`.
  Ce vidage est global (bibliothèques, pilote iHD, FFmpeg compris) : il mesure une machine
  froide plutôt qu'un fichier froid ;
- `--strace` : la 1re exécution de chaque mode passe sous
  `strace -f -tt -T -e trace=openat,read,pread64`, sortie dans `strace-<mode>-1.txt`. La durée
  de l'`openat` de `uhd_big.ts` (champ `<…>`) sépare l'ouverture bloquée du reste.

Variables : `MA_CORPUS_DIR` (défaut
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
   distant (`nfs4`) : blocs de 4 Mio, 32 blocs, 2 blocs anticipés. Puis attend la fin de la
   période de grâce NFSv4 (voir « Protocole »).
5. Lance `engine_bench --progressive --view fit:1920x1080 --dwell-ms 0` sur
   `/mnt/ma-nfs/uhd_big.ts`, en `auto` puis en `sw`, chaque exécution à cache froid (voir
   « Protocole »). Sorties dans `target/bench/nfs/<horodatage>/` : texte et JSON de chaque
   exécution, compteurs `OPEN`, `READ` et `GETATTR` du montage (`/proc/self/mountstats`) avant
   et après (`mountstats-<mode>-<i>-avant.txt`, `-apres.txt`), et `strace-<mode>-1.txt` avec
   `--strace`.
6. Avec `--truncate` :
   - crée `$MA_CORPUS_DIR/nfs-trunc/` et y copie `uhd_big.ts`. Le script refuse si ce dossier
     existe déjà, et ne le retire au nettoyage que s'il l'a créé ;
   - monte une 2e fois le partage sur `/mnt/ma-nfs-noac`, **sans cache d'attributs**
     (`actimeo=0,nosharecache`) : la nouvelle taille est vue dès le contrôle suivant de `ma-io`.
     L'attente de fin de grâce est refaite sur ce montage (immédiate si elle a déjà eu lieu) ;
   - lance le banc sur la copie à travers ce montage et attend la fin de l'indexation (ligne
     `uhd_big : N images …`) ;
   - tronque la copie de moitié **côté serveur** pendant les sauts. Si l'indexation n'est pas
     finie au bout de 300 s, le script l'annonce (« indexation non finie en 300 s ») et tronque
     quand même, donc pendant l'indexation. La troncature passe par `truncate(2)` (Python
     `os.truncate`) et non par `truncate` de coreutils : le serveur a accordé au client une
     délégation NFSv4 de lecture, protégée par un bail noyau, et coreutils ouvre le fichier en
     `O_NONBLOCK`, ce qui échoue en `EAGAIN` au lieu d'attendre le rappel de la délégation. Un
     échec de la troncature arrête le script ;
   - affiche la taille vue par le client à +0, +2 et +10 s, les options effectives du montage
     et les compteurs `OPEN`, `GETATTR` et `DELEGRETURN` (diagnostic) ;

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

## Protocole (décision R24)

Deux artefacts ont faussé les premiers passages (`target/bench/nfs/20261108-*`, non publiables,
voir `docs/07-mesures-m2a.md` §7) :

- **Période de grâce NFSv4.** Le serveur vient de démarrer : pendant 90 s, il refuse tout `OPEN`
  (`NFS4ERR_GRACE`, journal noyau : `NFSD: starting 90-second grace period`), et l'image n'a pas
  de `nfsdcld` pour la lever plus tôt. Le client réessaie en doublant son délai de 0,1 à 15 s :
  le `open()` de la 1re exécution restait bloqué 85 ou 100 s, comptés dans la 1re image et
  l'indexation. Le script attend donc, après chaque montage, l'ouverture (sans lecture) d'un
  petit fichier du corpus, `ts_ref.ts` :
  `timeout 150 sh -c ': < "$MNT/ts_ref.ts"'`. Il affiche « attente de la fin de la période de
  grâce NFSv4 (≤ 90 s) » puis la durée de l'attente, et s'arrête si la grâce n'est pas levée en
  150 s.
- **Cache de pages chaud.** Le serveur est sur la même machine : le fichier exporté (côté
  serveur) et l'inode NFS (côté client) sont tous les deux dans le cache de pages de l'hôte. Les
  exécutions 2 et 3 mesuraient ce cache, pas le réseau. Avant **chaque** exécution, le script
  retire `uhd_big.ts` du cache, côté client (`$MNT/uhd_big.ts`) et côté serveur
  (`$MA_CORPUS_DIR/uhd_big.ts`), par `POSIX_FADV_DONTNEED`, sans privilège
  (`python3 -I tools/nfs-bench/cache.py drop …`). Il affiche ensuite la part résidente des deux
  (`cache.py resident`, par `mmap` et `mincore`, sans rien lire). Au-delà de 1 %, il avertit et
  propose `--drop-caches` ; avec cette option, il fait le vidage global puis contrôle de nouveau.

`cache.py` s'utilise aussi seul :

```bash
python3 -I tools/nfs-bench/cache.py drop FICHIER...
python3 -I tools/nfs-bench/cache.py resident [--max-percent P] FICHIER...   # code 3 si > P %
```

## Résultats à publier

Pour chaque mode, sur les 3 exécutions (champs du JSON) : fin de l'ouverture
(`progressive.open_ms`, retour de `TsAuSource::open_progressive`), taille d'image connue
(`progressive.probe_ms`), 1re image (`progressive.first_frame_ms`), tous trois comptés depuis le
même instant, avant l'ouverture ; durée d'indexation (`progressive.index_ms`), saut p95
(`scenarios[jump].p95_ms`), pas suivant et précédent p95 (`step_forward`, `step_backward`),
octets lus rapportés à la taille du fichier (`progressive.read_ratio`).

## Limites

- Le serveur et le client sont sur la même machine. Le cache froid côté serveur oblige à relire
  le disque local (NVMe) : seuls la latence et le protocole NFS sont simulés, pas un disque de
  serveur ni un débit réseau limité.
- `POSIX_FADV_DONTNEED` ne libère que les pages propres et non projetées : c'est le cas ici
  (export en lecture seule), d'où le contrôle de résidence.
- La latence n'est ajoutée que dans un sens (hôte → serveur), donc 5 ms par aller-retour.
