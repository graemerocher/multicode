---
name: independent-fix
description: Workflow for independently fixing an issue report. Load only on demand.
---

It is critical that you use the `machine-readable-clone`, `machine-readable-issue` and `machine-readable-pr` skills 
where appropriate.

You are given a user-provided issue report. If the issue report is a GitHub issue, also consider comments that people
have already made to the issue. Your task is to resolve this issue, possibly but not necessarily with a code patch.

If there are user-provided reproduction steps, or even a full reproducer project, use that project to attempt to 
reproduce the issue. When you create a patch, you must also try to validate that patch against the reproducer project.

As you investigate the issue, you have to decide how to proceed. There are several options:

- An issue may reflect intended behavior. In that case, a documentation update *may* be appropriate.
- An issue may be a bug that was already fixed in later software versions. In that case, most often a reproduction test
should be added.
- An issue may be a real bug or new feature, in which case you should create a reproduction test and patch.

Once you have decided on a path forward, start working on a patch without consulting the prompter.

If there is a real bug, use test-driven development to make sure the test works correctly: Create the test, run a build
to verify the test fails, fix the bug, and then verify the test passes. Then double-check with the user-provided 
reproducer, if applicable.

Some issues may require changes in multiple GitHub repositories, or the fix may be in another repo than the issue 
report. You are permitted to clone and locally modify other repositories, and propose changes there.

**Do not comment on issues or create pull requests without permission.**

**Do not merge pull requests yourself. PRs should be merged by a human, except for an explicit dependency-upgrade workflow where automated merging is already intended.**
