/** Connection to a `vmlayerd` daemon. */

import { Connection, type ConnectionOptions, labels } from "./rpc.js";
import type { Backend, BackendInfo, Capabilities, ImageInfo, SystemInfo } from "./types.js";

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
    egressPolicy: !!c?.egress_policy,
    ioLimits: !!c?.io_limits,
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
      blobStore: r.blob_store || null,
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

  /** Pull an image ahead of time (and build its root filesystem on microVM backends). */
  async pullImage(image: string, opts: { backend?: Backend } = {}): Promise<ImageInfo> {
    const r: any = await this.conn.call(
      this.conn.system,
      "PullImage",
      { image, backend: opts.backend ?? "auto" },
      1_800_000,
    );
    return {
      image: r.image,
      backend: r.backend,
      rootfsKind: r.rootfs_kind,
      rootfsPath: r.rootfs_path || null,
      entrypoint: r.entrypoint ?? [],
      workdir: r.workdir || null,
    };
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

  /** This daemon's volume snapshots, or with `remote` every one in its blob store. */
  async listVolumeSnapshots(
    filter: { labels?: Record<string, string>; remote?: boolean } = {},
  ): Promise<import("./types.js").VolumeSnapshot[]> {
    const { volumeSnapshotFrom } = await import("./volume.js");
    const r: any = await this.conn.call(
      this.conn.volumes,
      "ListSnapshots",
      { labels: labels(filter.labels), remote: !!filter.remote },
      300_000,
    );
    return (r.snapshots ?? []).map(volumeSnapshotFrom);
  }

  /**
   * Delete blob-store chunks no snapshot references any more. Chunks younger
   * than the daemon's grace period are kept, so uploads in progress are safe.
   */
  async collectGarbage(
    opts: { dryRun?: boolean } = {},
  ): Promise<import("./types.js").GarbageCollection> {
    const r: any = await this.conn.call(
      this.conn.system,
      "CollectGarbage",
      { dry_run: !!opts.dryRun },
      3_600_000,
    );
    return {
      snapshots: Number(r.snapshots ?? 0),
      chunks: Number(r.chunks ?? 0),
      referencedChunks: Number(r.referenced_chunks ?? 0),
      recentChunks: Number(r.recent_chunks ?? 0),
      deletedChunks: Number(r.deleted_chunks ?? 0),
      deletedBytes: Number(r.deleted_bytes ?? 0),
    };
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
