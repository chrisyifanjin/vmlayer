# Common tasks. CI runs the same commands.
.PHONY: build check test test-live proto sdk fmt clean

build:
	cargo build --release

check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
	cd sdk/python && uv run ruff check . && uv run ruff format --check . && uv run mypy vmlayer

test:
	cargo test --workspace

test-live:
	VMLAYER_DOCKER_TESTS=1 cargo test --workspace
	cargo build -p vmlayerd
	cd sdk/python && uv run pytest -q

proto:
	./scripts/gen-proto.sh

sdk:
	cd sdk/python && uv sync --group dev

fmt:
	cargo fmt --all
	cd sdk/python && uv run ruff format . && uv run ruff check --fix .

clean:
	cargo clean
