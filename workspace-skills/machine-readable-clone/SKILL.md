---
name: machine-readable-clone
description: Rules for machine-readable git clone metadata. Load before cloning any git repository.
---

When cloning, never do so in the agent work directory, only in a subdirectory. Use HTTPS-based repository URLs, not SSH.

After cloning any git repository, immediately emit the absolute path to the clone like this:

```
<multicode:repo>/home/example/work/repo_path</multicode:repo>
```
