---
name: micronaut-projects-guide
description: Guidance for working with Micronaut projects. Load when working on any project under https://github.com/micronaut-projects/
---

## Building

This is the recommended build command:

```
./gradlew spotlessApply check -q -x japiCmp -x checkVersionCatalogCompatibility
```

Note that for projects using the Micronaut build systems, gradle modules have a prefix: For example the folder 
`context-propagation` corresponds to the gradle module `:micronaut-context-propagation`.

When a Micronaut Gradle build fails on `findBaseline` with an error like `Could not find a previous version for X.Y.Z`, do not disable the task with:

```groovy
tasks.named("findBaseline") {
    enabled = false
}
```

Use Micronaut build's binary compatibility gate instead:

```groovy
micronautBuild {
    binaryCompatibility.enabledAfter("X.Y.Z")
}
```

Set `X.Y.Z` to the next unreleased major, minor, or patch version as appropriate for the branch you are changing.

Rules for creating new tests:

- Prefer junit over spock, unless there is already a spock test that can easily be altered to test this issue
- If the test runs with native image avoid using Mockito since it creates issue with Native Image
- Where available, prefer writing a TCK test over a test for a specific module, even if the TCK fails for another module
- When Docker / Testcontainers is used for testing and the Docker environment is not available write a unit test that doesn't require docker as well as the docker-based test then rely on dowstream CI checks for Docker-based testing results

## Java style

When adding or updating source file copyright headers, keep the ending year as the current calendar year. For example, in 2026 use `Copyright 2017-2026 original authors`; do not leave an older ending year on newly touched headers.

When creating new Java classes, avoid adding `@author` Javadoc unless the surrounding module consistently uses it. If you do add an `@author` tag to match existing style, use the current human user from git or GitHub identity, not the agent name and not a fabricated author.

When adding or updating public APIs that need Javadoc `@since`, derive the value from the pull request target branch, not from the local dependency versions or the agent's guess:

- For a next-minor or milestone target version with a suffix, such as `5.0.0-M22`, strip the suffix and use the base major/minor release with patch zero: `@since 5.0.0`.
- For a maintenance patch branch such as `5.1.x`, find the latest already released `5.1.*` tag or version and increment the patch by one. For example, if the latest released `5.1.*` is `5.1.3`, use `@since 5.1.4`.
- If the target branch itself is `major.minor.x`, treat it as a patch branch and use the next patch after the latest released version in that line.
- If you cannot determine the latest released patch version from tags, release notes, or repository metadata, stop and ask instead of inventing an `@since` value.

## Multi-project development

When a fix needs validation across multiple Gradle projects:

- Use Gradle `includeBuild` as documented in the [Micronaut core build tips](https://github.com/micronaut-projects/micronaut-core/wiki/Gradle-Build-Tips#building-a-module-against-a-local-version-of-micronaut-core).
- For Micronaut projects, [prefer `requiresDevelopmentVersion` when appropriate](https://github.com/micronaut-projects/micronaut-build/wiki).
- Do not rely on `requiresDevelopmentVersion` when testing against a local version of a dependency repository if that setup does not support it.

You can also use these features to verify patches against a user-provided or out-of-tree reproducer.

## Documentation

When writing documentation:

- Prefer the `snippet:` macro instead of inline code blocks so snippets can be generated for all supported languages.
- Unless the project only supports a narrower set, create snippets for Java, Kotlin, and Groovy.
- Resolve documentation snippets from the project's `doc-examples` subdirectory.
- Structure `doc-examples` in the same style used by `micronaut-graphql`'s `docs-examples` reference project on the `5.0.x` branch.
- For configuration examples, prefer the `configuration` macro:

```adoc
[configuration]
----
YAML GOES HERE
----
```

- Do not use `[source,yaml]` for configuration snippets when the `configuration` macro applies, because `configuration` renders the example across configuration formats such as properties, YAML, and TOML.

## PR creation

Unless requested otherwise, target fixes against the default branch, which will be the next minor release.

Do not merge Micronaut pull requests yourself. Leave the PR open for human review and human merge.

The only exception is an explicit dependency-upgrade use case where automated merge is already intended by the workflow or requested by the user.

Tag PRs with the following GitHub tags where appropriate:

- `type: docs`
- `type: bug`
- `type: improvement`
- `type: enhancement`
- `type: breaking`
