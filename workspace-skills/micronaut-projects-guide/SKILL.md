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

Rules for creating new tests:

- Prefer junit over spock, unless there is already a spock test that can easily be altered to test this issue
- If the test runs with native image avoid using Mockito since it creates issue with Native Image
- Where available, prefer writing a TCK test over a test for a specific module, even if the TCK fails for another module
- When Docker / Testcontainers is used for testing and the Docker environment is not available write a unit test that doesn't require docker as well as the docker-based test then rely on dowstream CI checks for Docker-based testing results

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
