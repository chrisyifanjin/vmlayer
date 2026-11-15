"""Images that are servers: start, wait until it listens, reach it from the host."""

import urllib.request
from datetime import timedelta

from vmlayer import Sandbox

with Sandbox.create(
    "python:3.12-alpine",
    entrypoint=["sh", "-c", "mkdir -p /srv && echo hello > /srv/index.html && cd /srv && exec python3 -m http.server 8080"],
) as sb:
    sb.wait_for_port(8080, timeout=timedelta(seconds=20))
    url = sb.expose(8080).url
    print("serving at", url)
    print(urllib.request.urlopen(f"{url}/index.html").read().decode().strip())
    print("server processes:", [p.argv[:2] for p in sb.processes() if p.is_entrypoint])
