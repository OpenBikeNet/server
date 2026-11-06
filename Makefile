# OpenBikeNet server — common tasks. Run `make` to see all targets.
#
# Rust is optional on the host: if `cargo` is missing, the targets run it in
# throwaway Docker containers instead.

SHELL := /bin/bash
.DEFAULT_GOAL := help

COMPOSE ?= docker compose
RUST_IMAGE ?= rust:1-bookworm
HEALTH_TIMEOUT ?= 90

# Build number shown in /internal/metrics (compose passes these as build args).
export OBN_BUILD_SHA ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo dev)
export OBN_BUILD_DATE ?= $(shell date -u +%Y-%m-%dT%H:%MZ)

# Host port, as in .env (default 8090).
PORT = $$(grep -E '^OBN_HTTP_PORT=' .env 2>/dev/null | cut -d= -f2 | grep . || echo 8090)

ifneq ($(shell command -v cargo 2>/dev/null),)
  CARGO := cargo
else
  CARGO := docker run --rm -t --network host -v "$(CURDIR)":/src -w /src -v obn-cargo-registry:/usr/local/cargo/registry -v obn-cargo-target:/src/target $(RUST_IMAGE) sh -c 'rustup component add clippy rustfmt >/dev/null 2>&1; cargo "$$@"' cargo
endif

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage: make \033[36m<target>\033[0m\n"} \
		/^[a-zA-Z0-9_-]+:.*?##/ { printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2 } \
		/^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)

##@ Run

.PHONY: setup
setup: ## Create .env from .env.example, or add settings that are new there
	@if [ -f .env ]; then sh tools/env-sync.sh .env.example .env; \
	else cp .env.example .env && echo "Created .env — adjust it if needed."; fi

.PHONY: up
up: setup ## Build and start in the background, then wait until healthy
	$(COMPOSE) up -d --build --remove-orphans
	@$(MAKE) --no-print-directory health
	@echo "OpenBikeNet is running: http://localhost:$(PORT)"

.PHONY: down
down: guard ## Stop and remove the containers (asks first if riders are on; FORCE=1 skips)
	$(COMPOSE) down

.PHONY: restart
restart: guard ## Restart the containers without rebuilding (asks first if riders are on; FORCE=1 skips)
	$(COMPOSE) restart

.PHONY: logs
logs: ## Follow the logs
	$(COMPOSE) logs -f --tail=200

.PHONY: guard
guard: # Ask before dropping riders: used by update, restart and down (FORCE=1 skips it)
	@if [ -n "$(FORCE)" ]; then exit 0; fi; \
	out=$$($(COMPOSE) exec -T server obn-server riders 2>/dev/null) || { echo "No running server to check for riders — continuing."; exit 0; }; \
	n=$$(printf '%s\n' "$$out" | head -1); \
	case "$$n" in ''|*[!0-9]*) echo "Could not read the rider count — continuing."; exit 0;; esac; \
	[ "$$n" -gt 0 ] || exit 0; \
	where=$$(printf '%s\n' "$$out" | tail -n +2 | awk -F'\t' '{ printf "%s%s %s", (NR > 1 ? ", " : ""), $$1, $$2 }'); \
	napps=$$(printf '%s\n' "$$out" | tail -n +2 | grep -c .); \
	if [ "$$n" -eq 1 ]; then who="1 rider from"; else who="$$n riders from"; fi; \
	if [ "$$napps" -eq 1 ]; then from="1 app is"; else from="$$napps apps are"; fi; \
	echo "$$who $$from on the server ($$where)."; \
	echo "This restarts it and drops them."; \
	if [ ! -t 0 ]; then echo "No terminal to ask on: run with FORCE=1 to go ahead anyway."; exit 1; fi; \
	read -r -p "Go ahead anyway? [y/N] " answer; \
	case "$$answer" in y|Y|yes|YES) ;; *) echo "Aborted, nothing changed."; exit 1;; esac

.PHONY: ps
ps: ## Show container status
	$(COMPOSE) ps

.PHONY: health
health: ## Wait until the server reports healthy
	@echo -n "Waiting for the server to become healthy"; \
	for i in $$(seq 1 $(HEALTH_TIMEOUT)); do \
		if $(COMPOSE) exec -T server obn-server healthcheck >/dev/null 2>&1; then echo " ok"; exit 0; fi; \
		echo -n "."; sleep 1; \
	done; \
	echo " timed out"; $(COMPOSE) logs --tail=50 server; exit 1

##@ Deploy

.PHONY: update
update: setup guard ## Pull, rebuild and restart (asks first if riders are on; FORCE=1 skips)
	@if [ -n "$$(git status --porcelain --untracked-files=no)" ]; then \
		echo "Local changes found — commit or stash them before updating."; exit 1; fi
	git pull --ff-only
	git submodule update --init
	@$(MAKE) --no-print-directory setup
	$(COMPOSE) build --pull
	$(COMPOSE) up -d --remove-orphans
	@$(MAKE) --no-print-directory health
	docker image prune -f >/dev/null
	@echo "Updated to $$(git rev-parse --short HEAD)."

.PHONY: build
build: ## Build the Docker image without starting it
	$(COMPOSE) build --pull

##@ Develop

.PHONY: dev
dev: ## Run the server locally with cargo on http://localhost:8090 (reads .env.dev)
	set -a && { [ ! -f .env.dev ] || . ./.env.dev; } && set +a && \
		OBN_BIND=127.0.0.1:8090 RUST_LOG=$${RUST_LOG:-info,obn_server=debug} cargo run -p obn-server

.PHONY: fake
fake: ## Start fake riders against the local server (pass ARGS="--riders 3")
	$(CARGO) run -p obn-fake-app -- --url ws://localhost:$(PORT)/v0 $(ARGS)

.PHONY: test
test: ## Run the Rust tests (unit and end-to-end)
	$(CARGO) test --workspace

.PHONY: lint
lint: ## rustfmt check and clippy with warnings as errors
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format the Rust code
	$(CARGO) fmt --all

.PHONY: check
check: lint test ## Everything CI runs

.PHONY: lock
lock: ## Create or refresh Cargo.lock (commit it afterwards)
	$(CARGO) generate-lockfile

.PHONY: clean
clean: ## Remove build output
	rm -rf target
