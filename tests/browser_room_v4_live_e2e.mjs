#!/usr/bin/env node

/**
 * Opt-in Room-v4 browser release gate.
 *
 * Prerequisites:
 *   - a production web build served at WALKIE_WEB_URL (default :4173)
 *   - Chromium with remote debugging at WALKIE_CDP_URL (default :9222)
 *   - `room-v4-native-probe` built with `--features native-net`
 *
 * The harness creates and disposes isolated browser contexts. It never selects,
 * navigates, or closes a pre-existing tab in the attached browser.
 */

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { resolve } from "node:path";

const WEB_URL = process.env.WALKIE_WEB_URL ?? "http://127.0.0.1:4173";
const CDP_URL = process.env.WALKIE_CDP_URL ?? "http://127.0.0.1:9222";
const NATIVE_PROBE = resolve(
  process.env.WALKIE_NATIVE_PROBE ?? "target/debug/room-v4-native-probe",
);
const STEP_TIMEOUT_MS = Number(process.env.WALKIE_E2E_TIMEOUT_MS ?? 70_000);

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.eventListeners = new Map();
    socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (message.id === undefined) {
        for (const listener of this.eventListeners.get(message.sessionId) ?? []) {
          listener(message);
        }
        return;
      }
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
      else pending.resolve(message.result ?? {});
    });
    socket.addEventListener("close", () => {
      for (const pending of this.pending.values()) {
        pending.reject(new Error("Chrome DevTools connection closed"));
      }
      this.pending.clear();
    });
  }

  static async connect(baseUrl) {
    const version = await fetch(new URL("/json/version", baseUrl));
    assert.equal(version.ok, true, `CDP version request failed: ${version.status}`);
    const { webSocketDebuggerUrl } = await version.json();
    assert.ok(webSocketDebuggerUrl, "CDP did not publish a browser WebSocket URL");
    const socket = new WebSocket(webSocketDebuggerUrl);
    await new Promise((resolveOpen, reject) => {
      socket.addEventListener("open", resolveOpen, { once: true });
      socket.addEventListener("error", reject, { once: true });
    });
    return new Cdp(socket);
  }

  send(method, params = {}, sessionId = undefined) {
    const id = this.nextId++;
    const message = { id, method, params };
    if (sessionId !== undefined) message.sessionId = sessionId;
    return new Promise((resolveMessage, reject) => {
      this.pending.set(id, { resolve: resolveMessage, reject });
      this.socket.send(JSON.stringify(message));
    });
  }

  on(sessionId, listener) {
    const listeners = this.eventListeners.get(sessionId) ?? [];
    listeners.push(listener);
    this.eventListeners.set(sessionId, listeners);
  }

  async page(url) {
    const { browserContextId } = await this.send("Target.createBrowserContext", {
      disposeOnDetach: true,
    });
    const { targetId } = await this.send("Target.createTarget", {
      url: "about:blank",
      browserContextId,
    });
    const { sessionId } = await this.send("Target.attachToTarget", {
      targetId,
      flatten: true,
    });
    await this.send("Page.enable", {}, sessionId);
    await this.send("Runtime.enable", {}, sessionId);
    const page = new Page(this, browserContextId, sessionId, url);
    await page.navigate(url);
    return page;
  }

  async close() {
    this.socket.close();
  }
}

class Page {
  constructor(cdp, contextId, sessionId, url) {
    this.cdp = cdp;
    this.contextId = contextId;
    this.sessionId = sessionId;
    this.url = url;
    this.console = [];
    cdp.on(sessionId, (message) => {
      if (message.method !== "Runtime.consoleAPICalled") return;
      const values = message.params.args.map((arg) => arg.value ?? arg.description ?? "");
      this.console.push(`${message.params.type}: ${values.join(" ")}`);
    });
  }

  async navigate(url = this.url) {
    this.url = url;
    await this.cdp.send("Page.navigate", { url }, this.sessionId);
    await this.waitFor(
      `document.readyState === "complete" && document.querySelector(".keyboard") !== null`,
      "browser app to render",
    );
  }

  async reload() {
    await this.cdp.send("Page.reload", { ignoreCache: true }, this.sessionId);
    await this.waitFor(`document.readyState === "complete"`, "page reload");
  }

  async evaluate(expression) {
    const response = await this.cdp.send(
      "Runtime.evaluate",
      {
        expression,
        awaitPromise: true,
        returnByValue: true,
        userGesture: true,
      },
      this.sessionId,
    );
    if (response.exceptionDetails) {
      throw new Error(
        response.exceptionDetails.exception?.description ??
          response.exceptionDetails.text ??
          "browser evaluation failed",
      );
    }
    return response.result?.value;
  }

