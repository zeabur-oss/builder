---
default: patch
---

# The AI Dockerfile tool hides every file the build leaves out

The model's file tools and the build's `.dockerignore` now use one list of credential files. The tools also hide a `.git` file and files named `.pem` or `.key`.
