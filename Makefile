# genparse — common developer tasks. Run `make help` for a list.

BIN      := genparse
IMAGE    ?= genparse
TAG      ?= latest
CONFIG   ?= config.toml
PORT     ?= 8080

.DEFAULT_GOAL := help

.PHONY: help build release run test lint fmt check clean docker-build docker-run

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

build: ## Debug build
	cargo build

release: ## Optimised build (target/release/genparse)
	cargo build --release

run: ## Run the HTTP service with the browser UI enabled
	cargo run --release -- --config $(CONFIG) serve --ui

test: ## Run the test suite
	cargo test

lint: ## Clippy with warnings as errors, plus a formatting check
	cargo clippy --all-targets -- -D warnings
	cargo fmt --all -- --check

fmt: ## Format the code
	cargo fmt --all

check: lint test ## Everything CI runs

clean: ## Remove build output and the on-disk cache
	cargo clean
	rm -rf .genparse-cache

docker-build: ## Build the container image
	docker build -t $(IMAGE):$(TAG) .

docker-run: ## Run the image, passing provider keys from the environment
	docker run --rm -p $(PORT):8080 \
		-e ANTHROPIC_API_KEY -e OPENAI_API_KEY -e GEMINI_API_KEY \
		-v genparse-cache:/app/.genparse-cache \
		$(IMAGE):$(TAG)
