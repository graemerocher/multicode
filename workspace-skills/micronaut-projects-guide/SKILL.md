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
- Where available, prefer writing a TCK test over a test for a specific module, even if the TCK fails for another module

## Multi-project development

When a fix needs validation across multiple Gradle projects:

- Use Gradle `includeBuild` as documented in the [Micronaut core build tips](https://github.com/micronaut-projects/micronaut-core/wiki/Gradle-Build-Tips#building-a-module-against-a-local-version-of-micronaut-core).
- For Micronaut projects, [prefer `requiresDevelopmentVersion` when appropriate](https://github.com/micronaut-projects/micronaut-build/wiki).
- Do not rely on `requiresDevelopmentVersion` when testing against a local version of a dependency repository if that setup does not support it.

You can also use these features to verify patches against a user-provided or out-of-tree reproducer.

## PR creation

Unless requested otherwise, target fixes against the default branch, which will be the next minor release.

Tag PRs with the following GitHub tags where appropriate:

- `type: bug`
- `type: improvement`
- `type: breaking`


