/** `Sandbox`: the unit of isolation and the entry point of the SDK. */

import { Client, capabilitiesFrom, getDefaultClient } from "./client.js";
import { ExecTimeout } from "./errors.js";
import { Files } from "./files.js";
import { Collector, Process, Pty, outputChunk } from "./process.js";
import { duration, labels } from "./rpc.js";
import type {
  Backend,
  Capabilities,
  Endpoint,
  ExecResult,
  IoLimits,
  ProcessInfo,
  PtySize,
  SandboxState,
  Snapshot,
} from "./types.js";
import { Volume, snapshotFrom } from "./volume.js";

const STATES: Record<string, SandboxState> = {
  SANDBOX_STATE_CREATING: "creating",
  SANDBOX_STATE_RUNNING: "running",
  SANDBOX_STATE_PAUSED: "paused",
  SANDBOX_STATE_STOPPED: "stopped",
  SANDBOX_STATE_FAILED: "failed",
};

function argvOf(cmd: string | string[]): string[] {
  if (typeof cmd === "string") return ["/bin/sh", "-c", cmd];
  if (cmd.length === 0) throw new Error("command must not be empty");
  return [...cmd];
}

export interface CreateOptions {
  cpus?: number;
  memoryMb?: number;
  diskMb?: number;
  env?: Record<string, string>;
  workdir?: string;
  /** `"auto"` (default): anonymous volume at workdir; a Volume or name; or `null` for none. */
  workspace?: Volume | string | "auto" | null;
  /** Extra volumes by guest path, optionally read-only. */
  mounts?: Record<string, Volume | [Volume, "ro" | "rw"]>;
  /** `"auto"` (default): the image's ENTRYPOINT/CMD; an argv; or `null` to start nothing. */
  entrypoint?: string[] | "auto" | null;
  /** Hard lifetime in ms; `null` disables. Default 15 minutes. */
  timeoutMs?: number | null;
  backend?: Backend;
  network?: boolean;
  /**
   * Where the sandbox may connect, e.g. `["pypi.org", "*.pythonhosted.org:443",
   * "10.0.0.0/8:5432"]`; everything else is rejected and DNS answers only
   * allowed names. `[]` blocks all egress. Omitted allows everything except
   * cloud metadata endpoints. Needs a backend with `capabilities.egressPolicy`.
   */
  egress?: string[];
  /** Network and disk rate limits. Needs a backend with `capabilities.ioLimits`. */
  limits?: IoLimits;
  /**
   * At most one live sandbox with this name across every daemon sharing the
   * blob store; a second create fails while the first lives.
   */
  lease?: string;
  /**
   * With `lease`: checkpoint the workspace to the blob store, and let a daemon
   * running with `--rescue` recreate the sandbox from it if this host dies.
   */
  reschedule?: boolean;
  labels?: Record<string, string>;
  client?: Client;
}

export interface ExecOptions {
  cwd?: string;
  env?: Record<string, string>;
  stdin?: Buffer | string;
  /** Default 5 minutes for `exec`; none for `spawn`. `null` disables. */
  timeoutMs?: number | null;
  /** Run as this user: a name in the guest's /etc/passwd, a UID, or "uid:gid". */
  user?: string;
}

/**
 * One isolated environment. Create with `Sandbox.create`, restore with
 * `Sandbox.restore`, attach to an existing one with `Sandbox.connect`.
 * Supports `await using sb = ...` (Symbol.asyncDispose calls `kill`).
 */
export class Sandbox {
  readonly files: Files;

  /** @internal */
  constructor(
    readonly client: Client,
    private pb: any,
  ) {
    this.files = new Files(client, pb.id);
  }

  get id(): string {
    return this.pb.id;
  }
  get backend(): Backend {
    return this.pb.backend as Backend;
  }
  get capabilities(): Capabilities {
    return capabilitiesFrom(this.pb.capabilities);
  }
  get image(): string {
    return this.pb.image;
  }
  get workdir(): string {
    return this.pb.workdir;
  }
  get labels(): Record<string, string> {
    return labels(this.pb.labels);
  }

  /** The volume mounted at workdir, or null if created with `workspace: null`. */
  async workspace(): Promise<Volume | null> {
    if (!this.pb.workspace_volume_id) return null;
    return Volume.get(this.pb.workspace_volume_id, this.client);
  }

  async state(): Promise<SandboxState> {
    await this.refresh();
    return STATES[this.pb.state] ?? "failed";
  }

  async exposed(): Promise<Record<number, Endpoint>> {
    await this.refresh();
    const out: Record<number, Endpoint> = {};
    for (const [p, e] of Object.entries(this.pb.exposed ?? {}))
      out[Number(p)] = endpoint((e as any).host, (e as any).port);
    return out;
  }

