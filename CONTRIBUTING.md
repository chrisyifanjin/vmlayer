# Contributing

## Ground rules

- **Decisions get a record.** Anything non-obvious (a dependency, a protocol,
  a layout) gets a numbered file in `docs/decisions/` with the options
  considered and the evidence, before the code. A reversal is a new record.
- **The two API files are the contract.** `sdk/python/vmlayer/` and
  `core/src/backend.rs` plus `core/src/types/`. A change to either is an API change and says so in
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
uv sync --group dev
uv run ruff check . && uv run ruff format --check .
uv run mypy vmlayer
```

Tool versions are pinned in `sdk/python/pyproject.toml` (`dev` group) so
local runs and CI agree.

## Layout

```
core/src/backend.rs   the Backend / SandboxHandle / Process / Pty traits
core/src/types/       value types, one module per concern (ids, capabilities,
                      spec, volume, process, snapshot), re-exported flat
core/src/error.rs     the error enum (maps 1:1 to gRPC codes and SDK exceptions)
sdk/python/vmlayer/   Python SDK: errors, types, volume, process, files,
                      client, sandbox, aio; __init__ re-exports the public names
docs/decisions/       design records
docs/                 integrations study, research notes, diagram
```

One module per concern, re-exported from one place. A new concept gets a new
module, not a new section in an existing file.

Planned, one crate or package each: `daemon/` (`vmlayerd`), `backends/{firecracker,libkrun,docker}`,
`guest-agent/`, `sdk/typescript/`, `integrations/<runtime>/`.

## Commits

Imperative subject line, body explains why. Reference the decision record
when a commit implements one.
