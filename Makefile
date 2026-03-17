.PHONY: build release test test-verbose lint format format-check check ci install reinstall restart clean size init start stop status serve serve-tmux logs help

UNAME_S := $(shell uname -s)

# ─── Build ────────────────────────────────────────────────────────────
build:            ## Build debug binary
	cargo build

release:          ## Build release binary
	cargo build --release

# ─── Test ─────────────────────────────────────────────────────────────
test:             ## Run all tests
	cargo test --all-targets

test-verbose:     ## Run tests with output
	cargo test --all-targets -- --nocapture

# ─── Lint & Format ────────────────────────────────────────────────────
lint:             ## Run clippy (warnings = errors)
	cargo clippy --all-targets -- -D warnings

format:           ## Format code
	cargo fmt

format-check:     ## Check formatting without changes
	cargo fmt -- --check

# ─── Combined ─────────────────────────────────────────────────────────
check: format-check lint test  ## Run format check + lint + test
	@echo "✓ All checks passed."

ci: check release  ## Run full CI pipeline locally (same as GitHub Actions)
	@echo "✓ CI passed. Release binary ready."

# ─── Install ──────────────────────────────────────────────────────────
install:          ## Install githubclaw binary to ~/.cargo/bin
	cargo install --path .

reinstall: install  ## Rebuild, install, and restart using the recommended mode for this OS
	@if githubclaw status 2>/dev/null | grep -q "is running"; then \
		githubclaw stop && sleep 2; \
		githubclaw start; \
		echo "✓ Server restarted with latest build."; \
	else \
		if [ "$(UNAME_S)" = "Darwin" ]; then \
			echo "✓ Installed. Server not running (use 'make start' to start on macOS)."; \
		else \
			echo "✓ Installed. Server not running (use 'make start' to start)."; \
		fi; \
	fi

restart:          ## Restart the server using the recommended mode for this OS
	@githubclaw stop && sleep 2
	@githubclaw start

clean:            ## Remove build artifacts
	cargo clean

size: release     ## Show release binary size
	@ls -lh target/release/githubclaw

# ─── Run ──────────────────────────────────────────────────────────────
init:             ## Scaffold .githubclaw/ in current repo
	cargo run -- init

start:            ## Start webhook server using the recommended runtime for this OS
	cargo run -- start

stop:             ## Stop the webhook server (daemon or tmux mode)
	cargo run -- stop

status:           ## Show server status + registered repos
	cargo run -- status

serve:            ## Run webhook server inline in the current shell
	cargo run -- serve

serve-tmux:       ## Start webhook server in tmux explicitly (low-level macOS helper)
	tmux new-session -d -s githubclaw 'githubclaw serve'

logs:             ## Tail daemon logs or attach to tmux-backed inline output
	cargo run -- logs --follow

# ─── Help ─────────────────────────────────────────────────────────────
help:             ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
