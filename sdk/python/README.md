# vmlayer (Python SDK)

Client for [vmlayer](https://github.com/chrisyifanjin/vmlayer): one sandbox
API over Firecracker, libkrun and Docker, with snapshot, restore and fork.

```python
from vmlayer import Sandbox

with Sandbox.create("python:3.12") as sb:
    print(sb.exec("python -c 'print(42)'").stdout)
```

The daemon and both Docker and Firecracker backends work. See the repository
README for current platform coverage and benchmark results.
