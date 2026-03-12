## Skills
A skill is a set of local instructions stored in a `SKILL.md` file. The smoke repo includes the small set of repo-local skills needed to exercise Symphony's full PR workflow inside a disposable workspace.

### Available skills
- commit: Create a clean commit for the current branch when the task is ready to publish. (file: `.codex/skills/commit/SKILL.md`)
- pull: Merge the latest `origin/main` into the current branch and resolve conflicts. (file: `.codex/skills/pull/SKILL.md`)
- push: Push the current branch and create or update the corresponding pull request. (file: `.codex/skills/push/SKILL.md`)
- land: Merge a ready pull request, keep checks green, and close the guarded Todoist task when appropriate. (file: `.codex/skills/land/SKILL.md`)

### How to use skills
- Use a listed skill whenever the task explicitly names it or the workflow step clearly matches its description.
- Read only the relevant `SKILL.md` for the current step.
- If a skill references helper scripts or sibling files, resolve them relative to that skill directory.
- Keep the workflow narrow: use the minimum skill set needed for the current step.
