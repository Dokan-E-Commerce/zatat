// Fan-out latency: subscribe N clients to one channel, POST an event, and
// measure how long it takes for every subscriber to receive it.
//
// Usage:  node broadcast-fanout.mjs <host> <port> <subscribers>

import WebSocket from "ws";
import crypto from "node:crypto";

const [host, portArg, subsArg] = process.argv.slice(2);
const HOST = host || "127.0.0.1";
const PORT = Number(portArg || 8080);
const SUBS = Number(subsArg || 1000);

const APP_ID = "bench";
const KEY    = "bench-key";
const SECRET = "bench-secret";
const CHANNEL = "bench-fanout";
const WS_URL = `ws://${HOST}:${PORT}/app/${KEY}`;

function subscribe() {
  return new Promise((resolve) => {
    const ws = new WebSocket(WS_URL, { perMessageDeflate: false });
    let ready = false;
    ws.on("message", (buf) => {
      const txt = buf.toString("utf8");
      if (!ready && txt.includes("connection_established")) {
        ws.send(JSON.stringify({ event: "pusher:subscribe", data: { channel: CHANNEL } }));
        ready = true;
        resolve(ws);
      }
    });
    ws.on("error", () => resolve(null));
  });
}

function signedPublishUrl(bodyStr) {
  const path = `/apps/${APP_ID}/events`;
  const params = {
    auth_key: KEY,
    auth_timestamp: Math.floor(Date.now() / 1000).toString(),
    auth_version: "1.0",
    body_md5: crypto.createHash("md5").update(bodyStr).digest("hex"),
  };
  const sorted = Object.keys(params).sort().map((k) => `${k}=${params[k]}`).join("&");
  const canon = ["POST", path, sorted].join("\n");
  const sig = crypto.createHmac("sha256", SECRET).update(canon).digest("hex");
  const qs = Object.entries({ ...params, auth_signature: sig })
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join("&");
  return `http://${HOST}:${PORT}${path}?${qs}`;
}

async function main() {
  const pending = [];
  for (let i = 0; i < SUBS; i++) {
    pending.push(subscribe());
    if (i % 200 === 0) await new Promise((r) => setTimeout(r, 50));
  }
  const sockets = (await Promise.all(pending)).filter(Boolean);
  await new Promise((r) => setTimeout(r, 1500));

  const deliveries = sockets.map(
    (ws) => new Promise((resolve) => {
      ws.on("message", (buf) => {
        const txt = buf.toString("utf8");
        if (txt.includes('"event":"bench-event"')) resolve(Date.now());
      });
    }),
  );

  const body = JSON.stringify({
    name: "bench-event",
    data: JSON.stringify({ t: Date.now() }),
    channels: [CHANNEL],
  });
  const t0 = Date.now();
  const res = await fetch(signedPublishUrl(body), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body,
  });
  if (!res.ok) {
    console.error("publish failed", res.status, await res.text());
    process.exit(1);
  }

  const times = await Promise.all(deliveries.map(
    (p) => Promise.race([p, new Promise((r) => setTimeout(() => r(null), 10_000))]),
  ));

  const latencies = times.filter((t) => t !== null).map((t) => t - t0).sort((a, b) => a - b);
  const got = latencies.length;
  const p = (q) => latencies[Math.min(latencies.length - 1, Math.floor(latencies.length * q))];

  console.log(JSON.stringify({
    target_subscribers: SUBS,
    connected: sockets.length,
    received: got,
    missing: sockets.length - got,
    p50_ms: p(0.50),
    p95_ms: p(0.95),
    p99_ms: p(0.99),
    max_ms: latencies[latencies.length - 1] ?? null,
  }, null, 2));

  sockets.forEach((ws) => ws.terminate());
  process.exit(0);
}

main();
