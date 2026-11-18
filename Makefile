# The server embeds web/chart.wasm, so build the wasm first.
CARGO ?= cargo
wasm:
	$(CARGO) build -p chart --profile wasm --target wasm32-unknown-unknown
	cp target/wasm32-unknown-unknown/wasm/chart.wasm web/chart.wasm
	# The features rustc's wasm32 target enables by default (all baseline in browsers).
	if command -v wasm-opt >/dev/null; then wasm-opt -Oz --enable-sign-ext --enable-mutable-globals --enable-multivalue \
		--enable-reference-types --enable-bulk-memory --enable-nontrapping-float-to-int web/chart.wasm -o web/chart.wasm; fi

test: wasm
	$(CARGO) test --workspace

run: wasm
	$(CARGO) run --release -p plainscreener

.PHONY: wasm test run
