fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test

run-sample:
	cargo run -p glowtail-cli -- view samples/mixed.log

run-gui:
	cargo run -p glowtail-gui -- samples/mixed.log

run-gpui:
	cargo run -p glowtail-gpui -- samples/mixed.log
