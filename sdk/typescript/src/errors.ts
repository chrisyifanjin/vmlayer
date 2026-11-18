/** Exceptions raised by the SDK. One class per failure the daemon reports. */

import type { ExecResult } from "./types.js";

export class VmlayerError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/** No daemon answers at the endpoint. Is `vmlayerd` running? */
export class DaemonUnavailable extends VmlayerError {}
export class InvalidArgument extends VmlayerError {}
/** The daemon's host is full (admission control), or no host in a cluster has room. */
export class ResourceExhausted extends VmlayerError {}
/** The daemon did not accept the token. */
export class Unauthenticated extends VmlayerError {}
/** The token's role does not allow this call. */
export class PermissionDenied extends VmlayerError {}
/** A file or process was not found. */
export class NotFound extends VmlayerError {}
export class SandboxNotFound extends NotFound {}
export class VolumeNotFound extends NotFound {}
export class SnapshotNotFound extends NotFound {}
/** The volume is attached read-write elsewhere (single-writer rule). */
export class VolumeBusy extends VmlayerError {}
/** The backend lacks the feature; check `sandbox.capabilities` first. */
export class BackendUnsupported extends VmlayerError {
  backend = "";
  feature = "";
  static fromMessage(message: string): BackendUnsupported {
    const e = new BackendUnsupported(message);
    const m = /backend (\S+) does not support (.+)/.exec(message);
    if (m) {
      e.backend = m[1] ?? "";
      e.feature = m[2] ?? "";
    }
    return e;
  }
}
/** A daemon-side wait exceeded its timeout (e.g. `waitForPort`). */
export class TimeoutError extends VmlayerError {}
/** A command exceeded its timeout and was killed; `partial` holds its output so far. */
export class ExecTimeout extends TimeoutError {
  constructor(
    public readonly timeoutMs: number,
    public readonly partial: ExecResult,
  ) {
    super(`command exceeded ${timeoutMs} ms`);
  }
}
/** A snapshot cannot run on this host (CPU, kernel or backend differs). */
export class SnapshotIncompatible extends VmlayerError {}
