.PHONY: build release test lint format check clean install all

# Build debug binary
build:
	cargo build

# Build release binary
release:
	cargo build --release

# Run all tests
test:
	cargo test

# Run tests with output
test-verbose:
	cargo test -- --nocapture

# Lint with clippy
lint:
	cargo clippy -- -D warnings

# Format code
format:
	cargo fmt

# Check formatting without changing
format-check:
	cargo fmt -- --check

# Run all checks (format + lint + test)
check: format-check lint test
	@echo "All checks passed."

# Run everything (format + check)
all: format check

# Install the binary
install:
	cargo install --path .

# Clean build artifacts
clean:
	cargo clean

# Show binary size
size: release
	@ls -lh target/release/githubclaw

# Initialize a repo for GithubClaw
init:
	cargo run -- init

# Start the webhook server
start:
	cargo run -- start

# Stop the webhook server
stop:
	cargo run -- stop

# Show status
status:
	cargo run -- status
