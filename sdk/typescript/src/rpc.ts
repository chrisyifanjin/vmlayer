/**
 * Connection plumbing: channel, dynamically loaded stubs, endpoint discovery,
 * error translation, and promise wrappers around grpc-js callbacks.
 */

import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import * as errors from "./errors.js";

const require = createRequire(import.meta.url);

export function defaultEndpoint(): string {
  const env = process.env;
  if (env.VMLAYER_ENDPOINT) return env.VMLAYER_ENDPOINT;
  if (env.VMLAYER_SOCKET) return `unix://${env.VMLAYER_SOCKET}`;
  if (env.XDG_RUNTIME_DIR)
    return `unix://${path.join(env.XDG_RUNTIME_DIR, "vmlayer", "vmlayerd.sock")}`;
  if (env.VMLAYER_DATA_DIR) return `unix://${path.join(env.VMLAYER_DATA_DIR, "vmlayerd.sock")}`;
  return `unix://${path.join(os.homedir(), ".vmlayer", "vmlayerd.sock")}`;
}

export function grpcTarget(endpoint: string): string {
  if (endpoint.startsWith("unix://")) return "unix:" + endpoint.slice("unix://".length);
  if (endpoint.startsWith("tcp://")) return endpoint.slice("tcp://".length);
  if (endpoint.startsWith("/")) return "unix:" + endpoint;
  return endpoint;
}

/** Where the .proto files are: shipped in the package, or the repo's proto/ during development. */
function protoRoot(): string {
  const here = path.dirname(fileURLToPath(import.meta.url));
  for (const candidate of [
    path.join(here, "..", "proto"),
    path.join(here, "..", "..", "..", "proto"),
  ]) {
    try {
      require("node:fs").statSync(path.join(candidate, "vmlayer", "v1", "common.proto"));
      return candidate;
    } catch {
      /* try next */
    }
  }
  throw new errors.VmlayerError("vmlayer proto files not found; reinstall the package");
}

let loaded: grpc.GrpcObject | undefined;
function pkg(): any {
  if (!loaded) {
    const root = protoRoot();
    const def = protoLoader.loadSync(
      ["sandboxes", "processes", "files", "volumes", "system"].map((n) => `vmlayer/v1/${n}.proto`),
      {
        includeDirs: [root],
        keepCase: true,
        longs: Number,
        enums: String,
        defaults: true,
        oneofs: true,
      },
    );
    loaded = grpc.loadPackageDefinition(def);
  }
  return (loaded as any).vmlayer.v1;
}

export interface ConnectionOptions {
  endpoint?: string;
  token?: string;
  timeoutMs?: number;
}

export class Connection {
  readonly endpoint: string;
  readonly timeoutMs: number;
  readonly sandboxes: any;
  readonly processes: any;
  readonly files: any;
  readonly volumes: any;
  readonly system: any;
  private readonly channelClients: any[];

  constructor(opts: ConnectionOptions = {}) {
    this.endpoint = opts.endpoint ?? defaultEndpoint();
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    const target = grpcTarget(this.endpoint);
    const options: grpc.ChannelOptions = {
      "grpc.max_receive_message_length": 64 * 1024 * 1024,
      "grpc.max_send_message_length": 64 * 1024 * 1024,
    };
    // grpc-js derives :authority from the socket path, which the server rejects.
    if (target.startsWith("unix:")) options["grpc.default_authority"] = "localhost";
    let creds = grpc.credentials.createInsecure();
    if (opts.token) {
      const token = opts.token;
      const call = grpc.credentials.createFromMetadataGenerator((_, cb) => {
        const md = new grpc.Metadata();
        md.set("authorization", `Bearer ${token}`);
        cb(null, md);
      });
      creds = grpc.credentials.combineChannelCredentials(grpc.credentials.createInsecure(), call);
    }
    const p = pkg();
    this.sandboxes = new p.Sandboxes(target, creds, options);
    this.processes = new p.Processes(target, creds, options);
    this.files = new p.Files(target, creds, options);
    this.volumes = new p.Volumes(target, creds, options);
    this.system = new p.System(target, creds, options);
    this.channelClients = [this.sandboxes, this.processes, this.files, this.volumes, this.system];
  }

