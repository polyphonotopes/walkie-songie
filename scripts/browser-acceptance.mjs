#!/usr/bin/env node

import assert from "node:assert/strict";
import { createReadStream, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const dist = resolve(process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/release-web"));
const reportPath = resolve(
  process.env.WALKIE_ACCEPTANCE_REPORT ?? join(repository, "output/playwright/browser-acceptance.json"),
);
const timeoutMs = Number(process.env.WALKIE_ACCEPTANCE_TIMEOUT_MS ?? 90_000);
const headed = process.env.WALKIE_HEADED === "1";
const targetKey = 36;

for (const required of ["index.html", "sw.js"]) {
  assert.ok(existsSync(join(dist, required)), `missing release artifact: ${join(dist, required)}`);
}

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
    if (request.method === "HEAD") {
      response.end();
    } else {
      createReadStream(file).pipe(response);
    }
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

function percentile(samples, fraction) {
  assert.ok(samples.length > 0);
  const sorted = [...samples].sort((left, right) => left - right);
  return sorted[Math.ceil(fraction * sorted.length) - 1];
}

function attachDiagnostics(page, label, diagnostics) {
  page.on("console", (message) => {
    const entry = { label, type: message.type(), text: message.text() };
    diagnostics.push(entry);
  });
  page.on("pageerror", (error) => {
    diagnostics.push({ label, type: "pageerror", text: String(error) });
  });
  page.on("response", (response) => {
    const url = new URL(response.url());
    if (url.hostname === "127.0.0.1" && response.status() >= 400) {
      diagnostics.push({
        label,
        type: "release-response-error",
        text: `${response.status()} ${response.url()}`,
      });
    }
  });
  page.on("requestfailed", (request) => {
    const url = new URL(request.url());
    if (url.hostname === "127.0.0.1") {
      diagnostics.push({
        label,
        type: "release-request-failed",
        text: `${request.failure()?.errorText ?? "failed"} ${request.url()}`,
      });
    }
  });
}

async function openPeer(context, label, url, diagnostics) {
  const page = await context.newPage();
  attachDiagnostics(page, label, diagnostics);
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  return page;
}

async function waitForSynchronized(page) {
  await page.waitForFunction(
    () => {
      const status = document.querySelector(".peer-status")?.textContent ?? "";
      return status.includes("Direct") && status.includes("synchronized");
    },
    undefined,
    { timeout: timeoutMs },
  );
}

async function installOverlayObserver(page) {
  await page.evaluate(() => {
    const keyboard = document.querySelector("all-around-keyboard");
    if (!keyboard) throw new Error("keyboard was not mounted");
    window.__walkieAcceptanceObserver?.disconnect();
    window.__walkieAcceptanceEvents = [];
    window.__walkieAcceptanceObserver = new MutationObserver((records) => {
      for (const record of records) {
        for (const node of record.addedNodes) {
          if (node instanceof Element && node.classList.contains("toggle-overlay")) {
            window.__walkieAcceptanceEvents.push({
              action: "add",
              key: Number(node.getAttribute("data-key-overlay")),
              at: Date.now(),
            });
          }
        }
        for (const node of record.removedNodes) {
          if (node instanceof Element && node.classList.contains("toggle-overlay")) {
            window.__walkieAcceptanceEvents.push({
              action: "remove",
              key: Number(node.getAttribute("data-key-overlay")),
              at: Date.now(),
            });
          }
        }
      }
    });
    window.__walkieAcceptanceObserver.observe(keyboard, { childList: true });
  });
}

async function resetOverlayEvents(page) {
  await page.evaluate(() => {
    window.__walkieAcceptanceEvents = [];
  });
}

async function overlayCount(page, key) {
  return page.locator(`all-around-keyboard > .toggle-overlay[data-key-overlay="${key}"]`).count();
}

async function waitForOverlay(page, key, present) {
  await page.waitForFunction(
    ({ target, expected }) =>
      document.querySelectorAll(
        `all-around-keyboard > .toggle-overlay[data-key-overlay="${target}"]`,
      ).length === expected,
    { target: key, expected: present ? 1 : 0 },
    { timeout: timeoutMs },
  );
}

async function dispatchPitch(page, note) {
  return page.evaluate((pitch) => {
    const keyboard = document.querySelector("all-around-keyboard");
    if (!keyboard) throw new Error("keyboard was not mounted");
    const started = Date.now();
    keyboard.dispatchEvent(
      new CustomEvent("keyclick", {
        bubbles: true,
        composed: true,
        detail: { index: 36 + pitch, note: pitch },
      }),
    );
    return started;
  }, note);
}

async function assertSingleMutation(page, label, action, key) {
  const events = await page.evaluate(
    ({ expectedAction, target }) =>
      (window.__walkieAcceptanceEvents ?? []).filter(
        (event) => event.action === expectedAction && event.key === target,
      ),
    { expectedAction: action, target: key },
  );
  assert.equal(
    events.length,
    1,
    `${label} expected one ${action} mutation for key ${key}, got ${JSON.stringify(events)}`,
  );
}

async function operate({ source, sourceLabel, peer, peerLabel, note, present, latencies }) {
  const key = 36 + note;
  await Promise.all([resetOverlayEvents(source), resetOverlayEvents(peer)]);
  const started = await dispatchPitch(source, note);
  await Promise.all([waitForOverlay(source, key, present), waitForOverlay(peer, key, present)]);
  const reachedPeer = await peer.evaluate(() => Date.now());
  latencies.push(reachedPeer - started);

  // The peer-visible mutation proves durable admission. Leave a short quiet
  // window to catch a redundant local confirmation repaint if one regresses.
  await source.waitForTimeout(250);
  const action = present ? "add" : "remove";
  await Promise.all([
    assertSingleMutation(source, sourceLabel, action, key),
    assertSingleMutation(peer, peerLabel, action, key),
  ]);
  assert.equal(await overlayCount(source, key), present ? 1 : 0);
  assert.equal(await overlayCount(peer, key), present ? 1 : 0);
}

function assertNoRepairFailures(diagnostics) {
  const repairFailure = diagnostics.filter(({ text }) =>
    /replica_repair.*(failed|failure|timed?\s*out|timeout|connection\s+lost)/i.test(text),
  );
  assert.deepEqual(repairFailure, [], `repair failures observed: ${JSON.stringify(repairFailure)}`);
  const pageFailures = diagnostics.filter(({ type }) => type === "pageerror");
  assert.deepEqual(pageFailures, [], `uncaught browser errors: ${JSON.stringify(pageFailures)}`);
  const releaseFailures = diagnostics.filter(({ type }) => type.startsWith("release-"));
  assert.deepEqual(
    releaseFailures,
    [],
    `missing or failed release assets: ${JSON.stringify(releaseFailures)}`,
  );
}

const diagnostics = [];
const latencies = [];
const room = `audit-${alphabetic(Date.now())}-${alphabetic(process.pid)}`;
const { server, origin } = await serveRelease();
let browser;

try {
  const launchOptions = { headless: !headed };
  if (process.env.WALKIE_BROWSER_EXECUTABLE) {
    launchOptions.executablePath = process.env.WALKIE_BROWSER_EXECUTABLE;
  }
  browser = await chromium.launch(launchOptions);
  const leftContext = await browser.newContext({ serviceWorkers: "allow" });
  const rightContext = await browser.newContext({ serviceWorkers: "allow" });
  const url = `${origin}/#${room}`;

  let left = await openPeer(leftContext, "left", url, diagnostics);
  let right = await openPeer(rightContext, "right", url, diagnostics);
  await Promise.all([waitForSynchronized(left), waitForSynchronized(right)]);
  await Promise.all([installOverlayObserver(left), installOverlayObserver(right)]);

  for (const [index, note] of [0, 2, 4, 5].entries()) {
    const sourceIsLeft = index % 2 === 0;
    const source = sourceIsLeft ? left : right;
    const peer = sourceIsLeft ? right : left;
    const sourceLabel = sourceIsLeft ? "left" : "right";
    const peerLabel = sourceIsLeft ? "right" : "left";
    await operate({ source, sourceLabel, peer, peerLabel, note, present: true, latencies });
    await operate({ source, sourceLabel, peer, peerLabel, note, present: false, latencies });
  }

  // Leave one durable fact present, remove its peer, and prove that the
  // remaining browser reconstructs it from IndexedDB before a peer can repair.
  await operate({
    source: left,
    sourceLabel: "left",
    peer: right,
    peerLabel: "right",
    note: 7,
    present: true,
    latencies,
  });
  await left.close();
  await right.reload({ waitUntil: "domcontentloaded" });
  await right.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  await waitForOverlay(right, targetKey + 7, true);

  // Reopen the other independent profile, prove its own durable reconstruction,
  // then wait for the normal carrier to report direct synchronized repair.
  const reconnectStarted = Date.now();
  left = await openPeer(leftContext, "left-reopened", url, diagnostics);
  await waitForOverlay(left, targetKey + 7, true);
  await Promise.all([waitForSynchronized(left), waitForSynchronized(right)]);
  const reconnectMs = Date.now() - reconnectStarted;
  await Promise.all([installOverlayObserver(left), installOverlayObserver(right)]);
  await operate({
    source: right,
    sourceLabel: "right",
    peer: left,
    peerLabel: "left-reopened",
    note: 7,
    present: false,
    latencies,
  });

  assertNoRepairFailures(diagnostics);
  const report = {
    schema: 1,
    capturedAt: new Date().toISOString(),
    room,
    releaseDist: relative(repository, dist),
    sampleCount: latencies.length,
    peerVisibleLatencyMs: {
      samples: latencies,
      p50: percentile(latencies, 0.5),
      p95: percentile(latencies, 0.95),
      hardBudgetApplied: false,
    },
    reconnectMs,
  };
  mkdirSync(dirname(reportPath), { recursive: true });
  writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));

  await Promise.all([leftContext.close(), rightContext.close()]);
} finally {
  if (browser) await browser.close();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
