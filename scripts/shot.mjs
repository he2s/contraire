// Headless UI check over the Chrome DevTools protocol (no npm deps; Node >= 22).
// Pages with a live SSE stream never go network-idle, so we wait wall-clock time instead.
// usage: node scripts/shot.mjs <url> <out.png> [--dark] [--print] [--width=1280] [--height=900] [--wait=2500] [--eval=<js>]
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const [url, out, ...flags] = process.argv.slice(2);
const opt = k => flags.find(f => f.startsWith(`--${k}=`))?.split("=").slice(1).join("=");
const W = +(opt("width") || 1280), H = +(opt("height") || 900), wait = +(opt("wait") || 2500);
const dark = flags.includes("--dark"), print = flags.includes("--print"), expr = opt("eval");
const chrome = process.env.CHROME || "google-chrome";
const profile = mkdtempSync(join(tmpdir(), "shot-"));
const port = 9300 + Math.floor(Math.random() * 500);
const proc = spawn(chrome, ["--headless=new", "--no-sandbox", "--disable-gpu", "--hide-scrollbars",
  `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, `--window-size=${W},${H}`, "about:blank"], { stdio: "ignore" });
const sleep = ms => new Promise(r => setTimeout(r, ms));
let errors = 0;
try {
  let target;
  for (let i = 0; i < 50 && !target; i++) {
    try { target = (await (await fetch(`http://127.0.0.1:${port}/json`)).json()).find(t => t.type === "page"); } catch { await sleep(100); }
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise(r => ws.onopen = r);
  let id = 0; const pending = new Map();
  ws.onmessage = m => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg.result); pending.delete(msg.id); }
    if (msg.method === "Runtime.exceptionThrown") { errors++; console.error("exception:", msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text); }
    if (msg.method === "Runtime.consoleAPICalled" && /error|warn/.test(msg.params.type)) { errors++; console.error(`console.${msg.params.type}:`, msg.params.args.map(a => a.value ?? a.description).join(" ")); }
    if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") { errors++; console.error("log:", msg.params.entry.text, msg.params.entry.url || ""); }
  };
  const send = (method, params = {}) => new Promise(r => { pending.set(++id, r); ws.send(JSON.stringify({ id, method, params })); });
  await send("Runtime.enable"); await send("Log.enable"); await send("Page.enable");
  if (dark || print) await send("Emulation.setEmulatedMedia", { media: print ? "print" : "", features: dark ? [{ name: "prefers-color-scheme", value: "dark" }] : [] });
  await send("Emulation.setDeviceMetricsOverride", { width: W, height: H, deviceScaleFactor: 1, mobile: W < 600 });
  await send("Page.navigate", { url });
  await sleep(wait);
  if (expr) {
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    console.log(JSON.stringify(r.result?.value ?? r.exceptionDetails?.text));
    await sleep(800);
  }
  const shot = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(out, Buffer.from(shot.data, "base64"));
  ws.close();
} finally {
  proc.kill();
  await sleep(200);
  rmSync(profile, { recursive: true, force: true });
}
process.exit(errors ? 1 : 0);
