#!/usr/bin/env bash
# Mesure NFS de M2a (PERF.NET.01, décision D6 du plan M2a) : voir README.md.
#
# 1. serveur NFSv4 en conteneur Docker --privileged qui exporte $MA_CORPUS_DIR ;
# 2. latence de 5 ms (tc netem) sur le pont Docker dédié ;
# 3. montage dans /mnt/ma-nfs, puis attente de la fin de la période de grâce NFSv4 (R24) ;
# 4. engine_bench --progressive sur uhd_big, en auto et en sw, RUNS exécutions par mode, chacune
#    à cache de pages froid (client et serveur, R24) ;
# 5. (option --truncate) troncature côté serveur d'une copie pendant un parcours ;
# 6. démontage et nettoyage, même en cas d'échec (trap).
#
# Les commandes privilégiées (modprobe, tc, mount, umount) passent par sudo, demandé
# explicitement au début. Ce script n'est pas lancé par `cargo xtask ci`.

set -euo pipefail

# Lancé par « sudo run.sh », HOME vaut /root : le corpus, cargo et target/ ne seraient pas ceux
# de l'utilisateur. Le script demande sudo lui-même, pour les seules commandes privilégiées.
if [[ $EUID -eq 0 ]]; then
    echo "à lancer sans sudo, en utilisateur normal : le script demande sudo lui-même pour les seules commandes privilégiées (cargo, corpus et target/ sont ceux de l'utilisateur)" >&2
    exit 2
fi

usage() {
    cat <<'EOF'
Usage : tools/nfs-bench/run.sh [--runs N] [--delay MS] [--truncate] [--no-bench]
                               [--drop-caches] [--strace]

  --runs N       exécutions par mode (défaut : 3)
  --delay MS     latence ajoutée par tc netem, en ms (défaut : 5)
  --truncate     ajoute le test de troncature côté serveur pendant un parcours
  --no-bench     saute les mesures (avec --truncate : troncature seule)
  --drop-caches  en secours, si uhd_big reste résident à plus de 1 % après
                 POSIX_FADV_DONTNEED : sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
  --strace       1re exécution de chaque mode sous strace (openat, read, pread64)

Variables : MA_CORPUS_DIR (défaut ~/media-analyzer-corpus/generated),
            MA_FFMPEG_FEATURE (défaut ma-ffmpeg/ffmpeg6),
            MA_NFS_IMAGE (défaut itsthenetwork/nfs-server-alpine:12).
EOF
}

RUNS=3
DELAY_MS=5
TRUNCATE=0
BENCH=1
DROP_CACHES=0
STRACE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --delay) DELAY_MS="$2"; shift 2 ;;
        --truncate) TRUNCATE=1; shift ;;
        --no-bench) BENCH=0; shift ;;
        --drop-caches) DROP_CACHES=1; shift ;;
        --strace) STRACE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "option inconnue : $1" >&2; usage >&2; exit 2 ;;
    esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CORPUS="${MA_CORPUS_DIR:-$HOME/media-analyzer-corpus/generated}"
if [[ ! -d "$CORPUS" ]]; then
    echo "corpus introuvable : $CORPUS (MA_CORPUS_DIR ou cargo xtask corpus gen)" >&2
    exit 1
fi
CORPUS="$(cd "$CORPUS" && pwd)"
FEATURE="${MA_FFMPEG_FEATURE:-ma-ffmpeg/ffmpeg6}"
IMAGE="${MA_NFS_IMAGE:-itsthenetwork/nfs-server-alpine:12}"
CONTAINER=ma-nfs-server
NETWORK=ma-nfs-net
BRIDGE=ma-nfs0
MNT=/mnt/ma-nfs
MNT_NOAC=/mnt/ma-nfs-noac
TRUNC_SUB=nfs-trunc
OUT="$REPO/target/bench/nfs/$(date +%Y%m%d-%H%M%S)"
BENCH_BIN="$REPO/target/release/examples/engine_bench"
CACHE_PY="$REPO/tools/nfs-bench/cache.py"
# Petit fichier du corpus de base, ouvert sans être lu pour attendre la fin de la grâce NFSv4.
GRACE_PROBE=ts_ref.ts

