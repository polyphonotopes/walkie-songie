#!/usr/bin/env node

import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
  evaluateReleaseTrials,
  grossCeilingMultiplier,
  latencyBudgetsMs,
  releaseTrialCount,
  requiredPassingTrials,
} from "./browser-latency-policy.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const output = resolve(
  process.env.WALKIE_ACCEPTANCE_REPORT ?? join(repository, "output/playwright/browser-acceptance.json"),
);
mkdirSync(dirname(output), { recursive: true });

const reports = [];
const trialPaths = [];
for (let trial = 1; trial <= releaseTrialCount; trial += 1) {
  const trialPath = join(dirname(output), `browser-acceptance-trial-${trial}.json`);
  const result = childProcess.spawnSync(process.execPath, [join(scriptDirectory, "browser-acceptance.mjs")], {
    cwd: repository,
    encoding: "utf8",
    env: {
      ...process.env,
      WALKIE_ACCEPTANCE_REPORT: trialPath,
      WALKIE_ENFORCE_LATENCY: "0",
    },
    maxBuffer: 8 * 1024 * 1024,
  });
  if (result.status !== 0) {
    process.stderr.write(result.stderr || result.stdout);
  }
  assert.equal(result.status, 0, `browser acceptance trial ${trial} failed functionally`);
  trialPaths.push(trialPath);
  reports.push(JSON.parse(readFileSync(trialPath, "utf8")));
}

const evaluation = evaluateReleaseTrials(reports);
const aggregate = {
  schema: 2,
  capturedAt: new Date().toISOString(),
  releaseDist: reports[0].releaseDist,
  allAroundKeyboard: reports[0].allAroundKeyboard,
  latencyBudgetsMs,
  fixedTrialPolicy: {
    trialCount: releaseTrialCount,
    requiredPassingTrials,
    grossCeilingMultiplier,
  },
  passingTrials: evaluation.passingTrials,
  outcome: evaluation.outcome,
  trials: reports.map((report, index) => ({
    trial: index + 1,
    report: relative(repository, trialPaths[index]),
    capturedAt: report.capturedAt,
    room: report.room,
    sampleCount: report.sampleCount,
    warmupSamplesExcluded: report.warmupSamplesExcluded,
    hostCondition: report.hostCondition,
    steadyStateLatencyMs: report.steadyStateLatencyMs,
    reconnectMs: report.reconnectMs,
    evaluation: evaluation.trials[index],
  })),
};
writeFileSync(output, `${JSON.stringify(aggregate, null, 2)}\n`);
console.log(
  JSON.stringify(
    {
      outcome: aggregate.outcome,
      passingTrials: aggregate.passingTrials,
      requiredPassingTrials,
      report: relative(repository, output),
      trials: aggregate.trials.map(trial => ({
        trial: trial.trial,
        strictPassed: trial.evaluation.strictPassed,
        grossPassed: trial.evaluation.grossPassed,
        hostLoadOneMinute: trial.hostCondition?.finished?.loadAverage?.oneMinute,
        p95Ms: Object.fromEntries(
          trial.evaluation.checks
            .filter(check => check.id !== "reconnect")
            .map(check => [check.id, check.actualMs]),
        ),
        reconnectMs: trial.reconnectMs,
      })),
    },
    null,
    2,
  ),
);

const grossFailure = evaluation.trials
  .flatMap((trial, index) =>
    trial.checks.filter(check => !check.grossPassed).map(check => ({ trial: index + 1, ...check })),
  )
  .at(0);
assert.ok(
  !grossFailure,
  `browser trial ${grossFailure?.trial} ${grossFailure?.label} ${grossFailure?.actualMs.toFixed(1)}ms exceeded the gross ${grossFailure?.grossCeilingMs}ms ceiling`,
);
assert.ok(
  evaluation.passingTrials >= requiredPassingTrials,
  `${evaluation.passingTrials}/${releaseTrialCount} browser trials met every latency budget; ${requiredPassingTrials} required`,
);
