import assert from "node:assert/strict";

// Regression ceilings for the durable Room-v5 path. These are deliberately
// looser than the future realtime/session protocol target, but they remain
// strict enough to catch a return to the pre-worker multi-hundred-ms local
// path.
export const latencyBudgetsMs = Object.freeze({
  localProjectionP95: 15,
  localVisibleP95: 30,
  peerProjectionP95: 75,
  peerVisibleP95: 100,
  localKeyboardRenderP95: 2,
  peerKeyboardRenderP95: 2,
  reconnect: 10_000,
});

export const releaseTrialCount = 3;
export const requiredPassingTrials = 2;
export const grossCeilingMultiplier = 4;

const metricDefinitions = Object.freeze([
  ["localProjection", "steady local projection", "localProjectionP95"],
  ["localVisible", "steady local visibility", "localVisibleP95"],
  ["peerProjection", "steady peer projection", "peerProjectionP95"],
  ["peerVisible", "steady peer visibility", "peerVisibleP95"],
  ["localRenderDuration", "steady local keyboard render", "localKeyboardRenderP95"],
  ["peerRenderDuration", "steady peer keyboard render", "peerKeyboardRenderP95"],
]);

export function percentile(samples, fraction) {
  assert.ok(samples.length > 0);
  const sorted = [...samples].sort((left, right) => left - right);
  return sorted[Math.ceil(fraction * sorted.length) - 1];
}

export function latencySummary(samples) {
  return {
    samples,
    p50: percentile(samples, 0.5),
    p95: percentile(samples, 0.95),
  };
}

export function evaluateLatencyTrial(report) {
  const checks = metricDefinitions.map(([reportKey, label, budgetKey]) => {
    const actualMs = report.steadyStateLatencyMs?.[reportKey]?.p95;
    const budgetMs = latencyBudgetsMs[budgetKey];
    assert.ok(Number.isFinite(actualMs), `missing finite ${reportKey} p95`);
    return latencyCheck(reportKey, label, actualMs, budgetMs);
  });
  assert.ok(Number.isFinite(report.reconnectMs), "missing finite reconnect latency");
  checks.push(latencyCheck("reconnect", "reconnect", report.reconnectMs, latencyBudgetsMs.reconnect));
  return {
    strictPassed: checks.every(check => check.passed),
    grossPassed: checks.every(check => check.grossPassed),
    checks,
  };
}

export function assertStrictLatencyTrial(report) {
  const evaluation = evaluateLatencyTrial(report);
  const failed = evaluation.checks.find(check => !check.passed);
  assert.ok(
    !failed,
    failed?.id === "reconnect"
      ? `${failed.label} ${failed.actualMs.toFixed(1)}ms exceeded the ${failed.budgetMs}ms release budget`
      : `${failed?.label} p95 ${failed?.actualMs.toFixed(1)}ms exceeded the ${failed?.budgetMs}ms release budget`,
  );
  return evaluation;
}

export function evaluateReleaseTrials(reports) {
  assert.equal(
    reports.length,
    releaseTrialCount,
    `release latency policy requires exactly ${releaseTrialCount} fixed trials`,
  );
  const trials = reports.map(evaluateLatencyTrial);
  const passingTrials = trials.filter(trial => trial.strictPassed).length;
  return {
    outcome:
      passingTrials >= requiredPassingTrials && trials.every(trial => trial.grossPassed)
        ? "passed"
        : "failed",
    passingTrials,
    trials,
  };
}

function latencyCheck(id, label, actualMs, budgetMs) {
  const grossCeilingMs = budgetMs * grossCeilingMultiplier;
  return {
    id,
    label,
    actualMs,
    budgetMs,
    passed: actualMs <= budgetMs,
    grossCeilingMs,
    grossPassed: actualMs <= grossCeilingMs,
  };
}
