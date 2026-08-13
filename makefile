.PHONY: all make_tmp install_tywaves_sigtrail install_sigtrail clean
TYWAVES_CHISEL_REPO = https://github.com/jarlb/tywaves-chisel.git
CARGO_BIN_DIR = ~/.cargo/bin/

all: install_tywaves_sigtrail install_sigtrail

make_tmp:
	mkdir -p tmp

clean:
	@rm -rf ./tmp

.ONESHELL:
install_tywaves_sigtrail: make_tmp
	cd tmp
	git clone $(TYWAVES_CHISEL_REPO)
	cd tywaves-chisel
	make all

.ONESHELL:
install_sigtrail:
	cargo tauri build --no-bundle
	cp ./target/release/sigtrail $(CARGO_BIN_DIR)
