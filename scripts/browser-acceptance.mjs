#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  createReadStream,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { availableParallelism, freemem, loadavg, totalmem } from "node:os";
import { dirname, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

import {
  assertStrictLatencyTrial,
  latencyBudgetsMs,
  latencySummary,
  percentile,
} from "./browser-latency-policy.mjs";
import {
  artifactTreeSha256,
  hhhsPinFromLock,
  sourceProvenance,
} from "./browser-provenance.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const dist = resolve(process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/release-web"));
const reportPath = resolve(
  process.env.WALKIE_ACCEPTANCE_REPORT ?? join(repository, "output/playwright/browser-acceptance.json"),
);
const runStartedAt = new Date();
rmSync(reportPath, { force: true });
const source = sourceProvenance(repository);
const hhhsPin = hhhsPinFromLock(repository);
const artifactSha256 = artifactTreeSha256(dist);
const artifactProfile = process.env.WALKIE_ARTIFACT_PROFILE ?? "unspecified";
const timeoutMs = Number(process.env.WALKIE_ACCEPTANCE_TIMEOUT_MS ?? 90_000);
const headed = process.env.WALKIE_HEADED === "1";
const targetKey = 36;
const keyboardVersion = "1.9.0";
const keyboardSha256 = "bdf2cf76fd1605f5d3923c0ac3b6758f22dbf1d6ead4d5c9f2fdc9aafbcf4a59";
const enforceLatency = process.env.WALKIE_ENFORCE_LATENCY !== "0";
const sessionTraceEnabled = process.env.WALKIE_SESSION_TRACE !== "0";
const hostConditionStarted = hostCondition();
// `timeOrigin + performance.now()` is comparable across Window/Worker globals,
// but Chromium independently quantizes each reading. Adjacent stages can
// therefore appear one 0.1 ms tick out of order. Preserve every adjustment in
// the report and refuse anything beyond two ticks.
const traceClockQuantizationToleranceMs = 0.2;
const traceClockQuantizationAdjustments = [];
// Use steady-state samples so first-write allocation, JIT, and carrier startup
// remain visible in the report without making a release gate host-dependent.
// Canonical Room-v5 rendering and the reversible local pressed-feedback facet
// are measured separately: the latter acknowledges input without becoming a
// second SharedPitchSet authority.

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

function hostCondition() {
  const [oneMinute, fiveMinutes, fifteenMinutes] = loadavg();
  return {
    capturedAt: new Date().toISOString(),
    logicalCpuCount: availableParallelism(),
    loadAverage: { oneMinute, fiveMinutes, fifteenMinutes },
    memoryBytes: { free: freemem(), total: totalmem() },
  };
}

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

function workerReadyCount(diagnostics, label) {
  return diagnostics.filter(
    (entry) =>
      entry.label === label &&
      entry.type === "info" &&
      /^\[replica_worker\] ready generation \d+$/.test(entry.text),
  ).length;
}

