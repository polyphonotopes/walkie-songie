#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from "node:fs";
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
const keyboardVersion = "1.9.0";
const keyboardSha256 = "bdf2cf76fd1605f5d3923c0ac3b6758f22dbf1d6ead4d5c9f2fdc9aafbcf4a59";

for (const required of ["index.html", "sw.js", "all-around-keyboard.esm.min.js"]) {
  assert.ok(existsSync(join(dist, required)), `missing release artifact: ${join(dist, required)}`);
}
const keyboardArtifact = join(dist, "all-around-keyboard.esm.min.js");
const actualKeyboardSha256 = createHash("sha256")
  .update(readFileSync(keyboardArtifact))
  .digest("hex");
assert.equal(actualKeyboardSha256, keyboardSha256, "unexpected all-around-keyboard artifact");

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
  await page.waitForFunction(
    (expectedVersion) => customElements.get("all-around-keyboard")?.version === expectedVersion,
    keyboardVersion,
    { timeout: timeoutMs },
  );
  const keyboardContract = await page.locator("all-around-keyboard").evaluate((keyboard) => ({
    version: keyboard.constructor.version,
    updateState: typeof keyboard.updateState,
    setOverlay: typeof keyboard.setOverlay,
    setIndicator: typeof keyboard.setIndicator,
  }));
  assert.deepEqual(keyboardContract, {
    version: keyboardVersion,
    updateState: "function",
    setOverlay: "function",
    setIndicator: "function",
  });
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
    window.__walkieKeyboardRenders = [];
    keyboard.onRenderStats = (stats) => {
      window.__walkieKeyboardRenders.push({
        ...stats,
        at: performance.timeOrigin + performance.now(),
      });
    };
    window.__walkieAcceptanceObserver = new MutationObserver((records) => {
      for (const record of records) {
        for (const node of record.addedNodes) {
          if (node instanceof Element && node.classList.contains("toggle-overlay")) {
            window.__walkieAcceptanceEvents.push({
              action: "add",
              key: Number(node.getAttribute("data-key-overlay")),
              at: performance.timeOrigin + performance.now(),
            });
          }
        }
        for (const node of record.removedNodes) {
          if (node instanceof Element && node.classList.contains("toggle-overlay")) {
            window.__walkieAcceptanceEvents.push({
              action: "remove",
              key: Number(node.getAttribute("data-key-overlay")),
              at: performance.timeOrigin + performance.now(),
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
    window.__walkieKeyboardRenders = [];
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

async function waitForOverlayRender(page) {
  await page.waitForFunction(
    () => (window.__walkieKeyboardRenders ?? []).some((stats) => stats.dirtyOverlays > 0),
    undefined,
    { timeout: timeoutMs },
  );
}

async function dispatchPitch(page, note) {
  const started = page.evaluate((pitch) => {
    const keyboard = document.querySelector("all-around-keyboard");
    if (!keyboard) throw new Error("keyboard was not mounted");
    return new Promise((resolvePromise) => {
      const onIntent = (event) => {
        if (event.detail?.type !== "press" || event.detail?.note !== pitch) return;
        keyboard.removeEventListener("keyboardintent", onIntent, true);
        resolvePromise(performance.timeOrigin + event.detail.timeStamp);
      };
      keyboard.addEventListener("keyboardintent", onIntent, true);
    });
  }, note);
  await page
    .locator("all-around-keyboard")
    .locator(`[data-key-index="${targetKey + note}"]`)
    .click();
  return started;
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
  return events[0];
}

async function assertSingleOverlayRender(page, label) {
  const renders = await page.evaluate(() =>
    (window.__walkieKeyboardRenders ?? []).filter((stats) => stats.dirtyOverlays > 0),
  );
  assert.equal(
    renders.length,
    1,
    `${label} expected one keyboard overlay render, got ${JSON.stringify(renders)}`,
  );
  return renders[0];
}

async function operate({ source, sourceLabel, peer, peerLabel, note, present, timings }) {
  const key = 36 + note;
  await Promise.all([resetOverlayEvents(source), resetOverlayEvents(peer)]);
  const started = await dispatchPitch(source, note);
  await Promise.all([waitForOverlay(source, key, present), waitForOverlay(peer, key, present)]);
  await Promise.all([waitForOverlayRender(source), waitForOverlayRender(peer)]);

  // The peer-visible mutation proves durable admission. Leave a short quiet
  // window to catch a redundant local confirmation repaint if one regresses.
  await source.waitForTimeout(250);
  const action = present ? "add" : "remove";
  const [sourceMutation, peerMutation, sourceRender, peerRender] = await Promise.all([
    assertSingleMutation(source, sourceLabel, action, key),
    assertSingleMutation(peer, peerLabel, action, key),
    assertSingleOverlayRender(source, sourceLabel),
    assertSingleOverlayRender(peer, peerLabel),
  ]);
  timings.localProjection.push(sourceMutation.at - started);
  timings.peerProjection.push(peerMutation.at - started);
  timings.localVisible.push(sourceRender.at - started);
  timings.peerVisible.push(peerRender.at - started);
  timings.localProjectionToVisible.push(sourceRender.at - sourceMutation.at);
  timings.peerProjectionToVisible.push(peerRender.at - peerMutation.at);
  timings.localRenderDuration.push(sourceRender.durationMs);
  timings.peerRenderDuration.push(peerRender.durationMs);
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
const timings = {
  localProjection: [],
  peerProjection: [],
  localVisible: [],
  peerVisible: [],
  localProjectionToVisible: [],
  peerProjectionToVisible: [],
  localRenderDuration: [],
  peerRenderDuration: [],
};
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

  for (const [index, note] of [0, 1, 2, 3, 4, 5, 6, 8, 9, 10].entries()) {
    const sourceIsLeft = index % 2 === 0;
    const source = sourceIsLeft ? left : right;
    const peer = sourceIsLeft ? right : left;
    const sourceLabel = sourceIsLeft ? "left" : "right";
    const peerLabel = sourceIsLeft ? "right" : "left";
    await operate({ source, sourceLabel, peer, peerLabel, note, present: true, timings });
    await operate({ source, sourceLabel, peer, peerLabel, note, present: false, timings });
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
    timings,
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
    timings,
  });

  assertNoRepairFailures(diagnostics);
  const report = {
    schema: 1,
    capturedAt: new Date().toISOString(),
    room,
    releaseDist: relative(repository, dist),
    allAroundKeyboard: {
      version: keyboardVersion,
      sha256: keyboardSha256,
    },
    sampleCount: timings.peerVisible.length,
    localProjectionLatencyMs: {
      samples: timings.localProjection,
      p50: percentile(timings.localProjection, 0.5),
      p95: percentile(timings.localProjection, 0.95),
    },
    localVisibleLatencyMs: {
      samples: timings.localVisible,
      p50: percentile(timings.localVisible, 0.5),
      p95: percentile(timings.localVisible, 0.95),
    },
    localProjectionToVisibleMs: {
      samples: timings.localProjectionToVisible,
      p50: percentile(timings.localProjectionToVisible, 0.5),
      p95: percentile(timings.localProjectionToVisible, 0.95),
    },
    localKeyboardRenderDurationMs: {
      samples: timings.localRenderDuration,
      p50: percentile(timings.localRenderDuration, 0.5),
      p95: percentile(timings.localRenderDuration, 0.95),
    },
    peerProjectionLatencyMs: {
      samples: timings.peerProjection,
      p50: percentile(timings.peerProjection, 0.5),
      p95: percentile(timings.peerProjection, 0.95),
    },
    peerVisibleLatencyMs: {
      samples: timings.peerVisible,
      p50: percentile(timings.peerVisible, 0.5),
      p95: percentile(timings.peerVisible, 0.95),
      hardBudgetApplied: false,
    },
    peerProjectionToVisibleMs: {
      samples: timings.peerProjectionToVisible,
      p50: percentile(timings.peerProjectionToVisible, 0.5),
      p95: percentile(timings.peerProjectionToVisible, 0.95),
    },
    peerKeyboardRenderDurationMs: {
      samples: timings.peerRenderDuration,
      p50: percentile(timings.peerRenderDuration, 0.5),
      p95: percentile(timings.peerRenderDuration, 0.95),
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
