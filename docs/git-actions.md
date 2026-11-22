# Committing, pushing and opening a pull request

The thread menu has **Commit, push and PR…**. It opens a small panel for the
thread's workspace; every branch, count and path in it is `git`'s answer from
the host that runs the thread. There is no cached guess and no sample row.

## What the panel shows

- the checked-out branch, and its upstream when it has one;
- how many commits the branch is **ahead** of and **behind** its upstream;
- the exact counts of **staged**, **unstaged** and **untracked** changes, with
  up to 100 paths in each list (the count stays exact when the list is capped);
- a rebase, merge or cherry-pick that is in progress, which blocks a commit.

## Commit

The message field is yours. **Generate** can fill it from a model, but it never
commits; only **Commit** does.

**Include unstaged changes** decides what is added to the index before the
commit:

- off — the commit records what is already staged;
- on — tracked changes and new files are staged first.

A commit is refused, with the reason, when the workspace has a detached `HEAD`,
when a rebase, merge or cherry-pick is in progress, or when the index is empty.
Git hooks run as usual: h4rn never passes `--no-verify`. A hook that fails stops
the commit, and its last lines are shown in the panel.

Messages and paths are never handed to git as options. The message is the value
of `-m`; path selection is validated as workspace-relative (no absolute paths,
no `..`, no names shaped like `--flag` or `:(magic)`), passed after `--`, and
parsed with `--literal-pathspecs`, so a file named `--amend` is only a file.

## Push

**Push** pushes the current branch to its upstream. A branch without one gets
`origin/<branch>` created, because the panel's button always asks for it. A push
is **never forced**: when the remote has commits the branch does not, the panel
says so and asks you to pull or rebase. A rejected credential is reported as a
credential problem, not a generic failure.

## Pull request

**Create pull request…** detects the forge remote from the workspace, then
offers a title, a description, a base branch (the repository's default branch is
preselected) and **Open as a draft**. **Generate** fills the title and body and
does nothing else.

The daemon pushes the branch first when it has no upstream or is ahead, then asks
the forge to open the request. It does not create one silently from Generate.

## Generated messages

Settings → Source control → **Commit & PR messages** picks **Off**, **Coding
agent** or **Text generator**, and accepts optional custom instructions. It is a
separate setting from the title model, so titles and messages can use different
models.

**Off** starts no job: Generate leaves the field empty and no model runs. A
coding agent runs once, non-interactively, with the same Claude Code flags as
title generation (no tools, no MCP servers, no user configuration, an empty
working directory). A text generator is one already configured under Settings →
Providers → Text generators.

What leaves the machine is only what the request needs:

- **Commit message**: the diff that would be committed, capped at 12,000
  characters, plus up to 20 recent commit subjects. A binary file appears as
  git's `Binary files … differ` line, never its bytes. With *Include unstaged*
  on, the diff is taken against `HEAD`, and untracked files are listed by name
  only — their contents are never read or sent.
- **Pull request**: the commits on the branch against the base (subjects and
  bodies, capped at 8,000 characters) and `git diff --stat` for the same range
  (capped at 4,000 characters).

Both are wrapped once in `<untrusted_request>` tags, exactly like a title
prompt, with any spelling of the closing tag escaped. Custom instructions are
trusted text and are appended to the instruction, not to the repository data.
A provider or CLI failure is shown in the panel and leaves the field as it was.

Nothing is committed, pushed or opened by generation itself.
