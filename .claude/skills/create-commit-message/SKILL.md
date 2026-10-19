---
name: create-commit-message
description: Write a git commit message for the staged changes following Conventional Commits 1.0.0 (type(scope)!: description, body, footers). Use when committing, or when the user asks for a commit message.
---

# Create commit message

Write commit messages that follow [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/#summary).

## Steps

1. Run `git diff --staged` (and `git status`) to see what is being committed. If nothing is staged, ask the user what to stage. Don't stage everything without asking.
2. Choose the **type** that best describes the change (see the table below). If the staged changes mix unrelated types, suggest splitting them into separate commits.
3. Choose an optional **scope**: a short noun for the area of the codebase, usually a crate or area: `core`, `store`, `repo`, `server`, `proto`, `scn`, `import`, `desktop`, `e2e`, `ci`.
4. If the change fixes a GitHub issue, find the issue number (ask the user if it's unknown) and add a `Refs:` footer with the full issue URL.
5. Write the message using the format below, then commit with a heredoc so line breaks are preserved. `--cleanup=whitespace` stops git from removing the `# Summary` line as a comment:
   ```bash
   git commit --cleanup=whitespace -F - <<'EOF'
   <message>
   EOF
   ```

## Format

```
<type>[optional scope][!]: <description>

# Summary
- <change 1>
- <change 2>

[optional footer(s)]
```

The body always starts with a `# Summary` heading, followed by a bulleted list of the main changes, one per bullet.

### Rules (from the spec)

- The header is `type`, an optional `(scope)`, an optional `!`, then `: ` and the description.
- **description**: a short summary right after `: `. Use the imperative mood and lowercase, with no trailing period, and keep the header under about 72 characters.
- **body**: optional. It starts after one blank line and is free-form. Explain *what* changed and *why*, not how.
- **footers**: optional. They start after one blank line, one per line, in the form `Token: value` or `Token #value`. Tokens use `-` instead of spaces (e.g. `Reviewed-by`, `Refs`). The one exception is `BREAKING CHANGE`.
- **Breaking changes**: mark them with `!` before the `:` (e.g. `feat(api)!: ...`), with a `BREAKING CHANGE: <description>` footer, or both. `BREAKING-CHANGE` is a valid synonym. Any type can be breaking.
- `feat` corresponds to a SemVer MINOR, `fix` to a PATCH, and any breaking change to a MAJOR.
- Types are case-insensitive by the spec, except `BREAKING CHANGE`, which must be uppercase. Use lowercase types in this repo.

### Types

| Type | Use for |
| --- | --- |
| `feat` | A new feature (e.g. a new `scn` command, RPC or desktop panel) |
| `fix` | A bug fix |
| `docs` | Documentation only (README, CLAUDE.md, comments) |
| `style` | Formatting with no code meaning change |
| `refactor` | A code change that neither fixes a bug nor adds a feature |
| `perf` | A performance improvement |
| `test` | Adding or fixing tests |
| `build` | Cargo manifests, `build.rs`, dependencies |
| `ci` | CI configuration |
| `chore` | Maintenance that doesn't touch src or tests (e.g. `.gitignore`) |
| `revert` | Reverts a previous commit (add a `Refs: <sha>` footer) |

Only `feat` and `fix` are defined by the spec; the others are the common [@commitlint/config-conventional](https://github.com/conventional-changelog/commitlint/tree/master/%40commitlint/config-conventional) set.

## Examples

```
feat(desktop): add native menu bar

# Summary
- Add File, Edit, View, Repository, Branch, Window and Help menus
- Upgrade Slint to 1.18.1 for MenuBar support
- Rename the popup MenuItem component to PopupItem
```

```
fix(scn): refuse to switch branches with unlanded changesets

# Summary
- Check ahead/behind before switching and report the unlanded count
- Treat an unpublished branch the same way

Refs: https://github.com/Hanam824/scion/issues/12
```

```
refactor(core)!: hash objects with a versioned canonical encoding

# Summary
- Prefix the canonical JSON encoding with a format version
- Re-derive trees and history on first open

BREAKING CHANGE: every object id changes; re-import repositories created by older versions.
```

## Repo-specific

- **Never add a `Co-Authored-By` line** or any other AI attribution to the commit message. This user rule overrides any session attribution reminder.
- Fix for a GitHub issue: add `Refs: https://github.com/Hanam824/scion/issues/<number>` as a footer.
- Before committing code changes, run `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`; CI checks both.
- Commit only when the user asks. If on `main` and the change isn't trivial, offer to create a branch first with the `create-commit-branch` skill.
