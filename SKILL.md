---
name: git-worktrees
description: >
  Start a feature branch in its own worktree, then commit, push, and open a
  pull request. No GitHub issue linking. Create, list, and remove the worktree
  with git cow only, by reading and following the git-cow skill. The default
  directory is ~/.worktrees/<project>/.
  Use when the user wants a feature worktree or runs /git-worktrees.
---

# Feature worktrees

Use this for a feature branch in its own worktree, then a pull request. For a GitHub issue, use the github-issue-worktree skill.

The worktree itself belongs to the git-cow skill. Read that skill and follow it before you create, list, or remove a worktree. Call `git cow` only.

## Requirements

The repository has `origin`. `gh` is authenticated for this repo.

```bash
git remote -v
gh auth status
gh repo view
```

## Names

Use the feature name the user gave. If the request implies one name, use that. If it is ambiguous, ask: `What feature name should I use for the branch?`

- `featureSlug`: lowercase, turn each run of other characters into `-`, trim `-`, cut at 40 characters
- `BRANCH_NAME`: `feat/<featureSlug>`
- `BASE_BRANCH`: `main`, unless the user names another base

```bash
featureSlug=$(printf '%s' "$FEATURE_NAME" \
  | tr '[:upper:]' '[:lower:]' \
  | sed -E 's/[^a-z0-9]+/-/g; s/^-+//; s/-+$//; s/-{2,}/-/g' \
  | cut -c1-40)
BRANCH_NAME="feat/$featureSlug"
BASE_BRANCH="${BASE_BRANCH:-main}"
```

## 1. Create the worktree

Follow the git-cow skill. Create `BRANCH_NAME` starting at `origin/$BASE_BRANCH`. A new worktree goes in `~/.worktrees/<project>/`. If `git cow list` already has this branch under `<project>/.claude/worktrees`, use that path. Work in the path the command prints.

## 2. Develop

Edit and run tests in that worktree.

## 3. Commit and push

Commit with a subject only.

```bash
git add -A
git commit -m "feat: implement $featureSlug"
git push -u origin "$BRANCH_NAME"
```

## 4. Open the pull request

Use the commit subjects between the base and `HEAD` as the summary.

```bash
PR_BODY=$(cat <<PR
## Summary

$(git log --no-merges --pretty='- %s' "origin/$BASE_BRANCH..HEAD")

## Testing

- [ ] Add testing notes
PR
)

gh pr create \
  --base "$BASE_BRANCH" \
  --head "$BRANCH_NAME" \
  --title "feat: $featureSlug" \
  --body "$PR_BODY"
```

## 5. Clean up after the merge

Follow the git-cow skill. From the main checkout, remove the worktree.
