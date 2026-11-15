/** Handles for things running inside a sandbox: `Process` and `Pty`. */

import type { Client } from "./client.js";
import { ExecTimeout } from "./errors.js";
import { fromDuration } from "./rpc.js";
import type { ExecResult, OutputChunk, PtySize } from "./types.js";

function chunk(c: any): OutputChunk {
  return {
    stream: c.stream === "OUTPUT_STREAM_STDERR" ? "stderr" : "stdout",
    data: Buffer.from(c.data ?? []),
  };
}

export class Collector {
  out: Buffer[] = [];
  err: Buffer[] = [];
  merged: Buffer[] = [];
  add(c: OutputChunk): void {
    (c.stream === "stdout" ? this.out : this.err).push(c.data);
    this.merged.push(c.data);
  }
  result(st: any): ExecResult {
    const signal: number | null =
      st.signal !== undefined && st.signal !== null ? Number(st.signal) : null;
    const exitCode =
      signal !== null
        ? 128 + signal
        : st.code !== undefined && st.code !== null
          ? Number(st.code)
          : -1;
    return {
      exitCode,
      stdout: Buffer.concat(this.out).toString("utf8"),
      stderr: Buffer.concat(this.err).toString("utf8"),
      output: Buffer.concat(this.merged).toString("utf8"),
      durationMs: fromDuration(st.duration),
      signal,
      ok: exitCode === 0,
    };
  }
}

/** Feeds the client-streaming Stdin RPC. */
class StdinWriter {
  private call: any;
  closed = false;
  constructor(client: Client, sandboxId: string, pid: number) {
    this.call = client.conn.processes.Stdin(() => {
      /* completion is observed through Output/Wait */
    });
    this.call.write({ sandbox_id: sandboxId, pid, data: Buffer.alloc(0) });
  }
  write(data: Buffer | string): void {
    if (this.closed) throw new Error("stdin is closed");
    this.call.write({ data: Buffer.from(data) });
  }
  /** Send EOF. */
  end(): void {
    if (!this.closed) {
      this.closed = true;
      this.call.end();
    }
  }
}

/** A command started with `Sandbox.spawn`. Iterate for output; `wait` for the result. */
export class Process {
  private _stdin: StdinWriter | undefined;
  private collector = new Collector();
  private status: any = undefined;

  /** @internal */
  constructor(
    private readonly client: Client,
    private readonly sandboxId: string,
    readonly pid: number,
    private readonly timeoutMs: number | undefined,
  ) {}

  /** Writer for the process's stdin; call `end()` to send EOF. */
  get stdin(): { write(data: Buffer | string): void; end(): void } {
    if (!this._stdin) this._stdin = new StdinWriter(this.client, this.sandboxId, this.pid);
    return this._stdin;
  }

  /** stdout/stderr chunks until the process exits. */
  async *[Symbol.asyncIterator](): AsyncGenerator<OutputChunk> {
    const req = { sandbox_id: this.sandboxId, pid: this.pid };
    for await (const ev of this.client.conn.stream<any>(
      this.client.conn.processes,
      "Output",
      req,
    )) {
      if (ev.output) {
        const c = chunk(ev.output);
        this.collector.add(c);
        yield c;
      } else if (ev.exited) {
        this.status = ev.exited;
      }
    }
  }

  /** Block until exit; buffers any output not already consumed via iteration. */
  async wait(): Promise<ExecResult> {
    if (!this.status) for await (const _ of this) void _;
    if (!this.status) {
      this.status = await this.client.conn.call(this.client.conn.processes, "Wait", {
        sandbox_id: this.sandboxId,
        pid: this.pid,
      });
    }
    const result = this.collector.result(this.status);
    if (this.status.timed_out && this.timeoutMs !== undefined)
      throw new ExecTimeout(this.timeoutMs, result);
    return result;
  }

  async kill(signal = 9): Promise<void> {
    await this.client.conn.call(this.client.conn.processes, "Kill", {
      sandbox_id: this.sandboxId,
      pid: this.pid,
      signal,
    });
  }
}

/** An interactive terminal: one byte stream with escapes intact. */
export class Pty {
  private readonly call: any;
  private exited: any = undefined;
  closed = false;

  /** @internal */
  constructor(client: Client, open: any) {
    this.call = client.conn.processes.Pty();
    this.call.write({ open });
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<Buffer> {
    for await (const msg of this.call as AsyncIterable<any>) {
      if (msg.data) yield Buffer.from(msg.data);
      else if (msg.exited) {
        this.exited = msg.exited;
        return;
      }
    }
  }

  write(data: Buffer | string): void {
    this.call.write({ data: Buffer.from(data) });
  }

  resize(size: PtySize): void {
    this.call.write({ resize: { cols: size.cols, rows: size.rows } });
  }

  /** Exit status once the session ended, else undefined. */
  get exitStatus(): any {
    return this.exited;
  }

  close(): void {
    if (!this.closed) {
      this.closed = true;
      this.call.end();
      try {
        this.call.cancel();
      } catch {
        /* already finished */
      }
    }
  }
}

export { chunk as outputChunk };
