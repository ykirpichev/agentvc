# Keeping PRs mergeable: `agentvc restack`

Parallel threads open PRs from the same main. Every merge on github.com makes the
others stale, and many then show "This branch has conflicts", although most of those
conflicts are both sides adding a line in the same place. `agentvc restack` re-merges
each open PR branch with the new main, using the same deterministic resolution as
`agentvc rebase` (#91: both-sides additions, same change twice, whitespace-only and
word-level edits, `Cargo.lock`, JSON, per-path rules), and:

- **clean:** pushes the result. A branch with one commit on top of its merge base
  (what `agentvc pr` exports) is rebased, keeping its message and author, and pushed
  with `--force-with-lease` on the old tip, so a concurrent push is never lost. A
  longer branch gets a merge commit of main instead, so its history stays and no force
  push is needed. `--mode rebase|merge` forces one.
- **still conflicting:** leaves the branch alone and, with `--comment`, keeps one
  comment on the PR (edited in place on later runs) naming the files and how many
  hunks are left. Once a later run restacks it cleanly, the comment says so.

```sh
agentvc restack --all-prs                    # dry run: what would happen to each open PR
agentvc restack --all-prs --push --comment   # what the GitHub Action runs
agentvc restack my-branch --push             # one branch
agentvc restack --all-prs --strategy 'CHANGELOG.md=union'
```

It needs no agentvc space, only a checkout with the remote and a GitHub token
(`GITHUB_TOKEN`/`GH_TOKEN`) for `--all-prs` and `--comment`. In a space it also uses
`agentvc config merge.strategy` and `merge.formatter`. It looks at open PRs into the
default branch whose head is in the same repository, and skips PRs labelled
`agentvc:no-restack`.

## The GitHub Action

`.github/workflows/restack.yml` runs `agentvc restack --all-prs --push --comment` on
every push to main (and by hand from the Actions tab). So the usual loop becomes: merge
a PR on github.com, wait a minute or two, and the other PRs are mergeable again, or
say exactly which files need a person.

One setting matters: pushes made with the workflow's default `GITHUB_TOKEN` do not
start CI on the restacked PRs (GitHub's rule against workflow loops), so checks stay
from the old commit and branch protection may wait for them. Add a fine-grained
personal access token with **Contents** and **Pull requests** read/write on this
repository as the secret `AGENTVC_RESTACK_TOKEN`, and the Action uses it instead.

## With agentvc attempts

A restacked agentvc PR is still the attempt's export on a newer base. When the thread
updates the PR later, `agentvc rebase aN` (step 5 of the dogfooding checklist) moves the
attempt onto the same main and `agentvc pr aN` pushes the same result again, so the two
never fight. Conflicts the Action reports are finished the same way:
`agentvc rebase aN --keep-conflicts`, fix the diff3 markers in the view,
`agentvc snapshot aN`, `agentvc pr aN`.
