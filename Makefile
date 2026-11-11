FOUNDRY_BIN ?= $(HOME)/.foundry/bin
FORGE := $(FOUNDRY_BIN)/forge
ANVIL := $(FOUNDRY_BIN)/anvil
NETWORK ?= ethereum
FORK_URL ?= https://ethereum-rpc.publicnode.com
RPC_URL ?= http://127.0.0.1:8545
# The fork tests' own anvil; tests/common/mod.rs connects to this port.
TEST_PORT := 8546
# The block the last passing test run was pinned at. The next run reuses it while anvil's RPC
# cache for it exists, so it reads remote state from disk instead of the public node, which
# serves old blocks' state only for minutes. A failed run leaves no file, so the run after it
# pins the current head.
TEST_BLOCK_FILE := target/fork-test-block
# Where anvil keeps its RPC cache per pinned mainnet block.
FORK_CACHE := $(HOME)/.foundry/cache/rpc/mainnet
BLOCK_NUMBER := {"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}

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

# Starts its own anvil on TEST_PORT, pinned and mining only on demand, runs the fork tests
# then the refork test, and stops the anvil (with SIGINT, so it writes its RPC cache) however
# the tests end. The public node resets connections under a cold load's burst of requests,
# so the anvil retries more often and backs off longer than its defaults.
test-fork:
	@set -e; mkdir -p target; \
	block=$$(cat $(TEST_BLOCK_FILE) 2>/dev/null || true); \
	rm -f $(TEST_BLOCK_FILE); \
	if [ -z "$$block" ] || [ ! -d "$(FORK_CACHE)/$$block" ]; then \
		head=$$(curl -s -X POST -H 'content-type: application/json' --data '$(BLOCK_NUMBER)' $(FORK_URL) | sed -E 's/.*"result":"(0x[0-9a-fA-F]+)".*/\1/'); \
		block=$$(( head )); \
	fi; \
	echo "test fork pinned at block $$block"; \
	$(ANVIL) --fork-url $(FORK_URL) --fork-block-number $$block --port $(TEST_PORT) \
		--retries 20 --fork-retry-backoff 2000 > target/anvil-test.log 2>&1 & \
	anvil=$$!; \
	trap 'kill -INT $$anvil; wait $$anvil || true' EXIT; \
	until curl -s -o /dev/null -X POST -H 'content-type: application/json' --data '$(BLOCK_NUMBER)' http://127.0.0.1:$(TEST_PORT); do sleep 0.2; done; \
	cargo test --features testkit --test fork -- --ignored --test-threads=1; \
	cargo test --features testkit --test refork -- --ignored --test-threads=1; \
	echo $$block > $(TEST_BLOCK_FILE)

bench:
	cargo bench --features testkit

lint:
	cargo fmt --check && cargo clippy --all-targets --features testkit -- -D warnings

teardown:
	scripts/teardown.sh
