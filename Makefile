# Rush Build System Makefile
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

.PHONY: all build release test clean help
.PHONY: fmt fmt-check clippy check lint style-check
.PHONY: install rushtop

# Default target
all: build

# Build targets
build:
	cargo build

release:
	cargo build --release

# Run tests
test: check
	cargo test

# Clean build artifacts
clean:
	cargo clean
	rm -rf target/

# Code formatting (REQUIRED before commits)
fmt:
	@echo "Formatting Rust code (Linux kernel style)..."
	cargo fmt

# Check formatting without modifying files (for CI)
fmt-check:
	@echo "Checking Rust code formatting..."
	cargo fmt -- --check

# Linter - catches common mistakes and enforces best practices
clippy:
	@echo "Running clippy linter..."
	cargo clippy --workspace --bins --tests -- -D warnings

# Fast syntax check without building
check:
	@echo "Type-checking Rust code..."
	cargo check --workspace --bins --tests

# Full lint: format check + clippy
lint: fmt-check clippy

# Aggressive stub detection - ZERO TOLERANCE
style-check:
	@echo "🔍 AGGRESSIVE STUB DETECTION - ZERO TOLERANCE"
	@echo ""
	@./scripts/check-stubs.sh

# Install Rush and rushtop binaries
install:
	cargo install --path .
	@echo "Installed: rush, rushtop"

# Build just rushtop
rushtop:
	cargo build --release --bin rushtop
	@echo "Built: target/release/rushtop"

# Pre-commit checks (run this before committing!)
pre-commit: fmt lint style-check test
	@echo ""
	@echo "✓ All pre-commit checks passed!"
	@echo "  - Code formatted (Linux kernel style)"
	@echo "  - Linter passed"
	@echo "  - Tests passed"
	@echo ""
	@echo "Ready to commit!"

# Help
help:
	@echo "Rush Build System Makefile"
	@echo ""
	@echo "Build targets:"
	@echo "  make build       - Build debug version"
	@echo "  make release     - Build optimized release version"
	@echo "  make test        - Run tests"
	@echo "  make clean       - Clean build artifacts"
	@echo ""
	@echo "Code quality (REQUIRED before commits):"
	@echo "  make fmt         - Format code (Linux kernel style)"
	@echo "  make fmt-check   - Check formatting (CI)"
	@echo "  make clippy      - Run linter"
	@echo "  make check       - Fast type-check"
	@echo "  make lint        - Full lint (format + clippy)"
	@echo "  make style-check - Aggressive stub detection (ZERO TOLERANCE)"
	@echo "  make pre-commit  - Run all pre-commit checks (includes style-check)"
	@echo ""
	@echo "Install:"
	@echo "  make install     - Install rush and rushtop"
	@echo "  make rushtop     - Build just rushtop"
	@echo ""
	@echo "IMPORTANT: Always run 'make pre-commit' before committing!"
