import assert from "node:assert/strict";

// Regression ceilings for canonical Room-v5 rendering plus the two causal
// music thesis targets. Host load is diagnostic only and never changes these
// values.
export const latencyBudgetsMs = Object.freeze({
  localVisibleFeedbackP95: 5,
  remoteCausalProjectionP95: 15,
  localDomMutationP95: 15,
  localVisibleP95: 30,
  peerDomMutationP95: 75,
  peerVisibleP95: 100,
  localKeyboardRenderP95: 2,
  peerKeyboardRenderP95: 2,
  reconnect: 10_000,
});

export const releaseTrialCount = 3;
export const requiredPassingTrials = 2;
export const grossCeilingMultiplier = 4;

const metricDefinitions = Object.freeze([
  ["localDomMutation", "steady local DOM mutation", "localDomMutationP95"],
  ["localVisible", "steady local visibility", "localVisibleP95"],
  ["peerDomMutation", "steady peer DOM mutation", "peerDomMutationP95"],
  ["peerVisible", "steady peer visibility", "peerVisibleP95"],
  ["localRenderDuration", "steady local keyboard render", "localKeyboardRenderP95"],
  ["peerRenderDuration", "steady peer keyboard render", "peerKeyboardRenderP95"],
]);

const thesisTargetDefinitions = Object.freeze([
  [
    "localVisibleFeedback",
    "reversible local pressed-feedback acknowledgement",
    "localVisibleFeedbackP95",
  ],
  [
    "remoteCausalProjection",
    "remote carrier receipt to worker-owned HHHS projection",
    "remoteCausalProjectionP95",
  ],
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
  for (const [reportKey, label, budgetKey] of thesisTargetDefinitions) {
    const target = report.performanceTargets?.[reportKey];
    const actualMs = target?.observedSteadyP95Ms;
    const budgetMs = latencyBudgetsMs[budgetKey];
    assert.ok(Number.isFinite(actualMs), `missing finite ${reportKey} p95`);
    assert.equal(target?.targetMs, budgetMs, `${reportKey} target does not match release policy`);
    assert.equal(target?.met, actualMs < budgetMs, `${reportKey} met flag is inconsistent`);
    checks.push(latencyCheck(reportKey, label, actualMs, budgetMs, true));
  }
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

function latencyCheck(id, label, actualMs, budgetMs, exclusive = false) {
  const grossCeilingMs = budgetMs * grossCeilingMultiplier;
  return {
    id,
    label,
    actualMs,
    budgetMs,
    passed: exclusive ? actualMs < budgetMs : actualMs <= budgetMs,
    grossCeilingMs,
    grossPassed: actualMs <= grossCeilingMs,
  };
}
