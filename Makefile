# Listaculous — run `make` or `make help` to see all targets.

SHELL := /bin/bash
.DEFAULT_GOAL := help

# Random free port in this range, picked once and saved to .env so it stays stable.
PICK_PORT = for i in $$(seq 1 50); do p=$$((RANDOM % 40000 + 20000)); \
	lsof -nP -iTCP:$$p -sTCP:LISTEN >/dev/null 2>&1 || { echo $$p; break; }; done

# First run: create a local .env from the example (links default to http://localhost:PORT).
ifeq ($(wildcard .env),)
$(shell sed -e 's/^BASE_URL=/\# BASE_URL=/' -e 's/^TRUST_PROXY=.*/TRUST_PROXY=false/' .env.example > .env; \
	printf '\033[33mCreated .env for local use (set BASE_URL in it before going public)\033[0m\n' >&2)
endif

# Settings from .env apply here too; the command line wins: make up PORT=9000
include .env

# No PORT yet: pick a random free one and remember it in .env.
ifeq ($(strip $(PORT)),)
PORT := $(shell $(PICK_PORT))
$(shell printf '\n# Host port for make/docker compose (randomly chosen; change freely or run make new-port)\nPORT=$(PORT)\n' >> .env; \
	printf '\033[33mPicked port $(PORT) and saved it to .env\033[0m\n' >&2)
endif
URL     := http://localhost:$(PORT)
COMPOSE := PORT=$(PORT) docker compose
SERVICE := listaculous
BACKUP  := backups/listaculous-$(shell date +%Y%m%d-%H%M%S).db

export PORT

