/**
 * Several daemons that share a blob store. Placement happens in the daemons
 * (any one picks a host from the shared heartbeats; the chosen host admits or
 * refuses), so this class only follows their answers.
 *
 * ```ts
 * const cluster = new Cluster("tcp://10.0.0.5:7070", { token });
 * const sb = await cluster.create("python:3.12", { memoryMb: 1024, lease: "repo-42" });
 * const same = await cluster.connect(sb.id);
 * ```
 */

import { Client } from "./client.js";
import { NotFound, ResourceExhausted } from "./errors.js";
import { Sandbox, type CreateOptions } from "./sandbox.js";
import type { TlsOptions } from "./rpc.js";
import type { Backend, HostInfo } from "./types.js";

export interface PlaceOptions {
  backend?: Backend | undefined;
  cpus?: number | undefined;
  memoryMb?: number | undefined;
  placement?: Record<string, string> | undefined;
  exclude?: string[] | undefined;
}

export class Cluster {
  private readonly seed: Client;
  private readonly clients = new Map<string, Client>();

  constructor(
    seed: Client | string,
    private readonly opts: { token?: string; tls?: TlsOptions } = {},
  ) {
    this.seed = typeof seed === "string" ? new Client(seed, this.connOpts()) : seed;
  }

  /** The token and TLS settings, used for every host reached through the registry. */
  private connOpts(): { token?: string; tls?: TlsOptions } {
    return {
      ...(this.opts.token === undefined ? {} : { token: this.opts.token }),
      ...(this.opts.tls === undefined ? {} : { tls: this.opts.tls }),
    };
  }

  hosts(): Promise<HostInfo[]> {
    return this.seed.listHosts();
  }

  client(host: HostInfo | string): Client {
    const endpoint = typeof host === "string" ? host : host.endpoint;
    if (endpoint === this.seed.endpoint) return this.seed;
    let c = this.clients.get(endpoint);
    if (!c) {
      c = new Client(endpoint, this.connOpts());
      this.clients.set(endpoint, c);
    }
    return c;
  }

  place(opts: PlaceOptions = {}): Promise<HostInfo> {
    return this.seed.placeSandbox({
      backend: opts.backend,
      cpus: opts.cpus,
      memoryMb: opts.memoryMb,
      placement: opts.placement,
      excludeHosts: opts.exclude,
    });
  }

  /** `Sandbox.create` on a host the cluster picks; full hosts are skipped. */
  async create(
    image: string,
    opts: CreateOptions & { placement?: Record<string, string> | undefined } = {},
  ): Promise<Sandbox> {
    const refused: string[] = [];
    for (;;) {
      const host = await this.place({
        backend: opts.backend,
        cpus: opts.cpus,
        memoryMb: opts.memoryMb,
        placement: opts.placement,
        exclude: refused,
      });
      try {
        return await Sandbox.create(image, { ...opts, client: this.client(host) });
      } catch (e) {
        if (!(e instanceof ResourceExhausted)) throw e;
        refused.push(host.hostId);
      }
    }
  }

  /** The sandbox with this id, on whichever host runs it. */
  async connect(sandboxId: string): Promise<Sandbox> {
    for (const host of await this.hosts()) {
      if (host.sandboxIds.includes(sandboxId)) return Sandbox.connect(sandboxId, this.client(host));
    }
    throw new NotFound(`no host in the registry runs sandbox ${sandboxId}`);
  }
}