  async refresh(): Promise<this> {
    this.pb = await this.client.conn.call(this.client.conn.sandboxes, "Get", { id: this.id });
    return this;
  }

  // -- construction -------------------------------------------------------

  static async create(image: string, opts: CreateOptions = {}): Promise<Sandbox> {
    const c = opts.client ?? getDefaultClient();
    const req: any = {
      image,
      vcpus: opts.cpus ?? 0,
      memory_mib: opts.memoryMb ?? 0,
      env: opts.env ?? {},
      workdir: opts.workdir ?? "",
      backend: opts.backend ?? "auto",
      network: opts.network === false ? "NETWORK_MODE_NONE" : "NETWORK_MODE_EGRESS_AND_EXPOSE",
      labels: labels(opts.labels),
      ttl: duration(opts.timeoutMs === undefined ? 15 * 60_000 : opts.timeoutMs),
      mounts: [],
    };
    if (opts.diskMb !== undefined) req.disk_mib = opts.diskMb;
    if (opts.lease) req.lease = opts.lease;
    if (opts.reschedule) req.reschedule = true;
    if (opts.egress !== undefined) req.egress = { allow: opts.egress };
    if (opts.limits !== undefined) {
      req.limits = {};
      if (opts.limits.networkMbps !== undefined) req.limits.network_mbps = opts.limits.networkMbps;
      if (opts.limits.diskMbps !== undefined) req.limits.disk_mbps = opts.limits.diskMbps;
      if (opts.limits.diskIops !== undefined) req.limits.disk_iops = opts.limits.diskIops;
    }
    const ws = opts.workspace ?? "auto";
    req.workspace =
      ws === "auto"
        ? { anonymous: {} }
        : ws === null
          ? { none: {} }
          : { volume: typeof ws === "string" ? ws : ws.id };
    for (const [p, spec] of Object.entries(opts.mounts ?? {})) {
      const [vol, mode] = Array.isArray(spec) ? spec : [spec, "rw" as const];
      req.mounts.push({ volume_id: vol.id, guest_path: p, read_only: mode === "ro" });
    }
    const ep = opts.entrypoint ?? "auto";
    req.entrypoint =
      ep === "auto" ? { image_default: {} } : ep === null ? { none: {} } : { argv: { argv: ep } };
    return new Sandbox(c, await c.conn.call(c.conn.sandboxes, "Create", req, 600_000));
  }

  /** New sandbox from a snapshot; every volume comes back as a fresh clone. */
  static async restore(
    snapshot: Snapshot | string,
    opts: { timeoutMs?: number | null; labels?: Record<string, string>; client?: Client } = {},
  ): Promise<Sandbox> {
    const c = opts.client ?? getDefaultClient();
    const req = {
      snapshot_id: typeof snapshot === "string" ? snapshot : snapshot.id,
      ttl: duration(opts.timeoutMs === undefined ? 15 * 60_000 : opts.timeoutMs),
      labels: labels(opts.labels),
    };
    return new Sandbox(c, await c.conn.call(c.conn.sandboxes, "Restore", req, 600_000));
  }

  static async connect(id: string, client?: Client): Promise<Sandbox> {
    const c = client ?? getDefaultClient();
    return new Sandbox(c, await c.conn.call(c.conn.sandboxes, "Get", { id }));
  }

  // -- commands -------------------------------------------------------------

  /** Run to completion. A string goes through `/bin/sh -c`. Non-zero exit is not an error. */
  async exec(cmd: string | string[], opts: ExecOptions = {}): Promise<ExecResult> {
    const timeoutMs = opts.timeoutMs === undefined ? 5 * 60_000 : opts.timeoutMs;
    const req = {
      sandbox_id: this.id,
      argv: argvOf(cmd),
      cwd: opts.cwd ?? "",
      user: opts.user ?? "",
      env: opts.env ?? {},
      timeout: duration(timeoutMs),
      stdin: opts.stdin ? Buffer.from(opts.stdin) : Buffer.alloc(0),
      detach: false,
    };
    const collector = new Collector();
    let status: any;
    const rpcTimeout = timeoutMs === null ? 0 : timeoutMs + 30_000;
    for await (const ev of this.client.conn.stream<any>(
      this.client.conn.processes,
      "Exec",
      req,
      rpcTimeout,
    )) {
      if (ev.output) collector.add(outputChunk(ev.output));
      else if (ev.exited) status = ev.exited;
    }
    if (!status) throw new Error("exec stream ended without an exit status");
    const result = collector.result(status);
    if (status.timed_out && timeoutMs !== null) throw new ExecTimeout(timeoutMs, result);
    return result;
  }

