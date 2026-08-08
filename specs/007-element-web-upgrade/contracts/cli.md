# Contract: the command line this feature adds

**Produced by**: `justfile` (T012, T018, T020)

Three recipes. The rule for all of them: a report a person reads, not a build log they have
to search. Each exits non-zero when the thing it checked is not true.

## `just element-web-sync <version>`

Move to an upstream release and find out whether we still work on it.

**Does**: fetch the named release → re-apply config and shims → run the shim contract checks
→ print a verdict.

**Reports**:

```
element-web sync v1.12.11 -> v1.12.25
  fetch           ok
  patch           ok    (4 injections asserted)
  shim contract   FAIL  e2ee-bridge: installed=false (no Worker.postMessage seen)
  release notes   https://github.com/element-hq/element-web/releases  v1.12.12 .. v1.12.25

FAILED: 1 of 8 shims did not install. The pin is unchanged.
```

**Rules**

- On failure the pin is **not** left moved. A half-applied upgrade is worse than none,
  because the next person cannot tell which version they are debugging.
- The release-notes range is printed whether or not it passed. It is the context for reading
  everything above it.

## `just element-web-rebase <version>`

Move the patch branch onto a new upstream tag.

**Reports**, one line per carried commit, with three outcomes — see research R3 for why
three and not two:

```
rebasing 3 patches onto v1.12.25
  applied    a1b2c3d  Show the call encryption state in the room header
  dropped    e4f5g6h  Fix the timestamp on a redacted event
             ^ upstream has this. If it has an open PR, it merged
  conflicted i7j8k9l  Prefer the native camera list
             ^ upstream may have taken this with changes made in review.
               Compare before resolving; `git rebase --skip` if it is theirs now.
```

**Rules**

- `dropped` is stated, never silent. It is how a contribution landing is discovered.
- A conflict on a commit carrying an `Elementium-Upstream:` trailer gets the second line
  above, because "amended in review" is the most likely cause and the least obvious.
- Refuses to run against a dirty tree. An Element Web build writes to `pnpm-lock.yaml`
  (research R1), so a dirty tree after a build is normal and must not be rebased over.

## `just element-web-pr <commit>`

Turn a carried commit into something that can be opened as a pull request.

**Does**: branch from the current upstream tag, cherry-pick that one commit, print the push
command. Does not push, and does not open anything.

**Rules**

- Never pushes on its own. The remote is someone's account; that is T023's subject and not a
  side effect of a local command.
- Refuses if the commit's `Elementium-Intent:` trailer is `permanent-fork` — that commit was
  classified as one we do not intend to offer, and if that has changed, the classification
  should change first.
