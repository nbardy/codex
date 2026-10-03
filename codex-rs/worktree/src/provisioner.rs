//! Chooses who materializes a managed worktree checkout.
//!
//! Codex's built-in path writes a full private copy of every tracked file. On
//! large repositories with many parallel agents that dominates disk, so an
//! external program (for example a copy-on-write worktree tool) can own
//! creation and removal instead. Codex still allocates the path, verifies the
//! result, and owns every safety check around removal.

use crate::git::GitOperation;
use crate::git::git_output;
use crate::git::git_stdout;
use crate::git::scrub_repository_env;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::shell_environment::scrub_non_inheritable_env_vars;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Environment variable naming the external worktree provisioner.
pub const WORKTREE_COMMAND_ENV: &str = "CODEX_WORKTREE_COMMAND";

/// How managed worktree checkouts are created and removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeProvisioner {
    /// `git worktree add --no-checkout` followed by `reset --hard`.
    Git,
    /// An external program invoked as:
    ///
    /// - `PROGRAM create <source-root> <worktree-root> <commit>`: register a
    ///   linked worktree of `<source-root>` at `<worktree-root>` with HEAD at
    ///   `<commit>` (detached or on a new branch) and clean tracked files.
    /// - `PROGRAM remove <source-root> <worktree-root>`: unregister and delete
    ///   it. Codex has already refused removal of a worktree with changes.
    ///
    /// The program runs with inherited repository-selecting `GIT_*` variables
    /// removed. Its stdout is ignored; stderr is reported on failure.
    Command(PathBuf),
}

impl WorktreeProvisioner {
    /// Reads [`WORKTREE_COMMAND_ENV`]; unset or empty selects [`Self::Git`].
    pub fn from_env() -> Self {
        match std::env::var_os(WORKTREE_COMMAND_ENV) {
            Some(program) if !program.is_empty() => Self::Command(PathBuf::from(program)),
            _ => Self::Git,
        }
    }
}

/// Creates the checkout at `root`. On error nothing is left registered.
pub(crate) fn create(
    provisioner: &WorktreeProvisioner,
    source_root: &Path,
    root: &Path,
    head_sha: &str,
) -> Result<()> {
    match provisioner {
        WorktreeProvisioner::Git => create_with_git(source_root, root, head_sha),
        WorktreeProvisioner::Command(program) => {
            create_with_command(program, source_root, root, head_sha)
        }
    }
}

/// Removes a checkout unconditionally. Used to roll back failed creations.
pub(crate) fn rollback(
    provisioner: &WorktreeProvisioner,
    source_root: &Path,
    root: &Path,
) -> Result<()> {
    match provisioner {
        WorktreeProvisioner::Git => remove_with_git(source_root, root, /*force*/ true),
        WorktreeProvisioner::Command(program) => {
            run_program(program, "remove", source_root, root, &[])
        }
    }
}

/// Removes a checkout that the caller has validated as a managed worktree,
/// refusing local changes.
pub(crate) fn remove(
    provisioner: &WorktreeProvisioner,
    source_root: &Path,
    root: &Path,
) -> Result<()> {
    match provisioner {
        WorktreeProvisioner::Git => {
            refuse_ignored_files(root)?;
            remove_with_git(source_root, root, /*force*/ false)
        }
        WorktreeProvisioner::Command(program) => {
            // Ignored paths (dependency clones, build output) belong to the
            // provisioner, so only tracked and untracked changes block removal,
            // matching what a non-forced `git worktree remove` refuses.
            let status = git_output(
                root,
                GitOperation::WorkingTree,
                [
                    OsStr::new("status"),
                    OsStr::new("--porcelain"),
                    OsStr::new("-z"),
                ],
            )?;
            if !status.stdout.is_empty() {
                bail!("worktree contains local changes; commit or discard them before deleting it");
            }
            run_program(program, "remove", source_root, root, &[])
        }
    }
}

