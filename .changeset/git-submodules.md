---
default: minor
---

# Builds fetch Git submodules

The `fetch` step fetches submodules at the commits the repository records. A submodule on the repository's `https://` host uses the same token. A submodule that cannot be fetched fails the step, and the message names its path.
