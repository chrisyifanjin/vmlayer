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
}

export interface VolumeSnapshot {
  id: string;
  volumeId: string;
  guestPath: string | null;
  createdAt: number;
  sizeBytes: number;
  durable: boolean;
  labels: Record<string, string>;
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
