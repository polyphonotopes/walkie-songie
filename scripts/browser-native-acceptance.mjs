#!/usr/bin/env node

import assert from "node:assert/strict";
import { createReadStream, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { dirname, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const dist = resolve(process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/release-web"));
const nativeProbe = resolve(
  process.env.WALKIE_NATIVE_PROBE ?? join(repository, "target/debug/room-v5-native-probe"),
);
const reportPath = resolve(
  process.env.WALKIE_BROWSER_NATIVE_REPORT ??
    join(repository, "output/playwright/browser-native-acceptance.json"),
);
const timeoutMs = Number(process.env.WALKIE_BROWSER_NATIVE_TIMEOUT_MS ?? 90_000);
const targetKey = 36;

for (const required of ["index.html", "sw.js", "all-around-keyboard.esm.min.js"]) {
  assert.ok(existsSync(join(dist, required)), `missing release artifact: ${join(dist, required)}`);
}
assert.ok(existsSync(nativeProbe), `missing native Room-v5 probe: ${nativeProbe}`);

const mimeTypes = new Map([
  [".css", "text/css; charset=utf-8"],
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".onnx", "application/octet-stream"],
  [".svg", "image/svg+xml"],
  [".wasm", "application/wasm"],
  [".webmanifest", "application/manifest+json"],
]);

function serveRelease() {
  const server = createServer((request, response) => {
    let pathname;
    try {
      pathname = decodeURIComponent(new URL(request.url ?? "/", "http://localhost").pathname);
    } catch {
      response.writeHead(400).end("Bad request");
      return;
    }
    const requested = pathname === "/" ? "index.html" : pathname.replace(/^\/+/, "");
    const file = resolve(dist, requested);
    if (file !== dist && !file.startsWith(`${dist}${sep}`)) {
      response.writeHead(403).end("Forbidden");
      return;
    }
    if (!existsSync(file) || !statSync(file).isFile()) {
      response.writeHead(404).end("Not found");
      return;
    }
    response.writeHead(200, {
      "Cache-Control": "no-store",
      "Content-Type": mimeTypes.get(extname(file)) ?? "application/octet-stream",
    });
    if (request.method === "HEAD") response.end();
    else createReadStream(file).pipe(response);
  });
  return new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      assert.ok(address && typeof address === "object");
      resolvePromise({ server, origin: `http://127.0.0.1:${address.port}` });
    });
  });
}

function alphabetic(value) {
  let number = BigInt(value);
  let result = "";
  do {
    result = String.fromCharCode(Number(number % 26n) + 97) + result;
    number /= 26n;
  } while (number > 0n);
  return result;
}

