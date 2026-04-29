---
name: sonar-pr-report
description: Workflow for inspecting SonarCloud or SonarQube pull request failures and fixing the reported issues.
---

# sonar-pr-report

Use this skill when a task needs to inspect and act on SonarCloud or SonarQube pull request failures.

## Workflow

1. Start from the existing pull request for the current task.
2. Use `gh pr view --json statusCheckRollup` to find the Sonar status entry and its details URL.
3. Inspect the quality gate result and the concrete actionable findings before editing code.
4. If the report or build logs mention vulnerable dependencies or CVEs, run the Gradle dependency tree or dependency insight to trace the exact path that brings the vulnerable artifact into the build.
5. Determine whether the vulnerable version is owned by the current module, `gradle/libs.versions.toml`, a Micronaut-managed dependency, or another third-party dependency, and update the narrowest correct source of truth.
6. If the vulnerable artifact comes through a Micronaut-managed dependency or Micronaut module, first try upgrading that Micronaut dependency itself to the newest relevant version that can solve the CVE instead of introducing a direct override for the transitive library.
7. Treat adding or changing the vulnerable transitive library directly in `gradle/libs.versions.toml` as a last resort, only when the correct higher-level Micronaut or existing direct dependency upgrade cannot solve the issue cleanly.
8. When evaluating dependency upgrades, handle both normal semantic versions and Micronaut milestone versions such as `5.0.0-M10` and `5.0.0-M11`; the higher milestone number is newer.
9. For any dependency you decide to update, maximize within the chosen release line: prefer the highest safe patch release, and for milestone releases prefer the highest available `-M` suffix in that line.
10. Prioritize real defects and maintainability issues first: bugs, vulnerabilities, security hotspots, broken quality gate conditions, and dependency CVEs.
11. If coverage, duplication, or code smells are part of the failure, fix them without weakening existing tests or changing intended behavior just to satisfy Sonar.
12. After code changes, run focused local verification, push updates if needed, and re-check the Sonar status.

## Rules

- Do not guess what Sonar is complaining about when the report can be fetched directly.
- Do not guess where a vulnerable dependency comes from when Gradle can show the dependency path.
- Prefer upgrading the owning Micronaut dependency or module before adding a direct override for the vulnerable transitive library.
- Prefer changing the narrowest correct version declaration instead of broad, unrelated dependency churn.
- Do not suppress, disable, or game checks just to get a green report.
- Preserve repository conventions and the intended behavior of the code under review.
