#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  artifactContains,
  artifactTreeSha256,
  hhhsPinFromLock,
  sourceProvenance,
} from "./browser-provenance.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(scriptDirectory, "..");
const dist = resolve(
  process.env.WALKIE_RELEASE_DIST ?? join(repository, "target/session-production-web"),
);
const reportPath = resolve(
  process.env.WALKIE_PRODUCTION_SAFETY_REPORT ??
    join(repository, "output/playwright/browser-production-safety.json"),
);
const runStartedAt = new Date();
rmSync(reportPath, { force: true });
const artifactProfile = process.env.WALKIE_ARTIFACT_PROFILE ?? "unspecified";

assert.equal(
  artifactProfile,
  "production",
  "renewal-cut exclusion must be proved against the explicitly identified production artifact",
);

const forbidden = [
  "renewalCut",
  "FloorPersistedBeforeEgressCut",
  "injected renewal crash cut",
];
const surviving = forbidden.filter((needle) => artifactContains(dist, needle));
assert.deepEqual(
  surviving,
  [],
  `production artifact retained acceptance-only renewal fault controls: ${surviving.join(", ")}`,
);

const report = {
  schema: "walkie.browser-production-safety@1",
  runStartedAt: runStartedAt.toISOString(),
  capturedAt: new Date().toISOString(),
  releaseDist: relative(repository, dist),
  provenance: {
    ...sourceProvenance(repository),
    hhhsPin: hhhsPinFromLock(repository),
    artifactSha256: artifactTreeSha256(dist),
    artifactProfile,
  },
  assertions: {
    renewalCutQueryAbsent: true,
    renewalCutTraceStageAbsent: true,
    renewalCutDiagnosticAbsent: true,
  },
};

mkdirSync(dirname(reportPath), { recursive: true });
writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
assert.ok(
  statSync(reportPath).mtimeMs >= runStartedAt.getTime(),
  "production-safety report predates the active run",
);
console.log(JSON.stringify(report, null, 2));
