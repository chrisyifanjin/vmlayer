/** The `sandbox.files` namespace: filesystem operations through the guest agent. */

import * as fs from "node:fs/promises";
import * as path from "node:path";

import type { Client } from "./client.js";
import { NotFound } from "./errors.js";
import { fromTimestamp, translate } from "./rpc.js";
import type { FileInfo } from "./types.js";

const CHUNK = 64 * 1024;

function info(f: any): FileInfo {
  const p: string = f.path;
  return {
    path: p,
    size: Number(f.size ?? 0),
    isDir: !!f.is_dir,
    mode: Number(f.mode ?? 0),
    modifiedAt: fromTimestamp(f.modified_at),
    name: p.replace(/\/+$/, "").split("/").pop() ?? p,
  };
}

export class Files {
  /** @internal */
  constructor(
    private readonly client: Client,
    private readonly id: string,
  ) {}

  async read(p: string): Promise<Buffer> {
    const parts: Buffer[] = [];
    for await (const ch of this.client.conn.stream<any>(this.client.conn.files, "Read", {
      sandbox_id: this.id,
      path: p,
    }))
      parts.push(Buffer.from(ch.data ?? []));
    return Buffer.concat(parts);
  }

  async readText(p: string, encoding: BufferEncoding = "utf8"): Promise<string> {
    return (await this.read(p)).toString(encoding);
  }

  /** Create or overwrite a file; parent directories are created. */
  write(p: string, data: Buffer | string, opts: { mode?: number } = {}): Promise<void> {
    const payload = Buffer.from(data);
    return new Promise((resolve, reject) => {
      const call = this.client.conn.files.Write((err: any) =>
        err ? reject(translate(err)) : resolve(),
      );
      call.write({ header: { sandbox_id: this.id, path: p, mode: opts.mode ?? 0o644 } });
      for (let i = 0; i < payload.length; i += CHUNK)
        call.write({ data: payload.subarray(i, i + CHUNK) });
      if (payload.length === 0) call.write({ data: Buffer.alloc(0) });
      call.end();
    });
  }

  async list(p = "/"): Promise<FileInfo[]> {
    const r: any = await this.client.conn.call(this.client.conn.files, "List", {
      sandbox_id: this.id,
      path: p,
    });
    return (r.entries ?? []).map(info);
  }

  async stat(p: string): Promise<FileInfo> {
    return info(
      await this.client.conn.call(this.client.conn.files, "Stat", { sandbox_id: this.id, path: p }),
    );
  }

  async exists(p: string): Promise<boolean> {
    try {
      await this.stat(p);
      return true;
    } catch (e) {
      if (e instanceof NotFound) return false;
      throw e;
    }
  }

  async mkdir(p: string): Promise<void> {
    await this.client.conn.call(this.client.conn.files, "Mkdir", { sandbox_id: this.id, path: p });
  }

  async remove(p: string, opts: { recursive?: boolean } = {}): Promise<void> {
    await this.client.conn.call(this.client.conn.files, "Remove", {
      sandbox_id: this.id,
      path: p,
      recursive: !!opts.recursive,
    });
  }

  /** Copy a local file or directory tree into the sandbox. */
  async upload(local: string, remote: string): Promise<void> {
    const st = await fs.stat(local);
    if (st.isDirectory()) {
      await this.mkdir(remote);
      for (const entry of await fs.readdir(local, { withFileTypes: true })) {
        const target = `${remote.replace(/\/+$/, "")}/${entry.name}`;
        if (entry.isDirectory()) await this.upload(path.join(local, entry.name), target);
        else if (entry.isFile()) {
          const src = path.join(local, entry.name);
          await this.write(target, await fs.readFile(src), {
            mode: (await fs.stat(src)).mode & 0o7777,
          });
        }
      }
    } else {
      await this.write(remote, await fs.readFile(local), { mode: st.mode & 0o7777 });
    }
  }

  /** Copy a file or directory tree out of the sandbox. */
  async download(remote: string, local: string): Promise<void> {
    const st = await this.stat(remote);
    if (st.isDir) {
      await fs.mkdir(local, { recursive: true });
      for (const entry of await this.list(remote))
        await this.download(entry.path, path.join(local, entry.name));
    } else {
      await fs.mkdir(path.dirname(local), { recursive: true });
      await fs.writeFile(local, await this.read(remote));
    }
  }
}
