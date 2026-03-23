CARGO ?= cargo
CROSS ?= cross
PROFILE ?= debug
REMOTE_PACKAGE := multicode-remote
TUI_PACKAGE := multicode-tui
REMOTE_STAGE_DIR := target/multicode-remote/tui
TUI_TARGETS := x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
ifeq ($(PROFILE),release)
CARGO_PROFILE_FLAG := --release
else
CARGO_PROFILE_FLAG :=
endif

.PHONY: multicode-remote remote-tui stage-remote-tui clean-remote-tui

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

clean-remote-tui:
	rm -rf $(REMOTE_STAGE_DIR)
