/**
 * End-to-end tests through a private vmlayerd and the Docker backend.
 * Mirrors sdk/python/tests/test_live.py. Needs a built daemon
 * (VMLAYERD_BIN, else target/debug/vmlayerd) and Docker.
 */

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  BackendUnsupported,
  Client,
  ExecTimeout,
  Sandbox,
  SandboxNotFound,
  SnapshotNotFound,
  TimeoutError,
  Volume,
  VolumeBusy,
  VolumeNotFound,
  setDefaultClient,
} from "../src/index.js";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const ALPINE = "alpine:3.20";
const PY = "python:3.12-alpine";
const SERVE =
  "mkdir -p /srv && echo served > /srv/index.html && cd /srv && exec python3 -m http.server 8080";

let daemon: ChildProcess | undefined;
let client: Client;
let tmp: string;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

before(async () => {
  const bin = process.env.VMLAYERD_BIN ?? path.join(REPO, "target", "debug", "vmlayerd");
  assert.ok(fs.existsSync(bin), `daemon binary missing at ${bin}; cargo build -p vmlayerd`);
  const root = process.env.VMLAYER_TEST_ROOT ?? path.join(os.homedir(), ".vmlayer-tests");
  fs.mkdirSync(root, { recursive: true });
  tmp = fs.mkdtempSync(path.join(root, "ts-"));
  const sock = path.join(tmp, "d.sock");
  const log = fs.openSync(path.join(tmp, "daemon.log"), "w");
  daemon = spawn(bin, ["--listen", `unix://${sock}`, "--data-dir", path.join(tmp, "data")], {
    stdio: ["ignore", log, log],
  });
  client = new Client(`unix://${sock}`);
  const deadline = Date.now() + 30_000;
  for (;;) {
    try {
      if (await client.health()) break;
    } catch {
      /* not up yet */
    }
    assert.ok(Date.now() < deadline, `daemon did not become healthy; see ${tmp}/daemon.log`);
    await sleep(100);
  }
  const info = await client.info();
  assert.ok(
    info.backends.some((b) => b.available),
    "no backend available on this host",
  );
  setDefaultClient(client);
});

after(async () => {
  for (const sb of await client.listSandboxes()) await sb.kill().catch(() => {});
  client.close();
  daemon?.kill("SIGTERM");
  await sleep(300);
  if (process.env.VMLAYER_KEEP_TEST_DATA !== "1") {
    try {
      fs.rmSync(tmp, { recursive: true, force: true });
    } catch {
      /* files created as root inside containers may be undeletable here */
    }
  }
});

test("info and backends", async () => {
  const info = await client.info();
  assert.ok(info.daemonVersion);
  assert.ok("docker" in (await client.availableBackends()));
});

test("exec basics, labels, connect, kill", async () => {
  const sb = await Sandbox.create(ALPINE, { env: { GREETING: "hi" }, labels: { suite: "ts" } });
  try {
    assert.equal(sb.backend, "docker");
    assert.equal(await sb.state(), "running");
    assert.ok(sb.capabilities.snapshot && !sb.capabilities.fork);
    const r = await sb.exec("echo $GREETING; pwd; echo oops >&2; exit 3");
    assert.deepEqual([r.exitCode, r.stdout, r.stderr, r.ok], [3, "hi\n/work\n", "oops\n", false]);
    assert.ok(r.output.includes("oops") && r.output.includes("hi"));
    assert.equal((await sb.exec(["sh", "-c", "pwd"], { cwd: "/tmp" })).stdout, "/tmp\n");
    assert.equal((await sb.exec("cat", { stdin: "ping" })).stdout, "ping");
    const listed = await client.listSandboxes({ labels: { suite: "ts" } });
    assert.ok(listed.some((s) => s.id === sb.id));
    assert.equal((await Sandbox.connect(sb.id)).id, sb.id);
  } finally {
    await sb.kill();
  }
  await assert.rejects(sb.refresh(), SandboxNotFound);
  await sb.kill(); // idempotent
});

