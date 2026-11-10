---
default: minor
---

# BuildRuns record their steps, the failed step, kstatus conditions and the rebuild request they answer

A finished BuildRun has `status.steps` (the last 20 BuildKit steps, each with `name`, `state`, `cached`, `startTime`, `completionTime` and an error `message`, with names and messages cut to 120 characters), `omittedSteps` (how many earlier steps were left out) and `failedStep` (the step that failed, also when `steps` leaves it out; `fetch` or `plan` when the run stopped there). The Job's termination message stays within 4096 bytes: it drops the oldest steps first, then base images, exposed ports, extra tags, the message and the phase times, and always keeps the result, reason, digest and failed step.

Every BuildRun has `Reconciling` (True while Pending, Planning or Building), `Stalled` (True on Failed or Cancelled, with the run's reason) and `Ready` (True on Succeeded, otherwise False) conditions, so `kubectl wait --for=condition=Ready buildrun/<name>` works. `status.observedRebuild` shows the run's own `builder.zeabur.com/rebuild` value.