  /** Start a command and return a streaming handle immediately. */
  async spawn(cmd: string | string[], opts: ExecOptions = {}): Promise<Process> {
    const req = {
      sandbox_id: this.id,
      argv: argvOf(cmd),
      cwd: opts.cwd ?? "",
      user: opts.user ?? "",
      env: opts.env ?? {},
      timeout: duration(opts.timeoutMs ?? null),
      detach: true,
    };
    let pid = 0;
    for await (const ev of this.client.conn.stream<any>(this.client.conn.processes, "Exec", req)) {
      if (ev.started) {
        pid = ev.started.pid;
        break;
      }
    }
    return new Process(this.client, this.id, pid, opts.timeoutMs ?? undefined);
  }

  async processes(): Promise<ProcessInfo[]> {
    const r: any = await this.client.conn.call(this.client.conn.processes, "List", {
      sandbox_id: this.id,
    });
    return (r.processes ?? []).map((p: any) => ({
      pid: p.pid,
      argv: p.argv ?? [],
      startedAt: p.started_at ? Number(p.started_at.seconds ?? 0) : 0,
      isEntrypoint: !!p.is_entrypoint,
    }));
  }

  /** Re-attach to a running process by pid; `process(1)` is the entrypoint. */
  process(pid: number): Process {
    return new Process(this.client, this.id, pid, undefined);
  }

  /** Interactive terminal. Requires `capabilities.pty`. */
  pty(
    cmd: string | string[] = "/bin/bash",
    opts: { size?: PtySize; cwd?: string; env?: Record<string, string>; user?: string } = {},
  ): Pty {
    const argv = typeof cmd === "string" ? [cmd] : [...cmd];
    const size = opts.size ?? { cols: 80, rows: 24 };
    return new Pty(this.client, {
      sandbox_id: this.id,
      argv,
      cwd: opts.cwd ?? "",
      env: opts.env ?? {},
      size,
      user: opts.user ?? "",
    });
  }

  // -- state ----------------------------------------------------------------

  async snapshot(
    opts: { labels?: Record<string, string>; stayPaused?: boolean } = {},
  ): Promise<Snapshot> {
    const r = await this.client.conn.call(
      this.client.conn.sandboxes,
      "TakeSnapshot",
      { id: this.id, labels: labels(opts.labels), stay_paused: !!opts.stayPaused },
      600_000,
    );
    return snapshotFrom(r);
  }

  async fork(count = 1): Promise<Sandbox[]> {
    const r: any = await this.client.conn.call(
      this.client.conn.sandboxes,
      "Fork",
      { id: this.id, count },
      600_000,
    );
    return (r.children ?? []).map((s: any) => new Sandbox(this.client, s));
  }

  async pause(): Promise<void> {
    await this.client.conn.call(this.client.conn.sandboxes, "Pause", { id: this.id });
  }

  async resume(): Promise<void> {
    await this.client.conn.call(this.client.conn.sandboxes, "Resume", { id: this.id });
  }

  /** Destroy the sandbox and release everything. Idempotent. */
  async kill(): Promise<void> {
    await this.client.conn.call(this.client.conn.sandboxes, "Kill", { id: this.id }, 120_000);
  }

  // -- network --------------------------------------------------------------

  /**
   * Make a guest port reachable from the host. `{ gateway: true }` also
   * publishes it on the daemon's authenticated HTTPS gateway: `url` is then
   * the gateway URL and `token` the credential to send.
   */
  async expose(port: number, opts: { gateway?: boolean } = {}): Promise<Endpoint> {
    const e: any = await this.client.conn.call(
      this.client.conn.sandboxes,
      "Expose",
      { id: this.id, guest_port: port, gateway: opts.gateway ?? false },
      120_000,
    );
    return endpoint(e.host, e.port, e.url, e.token);
  }

  async unexpose(port: number): Promise<void> {
    await this.client.conn.call(this.client.conn.sandboxes, "Unexpose", {
      id: this.id,
      guest_port: port,
    });
  }

  /** Block until something inside the guest listens on `port`. */
  async waitForPort(port: number, opts: { timeoutMs?: number } = {}): Promise<void> {
    const timeoutMs = opts.timeoutMs ?? 30_000;
    await this.client.conn.call(
      this.client.conn.sandboxes,
      "WaitForPort",
      { id: this.id, port, timeout: duration(timeoutMs) },
      timeoutMs + 10_000,
    );
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.kill();
  }
}

function endpoint(host: string, port: number, url?: string, token?: string): Endpoint {
  const e: Endpoint = { host, port, url: url || `http://${host}:${port}` };
  if (token) e.token = token;
  return e;
}
