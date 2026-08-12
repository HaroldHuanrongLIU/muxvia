# Issue tracker: GitHub

Issues and specs for this repository live in GitHub Issues at `HaroldHuanrongLIU/muxvia`. Use the `gh` CLI for operations after the local checkout has that repository as its `origin` remote.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`.
- **Read an issue**: `gh issue view <number> --comments`.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'`, adding suitable label and state filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`.
- **Apply or remove labels**: `gh issue edit <number> --add-label "..."` or `gh issue edit <number> --remove-label "..."`.
- **Close an issue**: `gh issue close <number> --comment "..."`.

Infer the repository from `git remote -v`; `gh` will then resolve it automatically from inside the checkout.

## Pull requests as a triage surface

**PRs as a request surface: no.**

GitHub shares one number space across issues and pull requests. When a bare number is ambiguous, try `gh pr view <number>` and fall back to `gh issue view <number>`.

## Skill operations

- When a skill says **publish to the issue tracker**, create a GitHub issue.
- When a skill says **fetch the relevant ticket**, run `gh issue view <number> --comments`.
- Publish tracer-bullet tickets in dependency order so their blocking references point to existing issues.
- Prefer GitHub's native issue dependencies. If they are unavailable, put `Blocked by: #<number>` near the top of the issue body.
- A ticket is on the implementation frontier when all its blockers are closed.
