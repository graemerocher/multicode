---
name: machine-readable-pr
description: Rules for machine-readable pull request metadata. Load when actively working on or creating any GitHub pull request.
---

When actively working on or creating a GitHub pull request, immediately emit the link to the PR like this:

```
<multicode:pr>https://github.com/example/example-core/pull/12345</multicode:pr>
```

If the PR resolves a specific issue, when writing the PR description, end it with `Resolves #1234`, where 1234 is the issue number. 

If you have write permission to the upstream repo, prefer pushing the PR branch there instead of in a fork.
