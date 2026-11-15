# vmlayer (TypeScript SDK)

Client for [vmlayer](https://github.com/chrisyifanjin/vmlayer): one sandbox API
over Firecracker, libkrun and Docker, with snapshot, restore and fork.

```ts
import { Sandbox } from "vmlayer";

const sb = await Sandbox.create("python:3.12");
try {
  const r = await sb.exec("python -c 'print(6*7)'");
  console.log(r.stdout); // "42\n"
} finally {
  await sb.kill();
}
```

Talks to a local `vmlayerd` over its unix socket by default
(`VMLAYER_ENDPOINT` to override). Node 18+. The API mirrors the Python SDK
one to one; see the repository's `docs/usage.md`.