  async waitFor(expression, description, timeoutMs = STEP_TIMEOUT_MS) {
    const started = Date.now();
    let lastError;
    while (Date.now() - started < timeoutMs) {
      try {
        if (await this.evaluate(expression)) return;
      } catch (error) {
        lastError = error;
      }
      await delay(200);
    }
    const body = await this.evaluate("document.body?.innerText ?? ''").catch(() => "");
    throw new Error(
      `timed out waiting for ${description}${lastError ? ` (${lastError.message})` : ""}\n${body}\nconsole:\n${this.console.join("\n")}`,
    );
  }

  async close() {
    await this.cdp
      .send("Target.disposeBrowserContext", { browserContextId: this.contextId })
      .catch(() => {});
  }
}

class NativePeer {
  constructor(binary, room) {
    this.child = spawn(binary, [room], {
      cwd: process.cwd(),
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.events = [];
    this.waiters = [];
    this.stderr = "";
    this.closed = false;
    this.child.stderr.setEncoding("utf8");
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk;
    });
    this.child.stdout.setEncoding("utf8");
    let pending = "";
    this.child.stdout.on("data", (chunk) => {
      pending += chunk;
      while (pending.includes("\n")) {
        const newline = pending.indexOf("\n");
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line.trim()) continue;
        let event;
        try {
          event = JSON.parse(line);
        } catch (error) {
          this.failWaiters(new Error(`native probe emitted non-JSON: ${line}`));
          continue;
        }
        this.events.push(event);
        this.flushWaiters();
      }
    });
    this.child.on("error", (error) => this.failWaiters(error));
    this.child.on("exit", (code, signal) => {
      this.closed = true;
      this.failWaiters(
        new Error(
          `native probe exited (code=${code}, signal=${signal})${this.stderr ? `\n${this.stderr}` : ""}`,
        ),
      );
    });
  }

  failWaiters(error) {
    for (const waiter of this.waiters.splice(0)) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
  }

  flushWaiters() {
    for (let index = this.waiters.length - 1; index >= 0; index--) {
      const waiter = this.waiters[index];
      const event = this.events
        .slice(waiter.after)
        .find((candidate) => waiter.predicate(candidate));
      if (!event) continue;
      this.waiters.splice(index, 1);
      clearTimeout(waiter.timer);
      waiter.resolve(event);
    }
  }

  waitFor(predicate, description, after = 0, timeoutMs = STEP_TIMEOUT_MS) {
    const existing = this.events.slice(after).find(predicate);
    if (existing) return Promise.resolve(existing);
    if (this.closed) return Promise.reject(new Error("native probe is already closed"));
    return new Promise((resolveEvent, reject) => {
      const waiter = {
        predicate,
        after,
        resolve: resolveEvent,
        reject,
        timer: setTimeout(() => {
          const index = this.waiters.indexOf(waiter);
          if (index !== -1) this.waiters.splice(index, 1);
          reject(
            new Error(
              `timed out waiting for native ${description}\n${JSON.stringify(this.events, null, 2)}\n${this.stderr}`,
            ),
          );
        }, timeoutMs),
      };
      this.waiters.push(waiter);
    });
  }

  send(command) {
    this.child.stdin.write(`${JSON.stringify(command)}\n`);
  }

  async status() {
    const after = this.events.length;
    this.send({ cmd: "status" });
    return this.waitFor((event) => event.event === "status", "status", after);
  }

  async shutdown() {
    if (this.closed) return;
    const after = this.events.length;
    this.send({ cmd: "shutdown" });
    await this.waitFor((event) => event.event === "shutdown", "shutdown", after, 10_000).catch(
      () => this.child.kill("SIGTERM"),
    );
  }
}

function delay(milliseconds) {
  return new Promise((resolveDelay) => setTimeout(resolveDelay, milliseconds));
}

function letters(value) {
  let number = BigInt(value);
  let result = "";
  do {
    result += String.fromCharCode(97 + Number(number % 26n));
    number /= 26n;
  } while (number > 0n);
  return result.padEnd(6, "a").slice(0, 10);
}

function roomName(label) {
  return `calm-${label}-${letters(BigInt(Date.now()) + BigInt(Math.floor(Math.random() * 1_000_000)))}`;
}

function roomUrl(room, peer) {
  const url = new URL(WEB_URL);
  url.searchParams.set("peer", peer);
  url.hash = room;
  return url.href;
}

const pressedExpression = (degree) =>
  `document.querySelector('.toggle-overlay[data-key-overlay="${36 + degree}"]') !== null`;
const pieceExpression = (emoji) =>
  `[...document.querySelectorAll(".piece-indicator")].some((piece) => piece.getAttribute("data-emoji") === ${JSON.stringify(emoji)})`;

