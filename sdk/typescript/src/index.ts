/**
 * vmlayer: the layer between agent runtimes and microVMs.
 *
 * ```ts
 * import { Sandbox, Volume } from "vmlayer";
 *
 * const sb = await Sandbox.create("python:3.12", { memoryMb: 512 });
 * await sb.files.write("/work/main.py", "print('hi')");
 * console.log((await sb.exec("python /work/main.py")).stdout);
 * const snap = await sb.snapshot();
 * await sb.kill();
 * const sb2 = await Sandbox.restore(snap);
 * ```
 */

export { Client, getDefaultClient, setDefaultClient } from "./client.js";
export { Cluster, type PlaceOptions } from "./cluster.js";
export * from "./errors.js";
export { Files } from "./files.js";
export { Process, Pty } from "./process.js";
export { Sandbox, type CreateOptions, type ExecOptions } from "./sandbox.js";
export * from "./types.js";
export { Volume, type CreateVolumeOptions } from "./volume.js";
export { defaultEndpoint, type TlsOptions } from "./rpc.js";
