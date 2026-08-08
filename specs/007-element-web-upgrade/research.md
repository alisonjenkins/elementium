# Phase 0 research: Element Web upgrade and patch maintenance

**Created**: 2026-08-08

Four unknowns went into this phase. Three came out answered with evidence; the fourth is
not a research question and is recorded as such.

## R1: How the current upstream actually builds

**Decision**: build with `pnpm`, from `apps/web`, via `nx build`, taking the output from
`apps/web/webapp`, after running `./scripts/layered.sh` at the repository root.

**Evidence**: `.github/workflows/build.yml` at tag `v1.12.25` is the authority, because it
is what upstream themselves run:

```yaml
- uses: pnpm/action-setup@...
- uses: actions/setup-node@...
  with:
    node-version: "lts/*"
- name: Fetch layered build
  run: ./scripts/layered.sh
- name: Copy config
  working-directory: apps/web
  run: cp element.io/develop/config.json config.json
- name: Build
  working-directory: apps/web
  env:
    CI_PACKAGE: true
  run: VERSION=$(scripts/get-version-from-git.sh) pnpm run build
...
  path: apps/web/webapp
```

and `apps/web/project.json` confirms where the output lands:

```json
"build": {
  "command": "webpack-cli --progress --mode production",
  "outputs": ["{projectRoot}/webapp"],
  "options": { "cwd": "apps/web" }
}
```

**Refines the spec.** `spec.md` says the output "is no longer at `webapp/`". More precisely:
the directory is still called `webapp`, it has moved to `apps/web/webapp`. The existing
`cp -r "$cache_dir/webapp"` is wrong about the path, not about the name — a distinction
worth having before someone goes looking for a renamed artefact that was never renamed.

**Two things the recipe adds that were not in the spec's reading:**

- `./scripts/layered.sh` runs first and is not optional. A build that skips it is not the
  build upstream ships.
- `prebuild:module_system` runs `node module_system/scripts/install.ts` and declares
  `{workspaceRoot}/pnpm-lock.yaml` among its **outputs**. A build therefore dirties the
  working tree. On a patch branch that matters: a rebase against a dirty tree fails, and
  the lockfile churn must not be mistaken for one of our patches.

**Alternatives considered**: inferring the build from `package.json` alone. That is what
produced the spec's imprecision — the root has 18 scripts and no `build`, which reads as
"there is no build entry point" when in fact the entry point is a workspace directory away.

### Verified by building, 2026-08-08 — and one claim above is wrong

The recipe was run against tag `v1.12.25`, first by hand and then through the repaired
`fetch_git()`. Both produced a complete `webapp` with `widgets/element-call` present.

| | |
|---|---|
| Build time after install | **39s** (nx, 5 tasks, cold cache) |
| Output | 153 MB at `apps/web/webapp` |
| Source cache on disk | **2.3 GB** — git-ignored, and worth deleting between uses |

**The claim that a build dirties the working tree is not true.** `prebuild:module_system`
declares `{workspaceRoot}/pnpm-lock.yaml` among its *outputs*, and I read that as "the build
writes it". After a full build `git status` is empty. Declaring a file as an nx output says
nx may invalidate on it, not that this build modifies it — with the default
`build_config.yaml` and no extra modules, nothing changes.

The rule it produced — that a rebase should refuse to run over a dirty tree — is still worth
keeping, but as ordinary hygiene rather than as a consequence of this build. Recorded because
the reasoning was wrong even though the conclusion was harmless.

## R2: Node version

**Decision**: build Element Web with Node 24. Do not change the version the rest of the
workspace uses.

**Evidence**: `.node-version` says `24`; upstream CI pins `node-version: "lts/*"`, which is
24 at the time of writing. `engines` says `>=22.18`, and our dev shell provides 22.23.2 —
inside the declared range, below what upstream builds and tests with.

**Rationale**: the engine range says what upstream tolerates; the CI pin says what upstream
*verifies*. For a build whose output we ship, matching what they verify is worth more than
sitting at the bottom of a permissive range. It also removes the version from the list of
suspects the first time a build fails.

**Alternatives considered**: staying on 22.23.2 because it satisfies `engines`. Not
rejected on evidence — no attempt has been made to build on 22 — but the cost of finding
out the hard way is a debugging session, and the cost of avoiding it is one entry in
`flake.nix`. T016 still builds to confirm, since this remains a prediction.

### Superseded 2026-08-08: the prediction was wrong, and nothing needs to change

**Node 22.23.2 builds v1.12.25 cleanly.** Twice — by hand and through `fetch_git()` — with
nx reporting success across all five tasks and a complete 153 MB `webapp`.

So the decision above is withdrawn. No Node 24 entry in `flake.nix`, and one fewer version
to keep in step. The reasoning that produced it — "the CI pin says what upstream verifies,
the engine range only what it tolerates" — is still a reasonable prior, and it was still
worth an hour to find out rather than carrying a toolchain change nobody needed. T016 said
this would be settled by building and not by reading version ranges, which is exactly what
happened, in the direction the reading did not favour.

Worth revisiting only if a future release raises `engines` above 22, which is the thing to
watch rather than `.node-version`.

## R3: Does a rebase really drop a patch once upstream takes it?

This is the load-bearing claim of the whole patch strategy, so it was tested rather than
asserted.

**Decision**: yes, when the change lands byte-for-byte. No, when a reviewer amends it —
that case needs `git rebase --skip`, and the sync tooling must say so.

**Evidence, case 1 — upstream takes the change verbatim, under a different commit message
and author:**

```
$ git rebase v2
hint: Disable this message with "git config set advice.skippedCherryPicks false"
Successfully rebased and updated refs/heads/ours.

$ git log --oneline v2..ours
c14c610 second change we keep
```

The carried commit disappeared and the one still ahead of upstream survived. Commit
message and authorship are irrelevant — `git` compares patch-ids, which are computed from
the diff.

**Evidence, case 2 — upstream takes the change but a reviewer reworded the line:**

```
Could not apply 2e443f0... our change
hint: You can instead skip this commit: run "git rebase --skip"
```

A conflict, not a drop. Which is the correct behaviour — git cannot know whether the
difference is a review tweak or a change we still need — but it is not the frictionless
story the strategy tells on its own.

**Consequence for the design**: T018's per-commit report needs three outcomes, not two:
applied, conflicted, and **dropped**. And a conflict on a commit that has an open pull
request is a specific, common case that deserves naming in the report — "upstream may have
taken this with changes; compare before resolving" — rather than being left to look like
any other conflict.

**Alternatives considered**: tracking upstreamed patches by hand in the manifest and
deleting them on merge. Rejected: it is a second source of truth that has to be kept
correct, and it fails in exactly the case the automatic mechanism handles best.

## R4: Where the fork lives

**Not resolved, and not resolvable here.** It is a repository on the user's account. Left
as `spec.md` open question 1 and tracked as T023, which blocks T017.

This is recorded rather than guessed because guessing would mean creating a repository
under someone's account on their behalf.