const idbJournalFunction = `
  async () => {
    const request = indexedDB.open("walkie-songie", 1);
    const db = await new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    const tx = db.transaction("settings", "readonly");
    const store = tx.objectStore("settings");
    const keysRequest = store.getAllKeys();
    const keys = await new Promise((resolve, reject) => {
      keysRequest.onsuccess = () => resolve(keysRequest.result);
      keysRequest.onerror = () => reject(keysRequest.error);
    });
    const key = keys.find((candidate) => String(candidate).startsWith("opjournal:v4:"));
    if (!key) throw new Error("v4 journal key is absent");
    const valueRequest = store.get(key);
    const value = await new Promise((resolve, reject) => {
      valueRequest.onsuccess = () => resolve(valueRequest.result);
      valueRequest.onerror = () => reject(valueRequest.error);
    });
    return { key: String(key), bytes: [...new Uint8Array(value)] };
  }
`;

async function auditBrowserJournal(page) {
  const journal = await page.evaluate(`(${idbJournalFunction})()`);
  const bytes = Uint8Array.from(journal.bytes);
  const marker = new TextEncoder().encode("walkie-songie/idb-op-journal/4\0");
  assert.deepEqual([...bytes.slice(0, marker.length)], [...marker], "journal has exact v4 marker");
  const musicMagic = new TextEncoder().encode("tutti.music.wire/2\0");
  const extensionMagic = new TextEncoder().encode("walkie.ext.wire/4\0");
  const records = [];
  let offset = marker.length;
  while (offset < bytes.length) {
    assert.ok(offset + 5 <= bytes.length, "complete v4 record header");
    const tag = bytes[offset];
    const length = new DataView(bytes.buffer, bytes.byteOffset + offset + 1, 4).getUint32(0, true);
    const start = offset + 5;
    const end = start + length;
    assert.ok(end <= bytes.length, "complete v4 record body");
    const wire = bytes.slice(start, end);
    if (tag === 1) {
      assert.equal(startsWith(wire, musicMagic), true, "music tag carries music wire");
      assert.equal(containsBytes(wire, extensionMagic), false, "music record has no extension wire");
    } else if (tag === 2) {
      assert.equal(startsWith(wire, extensionMagic), true, "extension tag carries extension wire");
      assert.equal(containsBytes(wire, musicMagic), false, "extension record has no music wire");
    } else {
      assert.fail(`unknown journal lane tag ${tag}`);
    }
    records.push(tag);
    offset = end;
  }
  assert.ok(records.includes(1), "browser journal contains the music lane");
  assert.ok(records.includes(2), "browser journal contains the extension lane");
  return journal;
}

function startsWith(bytes, prefix) {
  return prefix.every((byte, index) => bytes[index] === byte);
}

function containsBytes(bytes, needle) {
  outer: for (let offset = 0; offset + needle.length <= bytes.length; offset++) {
    for (let index = 0; index < needle.length; index++) {
      if (bytes[offset + index] !== needle[index]) continue outer;
    }
    return true;
  }
  return false;
}

async function corruptJournal(page) {
  await page.evaluate(`(async () => {
    const journal = await (${idbJournalFunction})();
    sessionStorage.setItem("room-v4-journal-key", journal.key);
    sessionStorage.setItem("room-v4-journal-backup", JSON.stringify(journal.bytes));
    const corrupted = new Uint8Array(journal.bytes);
    if (corrupted.length < 6) throw new Error("journal has no complete record to corrupt");
    corrupted[corrupted.length - 1] ^= 0xff;
    const request = indexedDB.open("walkie-songie", 1);
    const db = await new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    const tx = db.transaction("settings", "readwrite");
    tx.objectStore("settings").put(corrupted, journal.key);
    await new Promise((resolve, reject) => {
      tx.oncomplete = resolve;
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
  })()`);
}

async function restoreJournal(page) {
  await page.evaluate(`(async () => {
    const key = sessionStorage.getItem("room-v4-journal-key");
    const backup = JSON.parse(sessionStorage.getItem("room-v4-journal-backup"));
    const request = indexedDB.open("walkie-songie", 1);
    const db = await new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    const tx = db.transaction("settings", "readwrite");
    tx.objectStore("settings").put(new Uint8Array(backup), key);
    await new Promise((resolve, reject) => {
      tx.oncomplete = resolve;
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
  })()`);
}

