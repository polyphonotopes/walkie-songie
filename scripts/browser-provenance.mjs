import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

function git(repository, args, encoding = "utf8") {
  return execFileSync("git", args, {
    cwd: repository,
    encoding,
    maxBuffer: 64 * 1024 * 1024,
  });
}

export function sourceProvenance(repository) {
  const candidateRevision = git(repository, ["rev-parse", "HEAD"]).trim();
  const status = git(repository, ["status", "--porcelain=v1"]);
  const patch = git(repository, ["diff", "--binary", "HEAD", "--", "."], null);
  const untracked = git(
    repository,
    ["ls-files", "--others", "--exclude-standard", "-z"],
    "utf8",
  )
    .split("\0")
    .filter(Boolean)
    .sort();
  const digest = createHash("sha256");
  digest.update("tracked-and-staged-diff\0");
  digest.update(patch);
  for (const path of untracked) {
    digest.update("\0untracked\0");
    digest.update(path);
    digest.update("\0");
    digest.update(readFileSync(join(repository, path)));
  }
  return {
    candidateRevision,
    dirty: status.length > 0,
    dirtyPatchSha256: digest.digest("hex"),
    untrackedFileCount: untracked.length,
  };
}

export function hhhsPinFromLock(repository) {
  const lock = readFileSync(join(repository, "Cargo.lock"), "utf8");
  const pins = new Set(
    [...lock.matchAll(/git\+https:\/\/gitlab\.com\/micahscopes\/hhhs-rs\.git\?rev=([0-9a-f]{40})#/g)].map(
      (match) => match[1],
    ),
  );
  if (pins.size !== 1) {
    throw new Error(`expected one exact HHHS lock pin, found ${JSON.stringify([...pins])}`);
  }
  return [...pins][0];
}

function filesUnder(root, directory = root) {
  const files = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...filesUnder(root, path));
    else if (entry.isFile()) files.push(path);
  }
  return files.sort((left, right) => relative(root, left).localeCompare(relative(root, right)));
}

export function artifactTreeSha256(dist) {
  const digest = createHash("sha256");
  for (const file of filesUnder(dist)) {
    digest.update(relative(dist, file));
    digest.update("\0");
    digest.update(readFileSync(file));
    digest.update("\0");
  }
  return digest.digest("hex");
}

export function artifactContains(dist, needle) {
  const bytes = Buffer.from(needle);
  return filesUnder(dist).some((file) => {
    if (!existsSync(file) || !statSync(file).isFile()) return false;
    return readFileSync(file).includes(bytes);
  });
}
