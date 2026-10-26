---
name: create-commit-branch
description: Create a git branch named after a Conventional Commits type (type/scope-short-description) for the upcoming work. Use when starting a change, before committing on main, or when the user asks for a new branch.
---

# Create commit branch

Name branches with the same types used in commit messages ([Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/#summary)), so a branch name predicts its commits.

## Format

```
<type>/<optional-scope>-<short-description>
```

- **type**: one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`. These are the same types as in the `create-commit-message` skill.
- **scope** (optional): the area of the codebase, usually a crate or area: `core`, `store`, `repo`, `server`, `proto`, `scn`, `import`, `desktop`, `e2e`, `ci`.
- **short-description**: 2–5 words in kebab-case.
- Lowercase only, with `-` between words and a single `/` after the type. No spaces, underscores, or trailing slashes.
- Keep it under about 50 characters.
- For a breaking change, add `!` to the commit message, not to the branch name, because `!` is awkward in shell commands.

## Steps

1. Run `git status` and `git branch --show-current`. If the working tree is dirty, tell the user; uncommitted changes will move to the new branch.
2. Pick the type from what the user is about to do. If it's unclear, ask in one short question.
3. Propose the name, then create the branch from the current branch (normally `main`):
   ```bash
   git switch -c <type>/<scope>-<short-description>
   ```
4. If the branch already exists, ask whether to switch to it or choose a new name. Don't delete or reset it.
5. Push only when the user asks: `git push -u origin <branch>`.

## Examples

| Work | Branch |
| --- | --- |
| Add a native menu bar to the app | `feat/desktop-menu-bar` |
| Add a `scn stash` command | `feat/scn-stash` |
| Fix a land retry race | `fix/repo-land-cas-retry` |
| Update the README roadmap | `docs/readme-roadmap` |
| Upgrade Slint | `build/desktop-slint-upgrade` |
| Add the CI workflows | `ci/github-workflows` |
| Clean up `.gitignore` | `chore/gitignore-cleanup` |
