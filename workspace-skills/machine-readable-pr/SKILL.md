---
name: machine-readable-pr
description: Rules for machine-readable pull request metadata. Load when actively working on or creating any GitHub pull request.
---

When actively working on or creating a GitHub pull request, assign the PR to yourself as soon as the PR exists.

If you are using `gh`, prefer:

```bash
gh pr edit --add-assignee @me
```

For Micronaut projects, also assign the PR to the next Micronaut project release at the organization level under `https://github.com/orgs/micronaut-projects/projects`.

- Prefer the next semantically versioned release project.
- The next available project is typically suffixed with a milestone number, for example `5.0.0-M2`.
- Otherwise, use the semantically versioned project suffixed with `Release`, for example `5.0.0 Release`.

After assigning the PR to yourself, request a review from Copilot as soon as the PR exists.

After creating the PR, assigning yourself, and requesting the Copilot review, immediately emit the link to the PR like this:

```
<multicode:pr>https://github.com/example/example-core/pull/12345</multicode:pr>
```

If the PR resolves a specific issue, when writing the PR description, end it with `Resolves #1234`, where 1234 is the issue number. 

If you have write permission to the upstream repo, prefer pushing the PR branch there instead of in a fork.

When creating or updating the PR, add an appropriate type label. Choose the closest match for the actual change, for example:

- `type: docs` for documentation-only updates
- `type: bug` for bug fixes
- `type: improvement` for minor improvements
- `type: enhancement` for broader enhancements

Do not skip the label when the repository has a matching label available.

If you mention agent authorship anywhere in the PR title, body, comments, or related text, do not use `multicode <multicode@yawk.at>`. Use the actual agent name, actual model name, and an agent-appropriate email address instead:

```
Co-Authored-By: <agent> with <model> <<agent-email>>
```

Examples:

```text
Co-Authored-By: Codex with GPT-5 <codex@openai.com>
Co-Authored-By: OpenCode with Claude 4 <opencode@example.com>
```

Do not hard-code `Codex with GPT-5 <codex@openai.com>` unless that is actually the agent, model, and email identity used.

Do not merge pull requests yourself. A PR should be merged by a human, not by the agent.

The only exception is an explicit dependency-upgrade workflow where the user or repository policy already allows automated merging for that upgrade task. Outside that narrow case, stop at review-ready and leave the PR open.
