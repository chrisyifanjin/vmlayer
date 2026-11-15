/** Connection to a `vmlayerd` daemon. */

import { Connection, type ConnectionOptions, labels } from "./rpc.js";
import type { Backend, BackendInfo, Capabilities, SystemInfo } from "./types.js";

export function capabilitiesFrom(c: any): Capabilities {
  return {
    snapshot: !!c?.snapshot,
    memorySnapshot: !!c?.memory_snapshot,
    fork: !!c?.fork,
    pause: !!c?.pause,
    pty: !!c?.pty,
    exposePort: !!c?.expose_port,
    volumes: !!c?.volumes,
    hotAttach: !!c?.hot_attach,
  };
}

/**
 * Most callers never construct one: `Sandbox.create` uses a module-level
 * default that reads `VMLAYER_ENDPOINT` or falls back to the local unix
 * socket. Build a `Client` explicitly for a remote daemon.
 */
export class Client {
  /** @internal */
  readonly conn: Connection;

  constructor(endpoint?: string, opts: Omit<ConnectionOptions, "endpoint"> = {}) {
    this.conn = new Connection(endpoint === undefined ? opts : { ...opts, endpoint });
  }

  get endpoint(): string {
    return this.conn.endpoint;
  }

  async info(): Promise<SystemInfo> {
    const r: any = await this.conn.call(this.conn.system, "Info", {});
    return {
      daemonVersion: r.daemon_version,
      backends: (r.backends ?? []).map((b: any): BackendInfo => ({
        name: b.name,
        engineVersion: b.engine_version,
        capabilities: capabilitiesFrom(b.capabilities),
        unavailableReason: b.unavailable_reason,
        available: !b.unavailable_reason,
      })),
      defaultBackend: r.default_backend,
      dataDir: r.data_dir,
    };
  }

  async health(): Promise<boolean> {
    const r: any = await this.conn.call(this.conn.system, "Health", {});
    return !!r.ok;
  }

  /** Backends usable on this host, with the engine version of each. */
  async availableBackends(): Promise<Partial<Record<Backend, string>>> {
    const out: Partial<Record<Backend, string>> = {};
    for (const b of (await this.info()).backends)
      if (b.available) out[b.name as Backend] = b.engineVersion;
    return out;
  }

  async listSandboxes(
    filter: { labels?: Record<string, string> } = {},
  ): Promise<import("./sandbox.js").Sandbox[]> {
    const { Sandbox } = await import("./sandbox.js");
    const r: any = await this.conn.call(this.conn.sandboxes, "List", {
      labels: labels(filter.labels),
    });
    return (r.sandboxes ?? []).map((s: any) => new Sandbox(this, s));
  }

  async listVolumes(
    filter: { labels?: Record<string, string> } = {},
  ): Promise<import("./volume.js").Volume[]> {
    const { Volume } = await import("./volume.js");
    const r: any = await this.conn.call(this.conn.volumes, "List", {
      labels: labels(filter.labels),
    });
    return (r.volumes ?? []).map((v: any) => new Volume(this, v));
  }

  async listSnapshots(
    filter: { labels?: Record<string, string> } = {},
  ): Promise<import("./types.js").Snapshot[]> {
    const { snapshotFrom } = await import("./volume.js");
    const r: any = await this.conn.call(this.conn.system, "ListSnapshots", {
      labels: labels(filter.labels),
    });
    return (r.snapshots ?? []).map(snapshotFrom);
  }

  async deleteSnapshot(id: string): Promise<void> {
    await this.conn.call(this.conn.system, "DeleteSnapshot", { id });
  }

  close(): void {
    this.conn.close();
  }
}

let defaultClient: Client | undefined;

/** The process-wide client used when none is passed explicitly. */
export function getDefaultClient(): Client {
  if (!defaultClient) defaultClient = new Client();
  return defaultClient;
}

export function setDefaultClient(client: Client | undefined): void {
  defaultClient = client;
}