class NativePeer {
  constructor(binary, room) {
    this.events = [];
    this.waiters = [];
    this.stderr = "";
    this.closed = false;
    this.child = spawn(binary, [room], {
      cwd: repository,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stderr.setEncoding("utf8");
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk;
    });
    this.child.stdout.setEncoding("utf8");
    let pending = "";
    this.child.stdout.on("data", (chunk) => {
      pending += chunk;
      while (pending.includes("\n")) {
        const newline = pending.indexOf("\n");
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line.trim()) continue;
        try {
          this.events.push(JSON.parse(line));
          this.flushWaiters();
        } catch (error) {
          this.failWaiters(new Error(`native probe emitted non-JSON: ${line}`, { cause: error }));
        }
      }
    });
    this.child.on("error", (error) => this.failWaiters(error));
    this.child.on("exit", (code, signal) => {
      this.closed = true;
      this.failWaiters(
        new Error(
          `native probe exited (code=${code}, signal=${signal})${this.stderr ? `\n${this.stderr}` : ""}`,
        ),
      );
    });
  }

  flushWaiters() {
    for (const waiter of [...this.waiters]) {
      const found = this.events
        .slice(waiter.after)
        .find((event) => waiter.predicate(event));
      if (!found) continue;
      this.waiters.splice(this.waiters.indexOf(waiter), 1);
      clearTimeout(waiter.timer);
      waiter.resolve(found);
    }
  }

  failWaiters(error) {
    for (const waiter of this.waiters.splice(0)) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
  }

  waitFor(predicate, description, after = 0) {
    const found = this.events.slice(after).find(predicate);
    if (found) return Promise.resolve(found);
    if (this.closed) return Promise.reject(new Error("native probe is closed"));
    return new Promise((resolvePromise, reject) => {
      const waiter = {
        after,
        predicate,
        resolve: resolvePromise,
        reject,
        timer: setTimeout(() => {
          this.waiters.splice(this.waiters.indexOf(waiter), 1);
          reject(
            new Error(
              `timed out waiting for native ${description}\n${JSON.stringify(this.events, null, 2)}\n${this.stderr}`,
            ),
          );
        }, timeoutMs),
      };
      this.waiters.push(waiter);
    });
  }

  send(command) {
    assert.equal(this.closed, false, "native probe is closed");
    this.child.stdin.write(`${JSON.stringify(command)}\n`);
  }

  async command(command, predicate, description) {
    const after = this.events.length;
    this.send(command);
    return this.waitFor(predicate, description, after);
  }

  status() {
    return this.command({ cmd: "status" }, (event) => event.event === "status", "status");
  }

  async shutdown() {
    if (this.closed) return;
    const after = this.events.length;
    this.send({ cmd: "shutdown" });
    await this.waitFor((event) => event.event === "shutdown", "shutdown", after).catch(() => {});
    if (!this.closed) this.child.kill("SIGTERM");
  }
}

async function waitForNativeStatus(native, predicate, description) {
  const started = Date.now();
  let last;
  while (Date.now() - started < timeoutMs) {
    last = await native.status();
    if (predicate(last)) return last;
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 200));
  }
  throw new Error(`timed out waiting for ${description}: ${JSON.stringify(last)}`);
}

async function dispatchPitch(page, note) {
  await page
    .locator("all-around-keyboard")
    .locator(`[data-key-index="${targetKey + note}"]`)
    .click();
}

async function waitForOverlay(page, note, present = true) {
  await page.waitForFunction(
    ({ key, expected }) =>
      document.querySelectorAll(
        `all-around-keyboard > .toggle-overlay[data-key-overlay="${key}"]`,
      ).length === expected,
    { key: targetKey + note, expected: present ? 1 : 0 },
    { timeout: timeoutMs },
  );
}

async function waitForPiece(page, emoji, present = true) {
  await page.waitForFunction(
    ({ expectedEmoji, expected }) =>
      [...document.querySelectorAll(".piece-indicator")].some(
        (piece) => piece.getAttribute("data-emoji") === expectedEmoji,
      ) === expected,
    { expectedEmoji: emoji, expected: present },
    { timeout: timeoutMs },
  );
}

const room = `native-${alphabetic(Date.now())}-${alphabetic(process.pid)}`;
const native = new NativePeer(nativeProbe, room);
const diagnostics = [];
const { server, origin } = await serveRelease();
let browser;
let context;

