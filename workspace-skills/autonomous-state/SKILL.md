---
name: autonomous-state
description: Maintain the multicode autonomous state file while working autonomously so the host can detect working, question, review, idle, and stalled states.
---

When operating in a multicode autonomous workspace, the environment variable `MULTICODE_AUTONOMOUS_STATE_PATH`
points to a writable state file owned by multicode. You must keep this file updated.

Write exactly one line to that file:

- `working`
- `question`
- `review`
- `idle`

Use shell commands like:

```sh
mkdir -p "$(dirname "$MULTICODE_AUTONOMOUS_STATE_PATH")"
printf '%s\n' working > "$MULTICODE_AUTONOMOUS_STATE_PATH"
```

Required workflow:

- As soon as you begin autonomous work, write `working`.
- Before any potentially long-running investigation, edit, build, or test step, write `working` again to refresh the heartbeat.
- If you need human input, approval, clarification, or are blocked waiting for a response, write `question` before stopping.
- When the change is ready for human review or publish approval, write `review` before you stop.
- Only write `idle` if the issue is fully complete and no further action is pending.
- After resuming from an interruption, attach, or restart, immediately write the current state again before continuing.

Do not write anything except the single state word to this file.
