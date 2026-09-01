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

test("host load never relaxes a thesis target", () => {
  const overloaded = report(0.5);
  overloaded.hostCondition = { finished: { loadAverage: { oneMinute: 999 } } };
  overloaded.performanceTargets.localVisibleFeedback.observedSteadyP95Ms =
    latencyBudgetsMs.localVisibleFeedbackP95;
  overloaded.performanceTargets.localVisibleFeedback.met = false;
  const evaluation = evaluateLatencyTrial(overloaded);
  assert.equal(evaluation.strictPassed, false);
  assert.equal(
    evaluation.checks.find(check => check.id === "localVisibleFeedback")?.passed,
    false,
  );
});

test("one trial reports every canonical and thesis budget", () => {
  const evaluation = evaluateLatencyTrial(report(0.5));
  assert.equal(evaluation.strictPassed, true);
  assert.equal(evaluation.grossPassed, true);
  assert.deepEqual(
    evaluation.checks.map(check => check.id),
    [
      "localDomMutation",
      "localVisible",
      "peerDomMutation",
      "peerVisible",
      "localRenderDuration",
      "peerRenderDuration",
      "localVisibleFeedback",
      "remoteCausalProjection",
      "reconnect",
    ],
  );
});

function report(factor) {
  return {
    steadyStateLatencyMs: {
      localDomMutation: { p95: latencyBudgetsMs.localDomMutationP95 * factor },
      localVisible: { p95: latencyBudgetsMs.localVisibleP95 * factor },
      peerDomMutation: { p95: latencyBudgetsMs.peerDomMutationP95 * factor },
      peerVisible: { p95: latencyBudgetsMs.peerVisibleP95 * factor },
      localRenderDuration: { p95: latencyBudgetsMs.localKeyboardRenderP95 * factor },
      peerRenderDuration: { p95: latencyBudgetsMs.peerKeyboardRenderP95 * factor },
    },
    performanceTargets: {
      localVisibleFeedback: {
        targetMs: latencyBudgetsMs.localVisibleFeedbackP95,
        observedSteadyP95Ms: latencyBudgetsMs.localVisibleFeedbackP95 * factor,
        met: factor < 1,
      },
      remoteCausalProjection: {
        targetMs: latencyBudgetsMs.remoteCausalProjectionP95,
        observedSteadyP95Ms: latencyBudgetsMs.remoteCausalProjectionP95 * factor,
        met: factor < 1,
      },
    },
    reconnectMs: latencyBudgetsMs.reconnect * factor,
  };
}
