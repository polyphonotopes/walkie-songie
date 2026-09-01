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
const dist = resolve(
  process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/lifecycle-acceptance-web"),
);
const reportPath = resolve(
  process.env.WALKIE_GENERATION_RECOVERY_REPORT ??
    join(repository, "output/playwright/browser-generation-recovery.json"),
);
const timeoutMs = Number(process.env.WALKIE_ACCEPTANCE_TIMEOUT_MS ?? 120_000);
const trialSelector = process.env.WALKIE_GENERATION_RECOVERY_TRIAL;
const runStartedAt = new Date();
const targetKey = 36;
const staleOfferKey = "walkie-acceptance-stale-renewal-offer";
const staleOfferDigestKey = "walkie-acceptance-stale-renewal-offer-digest";
const staleReplayArmKey = "walkie-acceptance-stale-renewal-replay-armed";
const authoritativeWorkerStateKey = "walkie-acceptance-authoritative-worker-state";
rmSync(reportPath, { force: true });

const trialSpecs = [
  {
    kind: "rejected-combined-frame",
    query: "sessionRejectRealtimeOnce=1",
    expectedDelta: 0,
  },
  {
    kind: "drain-before-commit",
    query: "sessionDrainCut=before",
    expectedDelta: 0,
  },
  {
    kind: "drain-after-commit",
    query: "sessionDrainCut=after",
    expectedDelta: 1,
  },
];
if (trialSelector) {
  assert.ok(
    trialSpecs.some(({ kind }) => kind === trialSelector),
    `unknown generation-recovery trial ${trialSelector}`,
  );
}

assert.ok(existsSync(join(dist, "index.html")), "missing instrumented browser artifact");
for (const marker of [
  "sessionRejectRealtimeOnce",
  "sessionDrainCut",
  "acceptanceWorkerStateTrace",
]) {
  assert.ok(artifactContains(dist, marker), `instrumented artifact lacks ${marker}`);
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

async function waitUntil(predicate, description) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = predicate();
    if (value) return value;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 40));
  }
  const boundary = predicate();
  if (boundary) return boundary;
  throw new Error(`timed out waiting for ${description}`);
}

function attachDiagnostics(page, label, diagnostics, renewalTraces, workerStates) {
  page.on("console", (message) => {
    const entry = { label, type: message.type(), text: message.text() };
    diagnostics.push(entry);
    if (entry.text.startsWith("[session_renewal_trace] ")) {
      try {
        renewalTraces.push({
          label,
          ...JSON.parse(entry.text.slice("[session_renewal_trace] ".length)),
        });
      } catch (error) {
        renewalTraces.push({ label, parseError: String(error), raw: entry.text });
      }
    }
    const match = /^\[replica_worker_state\] generation=(\d+) projection=(.*)$/.exec(entry.text);
    if (match) {
      try {
        workerStates.push({
          label,
          generation: Number(match[1]),
          projection: JSON.parse(match[2]),
        });
      } catch (error) {
        workerStates.push({ label, parseError: String(error), raw: entry.text });
      }
    }
  });
  page.on("pageerror", (error) => {
    diagnostics.push({ label, type: "pageerror", text: String(error), stack: error.stack ?? null });
  });
}

