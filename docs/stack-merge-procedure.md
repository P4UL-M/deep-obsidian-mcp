# Merging The Multi-Backend Stack

How to land a long linear stack of pull requests on `main`, in order, without losing the
per-PR review record and without a single conflict. Written for the 27-PR multi-backend
stack (issue #41) that precedes `v0.2.0-alpha.1`, but the procedure is general.

Once `main` carries the stack, the tag itself is
[release-checklist.md](./release-checklist.md).

## Step 0 — Before Merge Day

- [ ] **Open the top PR.** `chore/release-readiness` (row 27) has no pull request yet.
      Open it against `feat/docker` so the stack is complete and CI has run on it:
      `gh pr create --base feat/docker --head chore/release-readiness`.
- [ ] **Know that this repo's formula is mid-flight.** `Formula/deep-obsidian-mcp.rb` on the
      stack tip carries **placeholder `sha256` values** (all zeros) for both the source
      tarball and the `livesync-sidecar` resource, because neither artifact exists until the
      tag is pushed. So between the merge and step 3 of
      [release-checklist.md](./release-checklist.md), installing from *this* repo's formula
      copy (`brew install ./Formula/deep-obsidian-mcp.rb`) fails at fetch. That is
      deliberate and harmless — `brew install` uses the `P4UL-M/homebrew-tap` copy, which is
      untouched and still installs v0.1.0-alpha.12 until the tap is bumped at tag time.

## What Makes This Stack Easy

Every PR is based on the one below it, each adds its own commit(s) and nothing else, and
no two PRs edit the same lines from different starting points. So merging bottom-up is
mechanical: there is **nothing to resolve**. A conflict during this procedure means
something unexpected happened — stop and look, do not merge through it.

The two facts that shape the mechanics:

- **`delete_branch_on_merge` is `false`** on this repo, so head branches are not removed
  automatically. Pass `--delete-branch` when merging, or retarget the next PR by hand.
- **`main` has no branch protection**, so nothing blocks a merge on a red or missing
  check. CI is advisory here; waiting for it is the operator's job, not the platform's.

## The Order (bottom → top)

| # | PR | Branch |
|---|---|---|
| 1 | #42 | `test/mcp-contract-ci-baseline` |
| 2 | #43 | `refactor/consolidate-vault-helpers` |
| 3 | #44 | `feat/vault-backend-contract` |
| 4 | #45 | `feat/multi-vault-router` |
| 5 | #46 | `feat/per-mount-runtime` |
| 6 | #47 | `feat/livesync-sidecar` |
| 7 | #48 | `refactor/index-note-source` |
| 8 | #49 | `feat/couchdb-read-backend` |
| 9 | #50 | `feat/livesync-sidecar-writes` |
| 10 | #52 | `feat/couchdb-writes` |
| 11 | #53 | `feat/algolia-crate` |
| 12 | #54 | `feat/algolia-backend` |
| 13 | #55 | `feat/algolia-recall-tools` |
| 14 | #56 | `feat/algolia-cli-live` |
| 15 | #57 | `feat/federated-recall` |
| 16 | #58 | `feat/hardening-packaging` |
| 17 | #59 | `feat/resilience-stabilization` |
| 18 | #60 | `demo/multi-backend` |
| 19 | #61 | `feat/couchdb-virtual-grep` |
| 20 | #62 | `perf/backend-caches` |
| 21 | #65 | `feat/remote-root-mounts` |
| 22 | #66 | `feat/couchdb-delete-note` |
| 23 | #67 | `feat/mounts-cli` |
| 24 | #68 | `feat/setup-wizard` |
| 25 | #69 | `feat/secrets-set` |
| 26 | #70 | `feat/docker` |
| 27 | (this one) | `chore/release-readiness` |

Regenerate the chain rather than trusting this table if the stack has moved:

```bash
gh pr list --state open --limit 40 \
  --json number,headRefName,baseRefName \
  --template '{{range .}}{{.number}} {{.headRefName}} -> {{.baseRefName}}{{"\n"}}{{end}}'
```

### The two PRs that are not part of the stack

- **#40** (`feat/algolia-shared-wiki`) — its content was **lifted into #53**
  (`feat(algolia): lift the deep-obsidian-algolia crate from #40`). Merging it as well
  would re-apply a crate that is already on `main`. Close it with a comment pointing at
  #53; do not merge it.
- **#39** (`ci-publish-apt-dispatch`) — independent, targets `main`, and is the **one
  expected conflict**: it adds a `timeout: 1800000` to the "Deploy to Pages" step of
  `.github/workflows/release-deb.yml`, whose neighbouring "Attach .debs to the release"
  step the stack tip renames and extends (it now also attaches the sidecar bundle).
  Merge #39 **after** the stack, so the resolution happens once, in #39, instead of
  inside a rebase of the stack. The resolution is "keep both": the longer Pages timeout
  **and** the renamed attach step with its multi-line `files:` list.

## The Loop

For each PR `N` in the order above, with `H` = its head branch and `M` = the next PR's
head branch:

```bash
# 1. Check the state you are about to merge.
gh pr view <N> --json baseRefName,mergeable,statusCheckRollup \
  --jq '{base: .baseRefName, mergeable, checks: [.statusCheckRollup[]? | {name, conclusion}]}'
#    base must be `main` (the first PR already is; every later one was retargeted in
#    step 4 of the previous iteration). Never merge a PR whose base is still a stack
#    branch — that merges into the branch, not into main.

# 2. Merge. Rebase-merge keeps one commit per logical change and no merge bubbles.
gh pr merge <N> --rebase --delete-branch

# 3. Rebase the next PR onto the new main. Rebase-merge REWROTE the SHAs, so <M>'s
#    history still contains the pre-merge copy of <N>'s commit; without this step
#    GitHub computes <M>'s diff from a merge-base that has drifted back down the
#    stack and shows every already-merged change again.
git fetch origin --prune
git switch <M>
git rebase origin/main          # already-applied patches are detected and dropped
git push --force-with-lease

# 4. Make sure the base moved. GitHub retargets a child PR when its base branch is
#    merged/deleted, but verify rather than assume — this repo does not delete
#    branches on merge by default.
gh pr view <M> --json baseRefName --jq .baseRefName    # expect: main
gh pr edit <M> --base main                             # only if it is not

# 5. Let CI run on <M> before merging it (nothing enforces this — see above).
gh pr checks <M> --watch
```

Then repeat with `N = M`.

### If `git rebase` reports a conflict

Stop. In a linear stack there should be none. The realistic causes, in order of
likelihood:

1. **A PR was merged out of order** — check `git log --oneline origin/main` against the
   table.
2. **Someone pushed to `main` directly** in the middle of the procedure (or #39 / #40
   was merged early). `git log origin/main --not <M>` shows what arrived unexpectedly.
3. **The branch was amended after the stack was built** — the "already applied" patch
   detection only drops commits that are textually identical.

`git rebase --abort` returns to a known state and costs nothing. Resolving a surprise
conflict by hand mid-stack is how a 27-PR stack silently loses a hunk.

### Do not

- **Squash-merge.** Each PR is already one logical commit; squashing 27 of them
  discards the per-commit messages that carry the design rationale.
- **Merge-commit.** It preserves the pre-rebase SHAs, which makes the child PR's diff
  correct without a rebase, but leaves 27 merge bubbles on `main` for a stack that is
  linear by construction.
- **Merge top-down or skip a PR.** The stack compiles only in order; #58's packaging
  work assumes #47's sidecar exists, and so on.

## The One-Shot Alternative

Because the stack is strictly linear, `main` can also be fast-forwarded to the tip in a
single push:

```bash
git fetch origin
git merge-base --is-ancestor origin/main origin/chore/release-readiness && echo "fast-forward is possible"
git push origin origin/chore/release-readiness:main
```

Every PR whose commits become ancestors of `main` is then closed as merged by GitHub
automatically, with no rebasing and no retargeting at all.

The trade-off is real and it is why the loop above is the default: nothing runs CI on the
intermediate states, so `main` gets 27 commits of which exactly one arrangement was ever
tested (the tip). Use it only if the loop has already been started and needs to be
finished under time pressure, and say so in the release notes.

## Final State

- `main`'s tip is content-identical to `chore/release-readiness`. Verify:
  `git diff origin/main origin/chore/release-readiness` prints nothing.
- All 27 head branches are deleted (`git branch -r | grep -v main`).
- #40 is closed unmerged; #39 is merged (after the stack, with its one conflict
  resolved) or explicitly deferred.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets` on `main` match
  what the tip PR reported.
- Then, and only then: [release-checklist.md](./release-checklist.md) → "Cutting a
  Release".
