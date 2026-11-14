# Contributing

## Ground rules

- **Decisions get a record.** Anything non-obvious (a dependency, a protocol,
  a layout) gets a numbered file in `docs/decisions/` with the options
  considered and the evidence, before the code. A reversal is a new record.
- **The two API files are the contract.** `sdk/python/vmlayer/api.py` and
  `core/src/backend.rs`. A change to either is an API change and says so in
  the PR title.
- **Backends stay mechanical.** Policy (image resolution, single-writer rule,
  TTLs, `Auto` selection) lives in the daemon. A backend that cannot do
  something returns `Error::Unsupported`; it never approximates.
- **Done means tested and documented.** A PR adds or updates tests and the
  relevant doc in the same change.

## Checks

CI runs these; run them locally first.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

```bash
cd sdk/python
uvx ruff check . && uvx ruff format --check .
uvx mypy vmlayer
```

## Layout

```
core/            vmlayer-core: Backend trait, shared types, errors (no impl)
sdk/python/      Python SDK
docs/decisions/  design records
docs/            everything else
```

Planned, one crate or package each: `daemon/` (`vmlayerd`), `backends/{firecracker,libkrun,docker}`,
`guest-agent/`, `sdk/typescript/`, `integrations/<runtime>/`.

## Commits

Imperative subject line, body explains why. Reference the decision record
when a commit implements one.
