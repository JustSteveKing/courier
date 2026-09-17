# Courier's everyday commands. `make` on its own lists them.
#
#   make run PROJECT=~/Work/my-api    open a project
#   make test-one T=sends_a_request   one test, with its output
#   make check                        what CI would run

SHELL := bash
.DEFAULT_GOAL := help

# Where `make run` opens; empty means the current directory's project.
PROJECT ?=
# A test name filter for `make test-one`.
T ?=
# Passed to `make cli`, e.g. make cli ARGS="run -p ~/Work/my-api"
ARGS ?=
THEME ?=

BINARY := target/debug/courier
RELEASE := target/release/courier
# Preview an Omarchy theme without changing the desktop: make run THEME=tokyo-night
ENV := $(if $(THEME),COURIER_THEME_DIR=/usr/share/omarchy/themes/$(THEME),)

.PHONY: help
help: ## List these targets
	@echo "Courier — make <target>"
	@echo
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| sort \
		| awk -F':.*?## ' '{ printf "  \033[1m%-14s\033[0m %s\n", $$1, $$2 }'
	@echo
	@echo "  Variables: PROJECT=<dir>  T=<test filter>  ARGS=<cli args>  THEME=<omarchy theme>"

# MARK: Building and running

.PHONY: build
build: ## Build the debug binary
	cargo build

.PHONY: run
run: ## Run the app (PROJECT=<dir> to open one)
	$(ENV) cargo run -- $(PROJECT)

.PHONY: restart
restart: build ## Rebuild and restart the running app
	@pkill -f '^$(CURDIR)/$(BINARY)$$' 2>/dev/null || true
	@cd "$${HOME}" && setsid -f $(CURDIR)/$(BINARY) $(PROJECT) >/dev/null 2>&1
	@echo "restarted"

.PHONY: cli
cli: build ## Run the command line: make cli ARGS="run -p ~/Work/my-api"
	@./$(BINARY) $(ARGS)

.PHONY: release
release: ## Build the release binary (thin LTO, stripped)
	cargo build --release
	@ls -la $(RELEASE) | awk '{ printf "%.1f MB\n", $$5 / 1048576 }'

.PHONY: install
install: ## Build a release binary and install it into ~/.local
	./install.sh

# MARK: Checking

.PHONY: check
check: fmt-check lint test ## Everything CI would run: formatting, clippy, tests

.PHONY: test
test: ## Run the whole workspace's tests
	cargo test --workspace

.PHONY: test-one
test-one: ## Run one test with its output: make test-one T=name
	@test -n "$(T)" || { echo "give a test name: make test-one T=sends_a_request"; exit 2; }
	cargo test --workspace $(T) -- --nocapture

.PHONY: test-app test-core test-cli
test-app: ## Tests for the app only (headless UI tests)
	cargo test -p courier
test-core: ## Tests for the engine only
	cargo test -p courier-core
test-cli: ## Tests for the command line only
	cargo test -p courier-cli

.PHONY: lint
lint: ## Clippy over everything, including tests
	cargo clippy --workspace --all-targets

.PHONY: fmt
fmt: ## Format the code
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## Fail if anything isn't formatted
	cargo fmt --all -- --check

.PHONY: fix
fix: ## Apply clippy's and rustfmt's own suggestions
	cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
	cargo fmt --all

.PHONY: doc
doc: ## Open the engine's API documentation
	cargo doc -p courier-core --no-deps --open

# MARK: Housekeeping

.PHONY: outdated
outdated: ## Show dependencies with newer versions (needs cargo-outdated)
	@command -v cargo-outdated >/dev/null || { echo "cargo install cargo-outdated"; exit 2; }
	cargo outdated --workspace --root-deps-only

.PHONY: clean
clean: ## Remove build output
	cargo clean
