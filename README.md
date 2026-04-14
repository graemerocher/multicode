# multicode

… runs isolated AI coding agent instances in parallel.

The [Micronaut Project](https://micronaut.io/) gets hundreds of issue reports from users. Many of them are easy to 
solve, but still take time to understand, debug and fix. AI agents can solve many of these issues on their own. 
*multicode* speeds up and parallelizes this work.

> [!WARNING]
> This project is (obviously) heavily vibe-coded. Read the code at your own risk.

<img src="docs/images/overview.png" alt="Overview">

## Running

```bash
cargo run --bin multicode-tui config.toml
```

## Workspaces

*multicode* parallelizes work in **workspaces**. They are short-lived and isolated. Typically, a workspace is used for
a single issue report. A workspace gets its own working directory and agent session, so you can work from a blank
slate.

## Autonomous queueing

When a workspace is assigned to a GitHub repository, *multicode* can scan for issues and queue multiple issue tasks in
that workspace.

Queueing is controlled in `config.toml` with the `[autonomous]` section:

```toml
[autonomous]
max-parallel-issues = 5
issue-scan-delay-seconds = 900
```

Notes:

- `max-parallel-issues` controls how many issue tasks a workspace may queue at once.
- The default `max-parallel-issues` value is `5`.
- If a workspace already has that many queued tasks, it will not scan in additional issues until you remove, finish,
  or otherwise clear some of the existing tasks.
- `issue-scan-delay-seconds` controls how often background issue scans are retried. The default is `900` seconds
  (15 minutes).

## Agent configuration

The agent used inside workspaces is configured globally in `config.toml` with the `[agent]` section.

OpenCode remains the default:

```toml
[agent]
provider = "opencode"

# Backwards-compatible top-level command list.
opencode = ["opencode-cli", "opencode"]

[agent.opencode]
commands = ["opencode-cli", "opencode"]
```

To use Codex instead:

```toml
[agent]
provider = "codex"

[agent.codex]
commands = ["codex"]
model = "gpt-5-codex"
model-provider = "openai"
approval-policy = "never"
sandbox-mode = "external-sandbox"
network-access = "enabled"
```

Notes:

- `provider` is global for the whole multicode instance. A single TUI session uses either OpenCode or Codex for all workspaces.
- `commands` is the host-side command resolution order. The first installed command is used.
- OpenCode keeps the existing top-level `opencode = [...]` setting for backwards compatibility. If `[agent.opencode].commands` is omitted, multicode falls back to that list.
- Codex workspaces use `codex app-server` inside the isolate/container and `codex resume --remote ...` when attaching from the TUI.
- `profile` is optional for Codex. If you set it, it must name a real profile from your host `~/.codex/config.toml`.
- For Codex, `approval-policy = "never"` suppresses approval prompts, `sandbox-mode = "workspace-write"` keeps Codex's own sandbox active, and `sandbox-mode = "external-sandbox"` tells Codex to trust the outer multicode sandbox such as the Apple container runtime.
- `network-access = "enabled"` is the practical setting for issue fixing workflows that need GitHub, dependency downloads, or web access. With `external-sandbox`, this is sent as Codex app-server `sandboxPolicy.networkAccess`.
- `runtime.image` is still the global image override. If you want separate images, use `runtime.opencode-image` and `runtime.codex-image`.

## Isolation

Workspaces are *isolated* from each other. This isolation is for safety and convenience, it **does not provide 
security**. SSH keys to clone git repos and GitHub CLI credentials to create PRs remain available to each workspace, 
but are read-only to reduce potential impact when an AI agent randomly goes haywire. Only the workspace directory is
writable, and some shared directories (e.g. `~/.gradle`).

Workspaces also have get limited in the amount of RAM and CPU they can use. This prevents OOM conditions in one 
workspace from affecting other workspaces or even your desktop.

Isolation is implemented using `systemd-run` (for resource constraints) and 
[`bwrap`](https://github.com/containers/bubblewrap) (for read/write isolation). These tools are **Linux only**, so 
*multicode* will not work on other operating systems.

On newer Apple Silicon Macs, there is also an experimental Apple `container` runtime backend. It
reuses the existing `[isolation]` configuration for readable, writable, isolated, and `tmpfs`
paths, and maps CPU / memory limits onto container allocation settings.

### macOS setup for Apple containers

If you want to run *multicode* on macOS with `backend = "apple-container"`, the practical
requirements are:

- An Apple Silicon Mac. The Apple container backend is intended for newer Apple Silicon systems.
- The Apple `container` CLI installed and working on the host. `multicode` shells out to
  `container run`, `container exec`, `container list`, and `container build`.
- A working Rust toolchain on the host so you can run `cargo run --bin multicode-tui ...`.
- `tmux` on the host. The TUI uses it for interactive attach sessions.
- The host-side agent CLI installed for the provider you choose:
  - OpenCode: `opencode-cli` or `opencode`
  - Codex: `codex`
- A local Apple container image for the selected provider.
- GitHub authentication on the host if you want issue scanning, PR creation, builds, review
  status, or authenticated git pushes.

There are also image-level requirements. The container image must contain the tools the agent uses
inside the isolated workspace, not just on the host:

- `git`
- `gh`
- the provider CLI you selected:
  - OpenCode image: `opencode`
  - Codex image: `codex`
- the language/toolchain your repositories need, for example Java / Gradle for Micronaut work

This repository already contains an Apple-container build recipe. To build the local images:

```bash
./apple-container/build-local.sh
```

That script expects the host `container` CLI to be available and produces:

- `multicode-java25:latest` and `multicode-opencode-java25:latest` for OpenCode
- `multicode-codex-java25:latest` for Codex

Host configuration also matters because Apple-container workspaces deliberately reuse some host
state:

- OpenCode reuses host config from `~/.config/opencode` and authentication from
  `~/.local/share/opencode/auth.json` when you mount them.
- Codex workspaces synthesize a per-workspace `CODEX_HOME` from the host `~/.codex`
  configuration, including `config.toml`, `auth.json`, `AGENTS.md`, and `skills`.
- Git uses your host `~/.gitconfig`, which multicode exposes automatically inside Apple
  containers via `GIT_CONFIG_GLOBAL`.
- If you want GitHub integration or MCP access inside the container, configure `[github]` and
  pass through any required token env vars with `[isolation].inherit-env`.

The minimum setup flow on macOS is therefore:

1. Install and verify the Apple `container` CLI on the host.
2. Install Rust and `tmux` on the host.
3. Install the host agent CLI you want to use (`opencode` or `codex`).
4. Authenticate that agent on the host so the reused host config files actually contain valid
   credentials.
5. Build an Apple container image, or provide your own compatible image.
6. Set `[runtime].backend = "apple-container"` and point `image`, `opencode-image`, or
   `codex-image` at that image.
7. Configure `[isolation]` mounts for caches and credentials you want the workspace to reuse, such
   as `~/.gradle`, `~/.m2/repository`, `~/.config/gh`, and provider-specific config paths.

If `container` is missing, the image does not include the selected agent CLI, or the host does not
have the matching CLI for TUI attach, Apple-container workspaces will start or attach incorrectly.

OpenCode example:

```toml
[agent]
provider = "opencode"

[runtime]
backend = "apple-container"
opencode-image = "ghcr.io/example/multicode-opencode-java25:latest"

[isolation]
writable = ["~/.gradle", "~/.m2/repository", "~/.config/gh"]
readable = ["~/.config/opencode", "~/.local/share/opencode/auth.json"]
isolated = ["~/.local/share/opencode", "~/.local/state/opencode"]
tmpfs = ["/tmp"]
inherit-env = ["HOME", "PATH", "XDG_RUNTIME_DIR", "GITHUB_MCP_TOKEN"]
memory-max = "16 GiB"
cpu = "300%"
```

Mounting `~/.config/opencode` read-only lets the container see the same profiles, models,
skills, and other OpenCode configuration as the host. This is useful if you manage local
profiles with tools like `ocp`. Keep `~/.local/share/opencode` and `~/.local/state/opencode`
isolated so session state remains per-workspace.

Codex example:

```toml
[agent]
provider = "codex"

[agent.codex]
commands = ["codex"]
model = "gpt-5-codex"
model-provider = "openai"
approval-policy = "never"
sandbox-mode = "external-sandbox"
network-access = "enabled"

[runtime]
backend = "apple-container"
codex-image = "ghcr.io/example/multicode-codex-java25:latest"

[isolation]
add-skills-from = ["./workspace-skills"]
writable = ["~/.gradle", "~/.m2/repository", "~/.config/gh"]
inherit-env = ["HOME", "PATH", "XDG_RUNTIME_DIR", "GITHUB_MCP_TOKEN"]
memory-max = "16 GiB"
cpu = "300%"
```

For Codex, multicode prepares a synthetic per-workspace `CODEX_HOME` inside the isolate/container.
It copies the host `~/.codex/config.toml`, `~/.codex/auth.json`, `~/.codex/AGENTS.md`, and
`~/.codex/skills`, then merges in any `add-skills-from` mounts. This keeps Codex session state
isolated per workspace while still reusing your host configuration and credentials.

If you want Codex to behave more autonomously inside an Apple container, prefer:

```toml
[agent.codex]
approval-policy = "never"
sandbox-mode = "external-sandbox"
network-access = "enabled"
```

That combination keeps multicode's Apple container as the real isolation boundary while avoiding repeated Codex approval prompts for normal shell execution.

If you maintain two images, the practical split is:

```toml
[runtime]
backend = "apple-container"
opencode-image = "ghcr.io/example/multicode-opencode-java25:latest"
codex-image = "ghcr.io/example/multicode-codex-java25:latest"
```

Use the OpenCode image for the existing OpenCode workflow and a Codex image that includes the `codex` CLI and any Codex-specific bootstrap you need.

To build both local Apple-container images from this repository:

```bash
./apple-container/build-local.sh
```

The Codex image build pins the installed Codex CLI to the version declared in
[`apple-container/Containerfile`](/Users/graemerocher/dev/micronaut/multicode/apple-container/Containerfile)
via `CODEX_VERSION` so container behavior stays reproducible across rebuilds. Update that build arg
when you intentionally want to move the image to a newer Codex release.

You can also override the pinned version at build time without editing the file:

```bash
CODEX_VERSION=0.120 ./apple-container/build-local.sh
```

That script produces:

- `multicode-java25:latest` and `multicode-opencode-java25:latest` for the OpenCode workflow
- `multicode-codex-java25:latest` for the Codex workflow

The split keeps the existing OpenCode image compatible while allowing the Codex image to install Codex-specific tooling without changing the OpenCode bootstrap path.

Apple workspaces also expose the host `~/.gitconfig` automatically. The runtime mounts it through
an internal read-only path and sets `GIT_CONFIG_GLOBAL` so git can use your host global identity
and defaults without requiring a direct file bind.

## Git / GitHub integration

With the GitHub integration you can see progress at a glance in the overview screen, and navigate to the issue or PR
without copying links.

<img src="docs/images/github-icons.png" alt="GitHub status icons">

When an agent emits a `<multicode:repo>/path/to/repository</multicode:repo>` message, that repository will show up in 
the overview panel. If you select the repo icon with your arrow or tab keys and press enter, the repository will be 
opened in a configurable git viewer (e.g. [sublime merge](https://www.sublimemerge.com/)).

<img src="docs/images/git-review.png" alt="Git review icon">

An issue link can be emitted as `<multicode:issue>https://github.com/example-org/example/issues/1234</multicode:issue>`.
The up-to-date issue status (open/closed) will be shown in the UI. Interacting with the icon opens it in the browser.

<img src="docs/images/github-issue.png" alt="GitHub status icon: Issue">

A pull request link can be emitted as `<multicode:pr>https://github.com/example-org/example/pull/1234</multicode:pr>`.
The current status will show (draft/open/merged/rejected) in the UI. There are also icons for build status 
(running/success/fail) and review status (no reviewers/pending/changes requested/approved).

<img src="docs/images/github-pr.png" alt="GitHub status icon: PR">

It is recommended to use a [skill](https://opencode.ai/docs/skills/) that instructs the agent to emit these tags while
working. 

Alternatively, you can add entries manually in the TUI, but this is tedious.

### Authentication

To authenticate with GitHub, you need to configure a [personal access token (PAT)](https://github.com/settings/tokens) 
(required scopes: `public_repo`, `read:user`). In `config.toml`, there are two approaches to configuring this token:

```toml
[github]
# Token from environment variable
token = {env = "GITHUB_MCP_TOKEN"}
# Token from command (GitHub CLI)
token = {command = "gh auth token"}
```

The env variable approach is recommended. It is prudent to use a token that has more limited access than the GitHub CLI.

To give agents access to GitHub, there are two options to set. `populate-git-credentials` will set environment 
variables that authenticate `git` instances inside the isolate with GitHub. This allows the agents to push changes to
repos they have cloned with HTTPS. The `inherit_env` option allows you to inherit the token from the environment, which
you can then configure the GitHub MCP server to use. This allows the agent to e.g. create pull requests from the 
changes it has pushed.

```toml
[github]
token = ...
populate-git-credentials = true
[isolation]
inherit_env = [..., "GITHUB_MCP_TOKEN"]
```

## Description

You can enter a custom description for each workspace to be shown in the overview panel. This is useful for keeping 
notes.

<img src="docs/images/manual-description.png" alt="Manual description">

## Archiving

When you're done with a workspace, you can *archive* it. The workspace files will be compressed, and the workspace will
be moved to the bottom of the UI. You can unarchive it again when needed.

<img src="docs/images/archived.png" alt="Archived entries">

## multicode-remote

*multicode-remote* is a helper tool to run a multicode instance on a remote machine. Features:

* Dependency installation (bubblewrap, opencode, codex, ...)
* Synchronization of local agent configuration (including authentication details)
* Synchronization of GitHub credentials
* Bi-directional synchronization of the agent workspace
* Opening links in the local browser or git diff viewer

```bash
# cross compilation of micronaut-tui
cargo install cross --git https://github.com/cross-rs/cross
make multicode-remote

target/debug/multicode-remote --ssh opc@192.168.0.1 config.toml
```
