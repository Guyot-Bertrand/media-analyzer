# Construire Media Analyzer

Plateformes : exécution sur **Ubuntu 24.04+** ; construction vérifiée aussi sur **Fedora**.

## 1. Dépendances système

### Ubuntu 24.04+

```bash
sudo apt install -y build-essential curl wget file pkg-config clang libclang-dev mold \
  libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libswresample-dev libva-dev
```

### Fedora

Les décodeurs H.264/HEVC de FFmpeg ne sont pas dans `ffmpeg-free` : activer **RPM Fusion** (free) puis installer `ffmpeg-devel`.

```bash
sudo dnf install -y https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm
sudo dnf install -y gcc gcc-c++ curl wget file pkgconf-pkg-config clang clang-devel mold \
  webkit2gtk4.1-devel gtk3-devel libsoup3-devel javascriptcoregtk4.1-devel \
  libxdo-devel openssl-devel libappindicator-gtk3-devel librsvg2-devel \
  ffmpeg-devel libva-devel
```

## 2. Rust et outils cargo

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y      # la version est fixée par rust-toolchain.toml
cargo install cargo-nextest cargo-llvm-cov cargo-deny --locked
```

## 3. Construire et vérifier

```bash
cargo build --workspace
cargo xtask ci          # formatage, clippy, licences, tests + couverture, traçabilité
```

### Version de FFmpeg

La crate `ma-ffmpeg` se lie à la bibliothèque FFmpeg du système (rsmpeg, `link_system_ffmpeg`).
Sa feature doit correspondre à la version de libavcodec (`pkg-config --modversion libavcodec`) :

| libavcodec | FFmpeg | feature |
|---|---|---|
| 60 | 6.x (Ubuntu 24.04) | `ffmpeg6` (par défaut) |
| 61 | 7.x | `ffmpeg7` |
| 62 | 8.x | `ffmpeg8` |

`cargo xtask ci` la détecte et l'ajoute à toutes les commandes cargo (`--features ma-ffmpeg/<f>`).
À la main, hors FFmpeg 6 : `cargo build --workspace --features ma-ffmpeg/ffmpeg7`. Les features
sont cumulatives (`ffmpeg7` inclut `ffmpeg6`).

### Décodage matériel dans les tests

Si un périphérique `/dev/dri/renderD*` existe, `cargo xtask ci` fixe `MA_REQUIRE_GPU=1` : les tests
VA-API échouent au lieu d'être sautés. Sans périphérique, ils affichent `VA-API absent : test non
exécuté`, et la couverture de `ma-ffmpeg` est affichée sans être bloquante (avertissement
`couverture de ma-ffmpeg non garantie : pas de VA-API`).

### Traitement vidéo VA-API (VPP)

La réduction et le recadrage sur le GPU (`scale_vaapi`) demandent l'entrypoint
`VAProfileNone : VAEntrypointVideoProc` (`vainfo`). Le pilote Intel libre d'Ubuntu
(`intel-media-va-driver`) ne l'a pas : installer `intel-media-va-driver-non-free` (multiverse) ;
sous Fedora, `intel-media-driver` de RPM Fusion nonfree. Sans VPP, le moteur reste en décodage
matériel mais lit la surface entière (`ConvertPath::VaapiMapped`). Les tests de `ma-ffmpeg` qui
demandent la VPP (dont PERF.XFER.01) affichent alors `VPP VA-API absente : test non exécuté`, ou
échouent si `MA_REQUIRE_GPU=1` (fixé par `cargo xtask ci` dès qu'un périphérique existe : sur
une machine VA-API sans VPP, lancer les tests à la main ou installer le pilote non libre).

## 4. Outils de test uniquement (non requis pour construire)

- **TSDuck** (oracle TS) : paquet `.deb` / `.rpm` des releases https://github.com/tsduck/tsduck/releases
- **Tests d'interface de bout en bout** : `webkit2gtk-driver` (Ubuntu) / `webkit2gtk4.1` fournit `WebKitWebDriver` (Fedora), `xvfb` / `xorg-x11-server-Xvfb`, `cargo install tauri-driver --locked`
- **Corpus généré** (`cargo xtask corpus gen`, `--big` pour `uhd_big`, ≥ 4 Go) : `ffmpeg` et `ffprobe` avec `libx264` et `libx265` (`sudo apt install ffmpeg` ; sous Fedora, `sudo dnf install -y ffmpeg` depuis RPM Fusion, le paquet `ffmpeg-free` n'ayant pas ces encodeurs). Les fichiers vont dans `$MA_CORPUS_DIR` (défaut `~/media-analyzer-corpus/generated`), hors Git.
- **Décodage matériel** : `vainfo` pour vérifier VA-API
