# Benchmark harness

Reproducible benchmarks comparing zatat against [Laravel Reverb][reverb] on
the same hardware. Targets a fresh Ubuntu 24.04 host; tested on Hetzner
CX23 (2 vCPU / 4 GB / shared-AMD).

[reverb]: https://github.com/laravel/reverb

## Scenarios

1. **HTTP throughput** — `wrk -t2 -c64 -d20s` hammering `POST /apps/:id/events`
   with a signed body, no WebSocket subscribers. Measures the REST ingest
   path.
2. **Idle connection ramp** — open 10,000 WebSocket connections in batches
   of 100 and count how many complete the Pusher handshake.
3. **Broadcast fan-out** — 1,000 subscribers on one channel, publish one
   event, measure p50 / p95 / p99 time-to-delivery.

Each scenario writes its raw output under `results/<label>/`.

## One-time setup

```sh
sudo bash setup.sh
```

Installs: Rust (for the current user), Node.js 20, PHP 8.3, Composer, wrk,
Redis, and scaffolds a minimal Laravel + Reverb app under
`~/bench-work/reverb-app`. Also raises file-descriptor limits so the
10k-connection ramp has room to run.

Log out and back in after `setup.sh` so the new `ulimit -n` takes effect.

## Building and starting the two servers

### zatat

```sh
cargo build --release --bin zatat
./target/release/zatat start --config bench/zatat.bench.toml
```

Listens on `127.0.0.1:8080`.

### Reverb

```sh
cd ~/bench-work/reverb-app

cat >> .env <<'EOF'
REVERB_APP_ID=bench
REVERB_APP_KEY=bench-key
REVERB_APP_SECRET=bench-secret
REVERB_HOST=127.0.0.1
REVERB_PORT=8081
REVERB_SCHEME=http
EOF

php artisan reverb:start --host=127.0.0.1 --port=8081
```

## Running the scenarios

```sh
bash run.sh 127.0.0.1 8080 zatat
bash run.sh 127.0.0.1 8081 reverb
```

Outputs land in `results/zatat/` and `results/reverb/`.
