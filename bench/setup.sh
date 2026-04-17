#!/usr/bin/env bash
# One-shot setup for a fresh Ubuntu 24.04 host to run the zatat vs. Reverb
# benchmark. Installs everything, builds zatat, and scaffolds a minimal
# Laravel + Reverb app.
#
# Usage:  sudo bash setup.sh
#
# Tested on Hetzner CX23 (2 vCPU / 4 GB / Ubuntu 24.04).

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "setup.sh needs sudo (installs packages and raises file-descriptor limits)"
  exit 1
fi

BENCH_USER="${SUDO_USER:-root}"
BENCH_HOME="$(getent passwd "$BENCH_USER" | cut -d: -f6)"
BENCH_DIR="$BENCH_HOME/bench-work"

echo "==> installing OS packages"
apt-get update -y
apt-get install -y --no-install-recommends \
  curl ca-certificates git build-essential pkg-config \
  software-properties-common \
  wrk redis-server \
  php-cli php-mbstring php-xml php-curl php-zip php-sqlite3 php-bcmath \
  unzip

echo "==> installing Rust (for the $BENCH_USER user)"
sudo -u "$BENCH_USER" bash -lc '
  command -v cargo >/dev/null 2>&1 || \
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
'

echo "==> installing Node.js 20"
curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
apt-get install -y nodejs

echo "==> installing composer"
if ! command -v composer >/dev/null 2>&1; then
  curl -sS https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer
fi

echo "==> raising file-descriptor limit"
cat >/etc/security/limits.d/99-bench.conf <<'LIMITS'
*       soft    nofile  200000
*       hard    nofile  200000
root    soft    nofile  200000
root    hard    nofile  200000
LIMITS
grep -q '^DefaultLimitNOFILE=' /etc/systemd/system.conf \
  || echo 'DefaultLimitNOFILE=200000' >>/etc/systemd/system.conf
grep -q '^DefaultLimitNOFILE=' /etc/systemd/user.conf \
  || echo 'DefaultLimitNOFILE=200000' >>/etc/systemd/user.conf
sysctl -w fs.file-max=2000000 >/dev/null
sysctl -w net.core.somaxconn=65535 >/dev/null
sysctl -w net.ipv4.ip_local_port_range="1024 65535" >/dev/null
sysctl -w net.ipv4.tcp_tw_reuse=1 >/dev/null
ulimit -n 200000 || true

echo "==> preparing bench workspace at $BENCH_DIR"
sudo -u "$BENCH_USER" mkdir -p "$BENCH_DIR"

echo "==> scaffolding Laravel + Reverb baseline"
if [[ ! -d "$BENCH_DIR/reverb-app" ]]; then
  sudo -u "$BENCH_USER" bash -lc "
    set -e
    cd '$BENCH_DIR'
    composer create-project laravel/laravel reverb-app --no-interaction
    cd reverb-app
    php artisan install:broadcasting --no-interaction --reverb || composer require laravel/reverb --no-interaction
  "
fi

cat <<EOF

==> done. Log out, log back in, then see bench/README.md for next steps.
EOF
