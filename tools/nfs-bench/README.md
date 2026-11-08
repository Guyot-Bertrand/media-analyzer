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

```bash
tools/nfs-bench/run.sh               # 3 exécutions en auto, 3 en sw
tools/nfs-bench/run.sh --truncate    # idem, puis le test de troncature
tools/nfs-bench/run.sh --no-bench --truncate   # troncature seule
```

Options : `--runs N` (défaut 3), `--delay MS` (défaut 5). Variables : `MA_CORPUS_DIR` (défaut
`~/media-analyzer-corpus/generated`), `MA_FFMPEG_FEATURE` (défaut `ma-ffmpeg/ffmpeg6`),
`MA_NFS_IMAGE` (défaut `itsthenetwork/nfs-server-alpine:12`).

## Ce que fait le script

1. Construit `engine_bench` en release, puis demande `sudo -v`.
2. Crée un réseau Docker dédié (pont `ma-nfs0`, pour ne pas toucher `docker0`) et lance le
   conteneur `ma-nfs-server` (`--privileged`) qui exporte `$MA_CORPUS_DIR` en NFSv4.
3. `tc qdisc add dev ma-nfs0 root netem delay 5ms` : 5 ms sur les paquets de l'hôte vers le
   serveur.
4. Monte le partage en lecture seule sur `/mnt/ma-nfs` (`vers=4.2`). `ma-io` le voit comme
   distant (`nfs4`) : blocs de 4 Mio, 32 blocs, 2 blocs anticipés.
5. Lance `engine_bench --progressive --view fit:1920x1080 --dwell-ms 0` sur
   `/mnt/ma-nfs/uhd_big.ts`, en `auto` puis en `sw`. Sorties texte et JSON dans
   `target/bench/nfs/<horodatage>/`.
6. Avec `--truncate` : copie `uhd_big.ts` dans `$MA_CORPUS_DIR/nfs-trunc/`, lance le banc
   sur la copie à travers le montage, attend la fin de l'indexation (ligne
   `uhd_big : N images …`), puis tronque la copie de moitié **côté serveur** pendant les
   sauts. Attendu : le banc affiche une erreur (`engine_bench : …`, code 1) et ne plante pas
   (aucune panique, aucun signal). Le script échoue sinon.
7. Démonte, retire la règle `tc`, le conteneur, le réseau et la copie, même en cas d'échec ou
   d'interruption (`trap`).

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
