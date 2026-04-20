---
name: autonomous-state
description: Maintain the multicode autonomous state file while working autonomously so the host can detect working, question, review, idle, and stalled states.
---

When operating in a multicode autonomous workspace, multicode may either:

- provide an explicit task-specific state file path in the prompt, or
- provide a fallback path via the environment variable `MULTICODE_AUTONOMOUS_STATE_PATH`.

Always prefer the explicit task-specific path from the prompt when one is provided. Only fall back to
`MULTICODE_AUTONOMOUS_STATE_PATH` when no explicit task file path was given.

Write exactly one line to that file.

If multicode tells you the current task session or thread id, write the state as:

- `working:<session-id>`
- `question:<session-id>`
- `review:<session-id>`
- `idle:<session-id>`

If no session/thread id was provided, fall back to the plain state word:

- `working`
- `question`
- `review`
- `idle`

Use shell commands like:

```sh
STATE_FILE="${MULTICODE_AUTONOMOUS_STATE_PATH}"
mkdir -p "$(dirname "$STATE_FILE")"
printf '%s\n' working > "$STATE_FILE"
```

Required workflow:

- As soon as you begin autonomous work, write `working`.
- Before any potentially long-running investigation, edit, build, or test step, write `working` again to refresh the heartbeat.
- If you need human input, approval, clarification, or are blocked waiting for a response, write `question` before stopping.
- When the change is ready for human review or publish approval, write `review` before you stop.
- Only write `idle` if the issue is fully complete and no further action is pending.
- After resuming from an interruption, attach, or restart, immediately write the current state again before continuing.
- When a session/thread id was provided for the task, include it after the colon every time you write the state so multicode can distinguish parallel sessions on the same VM.

Do not write anything except the single state word, or the state followed by `:<session-id>`, to this file.
