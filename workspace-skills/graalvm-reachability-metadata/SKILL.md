---
name: graalvm-reachability-metadata
description: Use when a native-image failure is caused by missing reflection, resource, proxy, serialization, JNI, or class-initialization metadata that should be fixed upstream in GraalVM reachability metadata rather than only patched downstream. Covers Micronaut `@TypeHint`, reachability metadata JSON, and filing upstream issues in oracle/graalvm-reachability-metadata similar to issue #2110.
---

# graalvm-reachability-metadata

Use this skill when the current task hits a GraalVM native-image problem such as:

- missing reflection config
- missing resource config
- missing proxy or serialization config
- class initialization problems that require `--initialize-at-run-time` / `--initialize-at-build-time` metadata or upstream library changes
- Micronaut `@TypeHint` or other downstream hints that are compensating for third-party library metadata gaps

Typical signals:

- `ClassNotFoundException`, `NoSuchMethodException`, `NoSuchFieldException` during native execution
- downstream `@TypeHint` blocks listing third-party or shaded library classes
- downstream `META-INF/native-image/**/native-image.properties` with third-party initialization args
- a library works on the JVM but fails only in native mode
- the correct long-term fix is for downstream to consume upstream metadata rather than carry permanent local hints

## Goal

Drive the fix upstream first.

If the failure is really caused by missing third-party metadata, collect enough evidence to open or update an issue in `oracle/graalvm-reachability-metadata`, similar in quality and structure to issue `#2110`. Downstream may still need a temporary workaround, but the skill should treat the upstream report as part of the solution, not an optional extra.

## Workflow

1. Identify the real owning library and version.
   - Do not stop at the Micronaut module that exposed the failure.
   - Map the failing classes back to the third-party artifact that should own the metadata.
   - If classes are shaded, call that out explicitly and identify which published artifact ships the shaded classes.

2. Confirm the failure is native-specific.
   - Record the exact native exception and stack segment.
   - Prefer a minimal reproducer or the smallest existing downstream reproducer that still fails.

3. Check whether existing upstream metadata already exists.
   - Inspect the current `oracle/graalvm-reachability-metadata` tree for that library.
   - Note the exact metadata path if present, for example `metadata/<group>/<artifact>/<version>/reachability-metadata.json`.
   - If metadata exists for an older version, verify whether vendoring it locally changes the failure or leaves it unchanged.

4. Determine whether the failure is truly a metadata gap.
   - If the issue is just a downstream dependency version mismatch or a Micronaut bug, do not file upstream.
   - If the failure is caused by missing metadata for third-party classes or shaded classes, continue.

5. Capture the exact missing metadata surface.
   - For reflection-style issues, list the concrete class names and required access kinds.
   - For cases currently solved by Micronaut `@TypeHint`, include the exact `typeNames` and access requirements.
   - For runtime-init issues, list the exact classes or packages passed to `--initialize-at-run-time` or `--initialize-at-build-time`.
   - For `native-image.properties`, include the exact file path and `Args` values, reduced to the third-party classes that upstream metadata can own.
   - If only some subset is really needed, reduce the list instead of copying broad hints blindly.

6. Draft the upstream issue in the same shape as `oracle/graalvm-reachability-metadata#2110`.
   - Use clear sections:
     - `Summary`
     - `What I verified`
     - `Why the current metadata appears insufficient`
     - `Reproducer`
     - `Requested follow-up`
   - Include downstream issue or PR links when relevant.
   - Include exact library versions and exact native-image version.
   - Include the concrete failure text.

7. If the user wants the issue filed, prefer `gh issue create` with a body file.
   - Do not embed literal `\n` escape sequences.
   - Keep the title library-specific and version-specific.

8. If downstream still needs a temporary workaround, keep it narrow and describe it as temporary.
   - Prefer downstream consumption of upstream metadata over permanent downstream `@TypeHint` growth.
   - If downstream replaces code paths, such as a Micronaut-specific native-image UDP sender, separate that from the upstream metadata request. File upstream only for third-party metadata surfaces, not Micronaut-specific implementation choices.

## Issue-writing rules

- Be precise about the affected artifact, version, and metadata path.
- Be explicit about whether existing upstream metadata was tested and whether it failed to resolve the problem.
- Do not dump a large downstream patch without explaining why the upstream library should own the metadata.
- Do not ask upstream maintainers to reverse-engineer the failure from a vague stack trace.
- Do not claim a class needs metadata unless you actually observed the failure or derived it from a validated downstream workaround.
- If the current downstream fix is a Micronaut `@TypeHint`, explain that this is evidence of missing upstream metadata and include the exact hint block or a reduced equivalent.
- If the current downstream fix is `native-image.properties`, explain which runtime-initialized third-party classes or packages are candidates for upstream reachability metadata.
- If the downstream PR also contains non-metadata code changes, identify them as downstream workarounds and do not ask GraalVM metadata maintainers to adopt them.

## Recommended issue shape

Use this structure:

```text
<group>:<artifact>:<version> native image still fails because required metadata for <library area> is missing

## Summary

Current `<group>:<artifact>:<version>` still fails in a GraalVM native image. Downstream currently needs local metadata or `@TypeHint` entries for:
- `<class 1>`
- `<class 2>`

Downstream also needs native-image initialization metadata for:
- `<--initialize-at-run-time class or package>`

This was discovered while validating `<downstream issue or PR link>`.

## What I verified

### 1. Current native failure

<exact exception and where it occurs>

### 2. Existing upstream metadata does not resolve it

<what metadata exists today, what path it lives under, and what happened when it was vendored or tested>

### 3. Downstream workaround that proves the gap

<exact @TypeHint block, reduced reflection config, native-image.properties Args, or equivalent evidence>

### 4. Downstream-only workaround, if any

<describe Micronaut-specific replacement code separately from metadata candidates>

## Why the current metadata appears insufficient

<explain why the missing classes or access kinds belong to the upstream library metadata>

## Reproducer

Environment used:
- OS / arch
- GraalVM version
- native-image version

<minimal app or reduced reproducer steps>

## Requested follow-up

Please add or update reachability metadata for `<group>:<artifact>` so the current version line works in native mode without downstream-only hints.
```

## Practical notes

- For Micronaut tasks, link the downstream Micronaut issue or PR that exposed the failure.
- For shaded libraries, call out both the shaded class names and the artifact that publishes them.
- For `io.micrometer.shaded.io.netty...` style classes, identify the owning artifact as the Micrometer registry artifact that publishes the shaded Netty classes, not unshaded `io.netty`.
- If the upstream issue already exists, update it or comment there instead of opening a duplicate.
- Before filing a new external issue, make sure the user has actually asked for or approved that external filing.
