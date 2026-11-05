#!/usr/bin/env bash
# Compila e instala para o usuário atual (sem root): binários, serviço e atalho.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build --release
install -Dm755 target/release/mockcamd       ~/.local/bin/mockcamd
install -Dm755 target/release/mockcam        ~/.local/bin/mockcam
install -Dm755 target/release/mockcam-panel  ~/.local/bin/mockcam-panel
install -Dm644 packaging/mockcam.service     ~/.config/systemd/user/mockcam.service
install -Dm644 packaging/mockcam.desktop     ~/.local/share/applications/mockcam.desktop
systemctl --user daemon-reload
systemctl --user enable --now mockcam.service
echo "Instalado. Abra o painel com: mockcam-panel"
