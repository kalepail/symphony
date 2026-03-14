.PHONY: help all setup deps build fmt fmt-check lint test coverage ci dialyzer e2e pr-body-check

MIX ?= $(shell if command -v mix >/dev/null 2>&1; then echo mix; elif command -v mise >/dev/null 2>&1; then echo 'mise exec -- mix'; else echo mix; fi)

help:
	@echo "Targets: setup, deps, fmt, fmt-check, lint, test, coverage, dialyzer, e2e, pr-body-check, ci"

setup:
	$(MIX) setup

deps:
	$(MIX) deps.get

build:
	$(MIX) build

fmt:
	$(MIX) format

fmt-check:
	$(MIX) format --check-formatted

lint:
	$(MIX) lint

coverage:
	$(MIX) test --cover

test:
	$(MIX) test

dialyzer:
	$(MIX) deps.get
	$(MIX) dialyzer --format short

e2e:
	@set -a; \
	if [ -f .env ]; then . ./.env; fi; \
	if [ -f .env.local ]; then . ./.env.local; fi; \
	set +a; \
	if [ -z "$$LINEAR_API_KEY" ]; then \
		echo "LINEAR_API_KEY is required for \`make e2e\`."; \
		echo "Set it in elixir/.env, elixir/.env.local, or export it first."; \
		exit 1; \
	fi; \
	if ! command -v codex >/dev/null 2>&1; then \
		echo "\`codex\` must be on PATH for \`make e2e\`."; \
		exit 1; \
	fi; \
	SYMPHONY_RUN_LIVE_E2E=1 $(MIX) test test/symphony_elixir/live_e2e_test.exs

pr-body-check:
	@if [ -z "$(FILE)" ]; then \
		echo "FILE is required, e.g. make pr-body-check FILE=/tmp/pr_body.md"; \
		exit 1; \
	fi
	$(MIX) pr_body.check --file "$(FILE)"

ci:
	$(MAKE) setup
	$(MAKE) build
	$(MAKE) fmt-check
	$(MAKE) lint
	$(MAKE) coverage
	$(MAKE) dialyzer

all: ci
