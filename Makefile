EXAMPLE_WORKSPACE = examples/mock-dsl-compiler/workspace

.PHONY: build example test

build: test
	cargo build --workspace --release

example:
	cargo run -p mock-dsl-compiler -- $(EXAMPLE_WORKSPACE)

test:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
