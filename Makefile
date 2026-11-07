FOUNDRY_BIN ?= $(HOME)/.foundry/bin
FORGE := $(FOUNDRY_BIN)/forge
ANVIL := $(FOUNDRY_BIN)/anvil
NETWORK ?= ethereum
FORK_URL ?= https://ethereum-rpc.publicnode.com
RPC_URL ?= http://127.0.0.1:8545

-include .env
export

.PHONY: setup abi test-contracts fork refork fund sweep journal run run-headless test test-fork bench lint teardown

setup:
	@if [ ! -x "$(FORGE)" ]; then \
		curl -L https://foundry.paradigm.xyz | bash; \
		$(FOUNDRY_BIN)/foundryup --install stable; \
	fi
	@if [ ! -d lib/forge-std ]; then \
		$(FORGE) install foundry-rs/forge-std --no-git; \
	fi
	$(FORGE) --version

abi:
	$(FORGE) build
	cp out/FlashArbitrage.sol/FlashArbitrage.json abi/

test-contracts:
	$(FORGE) test --fork-url $(FORK_URL)

fork:
	$(ANVIL) --fork-url $(FORK_URL) --block-time 2

refork:
	curl -s -X POST -H 'content-type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"anvil_reset","params":[{"forking":{"jsonRpcUrl":"$(FORK_URL)"}}]}' $(RPC_URL)

fund:
	cargo run --release --bin fund

sweep:
	cargo run --release --bin sweep

journal:
	cargo run --release --bin journal

run:
	NETWORK=$(NETWORK) cargo run --release --bin arbitrage

run-headless:
	NETWORK=$(NETWORK) cargo run --release --bin arbitrage -- --headless

test:
	cargo test

test-fork:
	cargo test --features testkit --test fork -- --ignored --test-threads=1
	cargo test --features testkit --test refork -- --ignored --test-threads=1

bench:
	cargo bench --features testkit

lint:
	cargo fmt --check && cargo clippy --all-targets --features testkit -- -D warnings

teardown:
	scripts/teardown.sh