# Colors
B := \033[1m
G := \033[32m
Y := \033[33m
R := \033[31m
N := \033[0m

##@ Docker (production-like)

.PHONY: up
up: port-check ## Build, start, wait until healthy, and open in the browser
	@printf "$(B)▶ Starting Listaculous on $(URL)…$(N)\n"
	@$(COMPOSE) build --quiet || { printf "$(R)✘ Build failed$(N) — rerun with: docker compose build\n"; exit 1; }
	@$(COMPOSE) up -d --quiet-pull 2>&1 | grep -v -e '^ *$$' -e 'Creat' -e 'Start' || true
	@$(MAKE) --no-print-directory wait
	@printf "$(G)✔ Listaculous is up at $(URL)$(N)\n"
	@$(MAKE) --no-print-directory open

.PHONY: down
down: ## Stop and remove the container (data volume is kept)
	@$(COMPOSE) down

.PHONY: restart
restart: ## Restart the container
	@$(COMPOSE) restart
	@$(MAKE) --no-print-directory wait

.PHONY: rebuild
rebuild: ## Rebuild the image from scratch (no cache) and restart
	@$(COMPOSE) build --no-cache
	@$(COMPOSE) up -d
	@$(MAKE) --no-print-directory wait

.PHONY: logs
logs: ## Follow container logs
	@$(COMPOSE) logs -f --tail=100

.PHONY: ps
ps: ## Show container status and memory usage
	@$(COMPOSE) ps
	@docker stats --no-stream --format 'table {{.Name}}\t{{.MemUsage}}\t{{.CPUPerc}}' $$($(COMPOSE) ps -q) 2>/dev/null || true

.PHONY: shell
shell: ## Open a shell inside the running container
	@$(COMPOSE) exec $(SERVICE) sh

.PHONY: open
open: ## Open the admin UI in your browser
	@if command -v open >/dev/null; then open $(URL); \
	elif command -v xdg-open >/dev/null; then xdg-open $(URL) >/dev/null 2>&1; \
	else printf "Open $(URL) in your browser\n"; fi

.PHONY: wait
wait: ## Wait until the server answers its health check
	@printf "Waiting for $(URL)/healthz "
	@for i in $$(seq 1 60); do \
		if curl -fs $(URL)/healthz >/dev/null 2>&1; then printf " $(G)ready$(N)\n"; exit 0; fi; \
		printf "."; sleep 1; \
	done; printf " $(R)timed out$(N) — check 'make logs'\n"; exit 1

##@ Data

.PHONY: backup
backup: ## Back up the SQLite database AND its encryption key into ./backups
	@mkdir -p backups
	@$(COMPOSE) exec -T $(SERVICE) sh -c 'cat /data/listaculous.db' > $(BACKUP)
	@# Stored secrets (SES/SMTP/AWS/Porkbun keys) are encrypted with this key; a backup without it
	@# can't decrypt them. (Not needed if you set LISTACULOUS_SECRET_KEY yourself.)
	@$(COMPOSE) exec -T $(SERVICE) sh -c 'cat /data/secret.key 2>/dev/null' > $(BACKUP).key; \
	if [ -s $(BACKUP).key ]; then chmod 600 $(BACKUP).key; \
	  printf "$(G)✔ Saved $(BACKUP) + .key$(N) (keep the .key private; restore both with make restore FILE=$(BACKUP))\n"; \
	else rm -f $(BACKUP).key; printf "$(G)✔ Saved $(BACKUP)$(N)\n"; fi
	@printf "  Tip: stop the container first for a fully consistent copy.\n"

.PHONY: restore
restore: ## Restore a backup made by `make backup` (FILE=backups/….db); replaces current data
	@test -n "$(FILE)" -a -f "$(FILE)" || { printf "$(R)✘ usage: make restore FILE=backups/listaculous-….db$(N)\n"; exit 1; }
	@read -p "Replace the current database with $(FILE)? Type 'yes': " a; [ "$$a" = "yes" ] || { echo Aborted.; exit 1; }
	@$(COMPOSE) stop $(SERVICE)
	@$(COMPOSE) run --rm -T --no-deps --entrypoint sh $(SERVICE) -c 'cat > /data/listaculous.db && rm -f /data/listaculous.db-wal /data/listaculous.db-shm' < $(FILE)
	@if [ -f "$(FILE).key" ]; then $(COMPOSE) run --rm -T --no-deps --entrypoint sh $(SERVICE) -c 'cat > /data/secret.key && chmod 600 /data/secret.key' < $(FILE).key; fi
	@$(COMPOSE) up -d $(SERVICE)
	@$(MAKE) --no-print-directory wait
	@printf "$(G)✔ Restored $(FILE)$(N)\n"

.PHONY: nuke
nuke: ## ⚠ Stop and DELETE all data (asks for confirmation)
	@read -p "Delete the container AND all subscriber data? Type 'yes': " a; \
	if [ "$$a" = "yes" ]; then $(COMPOSE) down -v && printf "$(Y)Data deleted.$(N)\n"; else echo "Aborted."; fi

##@ Local development

.PHONY: dev
dev: web/node_modules ## Run API (cargo) + UI dev server (vite, hot reload) together
	@printf "$(B)▶ API on :8080, UI on http://localhost:5173$(N)\n"
	@trap 'kill 0' INT TERM EXIT; \
		BASE_URL=http://localhost:5173 cargo run & \
		(cd web && npm run dev -- --open) & \
		wait

.PHONY: run
run: web-build ## Run the release-ish server locally (serves built UI on PORT)
	LISTACULOUS_BIND=127.0.0.1:$(PORT) BASE_URL=$(URL) cargo run --release

.PHONY: build
build: web-build ## Build the release binary and the UI
	cargo build --release

.PHONY: web-build
web-build: web/node_modules ## Build the Svelte UI into web/dist
	cd web && npm run build

web/node_modules: web/package.json
	cd web && npm install
	@touch $@

##@ Quality

.PHONY: check
check: fmt-check lint test ## Run everything CI should run

.PHONY: test
test: ## Run Rust tests
	cargo test

.PHONY: lint
lint: ## Clippy with warnings as errors
	cargo clippy --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format Rust code
	cargo fmt

.PHONY: fmt-check
fmt-check: ## Verify Rust formatting
	cargo fmt -- --check

##@ Housekeeping

.PHONY: clean
clean: ## Remove build artifacts (keeps your data)
	cargo clean
	rm -rf web/dist web/node_modules

.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\n$(B)Listaculous$(N)  —  make $(G)<target>$(N) [PORT=$(PORT)]\n"} \
		/^[a-zA-Z_-]+:.*?##/ { printf "  $(G)%-12s$(N) %s\n", $$1, $$2 } \
		/^##@/ { printf "\n$(B)%s$(N)\n", substr($$0, 5) }' $(MAKEFILE_LIST)
	@echo

.PHONY: new-port
new-port: ## Pick a new random free port, save it to .env (then run make up)
	@p=$$($(PICK_PORT)); \
	if grep -q '^PORT=' .env; then sed -i.bak "s/^PORT=.*/PORT=$$p/" .env && rm -f .env.bak; else printf '\nPORT=%s\n' $$p >> .env; fi; \
	printf "$(G)✔ PORT=$$p saved to .env$(N) — run 'make down && make up' to switch\n"

# --- internal -------------------------------------------------------------------------

# Fail early with a helpful message if something else already owns the port.
.PHONY: port-check
port-check:
	@if lsof -nP -iTCP:$(PORT) -sTCP:LISTEN >/dev/null 2>&1 && \
	   ! $(COMPOSE) ps --status running -q 2>/dev/null | grep -q .; then \
		printf "$(R)✘ Port $(PORT) is already in use by another program.$(N) Run: make new-port\n"; exit 1; \
	fi