  close(): void {
    for (const c of this.channelClients) c.close();
  }

  /** Call options with a deadline; `0` means no deadline (long streams). */
  deadline(timeoutMs?: number): grpc.CallOptions {
    const ms = timeoutMs ?? this.timeoutMs;
    return ms > 0 ? { deadline: Date.now() + ms } : {};
  }

  /** Unary call with error translation. */
  call<T>(client: any, method: string, request: any, timeoutMs?: number): Promise<T> {
    return new Promise((resolve, reject) => {
      client[method](
        request,
        this.deadline(timeoutMs),
        (err: grpc.ServiceError | null, resp: T) => {
          if (err) reject(translate(err));
          else resolve(resp);
        },
      );
    });
  }

  /** Server-streaming call as an async iterable with error translation. */
  async *stream<T>(
    client: any,
    method: string,
    request: any,
    timeoutMs?: number,
  ): AsyncGenerator<T> {
    const call: grpc.ClientReadableStream<T> = client[method](
      request,
      this.deadline(timeoutMs ?? 0),
    );
    try {
      for await (const item of call as AsyncIterable<T>) yield item;
    } catch (e) {
      throw translate(e as grpc.ServiceError);
    }
  }
}

export function translate(e: grpc.ServiceError | Error): errors.VmlayerError {
  const se = e as grpc.ServiceError;
  const code = se.code ?? grpc.status.UNKNOWN;
  const message = se.details ?? e.message ?? "";
  let kind = "";
  try {
    const v = se.metadata?.get("vmlayer-error");
    if (v && v.length) kind = String(v[0]);
  } catch {
    /* no metadata */
  }
  if (code === grpc.status.UNAVAILABLE) return new errors.DaemonUnavailable(message);
  if (kind === "sandbox_not_found") return new errors.SandboxNotFound(message);
  if (kind === "volume_not_found") return new errors.VolumeNotFound(message);
  if (kind === "snapshot_not_found") return new errors.SnapshotNotFound(message);
  if (kind === "unsupported" || code === grpc.status.UNIMPLEMENTED)
    return errors.BackendUnsupported.fromMessage(message);
  if (kind === "volume_busy") return new errors.VolumeBusy(message);
  if (kind === "snapshot_incompatible") return new errors.SnapshotIncompatible(message);
  if (kind === "timeout" || code === grpc.status.DEADLINE_EXCEEDED)
    return new errors.TimeoutError(message);
  if (code === grpc.status.INVALID_ARGUMENT) return new errors.InvalidArgument(message);
  if (code === grpc.status.NOT_FOUND) return new errors.NotFound(message);
  return new errors.VmlayerError(`${grpc.status[code]?.toLowerCase() ?? code}: ${message}`);
}

// -- conversions ------------------------------------------------------------

export function duration(ms: number | undefined | null): { seconds: number; nanos: number } | null {
  if (ms === undefined || ms === null) return null;
  return { seconds: Math.floor(ms / 1000), nanos: Math.round((ms % 1000) * 1e6) };
}

export function fromDuration(
  d: { seconds?: number | string; nanos?: number } | null | undefined,
): number {
  if (!d) return 0;
  return Number(d.seconds ?? 0) * 1000 + (d.nanos ?? 0) / 1e6;
}

export function fromTimestamp(
  t: { seconds?: number | string; nanos?: number } | null | undefined,
): number {
  if (!t) return 0;
  return Number(t.seconds ?? 0) + (t.nanos ?? 0) / 1e9;
}

export function labels(m: Record<string, string> | undefined): Record<string, string> {
  return m ? { ...m } : {};
}