try {
  const ready = await native.waitFor((event) => event.event === "ready", "readiness");
  const launchOptions = { headless: process.env.WALKIE_HEADED !== "1" };
  if (process.env.WALKIE_BROWSER_EXECUTABLE) {
    launchOptions.executablePath = process.env.WALKIE_BROWSER_EXECUTABLE;
  }
  browser = await chromium.launch(launchOptions);
  context = await browser.newContext({ serviceWorkers: "allow" });
  const page = await context.newPage();
  page.on("console", (message) => diagnostics.push({ type: message.type(), text: message.text() }));
  page.on("pageerror", (error) => diagnostics.push({ type: "pageerror", text: String(error) }));
  await page.goto(`${origin}/?peer=browser-native-v5#${room}`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  await native.waitFor((event) => event.event === "peer_up", "browser peer");
  await page.waitForFunction(
    () => (document.querySelector(".peer-status")?.textContent ?? "").includes("synchronized"),
    undefined,
    { timeout: timeoutMs },
  );

  await native.command(
    { cmd: "partition", enabled: true },
    (event) => event.event === "partition" && event.enabled === true,
    "partition start",
  );

  await dispatchPitch(page, 2);
  await waitForOverlay(page, 2);
  await page.locator(".lock-button").click();
  await page.waitForFunction(
    () => document.querySelector(".lock-button")?.textContent?.includes("🔒"),
    undefined,
    { timeout: timeoutMs },
  );

  await native.command(
    { cmd: "music", degree: 5, broadcast: false },
    (event) => event.event === "committed" && event.lane === "music",
    "offline music commit",
  );
  await native.command(
    { cmd: "piece", emoji: "🧪", degree: 7, broadcast: false },
    (event) => event.event === "committed" && event.lane === "extension",
    "offline extension commit",
  );

  const divergent = await native.status();
  assert.ok(divergent.degrees.includes(5), "native retained its offline music edit");
  assert.equal(divergent.degrees.includes(2), false, "native did not receive browser music while partitioned");
  assert.ok(divergent.pieces.includes("🧪"), "native retained its offline extension edit");
  assert.equal(divergent.pieces_locked, false, "native did not receive browser extension while partitioned");
  assert.equal(
    await page.locator(`all-around-keyboard > .toggle-overlay[data-key-overlay="${targetKey + 5}"]`).count(),
    0,
    "browser did not receive native music while partitioned",
  );
  assert.equal(await page.locator('.piece-indicator[data-emoji="🧪"]').count(), 0);

  await native.command(
    { cmd: "partition", enabled: false },
    (event) => event.event === "partition" && event.enabled === false,
    "partition heal",
  );
  const repairStart = native.events.length;
  native.send({ cmd: "repair" });
  const [musicRepair, extensionRepair] = await Promise.all([
    native.waitFor(
      (event) => event.event === "repair" && event.role === "initiator" && event.lane === "music" && event.ok,
      "complete music repair",
      repairStart,
    ),
    native.waitFor(
      (event) => event.event === "repair" && event.role === "initiator" && event.lane === "extension" && event.ok,
      "complete extension repair",
      repairStart,
    ),
  ]);

  await Promise.all([waitForOverlay(page, 5), waitForPiece(page, "🧪")]);
  const converged = await waitForNativeStatus(
    native,
    (status) => status.degrees.includes(2) && status.pieces_locked,
    "browser-authored music and extension state at native",
  );
  assert.ok(converged.degrees.includes(5));
  assert.ok(converged.pieces.includes("🧪"));
  assert.match(converged.music_root, /^[0-9a-f]{64}$/);
  assert.match(converged.extension_root, /^[0-9a-f]{64}$/);
  assert.ok(converged.music_frames > 0, "music repair exchanged frames");
  assert.ok(converged.extension_frames > 0, "extension repair exchanged frames");
  assert.deepEqual(converged.violations, [], "repair never carried a foreign-lane record");

  const pageErrors = diagnostics.filter((entry) => entry.type === "pageerror");
  assert.deepEqual(pageErrors, [], `browser page errors: ${JSON.stringify(pageErrors)}`);

  const report = {
    schema: "walkie.browser-native-acceptance@1",
    captured_at: new Date().toISOString(),
    room,
    release_dist: relative(repository, dist),
    native_endpoint: ready.endpoint,
    partition: {
      browser_music: 2,
      browser_extension: "pieces_locked",
      native_music: 5,
      native_extension: "🧪",
    },
    repair: { music: musicRepair, extension: extensionRepair },
    converged: {
      degrees: converged.degrees,
      pieces: converged.pieces,
      pieces_locked: converged.pieces_locked,
      music_root: converged.music_root,
      extension_root: converged.extension_root,
      music_frames: converged.music_frames,
      extension_frames: converged.extension_frames,
      lane_violations: converged.violations.length,
    },
  };
  mkdirSync(dirname(reportPath), { recursive: true });
  writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
} finally {
  if (context) await context.close();
  if (browser) await browser.close();
  await native.shutdown();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
