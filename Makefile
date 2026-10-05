.PHONY: run build clean

build:
	cd kernel && cargo build

run:
	cd kernel && cargo run

clean:
	cargo clean
