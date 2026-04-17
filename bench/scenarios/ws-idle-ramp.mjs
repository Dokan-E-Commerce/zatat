// Ramp up idle WebSocket connections against a Pusher-protocol server and
// report how many complete the handshake before the server starts rejecting
// or before the ramp target is hit.
//
// Usage:  node ws-idle-ramp.mjs <host> <port> <target> [batch_size] [delay_ms]

import WebSocket from "ws";

const [host, portArg, targetArg, batchArg, delayArg] = process.argv.slice(2);
const HOST   = host || "127.0.0.1";
const PORT   = Number(portArg   || 8080);
const TARGET = Number(targetArg || 10000);
const BATCH  = Number(batchArg  || 100);
const DELAY  = Number(delayArg  || 20);
const URL    = `ws://${HOST}:${PORT}/app/bench-key`;

let opened = 0;
let failed = 0;
let established = 0;
const sockets = [];
const started = Date.now();

function spawn() {
  const ws = new WebSocket(URL, { perMessageDeflate: false });
  sockets.push(ws);
  ws.on("open", () => { opened += 1; });
  ws.on("message", (buf) => {
    const text = buf.toString("utf8");
    if (text.includes("connection_established")) established += 1;
  });
  ws.on("error", () => { failed += 1; });
}

async function ramp() {
  for (let i = 0; i < TARGET; i += BATCH) {
    for (let j = 0; j < BATCH && (i + j) < TARGET; j++) spawn();
    await new Promise((r) => setTimeout(r, DELAY));
  }
  await new Promise((r) => setTimeout(r, 3000));

  const elapsed = (Date.now() - started) / 1000;
  console.log(JSON.stringify({
    target: TARGET,
    opened,
    established,
    failed,
    elapsed_seconds: Number(elapsed.toFixed(2)),
    conns_per_second: Number((established / elapsed).toFixed(0)),
  }, null, 2));

  sockets.forEach((s) => s.terminate());
  process.exit(0);
}

ramp();
