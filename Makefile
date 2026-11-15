# Common tasks. CI runs the same commands.
.PHONY: build check test test-live proto sdk fmt clean

build:
	cargo build --release

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
	cd sdk/python && uv run ruff check . && uv run ruff format --check . && uv run mypy vmlayer
	cd sdk/typescript && npm run build && npm run lint

test:
	cargo test --workspace

test-live:
	VMLAYER_DOCKER_TESTS=1 cargo test --workspace
	cargo build -p vmlayerd
	cd sdk/python && uv run pytest -q
	cd sdk/typescript && npm test

proto:
	./scripts/gen-proto.sh

sdk:
	cd sdk/python && uv sync --group dev
	cd sdk/typescript && npm ci && npm run sync-proto

fmt:
	cargo fmt --all
	cd sdk/python && uv run ruff format . && uv run ruff check --fix .

clean:
	cargo clean