function attachDiagnostics(page, label, diagnostics, sessionTraces) {
  page.on("console", (message) => {
    const entry = { label, type: message.type(), text: message.text() };
    diagnostics.push(entry);
    const prefix = "[session_trace] ";
    if (entry.text.startsWith(prefix)) {
      try {
        sessionTraces.push({ label, ...JSON.parse(entry.text.slice(prefix.length)) });
      } catch (error) {
        sessionTraces.push({ label, parseError: String(error), raw: entry.text });
      }
    }
  });
  page.on("pageerror", (error) => {
    diagnostics.push({
      label,
      type: "pageerror",
      text: String(error),
      stack: error.stack ?? null,
    });
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

async function openPeer(context, label, url, diagnostics, sessionTraces) {
  const page = await context.newPage();
  attachDiagnostics(page, label, diagnostics, sessionTraces);
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

function assertReplicaWorkersReady(diagnostics, labels) {
  for (const label of labels) {
    assert.ok(
      diagnostics.some(
        (entry) =>
          entry.label === label &&
          entry.type === "info" &&
          /^\[replica_worker\] ready generation \d+$/.test(entry.text),
      ),
      `${label} never reported a ready dedicated Replica worker`,
    );
  }
}

async function installOverlayObserver(page) {
  await page.evaluate(() => {
    const keyboard = document.querySelector("all-around-keyboard");
    if (!keyboard) throw new Error("keyboard was not mounted");
    window.__walkieAcceptanceObserver?.disconnect();
    window.__walkieAcceptanceEvents = [];
    window.__walkieKeyboardRenders = [];
    window.__walkiePressedFeedbackEvents = [];
    if (!window.__walkieOriginalKeyboardUpdateState) {
      const originalUpdateState = keyboard.updateState;
      if (typeof originalUpdateState !== "function") {
        throw new Error("all-around-keyboard updateState is unavailable");
      }
      window.__walkieOriginalKeyboardUpdateState = originalUpdateState;
      keyboard.updateState = function updateStateWithAcceptanceWitness(patch) {
        const result = originalUpdateState.call(this, patch);
        if (patch && Object.hasOwn(patch, "pressedNotes")) {
          window.__walkiePressedFeedbackEvents.push({
            at: performance.timeOrigin + performance.now(),
            notes: [...this.pressedNotes],
          });
        }
        return result;
      };
    }
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
    window.__walkiePressedFeedbackEvents = [];
  });
}

async function waitForPressedFeedback(page, note, started, label) {
  try {
    await page.waitForFunction(
      ({ expectedNote, notBefore }) =>
        (window.__walkiePressedFeedbackEvents ?? []).some(
          (event) => event.at >= notBefore && event.notes.includes(expectedNote),
        ),
      { expectedNote: note, notBefore: started },
      { timeout: timeoutMs },
    );
  } catch (error) {
    const events = await page
      .evaluate(() => window.__walkiePressedFeedbackEvents ?? [])
      .catch(() => []);
    throw new Error(
      `${label} did not acknowledge note ${note} through reversible pressed feedback: ${JSON.stringify(events)}`,
      { cause: error },
    );
  }
  return page.evaluate(
    ({ expectedNote, notBefore }) =>
      (window.__walkiePressedFeedbackEvents ?? []).find(
        (event) => event.at >= notBefore && event.notes.includes(expectedNote),
      ),
    { expectedNote: note, notBefore: started },
  );
}

async function overlayCount(page, key) {
  return page.locator(`all-around-keyboard > .toggle-overlay[data-key-overlay="${key}"]`).count();
}

async function waitForOverlay(page, key, present, label = "browser") {
  try {
    await page.waitForFunction(
      ({ target, expected }) =>
        document.querySelectorAll(
          `all-around-keyboard > .toggle-overlay[data-key-overlay="${target}"]`,
        ).length === expected,
      { target: key, expected: present ? 1 : 0 },
      { timeout: timeoutMs },
    );
  } catch (error) {
    const actual = await overlayCount(page, key).catch(() => -1);
    const active = await page
      .locator(".active-pitches")
      .textContent()
      .catch(() => null);
    throw new Error(
      `${label} expected overlay ${key} count ${present ? 1 : 0}; actual ${actual}; active=${JSON.stringify(active)}`,
      { cause: error },
    );
  }
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

function tokenKey(token) {
  return `${token?.scope ?? "missing"}:${token?.sequence ?? "missing"}`;
}

function correlationKey(correlation) {
  return [
    ...(correlation?.manifest ?? []),
    correlation?.epoch,
    correlation?.seat,
    correlation?.counter,
    ...(correlation?.event ?? []),
  ].join(":");
}

function firstStage(records, label, stage, predicate = () => true) {
  const record = records.find(
    (candidate) => candidate.label === label && candidate.stage === stage && predicate(candidate),
  );
  assert.ok(record, `${label} missing compact-session stage ${stage}`);
  assert.ok(Number.isFinite(record.atMicros), `${label} ${stage} has no comparable timestamp`);
  return record;
}

function pushDuration(target, endMicros, startMicros, label) {
  assert.ok(Number.isFinite(endMicros), `${label} end timestamp is missing`);
  assert.ok(Number.isFinite(startMicros), `${label} start timestamp is missing`);
  const duration = (endMicros - startMicros) / 1_000;
  assert.ok(
    duration >= -traceClockQuantizationToleranceMs,
    `${label} clock regressed by ${duration}ms beyond the ${traceClockQuantizationToleranceMs}ms quantization bound`,
  );
  if (duration < 0) {
    traceClockQuantizationAdjustments.push({ label, rawDurationMs: duration });
  }
  target.push(Math.max(0, duration));
}

function assertCompactSessionPipeline({
  records,
  sourceLabel,
  peerLabel,
  started,
  sourceMutation,
  peerMutation,
  sourceRender,
  peerRender,
  timings,
}) {
  assert.deepEqual(
    records.filter((record) => record.parseError),
    [],
    "malformed compact-session trace records",
  );
  const sent = firstStage(
    records,
    sourceLabel,
    "window_to_worker_sent",
    (record) => record.atMicros >= started * 1_000,
  );
  const intentToken = tokenKey(sent.token);
  const workerQueue = firstStage(
    records,
    sourceLabel,
    "worker_queue_acknowledged",
    (record) => tokenKey(record.token) === intentToken,
  );
  const sourceGate = firstStage(
    records,
    sourceLabel,
    "projection_gate_accepted",
    (record) => tokenKey(record.trace?.token) === intentToken,
  );
  assert.equal(sourceGate.trace.direction, "Local");
  const causalKey = correlationKey(sourceGate.trace.correlation);
  assert.notEqual(causalKey, correlationKey(undefined), "worker did not map token to correlation");
  const sameCorrelation = (record) => correlationKey(record.trace?.correlation) === causalKey;
  const sourceQueueAccepted = firstStage(
    records,
    sourceLabel,
    "window_queue_accepted",
    sameCorrelation,
  );
  const sourceAck = firstStage(records, sourceLabel, "sideband_acknowledged", sameCorrelation);
  const sourceSignal = firstStage(records, sourceLabel, "signal_applied", sameCorrelation);
  const broadcastStart = firstStage(
    records,
    sourceLabel,
    "carrier_broadcast_call_started",
    sameCorrelation,
  );
  const broadcastComplete = firstStage(
    records,
    sourceLabel,
    "carrier_broadcast_call_completed",
    sameCorrelation,
  );
  const peerAck = firstStage(records, peerLabel, "sideband_acknowledged", sameCorrelation);
  const peerQueueAccepted = firstStage(
    records,
    peerLabel,
    "window_queue_accepted",
    sameCorrelation,
  );
  const peerGate = firstStage(records, peerLabel, "projection_gate_accepted", sameCorrelation);
  const peerSignal = firstStage(records, peerLabel, "signal_applied", sameCorrelation);
  assert.equal(peerGate.trace.direction, "Remote");
  assert.equal(peerGate.trace.token, null, "trace token leaked into the peer carrier");

  const source = sourceGate.trace;
  const remote = peerGate.trace;
  for (const [label, trace] of [
    ["source", source],
    ["peer", remote],
  ]) {
    for (const field of [
      "worker_accepted_at_micros",
      "worker_authenticated_at_micros",
      "worker_authorized_at_micros",
      "worker_interpreted_at_micros",
      "worker_projected_at_micros",
    ]) {
      assert.ok(Number.isFinite(trace[field]), `${label} compact trace omitted ${field}`);
    }
  }
  assert.ok(
    Number.isFinite(remote.carrier_received_at_micros),
    "peer compact trace omitted correlated carrier receipt",
  );

  pushDuration(
    timings.windowToWorkerQueue,
    source.worker_accepted_at_micros,
    sent.atMicros,
    "window to worker queue",
  );
  pushDuration(
    timings.workerQueueAck,
    workerQueue.atMicros,
    sent.atMicros,
    "window-to-worker acknowledged sideband",
  );
  pushDuration(
    timings.localAuthenticate,
    source.worker_authenticated_at_micros,
    source.worker_accepted_at_micros,
    "local authenticate",
  );
  pushDuration(
    timings.localInterpret,
    source.worker_interpreted_at_micros,
    source.worker_authenticated_at_micros,
    "local typed interpretation",
  );
  pushDuration(
    timings.localAuthorize,
    source.worker_authorized_at_micros,
    source.worker_interpreted_at_micros,
    "local authorize interpreted event",
  );
  pushDuration(
    timings.localProject,
    source.worker_projected_at_micros,
    source.worker_authorized_at_micros,
    "local kernel and projection",
  );
  pushDuration(
    timings.localWorkerToWindowQueue,
    sourceQueueAccepted.atMicros,
    source.worker_projected_at_micros,
    "local worker-to-window queue",
  );
  pushDuration(
    timings.localSidebandAck,
    sourceAck.atMicros,
    sourceQueueAccepted.atMicros,
    "local sideband ACK",
  );
  pushDuration(
    timings.localGate,
    sourceGate.atMicros,
    sourceQueueAccepted.atMicros,
    "local projection gate",
  );
  pushDuration(
    timings.localSignal,
    sourceSignal.atMicros,
    sourceGate.atMicros,
    "local signal application",
  );
  pushDuration(
    timings.carrierBroadcastCall,
    broadcastComplete.atMicros,
    broadcastStart.atMicros,
    "carrier broadcast call",
  );
  pushDuration(
    timings.carrierCallStartToRemoteReceipt,
    remote.carrier_received_at_micros,
    broadcastStart.atMicros,
    "carrier broadcast-call start to remote receipt",
  );
  pushDuration(
    timings.peerWindowToWorker,
    remote.worker_accepted_at_micros,
    remote.carrier_received_at_micros,
    "peer window-to-worker",
  );
  pushDuration(
    timings.peerAuthenticate,
    remote.worker_authenticated_at_micros,
    remote.worker_accepted_at_micros,
    "peer authenticate",
  );
  pushDuration(
    timings.peerInterpret,
    remote.worker_interpreted_at_micros,
    remote.worker_authenticated_at_micros,
    "peer typed interpretation",
  );
  pushDuration(
    timings.peerAuthorize,
    remote.worker_authorized_at_micros,
    remote.worker_interpreted_at_micros,
    "peer authorize interpreted event",
  );
  pushDuration(
    timings.peerProject,
    remote.worker_projected_at_micros,
    remote.worker_authorized_at_micros,
    "peer kernel and projection",
  );
  pushDuration(
    timings.remoteCarrierReceiptToWorkerProjection,
    remote.worker_projected_at_micros,
    remote.carrier_received_at_micros,
    "remote carrier receipt to worker projection",
  );
  pushDuration(
    timings.peerWorkerToWindowQueue,
    peerQueueAccepted.atMicros,
    remote.worker_projected_at_micros,
    "peer worker-to-window queue",
  );
  pushDuration(
    timings.peerSidebandAck,
    peerAck.atMicros,
    peerQueueAccepted.atMicros,
    "peer sideband ACK",
  );
  pushDuration(
    timings.peerGate,
    peerGate.atMicros,
    peerQueueAccepted.atMicros,
    "peer projection gate",
  );
  pushDuration(
    timings.peerSignal,
    peerSignal.atMicros,
    peerGate.atMicros,
    "peer signal application",
  );
  pushDuration(
    timings.localSignalToDom,
    sourceMutation.at * 1_000,
    sourceSignal.atMicros,
    "local signal-to-DOM",
  );
  pushDuration(
    timings.peerSignalToDom,
    peerMutation.at * 1_000,
    peerSignal.atMicros,
    "peer signal-to-DOM",
  );
  pushDuration(
    timings.localDomToRender,
    sourceRender.at * 1_000,
    sourceMutation.at * 1_000,
    "local DOM-to-render",
  );
  pushDuration(
    timings.peerDomToRender,
    peerRender.at * 1_000,
    peerMutation.at * 1_000,
    "peer DOM-to-render",
  );
  timings.compactSessionProofs += 1;
}

async function operate({
  source,
  sourceLabel,
  peer,
  peerLabel,
  note,
  present,
  timings,
  sessionTraces,
}) {
  const key = 36 + note;
  const traceOffset = sessionTraces.length;
  await Promise.all([resetOverlayEvents(source), resetOverlayEvents(peer)]);
  const started = await dispatchPitch(source, note);
  const pressedFeedback = await waitForPressedFeedback(source, note, started, sourceLabel);
  await Promise.all([
    waitForOverlay(source, key, present, sourceLabel),
    waitForOverlay(peer, key, present, peerLabel),
  ]);
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
  timings.localDomMutation.push(sourceMutation.at - started);
  timings.localPressedFeedback.push(pressedFeedback.at - started);
  timings.peerDomMutation.push(peerMutation.at - started);
  timings.localVisible.push(sourceRender.at - started);
  timings.peerVisible.push(peerRender.at - started);
  timings.localDomToVisible.push(sourceRender.at - sourceMutation.at);
  timings.peerDomToVisible.push(peerRender.at - peerMutation.at);
  timings.localRenderDuration.push(sourceRender.durationMs);
  timings.peerRenderDuration.push(peerRender.durationMs);
  if (sessionTraceEnabled) {
    assertCompactSessionPipeline({
      records: sessionTraces.slice(traceOffset),
      sourceLabel,
      peerLabel,
      started,
      sourceMutation,
      peerMutation,
      sourceRender,
      peerRender,
      timings,
    });
  }
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
const sessionTraces = [];
const timings = {
  localPressedFeedback: [],
  localDomMutation: [],
  peerDomMutation: [],
  localVisible: [],
  peerVisible: [],
  localDomToVisible: [],
  peerDomToVisible: [],
  localRenderDuration: [],
  peerRenderDuration: [],
  windowToWorkerQueue: [],
  workerQueueAck: [],
  localAuthenticate: [],
  localAuthorize: [],
  localInterpret: [],
  localProject: [],
  localWorkerToWindowQueue: [],
  localSidebandAck: [],
  localGate: [],
  localSignal: [],
  carrierBroadcastCall: [],
  carrierCallStartToRemoteReceipt: [],
  peerWindowToWorker: [],
  peerAuthenticate: [],
  peerAuthorize: [],
  peerInterpret: [],
  peerProject: [],
  remoteCarrierReceiptToWorkerProjection: [],
  peerWorkerToWindowQueue: [],
  peerSidebandAck: [],
  peerGate: [],
  peerSignal: [],
  localSignalToDom: [],
  peerSignalToDom: [],
  localDomToRender: [],
  peerDomToRender: [],
  compactSessionProofs: 0,
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
  const chromiumProvenance = {
    executable:
      process.env.WALKIE_BROWSER_EXECUTABLE ?? chromium.executablePath(),
    version: browser.version(),
  };
  const leftContext = await browser.newContext({ serviceWorkers: "allow" });
  const rightContext = await browser.newContext({ serviceWorkers: "allow" });
  const url = `${origin}/${sessionTraceEnabled ? "?sessionTrace=1" : ""}#${room}`;

  let left = await openPeer(leftContext, "left", url, diagnostics, sessionTraces);
  let right = await openPeer(rightContext, "right", url, diagnostics, sessionTraces);
  await Promise.all([waitForSynchronized(left), waitForSynchronized(right)]);
  assertReplicaWorkersReady(diagnostics, ["left", "right"]);
  await Promise.all([installOverlayObserver(left), installOverlayObserver(right)]);

  for (const [index, note] of [0, 1, 2, 3, 4, 5, 6, 8, 9, 10].entries()) {
    const sourceIsLeft = index % 2 === 0;
    const source = sourceIsLeft ? left : right;
    const peer = sourceIsLeft ? right : left;
    const sourceLabel = sourceIsLeft ? "left" : "right";
    const peerLabel = sourceIsLeft ? "right" : "left";
    await operate({
      source,
      sourceLabel,
      peer,
      peerLabel,
      note,
      present: true,
      timings,
      sessionTraces,
    });
    await operate({
      source,
      sourceLabel,
      peer,
      peerLabel,
      note,
      present: false,
      timings,
      sessionTraces,
    });
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
    sessionTraces,
  });
  await left.close();
  const rightWorkerGenerationsBeforeReload = workerReadyCount(diagnostics, "right");
  await right.reload({ waitUntil: "domcontentloaded" });
  await right.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  await waitForOverlay(right, targetKey + 7, true);
  assert.ok(
    workerReadyCount(diagnostics, "right") > rightWorkerGenerationsBeforeReload,
    "right did not open a fresh Replica worker and recover after reload",
  );

  // Reopen the other independent profile, prove its own durable reconstruction,
  // then wait for the normal carrier to report direct synchronized repair.
  const reconnectStarted = Date.now();
  left = await openPeer(leftContext, "left-reopened", url, diagnostics, sessionTraces);
  await waitForOverlay(left, targetKey + 7, true);
  await Promise.all([waitForSynchronized(left), waitForSynchronized(right)]);
  assertReplicaWorkersReady(diagnostics, ["left-reopened"]);
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
    sessionTraces,
  });

  assertNoRepairFailures(diagnostics);
  // The first two operations from each source include first-write allocation,
  // JIT, and cold carrier scheduling. Preserve them in the primary samples,
  // but also report the musical steady-state budget independently.
  const warmupSamplesExcluded = 4;
  const steady = Object.fromEntries(
    Object.entries(timings)
      .filter(([, samples]) => Array.isArray(samples) && samples.length > warmupSamplesExcluded)
      .map(([name, samples]) => [
        name,
        latencySummary(samples.slice(warmupSamplesExcluded)),
      ]),
  );
  const report = {
    schema: "walkie.browser-acceptance@3",
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
    allAroundKeyboard: {
      version: keyboardVersion,
      sha256: keyboardSha256,
    },
    sampleCount: timings.peerVisible.length,
    compactSessionTrace: {
      schema: "walkie.session-compact-trace@3",
      enabled: sessionTraceEnabled,
      proofCount: timings.compactSessionProofs,
      observation: sessionTraceEnabled
        ? "Opt-in application-sideband trace; token maps explicitly to worker-minted correlation."
        : "Disabled at RoomWorkerOpen; trace=None and no scheduling timestamps sampled.",
      crossGlobalClockQuantization: {
        toleranceMs: traceClockQuantizationToleranceMs,
        adjustments: traceClockQuantizationAdjustments,
      },
    },
    warmupSamplesExcluded,
    steadyStateLatencyMs: steady,
    performanceTargets: {
      localVisibleFeedback: {
        targetMs: 5,
        metric: "intent to reversible all-around-keyboard pressedNotes acknowledgement",
        observedSteadyP95Ms: steady.localPressedFeedback?.p95 ?? null,
        met:
          Number.isFinite(steady.localPressedFeedback?.p95) &&
          steady.localPressedFeedback.p95 < 5,
        note:
          "This generation-scoped acknowledgement is bounded and reversible; canonical sunny membership, durable confirmation, component rendering, and paint are reported separately.",
      },
      remoteCausalProjection: {
        targetMs: 15,
        metric: "remote carrier receipt to worker-owned HHHS projection",
        observedSteadyP95Ms: steady.remoteCarrierReceiptToWorkerProjection?.p95 ?? null,
        met:
          Number.isFinite(steady.remoteCarrierReceiptToWorkerProjection?.p95) &&
          steady.remoteCarrierReceiptToWorkerProjection.p95 < 15,
        note: "Network transit and visible browser rendering are reported separately.",
      },
    },
    latencyBudgetsMs,
    latencyEnforcement: enforceLatency ? "single-trial" : "external-fixed-trial-policy",
    hostCondition: {
      started: hostConditionStarted,
      finished: hostCondition(),
      note: "Diagnostic only; host load never relaxes or overrides a latency budget.",
    },
    localDomMutationLatencyMs: {
      samples: timings.localDomMutation,
      p50: percentile(timings.localDomMutation, 0.5),
      p95: percentile(timings.localDomMutation, 0.95),
    },
    localPressedFeedbackLatencyMs: {
      samples: timings.localPressedFeedback,
      p50: percentile(timings.localPressedFeedback, 0.5),
      p95: percentile(timings.localPressedFeedback, 0.95),
    },
    localVisibleLatencyMs: {
      samples: timings.localVisible,
      p50: percentile(timings.localVisible, 0.5),
      p95: percentile(timings.localVisible, 0.95),
    },
    localDomToVisibleMs: {
      samples: timings.localDomToVisible,
      p50: percentile(timings.localDomToVisible, 0.5),
      p95: percentile(timings.localDomToVisible, 0.95),
    },
    localKeyboardRenderDurationMs: {
      samples: timings.localRenderDuration,
      p50: percentile(timings.localRenderDuration, 0.5),
      p95: percentile(timings.localRenderDuration, 0.95),
    },
    peerDomMutationLatencyMs: {
      samples: timings.peerDomMutation,
      p50: percentile(timings.peerDomMutation, 0.5),
      p95: percentile(timings.peerDomMutation, 0.95),
    },
    peerVisibleLatencyMs: {
      samples: timings.peerVisible,
      p50: percentile(timings.peerVisible, 0.5),
      p95: percentile(timings.peerVisible, 0.95),
      hardBudgetApplied: true,
      steadyStateP95BudgetMs: latencyBudgetsMs.peerVisibleP95,
    },
    peerDomToVisibleMs: {
      samples: timings.peerDomToVisible,
      p50: percentile(timings.peerDomToVisible, 0.5),
      p95: percentile(timings.peerDomToVisible, 0.95),
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
  assert.ok(
    statSync(reportPath).mtimeMs >= runStartedAt.getTime(),
    "acceptance report predates the active run",
  );
  console.log(JSON.stringify(report, null, 2));

  if (enforceLatency) assertStrictLatencyTrial(report);

  await Promise.all([leftContext.close(), rightContext.close()]);
} catch (error) {
  console.error(
    JSON.stringify(
      {
        failure: String(error),
        diagnostics,
      },
      null,
      2,
    ),
  );
  throw error;
} finally {
  if (browser) await browser.close();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
