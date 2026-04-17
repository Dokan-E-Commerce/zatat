// Generate a wrk Lua script that signs a POST /apps/<id>/events request for
// a running zatat or Reverb server. Write the script to stdout so wrk can
// read it with -s.
//
// Usage:  node gen-wrk-script.mjs <host> <port>

import crypto from "node:crypto";

const HOST = process.argv[2] || "127.0.0.1";
const PORT = Number(process.argv[3] || 8080);

const APP_ID = "bench";
const KEY    = "bench-key";
const SECRET = "bench-secret";
const PATH   = `/apps/${APP_ID}/events`;

const BODY = JSON.stringify({
  name: "bench-event",
  data: JSON.stringify({ t: 1 }),
  channels: ["bench-chat"],
});

const params = {
  auth_key: KEY,
  auth_timestamp: Math.floor(Date.now() / 1000).toString(),
  auth_version: "1.0",
};

const bodyMd5 = crypto.createHash("md5").update(BODY).digest("hex");
const signed = { ...params, body_md5: bodyMd5 };
const sorted = Object.keys(signed).sort();
const qs = sorted.map((k) => `${k}=${signed[k]}`).join("&");
const canon = ["POST", PATH, qs].join("\n");
params.auth_signature = crypto.createHmac("sha256", SECRET).update(canon).digest("hex");

const queryString = Object.entries(params)
  .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
  .join("&");

console.log(`-- wrk script for ${HOST}:${PORT}${PATH}
wrk.method = "POST"
wrk.path   = "${PATH}?${queryString}"
wrk.body   = [[${BODY}]]
wrk.headers["Content-Type"] = "application/json"
`);
