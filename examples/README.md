# Examples

Each example runs against a local `vmlayerd` with the Docker backend.

| File | Shows |
|---|---|
| [quickstart.py](quickstart.py) | create, exec, files, kill |
| [workspace.py](workspace.py) | a named volume shared by successive sandboxes, checkpoints as templates |
| [server.py](server.py) | an image that is a server: entrypoint, wait-for-port, expose |
| [streaming.py](streaming.py) | spawn with live output and stdin, re-attach by pid |
| [quickstart.ts](quickstart.ts) | the TypeScript equivalent of quickstart.py |

```bash
cd sdk/python && uv run python ../../examples/quickstart.py
cd examples && npm install && npx tsx quickstart.ts   # TypeScript, uses the SDK from ../sdk/typescript
```
