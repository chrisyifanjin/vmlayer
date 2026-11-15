**What this changes**

**Why**
Link the issue or the decision record. A non-obvious design choice gets a
new file in `docs/decisions/`.

**API change?**
Yes / No. If yes, both `sdk/python/vmlayer/` and `sdk/typescript/src/` and
the `.proto` files are updated together, and the title says so.

**Checklist**
- [ ] `make check` passes (fmt, clippy, doc, ruff, mypy, eslint)
- [ ] tests added or updated; `make test-live` passes with Docker
- [ ] docs updated (`docs/usage.md`, `docs/deploy.md`, README status table)
