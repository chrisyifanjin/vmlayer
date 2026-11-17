/** Value types shared across the SDK. */

export type Backend = "auto" | "firecracker" | "libkrun" | "docker";

export type SandboxState = "creating" | "running" | "paused" | "stopped" | "failed";

/** What the backend behind a sandbox can do; one flag per optional method. */
export interface Capabilities {
  snapshot: boolean;
  memorySnapshot: boolean;
  fork: boolean;
  pause: boolean;
  pty: boolean;
  exposePort: boolean;
  volumes: boolean;
  hotAttach: boolean;
  /** Enforces `egress` allowlists (microVM backends). */
  egressPolicy: boolean;
  /** Enforces `limits` (microVM backends). */
  ioLimits: boolean;
}

/** Device rate limits for `Sandbox.create`. Omitted means unlimited. */
export interface IoLimits {
  /** Megabits per second, each direction. */
  networkMbps?: number;
  /** Megabytes per second, per disk. */
  diskMbps?: number;
  /** Operations per second, per disk. */
  diskIops?: number;
}

export interface ExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
  /** stdout and stderr merged in arrival order, the way a terminal shows them. */
  output: string;
  durationMs: number;
  /** Set when the process was killed by a signal; exitCode is then 128 + signal. */
  signal: number | null;
  ok: boolean;
}

export interface OutputChunk {
  stream: "stdout" | "stderr";
  data: Buffer;
}

export interface FileInfo {
  path: string;
  size: number;
  isDir: boolean;
  mode: number;
  /** Unix timestamp, seconds. */
  modifiedAt: number;
  name: string;
}

export interface ProcessInfo {
  pid: number;
  argv: string[];
  startedAt: number;
  isEntrypoint: boolean;
}

export interface PtySize {
  cols: number;
  rows: number;
}

export interface Endpoint {
  host: string;
  port: number;
  url: string;
}

export interface BackendInfo {
  name: string;
  engineVersion: string;
  capabilities: Capabilities;
  unavailableReason: string;
  available: boolean;
}

export interface SystemInfo {
  daemonVersion: string;
  backends: BackendInfo[];
  defaultBackend: string;
  dataDir: string;
  /** URL of the blob store for durable snapshots, or null. */
  blobStore: string | null;
}

/** What `Client.collectGarbage` found and removed in the blob store. */
export interface GarbageCollection {
  snapshots: number;
  chunks: number;
  referencedChunks: number;
  /** Unreferenced but within the grace period, so kept. */
  recentChunks: number;
  deletedChunks: number;
  deletedBytes: number;
}

export interface VolumeSnapshot {
  id: string;
  volumeId: string;
  guestPath: string | null;
  createdAt: number;
  sizeBytes: number;
  /** Stored in the daemon's blob store, restorable by any daemon sharing it. */
  durable: boolean;
  labels: Record<string, string>;
  /** A copy is on this daemon's disk; false once dropped or if pushed elsewhere. */
  local: boolean;
}

export interface Snapshot {
  id: string;
  backend: string;
  engineVersion: string;
  cpuModel: string | null;
  createdAt: number;
  sizeBytes: number;
  includesMemory: boolean;
  volumes: VolumeSnapshot[];
  labels: Record<string, string>;
}

/** An image resolved for one backend. */
export interface ImageInfo {
  image: string;
  backend: string;
  /** "engine" (held by the container engine), "ext4" or "dir". */
  rootfsKind: string;
  rootfsPath: string | null;
  entrypoint: string[];
  workdir: string | null;
}