fn create_with_git(source_root: &Path, root: &Path, head_sha: &str) -> Result<()> {
    git_output(
        source_root,
        GitOperation::WorkingTree,
        [
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            OsStr::new("--no-checkout"),
            root.as_os_str(),
            OsStr::new(head_sha),
        ],
    )
    .context("cannot create managed worktree")?;

    // Write only the destination's config, without changing shared settings or
    // enabling worktreeConfig for the source repository.
    let populated = crate::git::git_path(
        root,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "config.worktree",
        ],
    )
    .and_then(|config| {
        git_output(
            root,
            GitOperation::Metadata,
            [
                OsStr::new("config"),
                OsStr::new("--file"),
                config.as_os_str(),
                OsStr::new("core.worktree"),
                root.as_os_str(),
            ],
        )?;
        // Discover destination-only filters before materializing files, and pin
        // the working tree even when per-worktree configuration is disabled.
        git_output(
            root,
            GitOperation::WorkingTree,
            [
                "--work-tree=.",
                "reset",
                "--hard",
                "--no-recurse-submodules",
                head_sha,
            ],
        )
    });
    if let Err(error) = populated {
        remove_with_git(source_root, root, /*force*/ true)
            .context("cannot roll back an incomplete managed worktree")?;
        return Err(error).context("cannot populate managed worktree");
    }
    Ok(())
}

fn create_with_command(
    program: &Path,
    source_root: &Path,
    root: &Path,
    head_sha: &str,
) -> Result<()> {
    if let Err(error) = run_program(program, "create", source_root, root, &[head_sha]) {
        // The program may have registered the worktree before failing.
        let _ = run_program(program, "remove", source_root, root, &[]);
        return Err(error);
    }
    // Never trust the external result: it must be indistinguishable from what
    // the Git provisioner produces, or later listing and removal misbehave.
    if let Err(error) = verify_external_checkout(source_root, root, head_sha) {
        run_program(program, "remove", source_root, root, &[])
            .context("cannot roll back an invalid externally provisioned worktree")?;
        return Err(error).context(format!(
            "{} produced an invalid worktree",
            program.display()
        ));
    }
    Ok(())
}

fn verify_external_checkout(source_root: &Path, root: &Path, head_sha: &str) -> Result<()> {
    let common = |cwd: &Path| -> Result<PathBuf> {
        let path = git_stdout(
            cwd,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        dunce::canonicalize(&path).with_context(|| format!("cannot resolve {path}"))
    };
    if common(root)? != common(source_root)? {
        bail!(
            "{} is not a linked worktree of {}",
            root.display(),
            source_root.display()
        );
    }
    let head = git_stdout(root, ["rev-parse", "--verify", "HEAD^{commit}"])?;
    if head != head_sha {
        bail!("worktree HEAD is {head}, expected {head_sha}");
    }
    let status = git_output(
        root,
        GitOperation::WorkingTree,
        [
            OsStr::new("status"),
            OsStr::new("--porcelain"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=no"),
        ],
    )?;
    if !status.stdout.is_empty() {
        bail!("worktree tracked files do not match {head_sha}");
    }
    Ok(())
}

fn refuse_ignored_files(root: &Path) -> Result<()> {
    let ignored = git_output(
        root,
        GitOperation::WorkingTree,
        [
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--ignored"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;
    if !ignored.stdout.is_empty() {
        bail!("worktree contains ignored local files; remove them before deleting it");
    }
    Ok(())
}

fn remove_with_git(source_root: &Path, root: &Path, force: bool) -> Result<()> {
    let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if force {
        args.push(OsStr::new("--force"));
    }
    args.push(root.as_os_str());
    git_output(source_root, GitOperation::WorkingTree, args).map(|_| ())
}

fn run_program(
    program: &Path,
    action: &str,
    source_root: &Path,
    root: &Path,
    extra: &[&str],
) -> Result<()> {
    let mut command = Command::new(program);
    scrub_repository_env(&mut command);
    command
        .current_dir(source_root)
        .arg(action)
        .arg(source_root)
        .arg(root)
        .args(extra);
    scrub_non_inheritable_env_vars(&mut command);
    let output = command.output().with_context(|| {
        format!(
            "cannot run {WORKTREE_COMMAND_ENV} program {}",
            program.display()
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{WORKTREE_COMMAND_ENV} program {} {action} failed: {}",
        program.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}