[[ -f "$CORPUS/uhd_big.ts" ]] || {
    echo "uhd_big absent : lancer cargo xtask corpus gen --big ($CORPUS)" >&2
    exit 1
}
[[ -f "$CORPUS/$GRACE_PROBE" ]] || {
    echo "$GRACE_PROBE absent : lancer cargo xtask corpus gen ($CORPUS)" >&2
    exit 1
}
command -v docker >/dev/null || { echo "docker absent" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 absent" >&2; exit 1; }
if [[ $STRACE -eq 1 ]] && ! command -v strace >/dev/null; then
    echo "AVERTISSEMENT : strace absent (paquet strace) : --strace ignoré" >&2
    STRACE=0
fi
command -v mount.nfs4 >/dev/null || command -v /sbin/mount.nfs4 >/dev/null || {
    echo "mount.nfs4 absent : installer nfs-common" >&2
    exit 1
}

echo "== construction de engine_bench (release, $FEATURE)"
(cd "$REPO" && cargo build --release -p ma-ffmpeg --features "$FEATURE" --example engine_bench)

echo "== droits administrateur : sudo est demandé pour modprobe, tc, mount et umount"
sudo -v

SUDO_KEEPALIVE=""
MOUNTED=0
MOUNTED_NOAC=0
QDISC=0
TRUNC_CREATED=0
cleanup() {
    local status=${1:-$?}
    trap - EXIT INT TERM
    set +e
    echo "== nettoyage"
    if [[ $MOUNTED_NOAC -eq 1 ]]; then
        sudo umount -f "$MNT_NOAC" || sudo umount -l "$MNT_NOAC"
        MOUNTED_NOAC=0
    fi
    if [[ $MOUNTED -eq 1 ]]; then
        sudo umount -f "$MNT" || sudo umount -l "$MNT"
        MOUNTED=0
    fi
    if [[ $QDISC -eq 1 ]]; then
        sudo tc qdisc del dev "$BRIDGE" root 2>/dev/null
        QDISC=0
    fi
    docker rm -f "$CONTAINER" >/dev/null 2>&1
    docker network rm "$NETWORK" >/dev/null 2>&1
    if [[ $TRUNC_CREATED -eq 1 ]]; then
        rm -rf "${CORPUS:?}/${TRUNC_SUB:?}"
        TRUNC_CREATED=0
    fi
    if [[ -n $SUDO_KEEPALIVE ]]; then
        kill "$SUDO_KEEPALIVE" 2>/dev/null
        wait "$SUDO_KEEPALIVE" 2>/dev/null
        SUDO_KEEPALIVE=""
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'cleanup 130' INT
trap 'cleanup 143' TERM

# L'horodatage de sudo expire (15 min par défaut) avant la fin des mesures : il est
# rafraîchi en fond, sans invite (sudo -n), après la pose du trap. Le sous-shell attend son
# `sleep` par `wait`, que le signal interrompt : son trap tue alors ce `sleep`, sans orphelin.
(
    trap 'kill "$nap" 2>/dev/null; exit 0' TERM
    nap=""
    while true; do
        sudo -n -v 2>/dev/null || true
        sleep 60 &
        nap=$!
        wait "$nap" || true
    done
) &
SUDO_KEEPALIVE=$!

echo "== serveur NFSv4 ($IMAGE) exportant $CORPUS (lecture seule)"
sudo modprobe nfsd
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker network rm "$NETWORK" >/dev/null 2>&1 || true
docker network create -o "com.docker.network.bridge.name=$BRIDGE" "$NETWORK" >/dev/null
docker run -d --name "$CONTAINER" --privileged --network "$NETWORK" \
    -v "$CORPUS:/export:ro" -e SHARED_DIRECTORY=/export "$IMAGE" >/dev/null
IP="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$CONTAINER")"
echo "   adresse du serveur : $IP"

echo "== latence : tc netem delay ${DELAY_MS}ms sur $BRIDGE"
sudo tc qdisc add dev "$BRIDGE" root netem delay "${DELAY_MS}ms"
QDISC=1

# mount_nfs <point de montage> <options supplémentaires> : 30 essais, le temps que le
# serveur démarre.
mount_nfs() {
    sudo mkdir -p "$1"
    for _ in $(seq 1 30); do
        if sudo mount -t nfs4 -o "vers=4.2,proto=tcp,port=2049,ro$2" "$IP:/" "$1" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# wait_grace <point de montage> : le serveur vient de démarrer et refuse tout OPEN pendant sa
# période de grâce NFSv4 (90 s, NFS4ERR_GRACE) ; le client réessaie en doublant son délai
# jusqu'à 15 s. Ouvrir un petit fichier sans le lire bloque jusqu'à la fin de la grâce, sans
# réchauffer uhd_big. Sans cette attente, la 1re exécution mesure la grâce (85 à 100 s).
wait_grace() {
    echo "== attente de la fin de la période de grâce NFSv4 (≤ 90 s) : ouverture de $1/$GRACE_PROBE"
    local start=$SECONDS
    if ! timeout 150 sh -c ': < "$1"' sh "$1/$GRACE_PROBE"; then
        echo "période de grâce NFSv4 non levée en 150 s ($1/$GRACE_PROBE)" >&2
        exit 1
    fi
    echo "   ouverture obtenue après $((SECONDS - start)) s"
}

# mountstats <point de montage> <fichier> : compteurs OPEN, READ et GETATTR du montage
# (/proc/self/mountstats : opérations, transmissions, délais cumulés en ms…).
mountstats() {
    awk -v m="$1" '$1 == "device" { on = ($5 == m) } on && /^[[:space:]]*(OPEN|READ|GETATTR):/' \
        /proc/self/mountstats >"$2"
}

# cold_cache : retire uhd_big du cache de pages, côté client (inode NFS) et côté serveur (fichier
# exporté, dans le cache de pages du même hôte), puis affiche la part résidente des deux.
cold_cache() {
    python3 -I "$CACHE_PY" drop "$MNT/uhd_big.ts" "$CORPUS/uhd_big.ts"
    local code=0
    python3 -I "$CACHE_PY" resident --max-percent 1 "$MNT/uhd_big.ts" "$CORPUS/uhd_big.ts" \
        | sed 's/^/   résidence : /' || code=$?
    [[ $code -eq 0 ]] && return 0
    [[ $code -ne 3 ]] && { echo "contrôle du cache de pages impossible (code $code)" >&2; exit 1; }
    if [[ $DROP_CACHES -eq 0 ]]; then
        echo "AVERTISSEMENT : uhd_big reste résident à plus de 1 % : mesure peut-être en cache chaud ; relancer avec --drop-caches (sync; echo 3 | sudo tee /proc/sys/vm/drop_caches)" >&2
        return 0
    fi
    echo "   plus de 1 % résident : sync; echo 3 | sudo tee /proc/sys/vm/drop_caches"
    sync
    echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    code=0
    python3 -I "$CACHE_PY" resident --max-percent 1 "$MNT/uhd_big.ts" "$CORPUS/uhd_big.ts" \
        | sed 's/^/   résidence : /' || code=$?
    if [[ $code -ne 0 ]]; then
        echo "AVERTISSEMENT : uhd_big reste résident à plus de 1 % malgré drop_caches" >&2
    fi
}

echo "== montage $IP:/ sur $MNT"
mount_nfs "$MNT" "" || { echo "montage NFS impossible" >&2; exit 1; }
MOUNTED=1
grep " $MNT " /proc/mounts
ping -c 3 -q "$IP" | tail -1 || true
wait_grace "$MNT"

mkdir -p "$OUT"
if [[ $BENCH -eq 1 ]]; then
    for mode in auto sw; do
        for i in $(seq 1 "$RUNS"); do
            echo "== engine_bench --progressive, uhd_big sur NFS, mode $mode, exécution $i/$RUNS (cache froid)"
            cold_cache
            TRACE=()
            if [[ $STRACE -eq 1 && $i -eq 1 ]]; then
                TRACE=(strace -f -tt -T -e trace=openat,read,pread64 -o "$OUT/strace-$mode-1.txt")
                echo "   sous strace : $OUT/strace-$mode-1.txt"
            fi
            mountstats "$MNT" "$OUT/mountstats-$mode-$i-avant.txt"
            "${TRACE[@]}" "$BENCH_BIN" --file "$MNT/uhd_big.ts" --mode "$mode" --view fit:1920x1080 \
                --dwell-ms 0 --progressive --json "$OUT/uhd_big-$mode-$i.json" \
                2>&1 | tee "$OUT/uhd_big-$mode-$i.txt"
            mountstats "$MNT" "$OUT/mountstats-$mode-$i-apres.txt"
        done
    done
fi

if [[ $TRUNCATE -eq 1 ]]; then
    echo "== troncature côté serveur pendant un parcours"
    if [[ -e "$CORPUS/$TRUNC_SUB" ]]; then
        echo "$CORPUS/$TRUNC_SUB existe déjà : le retirer à la main avant --truncate" >&2
        exit 1
    fi
    if ! ERR="$(mkdir "$CORPUS/$TRUNC_SUB" 2>&1)"; then
        echo "création de $CORPUS/$TRUNC_SUB impossible : $ERR" >&2
        exit 1
    fi
    TRUNC_CREATED=1
    cp --reflink=auto "$CORPUS/uhd_big.ts" "$CORPUS/$TRUNC_SUB/uhd_big.ts"
    # Montage sans cache d'attributs (actimeo=0) : la nouvelle taille est vue au contrôle
    # suivant de ma-io, pas après l'expiration du cache du client. nosharecache : sinon le
    # noyau réutilise le superbloc (et les options) du 1er montage.
    echo "== montage $IP:/ sur $MNT_NOAC (actimeo=0)"
    mount_nfs "$MNT_NOAC" ",actimeo=0,nosharecache" || { echo "montage NFS impossible" >&2; exit 1; }
    MOUNTED_NOAC=1
    wait_grace "$MNT_NOAC"
    LOG="$OUT/truncate.txt"
    set +e
    "$BENCH_BIN" --file "$MNT_NOAC/$TRUNC_SUB/uhd_big.ts" --mode auto --view fit:1920x1080 \
        --dwell-ms 0 --progressive >"$LOG" 2>&1 &
    PID=$!
    # Le banc écrit « uhd_big : N images … » quand l'indexation est finie, juste avant
    # les sauts : la troncature tombe donc pendant le parcours.
    INDEXED=0
    for _ in $(seq 1 600); do
        if grep -q "^uhd_big : " "$LOG"; then
            INDEXED=1
            break
        fi
        kill -0 "$PID" 2>/dev/null || break
        sleep 0.5
    done
    if [[ $INDEXED -eq 0 ]]; then
        if kill -0 "$PID" 2>/dev/null; then
            echo "   indexation non finie en 300 s : troncature pendant l'indexation"
        else
            echo "   le banc s'est arrêté avant la fin de l'indexation"
        fi
    fi
    SIZE="$(stat -c %s "$CORPUS/$TRUNC_SUB/uhd_big.ts")"
    # Le serveur NFS a accordé au client une délégation de lecture (bail noyau) sur ce fichier :
    # `truncate` (coreutils) ouvre en O_NONBLOCK et échoue alors en EAGAIN. truncate(2) par
    # chemin, lui, attend que le bail soit rompu (rappel de la délégation par le serveur).
    if ! ERR="$(python3 -I -c 'import os, sys; os.truncate(sys.argv[1], int(sys.argv[2]))' \
        "$CORPUS/$TRUNC_SUB/uhd_big.ts" "$((SIZE / 2))" 2>&1)"; then
        echo "ÉCHEC : troncature côté serveur impossible : $ERR" >&2
        kill "$PID" 2>/dev/null
        exit 1
    fi
    echo "   tronqué côté serveur à $((SIZE / 2)) octets ($(stat -c %s "$CORPUS/$TRUNC_SUB/uhd_big.ts") sur le serveur)"
    # Diagnostic : taille vue par le client NFS (attendue : la nouvelle) et options effectives.
    for delay in 0 2 10; do
        sleep "$delay"
        echo "   client après +${delay} s : $(stat -c %s "$MNT_NOAC/$TRUNC_SUB/uhd_big.ts" 2>&1) octets"
    done
    echo "   options du montage : $(grep " $MNT_NOAC " /proc/mounts || echo absent)"
    echo "   rappels de délégation (DELEGRETURN) sur ce montage :"
    awk -v m="$MNT_NOAC" '$1 == "device" { on = ($5 == m) } on && /^[[:space:]]*(DELEGRETURN|OPEN|GETATTR):/' \
        /proc/self/mountstats | sed 's/^/     /'
    wait "$PID"
    CODE=$?
    set -e
    cat "$LOG"
    if grep -q "panicked" "$LOG" || [[ $CODE -gt 1 ]]; then
        echo "ÉCHEC : le banc a planté (code $CODE)" >&2
        exit 1
    fi
    if [[ $CODE -eq 0 ]]; then
        echo "ÉCHEC : troncature non constatée (le parcours a fini sans erreur)" >&2
        exit 1
    fi
    # Code 1 : l'erreur doit venir de la troncature (IoError::Truncated, « fichier tronqué : … »),
    # ou d'une modification constatée (IoError::Modified, « fichier modifié … »), que
    # PERF.NET.01 accepte aussi. Toute autre erreur (EIO, ESTALE…) est un échec.
    ERRLINE="$(grep "^engine_bench : " "$LOG" | tail -1 || true)"
    if [[ $ERRLINE == *"fichier tronqué"* ]]; then
        echo "   troncature constatée : erreur affichée, sans plantage (code 1)"
    elif [[ $ERRLINE == *"fichier modifié"* ]]; then
        echo "   modification constatée, acceptée par PERF.NET.01 : erreur affichée, sans plantage (code 1)"
    else
        echo "ÉCHEC : le banc s'est arrêté sur une autre erreur que la troncature : ${ERRLINE:-aucune ligne « engine_bench : »}" >&2
        exit 1
    fi
fi

echo "== résultats : $OUT"
