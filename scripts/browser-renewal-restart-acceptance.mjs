#!/usr/bin/env node

import assert from "node:assert/strict";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";
import {
  artifactContains,
  artifactTreeSha256,
  hhhsPinFromLock,
  sourceProvenance,
} from "./browser-provenance.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const dist = resolve(process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/release-web"));
const reportPath = resolve(
  process.env.WALKIE_RENEWAL_REPORT ??
    join(repository, "output/playwright/browser-renewal-restart.json"),
);
const runStartedAt = new Date();
rmSync(reportPath, { force: true });
const source = sourceProvenance(repository);
const hhhsPin = hhhsPinFromLock(repository);
const artifactSha256 = artifactTreeSha256(dist);
const artifactProfile = process.env.WALKIE_ARTIFACT_PROFILE ?? "unspecified";
assert.equal(
  artifactProfile,
  "acceptance-instrumented",
  "renewal crash-cut evidence requires an explicitly instrumented acceptance artifact",
);
assert.ok(
  artifactContains(dist, "renewalCut"),
  "instrumented artifact does not contain the acceptance-only renewal cut hook",
);
const timeoutMs = Number(process.env.WALKIE_ACCEPTANCE_TIMEOUT_MS ?? 90_000);
const targetKey = 36;

for (const required of ["index.html", "sw.js", "all-around-keyboard.esm.min.js"]) {
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

function attachDiagnostics(page, label, diagnostics, renewalTraces) {
  page.on("console", (message) => {
    const entry = { label, type: message.type(), text: message.text() };
    diagnostics.push(entry);
    const prefix = "[session_renewal_trace] ";
    if (!entry.text.startsWith(prefix)) return;
    try {
      renewalTraces.push({ label, ...JSON.parse(entry.text.slice(prefix.length)) });
    } catch (error) {
      renewalTraces.push({ label, parseError: String(error), raw: entry.text });
    }
  });
  page.on("pageerror", (error) => {
    diagnostics.push({ label, type: "pageerror", text: String(error), stack: error.stack ?? null });
  });
}

async function openPeer(context, label, url, diagnostics, renewalTraces) {
  const page = await context.newPage();
  attachDiagnostics(page, label, diagnostics, renewalTraces);
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  return page;
}

async function waitUntil(predicate, description) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = predicate();
    if (value) return value;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
  }
  throw new Error(`timed out waiting for ${description}`);
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

async function togglePitch(page) {
  await page
    .locator("all-around-keyboard")
    .locator(`[data-key-index="${targetKey}"]`)
    .click();
}

const diagnostics = [];
const renewalTraces = [];
const room = `renewal-${alphabetic(Date.now())}-${alphabetic(process.pid)}`;
const { server, origin } = await serveRelease();
let browser;

