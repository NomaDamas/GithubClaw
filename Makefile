.PHONY: install lint format typecheck deps test check all clean

# Install dependencies with uv
install:
	uv sync --dev

# Lint with ruff
lint:
	ruff check src/

# Format with ruff
format:
	ruff format src/
	ruff check --fix src/

# Type check with ty
typecheck:
	ty check src/

# Check dependency issues with deptry
deps:
	deptry src/

# Run tests
test:
	uv run pytest tests/ -v

# Run all checks (lint + typecheck + deps)
check: lint typecheck deps
	@echo "All checks passed."

# Run everything (format + check + test)
all: format check test

# Clean build artifacts
clean:
	rm -rf dist/ build/ .ruff_cache/ .pytest_cache/
	find . -type d -name __pycache__ -exec rm -rf {} + 2>/dev/null || true
	find . -type f -name "*.pyc" -delete 2>/dev/null || true
