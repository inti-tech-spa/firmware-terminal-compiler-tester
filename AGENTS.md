# samdebug instructions

Use the relevant heading in `docs/architecture.md` and the task-specific document in `docs/` to locate the relevant crate; avoid reading entire documents by default. Search that crate and its tests before widening the search. Keep generated `target/` and `dist/` out of source discovery. Follow the milestone and independent audit gate described in `README.md` when the task changes a milestone implementation.

## Branch and release workflow

1. Start feature and fix branches from the latest `origin/develop`. Keep existing project-specific build, test, and hardware approval requirements.
2. Open an approved pull request into `develop`. Use a merge commit (equivalent to `--no-ff`) to preserve the branch history.
3. Test the exact pushed `develop` commit using this repository's integration procedure. Record the tests and results in the promotion pull request. After testing passes, delete only feature branches whose current tips are contained in that tested commit. Keep `develop` and leave automatic branch deletion disabled.
4. Promote the tested `develop` revision to `master` through an approved pull request using a merge commit. After promotion, merge `master` back into `develop` by pull request to carry the release history forward.
5. Release or deploy from the approved commit at `origin/master`. After verifying the release, push an annotated `production-YYYY-MM-DD-<short SHA>` tag on that exact commit. Use the release date in America/Santiago. Obtain any required deployment, publishing, or hardware approval separately.

### Record integration testing on GitHub

GitHub requires a successful `integration-testing` deployment record before promotion. For a firmware or tool repository, this record represents completed integration or bench validation. Create it only after the required tests pass on the exact pushed `develop` commit. This records the result in GitHub without running a deployment or hardware operation.

```bash
(
set -eu
git fetch origin develop
tested_commit="$(git rev-parse HEAD)"
test "$(git branch --show-current)" = develop
test "$tested_commit" = "$(git rev-parse origin/develop)"
test -z "$(git status --porcelain)"
repository=repos/inti-tech-spa/firmware-terminal-compiler-tester
deployment_id="$(
  gh api --method POST "$repository/deployments" --input - --jq .id <<JSON
{"ref":"$tested_commit","environment":"integration-testing","auto_merge":false,"required_contexts":[],"production_environment":false,"description":"Develop integration tests verified"}
JSON
)"
gh api --method POST "$repository/deployments/$deployment_id/statuses" -f state=success
)
```