async function openPeer(context, label, url, diagnostics, renewalTraces, workerStates) {
  const page = await context.newPage();
  attachDiagnostics(page, label, diagnostics, renewalTraces, workerStates);
  await page.goto(url, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("all-around-keyboard", { state: "attached", timeout: timeoutMs });
  return page;
}

async function waitForSynchronized(page, requireDirect = true) {
  await page.waitForFunction(
    (directRequired) => {
      const status = document.querySelector(".peer-status")?.textContent ?? "";
      return (!directRequired || status.includes("Direct")) && status.includes("synchronized");
    },
    requireDirect,
    { timeout: timeoutMs },
  );
}

async function waitForOverlay(page, present) {
  await page.waitForFunction(
    ({ key, expected }) =>
      document.querySelectorAll(
        `all-around-keyboard > .toggle-overlay[data-key-overlay="${key}"]`,
      ).length === expected,
    { key: targetKey, expected: present ? 1 : 0 },
    { timeout: timeoutMs },
  );
}

async function togglePitch(page) {
  await page
    .locator("all-around-keyboard")
    .locator(`[data-key-index="${targetKey}"]`)
    .click();
}

async function pressedNotes(page) {
  return page.locator("all-around-keyboard").evaluate((keyboard) =>
    Array.from(keyboard.pressedNotes ?? []).map(Number),
  );
}

async function waitForStoredAuthoritativeState(
  page,
  label,
  expectedGeneration,
  observationArmedAt,
  workerStates,
) {
  await page.waitForFunction(
    ({ key, expected }) => {
      try {
        const state = JSON.parse(localStorage.getItem(key) ?? "null");
        return state?.generation === expected && state?.projection;
      } catch {
        return false;
      }
    },
    { key: authoritativeWorkerStateKey, expected: expectedGeneration },
    { timeout: timeoutMs },
  );
  const state = await page.evaluate((key) => JSON.parse(localStorage.getItem(key)), authoritativeWorkerStateKey);
  const traced = {
    label,
    generation: Number(state.generation),
    projection: state.projection,
    observedAfterArmAt: new Date().toISOString(),
  };
  assert.equal(traced.generation, expectedGeneration, `${label} reopened an unexpected generation`);
  assert.ok(Date.now() >= observationArmedAt, `${label} worker state predates observation arm`);
  workerStates.push(traced);
  return traced;
}

function latestState(workerStates, label, generation) {
  return workerStates.findLast(
    (state) => state.label === label && state.generation === generation && !state.parseError,
  );
}

function latestAuthoritativeState(workerStates, label) {
  return workerStates.findLast((state) => state.label === label && !state.parseError);
}

function authoritativeMusicStateMatches(left, right) {
  return (
    left &&
    right &&
    left.projection.music_revision === right.projection.music_revision &&
    JSON.stringify(left.projection.music_history_root) ===
      JSON.stringify(right.projection.music_history_root) &&
    JSON.stringify(left.projection.view) === JSON.stringify(right.projection.view)
  );
}

function authoritativeStateSignature(state) {
  return JSON.stringify({
    generation: state.generation,
    musicRevision: state.projection.music_revision,
    musicHistoryRoot: state.projection.music_history_root,
    view: state.projection.view,
  });
}

async function waitForStableAuthoritativeBaseline(workerStates, kind) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const sourceState = latestAuthoritativeState(workerStates, "source");
    const peerState = latestAuthoritativeState(workerStates, "peer");
    if (!authoritativeMusicStateMatches(sourceState, peerState)) {
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 40));
      continue;
    }
    const sourceSignature = authoritativeStateSignature(sourceState);
    const peerSignature = authoritativeStateSignature(peerState);
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 500));
    const settledSource = latestAuthoritativeState(workerStates, "source");
    const settledPeer = latestAuthoritativeState(workerStates, "peer");
    if (
      authoritativeMusicStateMatches(settledSource, settledPeer) &&
      authoritativeStateSignature(settledSource) === sourceSignature &&
      authoritativeStateSignature(settledPeer) === peerSignature
    ) {
      return { sourceState: settledSource, peerState: settledPeer };
    }
  }
  throw new Error(`timed out waiting for ${kind} stable authoritative pre-fault baseline`);
}

