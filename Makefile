# The server embeds web/chart.wasm, so build the wasm first.
CARGO ?= cargo
wasm:
	$(CARGO) build -p chart --profile wasm --target wasm32-unknown-unknown
	cp target/wasm32-unknown-unknown/wasm/chart.wasm web/chart.wasm
	if command -v wasm-opt >/dev/null; then wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int web/chart.wasm -o web/chart.wasm; fi

test: wasm
	$(CARGO) test --workspace

run: wasm
	$(CARGO) run --release -p plainscreener

.PHONY: wasm test run
