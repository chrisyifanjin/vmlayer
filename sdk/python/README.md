# vmlayer (Python SDK)

Client for [vmlayer](https://github.com/chrisyifanjin/vmlayer): one sandbox
API over Firecracker, libkrun and Docker, with snapshot, restore and fork.

```python
from vmlayer import Sandbox

with Sandbox.create("python:3.12") as sb:
    print(sb.exec("python -c 'print(42)'").stdout)
```

Status: API surface only; the daemon does not exist yet. See the repository
README for the roadmap.
