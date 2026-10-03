# codex-worktree

Creates, lists, and removes the managed Git worktrees that `codex --worktree`
and the TUI use for isolated threads.

## Custom provisioner: `CODEX_WORKTREE_COMMAND`

By default Codex materializes a worktree with `git worktree add` followed by a
full checkout, which writes a private copy of every tracked file. With many
parallel agents on a large repository that dominates disk. Set
`CODEX_WORKTREE_COMMAND` to the path of a program that creates the checkout
instead, for example one that uses copy-on-write clones:

```sh
export CODEX_WORKTREE_COMMAND=/path/to/provisioner
```

Codex still chooses the destination path and verifies the result. The program
is called as:

```text
PROGRAM create <source-root> <worktree-root> <commit>
PROGRAM remove <source-root> <worktree-root>
```

- `create` must register a linked worktree of `<source-root>` at
  `<worktree-root>` whose `HEAD` is `<commit>` (detached or on a new branch)
  and whose tracked files are clean.
- `remove` must unregister and delete the worktree. Codex only calls it after
  refusing a worktree that has uncommitted tracked or untracked changes.
  Ignored files (dependency clones, build output) do not block removal, because
  the provisioner owns them.
- Inherited repository-selecting `GIT_*` variables (`GIT_DIR`, `GIT_INDEX_FILE`,
  ...) are removed from the program's environment. Its stdout is ignored and its
  stderr is shown on failure.
- A non-zero exit fails the operation. After `create`, Codex checks that the
  result is a linked worktree of the same repository, at the requested commit,
  with clean tracked files; otherwise it calls `remove` and reports an error.

Unset or empty means the built-in Git behavior.
