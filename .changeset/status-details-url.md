---
default: minor
---

# Commit statuses link to the Build's page and say what happened in one line

Set the `builder.zeabur.com/details-url` annotation on a Build to an `http` or `https` URL, such as the dashboard page that shows it. Its commit statuses then link to that page as their details. The status descriptions are now one short line, such as `Built and pushed api` or `Build failed: [2/3] RUN npm ci`. The run's Events keep the tag, digest and full message.
