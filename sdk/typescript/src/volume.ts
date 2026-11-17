/** Persistent storage: `Volume`, and the plain-object `VolumeSnapshot` / `Snapshot` records. */

import { Client, getDefaultClient } from "./client.js";
import { fromTimestamp, labels } from "./rpc.js";
import type { Backend, Snapshot, VolumeSnapshot } from "./types.js";

export function volumeSnapshotFrom(s: any): VolumeSnapshot {
  return {
    id: s.id,
    volumeId: s.volume_id,
    guestPath: s.guest_path || null,
    createdAt: fromTimestamp(s.created_at),
    sizeBytes: Number(s.size_bytes ?? 0),
    durable: !!s.durable,
    labels: labels(s.labels),
    local: !!s.local,
  };
}

export function snapshotFrom(s: any): Snapshot {
  return {
    id: s.id,
    backend: s.backend,
    engineVersion: s.engine_version,
    cpuModel: s.cpu_model || null,
    createdAt: fromTimestamp(s.created_at),
    sizeBytes: Number(s.size_bytes ?? 0),
    includesMemory: !!s.includes_memory,
    volumes: (s.volumes ?? []).map(volumeSnapshotFrom),
    labels: labels(s.labels),
  };
}

export interface CreateVolumeOptions {
  name?: string;
  sizeMb?: number;
  labels?: Record<string, string>;
  /** Decides the on-disk representation (directory for Docker, ext4 image for microVMs). */
  backend?: Backend;
  client?: Client;
}

/**
 * Persistent storage with a lifetime independent of any sandbox. Read-write
 * attachment to one sandbox at a time; `restore` and `fork` attach clones,
 * never the original; anonymous volumes die with their sandbox.
 */
export class Volume {
  /** @internal */
  constructor(
    readonly client: Client,
    private pb: any,
  ) {}

  get id(): string {
    return this.pb.id;
  }
  get name(): string | null {
    return this.pb.name || null;
  }
  get sizeMb(): number {
    return Number(this.pb.size_mib ?? 0);
  }
  get labels(): Record<string, string> {
    return labels(this.pb.labels);
  }
  get anonymous(): boolean {
    return !!this.pb.anonymous;
  }

  /** Sandbox id holding the read-write attachment, if any. */
  async attachedTo(): Promise<string | null> {
    await this.refresh();
    return this.pb.attached_to || null;
  }

  async refresh(): Promise<this> {
    this.pb = await this.client.conn.call(this.client.conn.volumes, "Get", { volume: this.id });
    return this;
  }

  static async create(opts: CreateVolumeOptions = {}): Promise<Volume> {
    const c = opts.client ?? getDefaultClient();
    const req: any = {
      name: opts.name ?? "",
      labels: labels(opts.labels),
      backend: opts.backend ?? "auto",
    };
    if (opts.sizeMb !== undefined) req.size_mib = opts.sizeMb;
    return new Volume(c, await c.conn.call(c.conn.volumes, "Create", req));
  }

  /**
   * A volume whose contents are a copy-on-write clone of a snapshot (templates).
   * A durable snapshot without a local copy, including one pushed by another
   * daemon sharing the blob store, is downloaded first.
   */
  static async fromSnapshot(
    snapshot: VolumeSnapshot | string,
    opts: { name?: string; labels?: Record<string, string>; client?: Client } = {},
  ): Promise<Volume> {
    const c = opts.client ?? getDefaultClient();
    const id = typeof snapshot === "string" ? snapshot : snapshot.id;
    const req = { snapshot_id: id, name: opts.name ?? "", labels: labels(opts.labels) };
    return new Volume(c, await c.conn.call(c.conn.volumes, "FromSnapshot", req, 3_600_000));
  }

  static async get(nameOrId: string, client?: Client): Promise<Volume> {
    const c = client ?? getDefaultClient();
    return new Volume(c, await c.conn.call(c.conn.volumes, "Get", { volume: nameOrId }));
  }

  /**
   * Filesystem snapshot: cheap and copy-on-write. With `durable`, also upload
   * it to the daemon's blob store (only blocks not already there are sent);
   * fails, keeping nothing, if there is no blob store or the upload fails.
   */
  async snapshot(
    opts: { durable?: boolean; labels?: Record<string, string> } = {},
  ): Promise<VolumeSnapshot> {
    const r = await this.client.conn.call(
      this.client.conn.volumes,
      "TakeSnapshot",
      { id: this.id, durable: !!opts.durable, labels: labels(opts.labels) },
      3_600_000,
    );
    return volumeSnapshotFrom(r);
  }

  async clone(opts: { name?: string } = {}): Promise<Volume> {
    const r = await this.client.conn.call(
      this.client.conn.volumes,
      "Clone",
      { id: this.id, name: opts.name ?? "" },
      600_000,
    );
    return new Volume(this.client, r);
  }

  /** Fails if the volume is attached. */
  async delete(): Promise<void> {
    await this.client.conn.call(this.client.conn.volumes, "Delete", { id: this.id });
  }

  /**
   * Delete a volume snapshot, including its blob-store copy. With `localOnly`,
   * free only the local copy of a durable snapshot.
   */
  async deleteSnapshot(
    snapshot: VolumeSnapshot | string,
    opts: { localOnly?: boolean } = {},
  ): Promise<void> {
    const id = typeof snapshot === "string" ? snapshot : snapshot.id;
    await this.client.conn.call(this.client.conn.volumes, "DeleteSnapshot", {
      id,
      local_only: !!opts.localOnly,
    });
  }
}
