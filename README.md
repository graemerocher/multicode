# multicode

… runs isolated [opencode](https://opencode.ai/) instances in parallel.

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
a single issue report. A workspace gets its own working directory and OpenCode session, so you can work from a blank 
slate.

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

* Dependency installation (bubblewrap, opencode, ...)
* Synchronization of local opencode configuration (including authentication details)
* Synchronization of GitHub credentials
* Bi-directional synchronization of the agent workspace
* Opening links in the local browser or git diff viewer

```bash
# cross compilation of micronaut-tui
cargo install cross --git https://github.com/cross-rs/cross
make multicode-remote

target/debug/multicode-remote --ssh opc@192.168.0.1 config.toml
```
