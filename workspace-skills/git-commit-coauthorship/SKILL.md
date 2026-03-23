---
name: git-commit-coauthorship
description: Rules for git commit authorship. Load when creating any form of git commit.
---

When creating any git commit, use the default git author information (from git-config). Additionally, sign your commit with:

```
Co-Authored-By: multicode <multicode@yawk.at>
```

This line should be at the bottom of the commit message, preceded by an empty line.