async function runTrial({ browser, origin, kind, query, expectedDelta }) {
  const diagnostics = [];
  const renewalTraces = [];
  const workerStates = [];
  // Room-v5 names are exactly three lowercase alphabetic segments.  Keeping
  // the fault kind inside one segment prevents an invalid hash from making
  // each page silently generate a different room.
  const room = `${kind.replace(/[^a-z]/g, "")}-${alphabetic(Date.now())}-${alphabetic(process.pid)}`;
  const sourceContext = await browser.newContext({ serviceWorkers: "allow" });
  const peerContext = await browser.newContext({ serviceWorkers: "allow" });
  const sourceUrl = `${origin}/?sessionTrace=1&renewalReplayStale=1&acceptanceWorkerStateTrace=1&${query}#${room}`;
  const peerUrl = `${origin}/?sessionTrace=1&renewalReplayStale=1&acceptanceWorkerStateTrace=1#${room}`;
  const source = await openPeer(
    sourceContext,
    "source",
    sourceUrl,
    diagnostics,
    renewalTraces,
    workerStates,
  );
  const peer = await openPeer(
    peerContext,
    "peer",
    peerUrl,
    diagnostics,
    renewalTraces,
    workerStates,
  );

  try {
    const [sourceHash, peerHash] = await Promise.all([
      source.evaluate(() => location.hash.slice(1)),
      peer.evaluate(() => location.hash.slice(1)),
    ]);
    assert.equal(sourceHash, room, `${kind} source did not retain the requested Room-v5 name`);
    assert.equal(peerHash, room, `${kind} peer did not retain the requested Room-v5 name`);
    await Promise.all([waitForSynchronized(source), waitForSynchronized(peer)]);
    const initialInstall = await waitUntil(() => {
      const sourceInstall = renewalTraces.find(
        (trace) => trace.label === "source" && trace.stage === "SessionInstalled",
      );
      return (
        sourceInstall &&
        renewalTraces.some(
          (trace) =>
            trace.label === "peer" &&
            trace.stage === "SessionInstalled" &&
            trace.epoch === sourceInstall.epoch,
        ) &&
        sourceInstall
      );
    }, `${kind} initial compact session installation`);
    const baseline = await waitForStableAuthoritativeBaseline(workerStates, kind);
    const initialWorker = baseline.sourceState;

    // Re-deliver the real signed offer after recovery. Both sides may retain
    // the first Offer (one outbound, one inbound), so storage presence does
    // not identify its receiver. Capture bytes+digest atomically, copy that
    // exact pair to both pages, then select the receiver from a fresh post-arm
    // decode probe whose computed digest matches these bytes.
    const [sourceRetained, peerRetained] = await Promise.all([
      source.evaluate(
        ({ offerKey, digestKey }) => ({
          offer: localStorage.getItem(offerKey),
          digest: localStorage.getItem(digestKey),
        }),
        { offerKey: staleOfferKey, digestKey: staleOfferDigestKey },
      ),
      peer.evaluate(
        ({ offerKey, digestKey }) => ({
          offer: localStorage.getItem(offerKey),
          digest: localStorage.getItem(digestKey),
        }),
        { offerKey: staleOfferKey, digestKey: staleOfferDigestKey },
      ),
    ]);
    const retained = sourceRetained.offer ? sourceRetained : peerRetained;
    const staleOffer = retained.offer;
    const staleOfferDigest = retained.digest;
    assert.ok(staleOffer, `${kind} did not retain a signed establishment offer`);
    assert.match(staleOfferDigest ?? "", /^[0-9a-f]{64}$/, `${kind} retained Offer lacks digest`);
    if (sourceRetained.offer && peerRetained.offer) {
      assert.equal(sourceRetained.offer, peerRetained.offer, `${kind} retained different old Offers`);
      assert.equal(
        sourceRetained.digest,
        peerRetained.digest,
        `${kind} retained Offer digest disagreement`,
      );
    }
    const staleReplayArmTraceIndex = renewalTraces.length;
    const staleReplayArmDiagnosticIndex = diagnostics.length;
    await Promise.all(
      [source, peer].map((page) =>
        page.evaluate(
          ({ offerKey, digestKey, armKey, offer, digest }) => {
            localStorage.setItem(offerKey, offer);
            localStorage.setItem(digestKey, digest);
            localStorage.setItem(armKey, "1");
          },
          {
            offerKey: staleOfferKey,
            digestKey: staleOfferDigestKey,
            armKey: staleReplayArmKey,
            offer: staleOffer,
            digest: staleOfferDigest,
          },
        ),
      ),
    );

    // The acceptance mirror is a query result for one checked Open snapshot,
    // not persistence evidence. Remove every initial value immediately before
    // the cut, then require the exact next generation to repopulate it.
    await Promise.all(
      [source, peer].map((page) =>
        page.evaluate((key) => localStorage.removeItem(key), authoritativeWorkerStateKey),
      ),
    );
    const workerStateObservationArmedAt = Date.now();

    await togglePitch(source);
    await waitUntil(
      () =>
        diagnostics.find(
          (entry) =>
            entry.label === "source" &&
            entry.text.includes("[native:replica_worker_generation_failed]"),
        ),
      `${kind} whole-placement generation failure`,
    );
    await waitUntil(
      () =>
        diagnostics.find(
          (entry) =>
            entry.label === "source" &&
            entry.text.includes("[native:replica_worker_generation_recovered]"),
        ),
      `${kind} generation recovery completion`,
    );
    const recoveredWorker = await waitForStoredAuthoritativeState(
      source,
      "source",
      initialWorker.generation + 1,
      workerStateObservationArmedAt,
      workerStates,
    );
    assert.ok(
      diagnostics.some(
        (entry) =>
          entry.label === "source" &&
          entry.text.includes("[native:replica_worker_generation_terminal]") &&
          entry.text.includes(`generation ${initialWorker.generation}`) &&
          entry.text.includes("Failed"),
      ),
      `${kind} did not observe the old BrowserReplica lifecycle become terminal`,
    );
    assert.ok(
      diagnostics.some(
        (entry) =>
          entry.label === "source" &&
          entry.text.includes("[native:performance_feedback_reset_applied]") &&
          entry.text.includes(`generation ${initialWorker.generation}`),
      ),
      `${kind} did not observe generation-scoped feedback reset`,
    );
    assert.equal(
      recoveredWorker.projection.music_revision,
      initialWorker.projection.music_revision + expectedDelta,
      `${kind} recovered an unexpected number of durable music transactions`,
    );
    assert.equal(
      JSON.stringify(recoveredWorker.projection.music_history_root) ===
        JSON.stringify(initialWorker.projection.music_history_root),
      expectedDelta === 0,
      `${kind} durable root did not match the exact absent-or-one cut`,
    );

    const recoveredInstall = await waitUntil(
      () =>
        renewalTraces.find(
          (trace) =>
            trace.label === "source" &&
            trace.stage === "SessionInstalled" &&
            trace.epoch > initialInstall.epoch,
        ),
      `${kind} higher-epoch session after canonical reopen`,
    );
    const replayTarget = await waitUntil(
      () =>
        diagnostics
          .slice(staleReplayArmDiagnosticIndex)
          .find(
            (entry) =>
              entry.text.includes("[native:session_stale_offer_replay_probe]") &&
              entry.text.includes(`installed_epoch=${recoveredInstall.epoch}`) &&
              entry.text.includes("armed=true") &&
              entry.text.includes("target_matches=true") &&
              entry.text.includes(`offer_digest=${staleOfferDigest}`),
          ),
      `${kind} exact retained Offer target identification`,
    );
    const staleReplayLabel = replayTarget.label;
    const staleRefusal = await waitUntil(
      () =>
        renewalTraces
          .slice(staleReplayArmTraceIndex)
          .find(
            (trace) =>
              trace.label === staleReplayLabel &&
              trace.stage === "StaleOfferRefused" &&
              trace.epoch === initialInstall.epoch,
          ),
      `${kind} stale old-session traffic refusal`,
    );
    await Promise.all([
      waitForSynchronized(source, false),
      waitForSynchronized(peer, false),
    ]);
    await Promise.all([waitForOverlay(source, expectedDelta === 1), waitForOverlay(peer, expectedDelta === 1)]);
    await source.waitForFunction(
      () => Array.from(document.querySelector("all-around-keyboard")?.pressedNotes ?? []).length === 0,
      undefined,
      { timeout: timeoutMs },
    );
    assert.deepEqual(await pressedNotes(source), [], `${kind} left pressed feedback behind`);

    // The recovered placement must accept, predict, durably admit, propagate,
    // and remove a subsequent edit without a page reload.  Dedicated repair
    // completion is covered by the separate browser repair acceptance gate;
    // this trial does not deliberately suppress a live durable record.
    if (expectedDelta === 1) {
      await togglePitch(source);
      await Promise.all([waitForOverlay(source, false), waitForOverlay(peer, false)]);
    }
    await togglePitch(source);
    await Promise.all([waitForOverlay(source, true), waitForOverlay(peer, true)]);
    await togglePitch(peer);
    await Promise.all([waitForOverlay(source, false), waitForOverlay(peer, false)]);

    // Overlay convergence can be entirely speculative.  Completion requires
    // both dedicated workers to publish the same authoritative durable music
    // revision, root, and materialized view after the recovered add/remove.
    const expectedPostRecoveryRevision =
      recoveredWorker.projection.music_revision + (expectedDelta === 1 ? 3 : 2);
    const authoritativeConvergence = await waitUntil(() => {
      const sourceState = latestAuthoritativeState(workerStates, "source");
      const peerState = latestAuthoritativeState(workerStates, "peer");
      return (
        authoritativeMusicStateMatches(sourceState, peerState) &&
        sourceState.projection.music_revision === expectedPostRecoveryRevision &&
        { sourceState, peerState }
      );
    }, `${kind} authoritative post-recovery durable convergence`);
    assert.equal(
      authoritativeConvergence.sourceState.projection.music_revision,
      expectedPostRecoveryRevision,
      `${kind} durable post-recovery command count was not exact`,
    );

    assert.deepEqual(
      renewalTraces.filter((trace) => trace.parseError),
      [],
      `${kind} emitted malformed renewal traces`,
    );
    assert.deepEqual(
      workerStates.filter((state) => state.parseError),
      [],
      `${kind} emitted malformed worker-state records`,
    );
    assert.deepEqual(
      diagnostics.filter((entry) => entry.type === "pageerror"),
      [],
      `${kind} emitted uncaught browser errors`,
    );

    return {
      kind,
      room,
      initialGeneration: initialWorker.generation,
      recoveredGeneration: recoveredWorker.generation,
      initialEpoch: initialInstall.epoch,
      recoveredEpoch: recoveredInstall.epoch,
      durableRevisionBefore: initialWorker.projection.music_revision,
      durableRevisionAfter: recoveredWorker.projection.music_revision,
      durableRootBefore: initialWorker.projection.music_history_root,
      durableRootAfter: recoveredWorker.projection.music_history_root,
      authoritativePostRecovery: {
        musicRevision: authoritativeConvergence.sourceState.projection.music_revision,
        musicHistoryRoot:
          authoritativeConvergence.sourceState.projection.music_history_root,
        view: authoritativeConvergence.sourceState.projection.view,
      },
      assertions: {
        oldLifecycleTerminal: true,
        feedbackReset: true,
        automaticReopenWithoutReload: true,
        strictlyNewWorkerGeneration: true,
        higherPersistedSessionEpoch: true,
        staleTrafficRefused: {
          page: staleRefusal.label,
          epoch: staleRefusal.epoch,
          armedTraceIndex: staleReplayArmTraceIndex,
        },
        exactAbsentOrOneDurableTransaction: true,
        postRecoveryAuthoritativeConvergence: true,
      },
      diagnostics,
      renewalTraces,
      workerStates,
    };
  } catch (error) {
    const [sourceStatus, peerStatus] = await Promise.all([
      source.locator(".peer-status").allTextContents().catch(() => []),
      peer.locator(".peer-status").allTextContents().catch(() => []),
    ]);
    console.error(
      JSON.stringify(
        {
          schema: "walkie.browser-generation-recovery-failure@1",
          kind,
          room,
          error: String(error),
          sourceStatus,
          peerStatus,
          diagnostics: diagnostics.filter(
            (entry) =>
              entry.type === "pageerror" ||
              entry.text.includes("[native:") ||
              entry.text.includes("[replica_worker]") ||
              entry.text.includes("[replica_repair"),
          ),
          renewalTraces,
          workerStates: [
            latestAuthoritativeState(workerStates, "source"),
            latestAuthoritativeState(workerStates, "peer"),
          ].filter(Boolean),
        },
        null,
        2,
      ),
    );
    throw error;
  } finally {
    await Promise.all([sourceContext.close(), peerContext.close()]);
  }
}

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
  const trials = [];
  for (const spec of trialSpecs) {
    if (!trialSelector || trialSelector === spec.kind) {
      trials.push(await runTrial({ browser, origin, ...spec }));
    }
  }

  if (trialSelector) {
    assert.equal(trials.length, 1, "diagnostic selector must run exactly one trial");
    assert.ok(!existsSync(reportPath), "diagnostic trial must not publish a success report");
    console.log(
      JSON.stringify(
        {
          schema: "walkie.browser-generation-recovery-diagnostic@1",
          runStartedAt: runStartedAt.toISOString(),
          capturedAt: new Date().toISOString(),
          releaseDist: relative(repository, dist),
          provenance: {
            ...sourceProvenance(repository),
            hhhsPin: hhhsPinFromLock(repository),
            artifactSha256: artifactTreeSha256(dist),
            artifactProfile: "acceptance-instrumented",
            chromium: chromiumProvenance,
          },
          trial: {
            kind: trials[0].kind,
            initialGeneration: trials[0].initialGeneration,
            recoveredGeneration: trials[0].recoveredGeneration,
            durableRevisionBefore: trials[0].durableRevisionBefore,
            durableRevisionAfter: trials[0].durableRevisionAfter,
            assertions: trials[0].assertions,
          },
        },
        null,
        2,
      ),
    );
  } else {
    const report = {
      schema: "walkie.browser-generation-recovery@1",
      runStartedAt: runStartedAt.toISOString(),
      capturedAt: new Date().toISOString(),
      releaseDist: relative(repository, dist),
      provenance: {
        ...sourceProvenance(repository),
        hhhsPin: hhhsPinFromLock(repository),
        artifactSha256: artifactTreeSha256(dist),
        artifactProfile: "acceptance-instrumented",
        chromium: chromiumProvenance,
      },
      trials,
    };
    mkdirSync(dirname(reportPath), { recursive: true });
    writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
    assert.ok(statSync(reportPath).mtimeMs >= runStartedAt.getTime(), "stale recovery report");
    console.log(JSON.stringify(report, null, 2));
  }
} finally {
  if (browser) await browser.close();
  await new Promise((resolvePromise) => server.close(resolvePromise));
}
