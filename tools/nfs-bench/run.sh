#!/usr/bin/env bash
# Mesure NFS de M2a (PERF.NET.01, décision D6 du plan M2a) : voir README.md.
#
# 1. serveur NFSv4 en conteneur Docker --privileged qui exporte $MA_CORPUS_DIR ;
# 2. latence de 5 ms (tc netem) sur le pont Docker dédié ;
# 3. montage dans /mnt/ma-nfs ;
# 4. engine_bench --progressive sur uhd_big, en auto et en sw, RUNS exécutions par mode ;
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

  --runs N     exécutions par mode (défaut : 3)
  --delay MS   latence ajoutée par tc netem, en ms (défaut : 5)
  --truncate   ajoute le test de troncature côté serveur pendant un parcours
  --no-bench   saute les mesures (avec --truncate : troncature seule)

Variables : MA_CORPUS_DIR (défaut ~/media-analyzer-corpus/generated),
            MA_FFMPEG_FEATURE (défaut ma-ffmpeg/ffmpeg6),
            MA_NFS_IMAGE (défaut itsthenetwork/nfs-server-alpine:12).
EOF
}

RUNS=3
DELAY_MS=5
TRUNCATE=0
BENCH=1
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --delay) DELAY_MS="$2"; shift 2 ;;
        --truncate) TRUNCATE=1; shift ;;
        --no-bench) BENCH=0; shift ;;
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

[[ -f "$CORPUS/uhd_big.ts" ]] || {
    echo "uhd_big absent : lancer cargo xtask corpus gen --big ($CORPUS)" >&2
    exit 1
}
command -v docker >/dev/null || { echo "docker absent" >&2; exit 1; }
command -v mount.nfs4 >/dev/null || command -v /sbin/mount.nfs4 >/dev/null || {
    echo "mount.nfs4 absent : installer nfs-common" >&2
    exit 1
}

echo "== construction de engine_bench (release, $FEATURE)"
(cd "$REPO" && cargo build --release -p ma-ffmpeg --features "$FEATURE" --example engine_bench)

echo "== droits administrateur : sudo est demandé pour modprobe, tc, mount et umount"
sudo -v
# L'horodatage de sudo expire (15 min par défaut) avant la fin des mesures : il est
# rafraîchi en fond, sans invite (sudo -n), et ce rafraîchissement est arrêté au nettoyage.
( while true; do sudo -n -v 2>/dev/null; sleep 60; done ) &
SUDO_KEEPALIVE=$!

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
    kill "$SUDO_KEEPALIVE" 2>/dev/null
    exit "$status"
}
trap cleanup EXIT
trap 'cleanup 130' INT
trap 'cleanup 143' TERM

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

echo "== montage $IP:/ sur $MNT"
mount_nfs "$MNT" "" || { echo "montage NFS impossible" >&2; exit 1; }
MOUNTED=1
grep " $MNT " /proc/mounts
ping -c 3 -q "$IP" | tail -1 || true

mkdir -p "$OUT"
if [[ $BENCH -eq 1 ]]; then
    for mode in auto sw; do
        for i in $(seq 1 "$RUNS"); do
            echo "== engine_bench --progressive, uhd_big sur NFS, mode $mode, exécution $i/$RUNS"
            "$BENCH_BIN" --file "$MNT/uhd_big.ts" --mode "$mode" --view fit:1920x1080 \
                --dwell-ms 0 --progressive --json "$OUT/uhd_big-$mode-$i.json" \
                2>&1 | tee "$OUT/uhd_big-$mode-$i.txt"
        done
    done
fi

if [[ $TRUNCATE -eq 1 ]]; then
    echo "== troncature côté serveur pendant un parcours"
    if ! mkdir "$CORPUS/$TRUNC_SUB"; then
        echo "$CORPUS/$TRUNC_SUB existe déjà : le retirer à la main avant --truncate" >&2
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
    LOG="$OUT/truncate.txt"
    set +e
    "$BENCH_BIN" --file "$MNT_NOAC/$TRUNC_SUB/uhd_big.ts" --mode auto --view fit:1920x1080 \
        --dwell-ms 0 --progressive >"$LOG" 2>&1 &
    PID=$!
    # Le banc écrit « uhd_big : N images … » quand l'indexation est finie, juste avant
    # les sauts : la troncature tombe donc pendant le parcours.
    for _ in $(seq 1 600); do
        grep -q "^uhd_big : " "$LOG" && break
        kill -0 "$PID" 2>/dev/null || break
        sleep 0.5
    done
    SIZE="$(stat -c %s "$CORPUS/$TRUNC_SUB/uhd_big.ts")"
    truncate -s "$((SIZE / 2))" "$CORPUS/$TRUNC_SUB/uhd_big.ts"
    echo "   tronqué côté serveur à $((SIZE / 2)) octets"
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
    # Code 1 : il faut aussi que l'erreur soit celle de la lecture du fichier tronqué
    # (FrameError::Io ⇒ « lecture de la source : fichier tronqué : … »).
    if ! grep -qE "^engine_bench : .*(lecture de la source|fichier tronqué)" "$LOG"; then
        echo "ÉCHEC : le banc s'est arrêté sur une autre erreur que la troncature" >&2
        exit 1
    fi
    echo "   troncature constatée : erreur de lecture affichée, sans plantage (code 1)"
fi

echo "== résultats : $OUT"
