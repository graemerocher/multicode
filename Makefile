CARGO ?= cargo
CROSS ?= cross
PROFILE ?= debug
REMOTE_PACKAGE := multicode-remote
TUI_PACKAGE := multicode-tui
REMOTE_STAGE_DIR := target/multicode-remote/tui
PACKAGE_ROOT := target/package/multicode-remote-bundle
PACKAGE_ZIP := target/package/multicode-remote-bundle.zip
TUI_TARGETS := x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
REMOTE_TARGET_LINUX := x86_64-unknown-linux-gnu
REMOTE_TARGET_MACOS := aarch64-apple-darwin
ifeq ($(PROFILE),release)
CARGO_PROFILE_FLAG := --release
else
CARGO_PROFILE_FLAG :=
endif

.PHONY: multicode-remote remote-tui stage-remote-tui clean-remote-tui \
	build-bundle-remote package-bundle verify-bundle bundle-zip clean-bundle

multicode-remote: stage-remote-tui
	$(CARGO) build -p $(REMOTE_PACKAGE) $(CARGO_PROFILE_FLAG)

remote-tui: stage-remote-tui

stage-remote-tui:
	mkdir -p $(REMOTE_STAGE_DIR)
	set -e; \
	for target in $(TUI_TARGETS); do \
		$(CROSS) build -p $(TUI_PACKAGE) --target $$target $(CARGO_PROFILE_FLAG); \
		cp target/$$target/$(PROFILE)/$(TUI_PACKAGE) $(REMOTE_STAGE_DIR)/$$target-$(TUI_PACKAGE); \
		chmod +x $(REMOTE_STAGE_DIR)/$$target-$(TUI_PACKAGE); \
	done

build-bundle-remote:
	$(CROSS) build -p $(REMOTE_PACKAGE) --target $(REMOTE_TARGET_LINUX) $(CARGO_PROFILE_FLAG)
	$(CROSS) build -p $(REMOTE_PACKAGE) --target $(REMOTE_TARGET_MACOS) $(CARGO_PROFILE_FLAG)

package-bundle: stage-remote-tui build-bundle-remote
	rm -rf $(PACKAGE_ROOT)
	mkdir -p $(PACKAGE_ROOT)/multicode-remote/tui
	cp target/$(REMOTE_TARGET_LINUX)/$(PROFILE)/$(REMOTE_PACKAGE) $(PACKAGE_ROOT)/multicode-remote-linux
	cp target/$(REMOTE_TARGET_MACOS)/$(PROFILE)/$(REMOTE_PACKAGE) $(PACKAGE_ROOT)/multicode-remote-macos
	cp config.toml $(PACKAGE_ROOT)/config.toml
	cp $(REMOTE_STAGE_DIR)/x86_64-unknown-linux-gnu-$(TUI_PACKAGE) $(PACKAGE_ROOT)/multicode-remote/tui/
	cp $(REMOTE_STAGE_DIR)/aarch64-unknown-linux-gnu-$(TUI_PACKAGE) $(PACKAGE_ROOT)/multicode-remote/tui/
	chmod +x $(PACKAGE_ROOT)/multicode-remote-linux
	chmod +x $(PACKAGE_ROOT)/multicode-remote-macos
	chmod +x $(PACKAGE_ROOT)/multicode-remote/tui/x86_64-unknown-linux-gnu-$(TUI_PACKAGE)
	chmod +x $(PACKAGE_ROOT)/multicode-remote/tui/aarch64-unknown-linux-gnu-$(TUI_PACKAGE)

verify-bundle: package-bundle
	test -x $(PACKAGE_ROOT)/multicode-remote-linux
	test -x $(PACKAGE_ROOT)/multicode-remote-macos
	test -f $(PACKAGE_ROOT)/config.toml
	test -x $(PACKAGE_ROOT)/multicode-remote/tui/x86_64-unknown-linux-gnu-$(TUI_PACKAGE)
	test -x $(PACKAGE_ROOT)/multicode-remote/tui/aarch64-unknown-linux-gnu-$(TUI_PACKAGE)

bundle-zip: verify-bundle
	rm -f $(PACKAGE_ZIP)
	python3 -c 'from pathlib import Path; import zipfile; workspace = Path.cwd(); package_root = workspace / "target/package/multicode-remote-bundle"; archive_path = workspace / "target/package/multicode-remote-bundle.zip"; archive = zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED); [archive.write(path, path.relative_to(package_root)) for path in sorted(package_root.rglob("*")) if path.is_file()]; archive.close()'
	unzip -l $(PACKAGE_ZIP)
	unzip -l $(PACKAGE_ZIP) | grep -F ' multicode-remote-linux'
	unzip -l $(PACKAGE_ZIP) | grep -F ' multicode-remote-macos'
	unzip -l $(PACKAGE_ZIP) | grep -F ' config.toml'
	unzip -l $(PACKAGE_ZIP) | grep -F ' multicode-remote/tui/x86_64-unknown-linux-gnu-$(TUI_PACKAGE)'
	unzip -l $(PACKAGE_ZIP) | grep -F ' multicode-remote/tui/aarch64-unknown-linux-gnu-$(TUI_PACKAGE)'

clean-remote-tui:
	rm -rf $(REMOTE_STAGE_DIR)

clean-bundle:
	rm -rf $(PACKAGE_ROOT) $(PACKAGE_ZIP)
