# Engineering standards: Zeabur Builder's part

Apply `zeabur-oss-standards`' STANDARDS.md first. Zeabur Builder adds these items to its
sections.

## 1. Comments, doc comments and commit messages

```rust
// Before
// Basically we retry here cuz the apiserver sometimes 409s us.
/// This is used for applying deployments after the user has approved things.
// Can't use ? here since the error type doesn't impl From.

// After
// Retry once on `409 Conflict`. Another writer changed the object after the read.
/// Applies the approved plan to the `Deployment`.
///
/// Before you call this function, get approval for the plan.
// Do not use `?` here. `KubeError` does not implement `From<ApplyError>`.
```

```text
Before: fixed webhooks getting lost when the pod restarts
After:  fix(controller): keep webhook wakes in Build status

        The receiver kept the last wake in memory. A restart or another replica lost it.
```

## 2. Tests

```rust
// Before: a change-detector
client.expect_create().with(eq(build_deployment(&spec))).times(1);
// After: assert what the API server holds
let deployment = api.get("web").await?;
assert_eq!(deployment.spec.and_then(|s| s.replicas), Some(2));
```

## 3. Design

Leave alone also: Rust newtypes and error enums.

```rust
// Before: one implementation, no boundary, every method forwards
trait ProjectStore { async fn get(&self, name: &str) -> Result<Project>; }
struct KubeProjectStore(Api<Project>);
impl ProjectStore for KubeProjectStore {
    async fn get(&self, name: &str) -> Result<Project> { Ok(self.0.get(name).await?) }
}
// After
let project = projects.get(name).await?;
```

## 4. Scale

- Watch with kube-rs `watcher`, `reflector` or `Controller`.

```rust
// Before: one GET per row
for name in &names { services.push(api.get(name).await?); }
// After: one LIST of the namespace, selected by label
let services = api.list(&ListParams::default().labels("app.kubernetes.io/part-of=shop")).await?;
```

```yaml
# Before: a CronJob per database, every finished Job kept
# After:  one CronJob per cluster that backs up every database it selects
successfulJobsHistoryLimit: 3
failedJobsHistoryLimit: 1
jobTemplate:
  spec:
    ttlSecondsAfterFinished: 86400
```