async function browserBrowserGate(cdp) {
  const room = roomName("browsers");
  const pageA = await cdp.page(roomUrl(room, "browser-a"));
  const pageB = await cdp.page(roomUrl(room, "browser-b"));
  try {
    await pageA.waitFor(
      `document.body.innerText.includes("synchronized")`,
      "browser A two-lane synchronization",
    );
    await pageB.waitFor(
      `document.body.innerText.includes("synchronized")`,
      "browser B two-lane synchronization",
    );

    await pageA.evaluate(
      `document.querySelector(".keyboard").dispatchEvent(new CustomEvent("keyclick", { bubbles: true, composed: true, detail: { index: 48, note: 0 } }))`,
    );
    await pageB.waitFor(pressedExpression(0), "browser/browser music gossip");

    await pageB.evaluate(`document.querySelector(".lock-button").click()`);
    await pageA.waitFor(
      `document.querySelector(".lock-button")?.textContent.includes("🔒")`,
      "browser/browser extension gossip",
    );
    await auditBrowserJournal(pageA);
    await auditBrowserJournal(pageB);

    await pageA.reload();
    await pageA.waitFor(pressedExpression(0), "music state after browser reload");
    await pageA.waitFor(
      `document.querySelector(".lock-button")?.textContent.includes("🔒")`,
      "extension state after browser reload",
    );

    await corruptJournal(pageA);
    await pageA.reload();
    await pageA.waitFor(
      `document.body.innerText.includes("durable room storage failed")`,
      "loud complete-record corruption refusal",
    );
    assert.equal(
      await pageA.evaluate(pressedExpression(0)),
      false,
      "corrupt durable history must not expose room state",
    );

    await restoreJournal(pageA);
    await pageA.reload();
    await pageA.waitFor(pressedExpression(0), "state after restoring valid journal");
    await pageA.waitFor(
      `document.querySelector(".lock-button")?.textContent.includes("🔒")`,
      "extension state after restoring valid journal",
    );
    return { room, journalIsolation: true, reload: true, corruptionRefusal: true };
  } finally {
    await pageB.close();
    await pageA.close();
  }
}

async function browserNativeGate(cdp) {
  const room = roomName("native");
  const native = new NativePeer(NATIVE_PROBE, room);
  let page;
  try {
    await native.waitFor((event) => event.event === "ready", "probe readiness");
    page = await cdp.page(roomUrl(room, "browser-native"));
    await native.waitFor((event) => event.event === "peer_up", "browser peer membership");
    await page.waitFor(
      `document.body.innerText.includes("synchronized")`,
      "browser/native two-lane synchronization",
    );

    await page.evaluate(
      `document.querySelector(".keyboard").dispatchEvent(new CustomEvent("keyclick", { bubbles: true, composed: true, detail: { index: 50, note: 2 } }))`,
    );
    await waitForNativeStatus(native, (status) => status.degrees.includes(2), "browser music at native");

    await page.evaluate(`document.querySelector(".lock-button").click()`);
    await waitForNativeStatus(native, (status) => status.pieces_locked, "browser extension at native");

    native.send({ cmd: "music", degree: 5, broadcast: true });
    native.send({ cmd: "piece", emoji: "🧪", degree: 7, broadcast: true });
    await page.waitFor(pressedExpression(5), "native music gossip at browser");
    await page.waitFor(pieceExpression("🧪"), "native extension gossip at browser");

    native.send({ cmd: "music", degree: 9, broadcast: false });
    await waitForNativeStatus(native, (status) => status.degrees.includes(9), "silent native commit");
    assert.equal(
      await page.evaluate(pressedExpression(9)),
      false,
      "deliberately dropped gossip creates a real browser/native gap",
    );
    await page.waitFor(
      pressedExpression(9),
      "periodic browser/native dropped-gossip repair",
      STEP_TIMEOUT_MS,
    );

    const status = await native.status();
    assert.deepEqual(status.violations, [], "no cross-lane RBSR bytes");
    assert.ok(status.music_frames > 0, "music repair exchanged frames");
    assert.ok(status.extension_frames > 0, "extension repair exchanged frames");
    assert.ok(status.degrees.includes(2), "native retained browser-authored music");
    assert.ok(status.degrees.includes(5), "native retained native-authored music");
    assert.ok(status.degrees.includes(9), "native retained dropped-gossip music");
    assert.ok(status.pieces.includes("🧪"), "native retained extension state");
    await auditBrowserJournal(page);
    return {
      room,
      browserNative: true,
      droppedGossipRepair: true,
      musicFrames: status.music_frames,
      extensionFrames: status.extension_frames,
      laneViolations: status.violations.length,
    };
  } finally {
    if (page) await page.close();
    await native.shutdown();
  }
}

async function waitForNativeStatus(native, predicate, description, timeoutMs = STEP_TIMEOUT_MS) {
  const started = Date.now();
  let last;
  while (Date.now() - started < timeoutMs) {
    last = await native.status();
    if (predicate(last)) return last;
    await delay(200);
  }
  throw new Error(`timed out waiting for ${description}: ${JSON.stringify(last)}`);
}

let cdp;
try {
  cdp = await Cdp.connect(CDP_URL);
  const browserBrowser = await browserBrowserGate(cdp);
  const browserNative = await browserNativeGate(cdp);
  process.stdout.write(
    `${JSON.stringify({ ok: true, browserBrowser, browserNative }, null, 2)}\n`,
  );
} catch (error) {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
} finally {
  if (cdp) await cdp.close();
}
