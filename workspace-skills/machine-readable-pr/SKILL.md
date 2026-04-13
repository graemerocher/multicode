---
name: machine-readable-pr
description: Rules for machine-readable pull request metadata. Load when actively working on or creating any GitHub pull request.
---

When actively working on or creating a GitHub pull request, assign the PR to yourself as soon as the PR exists.

If you are using `gh`, prefer:

```bash
gh pr edit --add-assignee @me
```

After assigning the PR to yourself, request a review from Copilot as soon as the PR exists.

After creating the PR, assigning yourself, and requesting the Copilot review, immediately emit the link to the PR like this:

```
<multicode:pr>https://github.com/example/example-core/pull/12345</multicode:pr>
```

If the PR resolves a specific issue, when writing the PR description, end it with `Resolves #1234`, where 1234 is the issue number. 

If you have write permission to the upstream repo, prefer pushing the PR branch there instead of in a fork.