try {
  const launchOptions = { headless: process.env.WALKIE_HEADED !== "1" };
  if (process.env.WALKIE_BROWSER_EXECUTABLE) {
    launchOptions.executablePath = process.env.WALKIE_BROWSER_EXECUTABLE;
  }
  browser = await chromium.launch(launchOptions);
  const chromiumProvenance = {
    executable: process.env.WALKIE_BROWSER_EXECUTABLE ?? chromium.executablePath(),
    version: browser.version(),
  };
  const cutContext = await browser.newContext({ serviceWorkers: "allow" });
  const peerContext = await browser.newContext({ serviceWorkers: "allow" });
  const cutUrl = `${origin}/?sessionTrace=1&renewalCut=1#${room}`;
  const normalUrl = `${origin}/?sessionTrace=1#${room}`;

  let cutPage = await openPeer(cutContext, "cut", cutUrl, diagnostics, renewalTraces);
  const peerPage = await openPeer(peerContext, "peer", normalUrl, diagnostics, renewalTraces);

  const cut = await waitUntil(
    () =>
      renewalTraces.find(
        (trace) => trace.label === "cut" && trace.stage === "FloorPersistedBeforeEgressCut",
      ),
    "a durable renewal floor followed by the injected pre-egress cut",
  );
  assert.ok(cut.epoch > 0, "the persisted cut floor has no positive epoch");
  assert.equal(cut.floor_epoch, cut.epoch, "the cut did not report the persisted floor");
  assert.equal(await overlayCount(cutPage, targetKey), 0, "cut exposed speculative pitch state");
  assert.equal(await overlayCount(peerPage, targetKey), 0, "peer exposed speculative pitch state");

  // Closing the only page in this context terminates its dedicated worker. The
  // same context is retained so the restarted page must recover the floor from
  // the real origin-scoped IndexedDB database.
  await cutPage.close();
  cutPage = await openPeer(cutContext, "reopened", normalUrl, diagnostics, renewalTraces);

  const recovered = await waitUntil(
    () =>
      renewalTraces.find(
        (trace) =>
          trace.label === "reopened" &&
          trace.stage === "RecoveredFloor" &&
          trace.floor_epoch === cut.epoch,
      ),
    "the restarted worker to recover the exact persisted renewal floor",
  );
  const higherOffer = await waitUntil(
    () =>
      renewalTraces.find(
        (trace) =>
          trace.label === "reopened" &&
          trace.stage === "OfferStarted" &&
          trace.epoch > recovered.floor_epoch,
      ),
    "a higher-epoch counter-offer after floor recovery",
  );
  const reopenedInstall = await waitUntil(
    () =>
      renewalTraces.find(
        (trace) =>
          trace.label === "reopened" &&
          trace.stage === "SessionInstalled" &&
          trace.epoch === higherOffer.epoch,
      ),
    "the restarted worker to install the higher session",
  );
  const peerInstall = await waitUntil(
    () =>
      renewalTraces.find(
        (trace) =>
          trace.label === "peer" &&
          trace.stage === "SessionInstalled" &&
          trace.epoch === higherOffer.epoch,
      ),
    "the peer to authenticate and install the same higher session",
  );

  const reopenedRegressions = renewalTraces.filter(
    (trace) =>
      trace.label === "reopened" &&
      trace.stage === "SessionInstalled" &&
      trace.epoch <= recovered.floor_epoch,
  );
  assert.deepEqual(reopenedRegressions, [], "restart reopened an old or same renewal epoch");
  assert.equal(await overlayCount(cutPage, targetKey), 0, "restart reopened speculative pitch state");

  // Installing the same higher epoch on both workers already proves that the
  // establishment carrier crossed the peer boundary. Direct-WebRTC status is
  // intentionally a separate acceptance gate; renewal recovery must remain
  // testable when Iroh/WebRTC changes paths immediately after establishment.
  await togglePitch(cutPage);
  await Promise.all([
    waitForOverlay(cutPage, targetKey, true),
    waitForOverlay(peerPage, targetKey, true),
  ]);
  await togglePitch(peerPage);
  await Promise.all([
    waitForOverlay(cutPage, targetKey, false),
    waitForOverlay(peerPage, targetKey, false),
  ]);

  const malformed = renewalTraces.filter((trace) => trace.parseError);
  const pageFailures = diagnostics.filter((entry) => entry.type === "pageerror");
  assert.deepEqual(malformed, [], "malformed renewal trace records");
  assert.deepEqual(pageFailures, [], `uncaught browser errors: ${JSON.stringify(pageFailures)}`);

  const report = {
    schema: "walkie.browser-renewal-restart@2",
    runStartedAt: runStartedAt.toISOString(),
    capturedAt: new Date().toISOString(),
    room,
    releaseDist: relative(repository, dist),
    provenance: {
      ...source,
      hhhsPin,
      artifactSha256,
      artifactProfile,
      chromium: chromiumProvenance,
    },
    traceSchema: "walkie.session-renewal-trace@1",
    injectedCut: cut,
    recoveredFloor: recovered,
    higherOffer,
    reopenedInstall,
    peerInstall,
    staleOfferRefused: renewalTraces.some(
      (trace) => trace.stage === "StaleOfferRefused" && trace.floor_epoch >= cut.epoch,
    ),
    assertions: {
      realIndexedDbTerminateReopen: true,
      persistedFloorNeverRegressed: true,
      oldSpeculativeStateNeverReopened: true,
      higherEpochAuthenticatedByBothPeers: true,
      addRemoveConvergedAfterRecovery: true,
    },
  };
  mkdirSync(dirname(reportPath), { recursive: true });
  writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  assert.ok(
    statSync(reportPath).mtimeMs >= runStartedAt.getTime(),
    "renewal report predates the active run",
  );
  console.log(JSON.stringify(report, null, 2));

  await Promise.all([cutContext.close(), peerContext.close()]);
} catch (error) {
  console.error(JSON.stringify({ failure: String(error), renewalTraces, diagnostics }, null, 2));
  throw error;
} finally {
  if (browser) await browser.close();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