test("exec timeout keeps partial output", async () => {
  await using sb = await Sandbox.create(ALPINE);
  await assert.rejects(
    sb.exec("echo started; sleep 30; echo never", { timeoutMs: 2000 }),
    (e: unknown) =>
      e instanceof ExecTimeout && e.partial.stdout === "started\n" && e.partial.signal === 9,
  );
});

test("spawn, stdin, re-attach, kill", async () => {
  await using sb = await Sandbox.create(ALPINE);
  const p = await sb.spawn("cat");
  p.stdin.write("a\n");
  p.stdin.write("b\n");
  p.stdin.end();
  const chunks: Buffer[] = [];
  for await (const c of p) chunks.push(c.data);
  assert.equal(Buffer.concat(chunks).toString(), "a\nb\n");
  assert.ok((await p.wait()).ok);
  assert.equal((await sb.process(p.pid).wait()).stdout, "a\nb\n");

  const long = await sb.spawn("sleep 60");
  await sleep(500);
  assert.ok((await sb.processes()).some((pi) => pi.pid === long.pid));
  await long.kill();
  assert.equal((await long.wait()).signal, 9);
});

test("files", async () => {
  await using sb = await Sandbox.create(ALPINE);
  await sb.files.write("/work/a/b.txt", "hello", { mode: 0o600 });
  assert.equal(await sb.files.readText("/work/a/b.txt"), "hello");
  assert.ok((await sb.files.exists("/work/a/b.txt")) && !(await sb.files.exists("/work/nope")));
  const st = await sb.files.stat("/work/a/b.txt");
  assert.deepEqual([st.size, st.mode, st.isDir, st.name], [5, 0o600, false, "b.txt"]);
  await sb.files.mkdir("/work/a/d");
  assert.deepEqual(
    (await sb.files.list("/work/a")).map((e) => e.name),
    ["b.txt", "d"],
  );
  const local = fs.mkdtempSync(path.join(tmp, "tree-"));
  fs.mkdirSync(path.join(local, "sub"));
  fs.writeFileSync(path.join(local, "x.txt"), "x");
  fs.writeFileSync(path.join(local, "sub", "y.txt"), "y");
  await sb.files.upload(local, "/work/tree");
  assert.equal(await sb.files.readText("/work/tree/sub/y.txt"), "y");
  const out = path.join(tmp, "out");
  await sb.files.download("/work/tree", out);
  assert.equal(fs.readFileSync(path.join(out, "sub", "y.txt"), "utf8"), "y");
  await sb.files.remove("/work/a", { recursive: true });
  assert.equal(await sb.files.exists("/work/a"), false);
});

test("workspace volume outlives sandboxes; single writer; volume snapshots", async () => {
  const vol = await Volume.create({ name: "ts-proj", sizeMb: 512, labels: { suite: "ts" } });
  try {
    {
      await using a = await Sandbox.create(ALPINE, { workspace: vol });
      assert.equal((await a.workspace())?.id, vol.id);
      await a.exec("echo v1 > /work/state.txt");
      assert.equal(await vol.attachedTo(), a.id);
      await assert.rejects(Sandbox.create(ALPINE, { workspace: "ts-proj" }), VolumeBusy);
      await using ro = await Sandbox.create(ALPINE, { mounts: { "/data": [vol, "ro"] } });
      assert.equal((await ro.exec("cat /data/state.txt")).stdout, "v1\n");
      assert.ok(!(await ro.exec("touch /data/x")).ok);
    }
    assert.equal(await vol.attachedTo(), null);
    {
      await using b = await Sandbox.create(PY, { workspace: vol });
      assert.equal((await b.exec("cat /work/state.txt")).stdout, "v1\n");
      await b.exec("echo v2 > /work/state.txt");
    }
    const ckpt = await vol.snapshot({ labels: { suite: "ts" } });
    assert.ok(ckpt.sizeBytes > 0);
    const restored = await Volume.fromSnapshot(ckpt, { name: "ts-proj-restored" });
    try {
      await using d = await Sandbox.create(ALPINE, { workspace: restored });
      assert.equal((await d.exec("cat /work/state.txt")).stdout, "v2\n");
    } finally {
      await restored.delete();
    }
    await vol.deleteSnapshot(ckpt);
  } finally {
    await vol.delete();
  }
  await assert.rejects(Volume.get("ts-proj"), VolumeNotFound);
});

