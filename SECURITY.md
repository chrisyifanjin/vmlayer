# Security policy

vmlayer runs untrusted code on purpose, so isolation bugs matter more here
than in most projects.

## Reporting a vulnerability

Email chris.yifanjin@gmail.com with a description and, if possible, a
reproduction. Please do not open a public issue for anything that lets a
sandbox affect the host, another sandbox, or the daemon's data. You will
get an acknowledgement within 72 hours and a fix or mitigation plan within
14 days for confirmed issues; credit is given in the release notes unless
you prefer otherwise.

## Scope

In scope: escaping a sandbox (container or microVM), reading or writing
another sandbox's volumes or snapshots, reaching the daemon's data
directory from a guest, bypassing the TCP listener's bearer token, denial
of service of the daemon from inside a sandbox.

Out of scope: vulnerabilities in Docker, Firecracker or the Linux kernel
themselves (report those upstream; we pin and track versions), and
misconfiguration such as running the TCP listener without TLS on an open
network.

## Supported versions

The latest minor release receives fixes. Firecracker and Docker engine
versions the daemon was tested with are recorded in each release.
