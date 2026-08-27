import assert from "node:assert/strict";
import test from "node:test";

import {
  evaluateLatencyTrial,
  evaluateReleaseTrials,
  latencyBudgetsMs,
  percentile,
} from "./browser-latency-policy.mjs";

test("nearest-rank percentile keeps the observed tail", () => {
  assert.equal(percentile([4, 1, 3, 2, 5], 0.5), 3);
  assert.equal(percentile([4, 1, 3, 2, 5], 0.95), 5);
});

test("one bounded noisy host trial cannot fail an otherwise repeatable release", () => {
  const evaluation = evaluateReleaseTrials([
    report(0.5),
    report(2.5),
    report(0.75),
  ]);
  assert.equal(evaluation.passingTrials, 2);
  assert.equal(evaluation.outcome, "passed");
});

test("two strict failures still fail the fixed trial set", () => {
  const evaluation = evaluateReleaseTrials([
    report(0.5),
    report(1.1),
    report(1.2),
  ]);
  assert.equal(evaluation.passingTrials, 1);
  assert.equal(evaluation.outcome, "failed");
});

test("a catastrophic trial is never outvoted", () => {
  const evaluation = evaluateReleaseTrials([
    report(0.5),
    report(0.75),
    report(4.1),
  ]);
  assert.equal(evaluation.passingTrials, 2);
  assert.equal(evaluation.outcome, "failed");
});

test("one trial reports every durable-path budget", () => {
  const evaluation = evaluateLatencyTrial(report(1));
  assert.equal(evaluation.strictPassed, true);
  assert.equal(evaluation.grossPassed, true);
  assert.deepEqual(
    evaluation.checks.map(check => check.id),
    [
      "localProjection",
      "localVisible",
      "peerProjection",
      "peerVisible",
      "localRenderDuration",
      "peerRenderDuration",
      "reconnect",
    ],
  );
});

function report(factor) {
  return {
    steadyStateLatencyMs: {
      localProjection: { p95: latencyBudgetsMs.localProjectionP95 * factor },
      localVisible: { p95: latencyBudgetsMs.localVisibleP95 * factor },
      peerProjection: { p95: latencyBudgetsMs.peerProjectionP95 * factor },
      peerVisible: { p95: latencyBudgetsMs.peerVisibleP95 * factor },
      localRenderDuration: { p95: latencyBudgetsMs.localKeyboardRenderP95 * factor },
      peerRenderDuration: { p95: latencyBudgetsMs.peerKeyboardRenderP95 * factor },
    },
    reconnectMs: latencyBudgetsMs.reconnect * factor,
  };
}