test("sandbox snapshot and restore", async () => {
  await using sb = await Sandbox.create(ALPINE, { labels: { suite: "ts" } });
  await sb.exec("echo keep > /work/w.txt; echo root > /root.txt");
  const snap = await sb.snapshot({ labels: { suite: "ts" } });
  assert.ok(!snap.includesMemory && snap.engineVersion);
  assert.equal(snap.volumes[0]?.guestPath, "/work");
  await sb.exec("echo changed > /work/w.txt");
  {
    await using r = await Sandbox.restore(snap);
    assert.equal((await r.exec("cat /work/w.txt")).stdout, "keep\n");
    assert.equal((await r.exec("cat /root.txt")).stdout, "root\n");
    assert.notEqual((await r.workspace())?.id, (await sb.workspace())?.id);
    assert.equal((await sb.exec("cat /work/w.txt")).stdout, "changed\n");
  }
  assert.ok(
    (await client.listSnapshots({ labels: { suite: "ts" } })).some((s) => s.id === snap.id),
  );
  await client.deleteSnapshot(snap.id);
  await assert.rejects(Sandbox.restore(snap), SnapshotNotFound);
  await assert.rejects(sb.fork(2), BackendUnsupported);
});

test("entrypoint, waitForPort, expose, pause", async () => {
  await using sb = await Sandbox.create(PY, { entrypoint: ["sh", "-c", SERVE] });
  await sb.waitForPort(8080, { timeoutMs: 20_000 });
  assert.ok((await sb.processes()).some((p) => p.isEntrypoint && p.pid === 1));
  const ep = await sb.expose(8080);
  const body = await new Promise<string>((resolve, reject) => {
    const s = net.createConnection({ host: ep.host, port: ep.port }, () => {
      s.write("GET /index.html HTTP/1.0\r\nHost: x\r\n\r\n");
    });
    let data = "";
    s.on("data", (d) => (data += d.toString()));
    s.on("end", () => resolve(data));
    s.on("error", reject);
  });
  assert.ok(body.includes("served"));
  assert.deepEqual(await sb.exposed(), { 8080: ep });
  await sb.unexpose(8080);
  await assert.rejects(sb.waitForPort(9999, { timeoutMs: 1000 }), TimeoutError);
  await sb.pause();
  assert.equal(await sb.state(), "paused");
  await sb.resume();
  assert.equal(await sb.state(), "running");
});

test("pty", async () => {
  await using sb = await Sandbox.create(ALPINE);
  const pty = sb.pty("sh", { size: { cols: 100, rows: 30 } });
  await sleep(500);
  pty.write("stty size; echo marker-$((1+1))\n");
  let seen = "";
  const deadline = Date.now() + 15_000;
  for await (const chunk of pty) {
    seen += chunk.toString();
    if (seen.includes("marker-2") || Date.now() > deadline) break;
  }
  pty.close();
  assert.ok(seen.includes("30 100") && seen.includes("marker-2"), seen);
});

test("ttl kills the sandbox", async () => {
  const sb = await Sandbox.create(ALPINE, { timeoutMs: 3000 });
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    try {
      await sb.refresh();
      await sleep(500);
    } catch (e) {
      assert.ok(e instanceof SandboxNotFound);
      return;
    }
  }
  assert.fail("sandbox outlived its ttl");
});
