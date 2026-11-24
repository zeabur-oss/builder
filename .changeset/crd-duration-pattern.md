---
default: major
---

# The CRDs refuse durations the controller cannot read

`triggers.pollInterval` and `planner.timeout` take hours, minutes, seconds and milliseconds, largest first and each once, such as `90s`, `1h30m` or `1.5h`. A fraction goes on the last unit only. The CRDs now refuse `1.5m30s`, `30s1m` and `1s2s` when you apply them. Before, the controller took such a `pollInterval` as unset and polled at the default interval. Write the value in the forms above.
