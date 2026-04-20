---
name: git-commit-coauthorship
description: Rules for git commit authorship. Load when creating any form of git commit.
---

When creating any git commit, use the default git author information (from git-config). Additionally, sign your commit with a co-author line that includes the actual agent name, the actual model name, and an agent-appropriate email address for the system that produced the change.

Format it like this:

```
Co-Authored-By: <agent> with <model> <<agent-email>>
```

Examples:

```text
Co-Authored-By: Codex with GPT-5 <codex@openai.com>
Co-Authored-By: OpenCode with Claude 4 <opencode@example.com>
```

Do not hard-code `Codex with GPT-5 <codex@openai.com>` unless that is actually the agent, model, and email identity used.

This line should be at the bottom of the commit message, preceded by an empty line.
