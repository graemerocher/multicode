- Git commits should have the user's "Author" and be "Co-Authored-By" the AI agent that made the change.
- Keep per-workspace metadata minimal and outside workspace directories where possible.
- In the TUI, key binds should only be shown when they are usable for the current entry.
- When interacting with the opencode server, prefer the existing SSE infrastructure over polling.
- IO operations should happen asynchronously so they do not make the TUI unresponsive.
- When adding dependencies, use the latest stable version. For the version number, specify only the major release, unless that major release is "0", in which case specify major + minor.
- Do not bother preserving rust API stability, this project is not a library

## Testing

- Follow test-driven development where possible.
- Do not test obvious things, e.g. that accessor methods return what you passed in.
- Prefer integration testing over mock-based testing. 
- Tests may assume availability of bwrap, systemd-run, and opencode-cli.
- When writing tests that involve running opencode-cli, make sure to isolate ~/.config/opencode, ~/.local/share/opencode, ~/.cache/opencode so that there is no interference from or to user settings. You can use a tmpfs to isolate. 

## Isolation

- Mounting policy: host filesystem is ro by default; use narrow rw binds only for required paths. Avoid broad rw mounts.
- Session isolation rule: isolate opencode **sessions** per workspace; do not broadly isolate unrelated opencode state.
- Environment passthrough should be minimal and explicit; do not copy broad env prefixes unless requested.
