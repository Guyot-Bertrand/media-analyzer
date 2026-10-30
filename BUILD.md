# Construire Media Analyzer

Plateformes : exécution sur **Ubuntu 24.04+** ; construction vérifiée aussi sur **Fedora**.

## 1. Dépendances système

### Ubuntu 24.04+

```bash
sudo apt install -y build-essential curl wget file pkg-config clang libclang-dev mold \
  libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libva-dev
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

## 4. Outils de test uniquement (non requis pour construire)

- **TSDuck** (oracle TS) : paquet `.deb` / `.rpm` des releases https://github.com/tsduck/tsduck/releases
- **Tests d'interface de bout en bout** : `webkit2gtk-driver` (Ubuntu) / `webkit2gtk4.1` fournit `WebKitWebDriver` (Fedora), `xvfb` / `xorg-x11-server-Xvfb`, `cargo install tauri-driver --locked`
- **Décodage matériel** : `vainfo` pour vérifier VA-API
