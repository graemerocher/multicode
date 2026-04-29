# Multicode Codex Subagents

These project-scoped agents are loaded by Codex from `.codex/agents/`.

They intentionally omit `model` and `model_reasoning_effort` so every agent uses the same model and reasoning defaults as the parent Codex session. Most agents are read-only because Multicode benefits most from parallel review, research, and state-machine analysis; implementation can use Codex's built-in `worker` agent when a parent task explicitly needs edits.

Suggested prompts:

```text
Review this branch. Spawn multicode_planner_architect, multicode_code_mapper,
multicode_reviewer, multicode_security_cve, multicode_test_risk, and
multicode_qa_scenarios in parallel, then summarize only confirmed findings,
the implementation risks, and the tests/live checks we should run.
```

```text
Investigate why this PR row is in Review Wait. Use multicode_github_researcher
for GitHub/CI state and multicode_code_mapper for the UI/runtime state path.
```

```text
Before changing this Codex integration, ask multicode_docs_researcher to verify
the current OpenAI Codex behavior and ask multicode_reviewer to check the
state-machine impact.
```

```text
Before implementing this cross-cutting TUI/runtime feature, ask
multicode_planner_architect for the smallest safe implementation shape and ask
multicode_qa_scenarios for the user-visible workflows to validate.
```
