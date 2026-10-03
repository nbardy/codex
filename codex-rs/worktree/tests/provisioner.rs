#![cfg(unix)]
#![allow(clippy::expect_used)]

//! Exercises `WorktreeProvisioner::Command` with real shell programs against
//! real repositories. The program is the only thing replaced: Codex's own path
//! allocation, verification, listing, and removal safety checks run for real.

use codex_worktree::CreateWorktree;
use codex_worktree::ManagedWorktree;
use codex_worktree::WorktreeManager;
use codex_worktree::WorktreeProvisioner;
use codex_worktree::WorktreeSettings;
use pretty_assertions::assert_eq;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// `create` and `remove` bodies for a stand-in provisioner. `$2` is the source
/// repository, `$3` the worktree root, `$4` the commit (create only). Every
/// invocation's arguments are appended to the log first.
struct Program {
    create: &'static str,
    remove: &'static str,
}

/// A well-behaved provisioner: a plain linked worktree at the requested commit.
const LINKED: Program = Program {
    create: r#"git -C "$2" worktree add --detach "$3" "$4" >/dev/null"#,
    remove: r#"git -C "$2" worktree remove --force "$3""#,
};

struct Fixture {
    _temp_dir: TempDir,
    codex_home: PathBuf,
    repository: PathBuf,
    log: PathBuf,
    program: PathBuf,
}

impl Fixture {
    fn new(program: &Program) -> Self {
        let temp_dir = tempfile::tempdir().expect("create temporary test directory");
        let root = dunce::canonicalize(temp_dir.path()).expect("canonicalize temporary root");
        let codex_home = root.join("codex-home");
        let repository = root.join("project");
        fs::create_dir_all(&codex_home).expect("create Codex home");
        fs::create_dir_all(&repository).expect("create repository");
        run_git(&repository, &["init", "--quiet"]);
        for (name, text) in [("one", "first\n"), ("two", "second\n")] {
            fs::write(repository.join("tracked.txt"), text).expect("write tracked file");
            run_git(&repository, &["add", "."]);
            run_git(&repository, &["commit", "--quiet", "--no-gpg-sign", "-m", name]);
        }

        let log = root.join("provisioner.log");
        let script = root.join("provisioner.sh");
        let body = format!(
            "#!/bin/sh\nset -eu\necho \"$@\" >> '{log}'\ncase \"$1\" in\n  create) {create} ;;\n  remove) {remove} ;;\nesac\n",
            log = log.display(),
            create = program.create,
            remove = program.remove,
        );
        fs::write(&script, body).expect("write provisioner script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("make provisioner executable");
        Self {
            _temp_dir: temp_dir,
            codex_home,
            repository,
            log,
            program: script,
        }
    }

    fn manager(&self) -> WorktreeManager {
        let settings = WorktreeSettings::from_desktop_config(&self.codex_home, /*desktop*/ None)
            .expect("load default worktree settings");
        WorktreeManager::new(settings)
            .with_provisioner(WorktreeProvisioner::Command(self.program.clone()))
    }

    fn create(&self, manager: &WorktreeManager) -> anyhow::Result<ManagedWorktree> {
        manager.create(&CreateWorktree {
            source_cwd: self.repository.clone(),
            base: None,
        })
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn registered_worktrees(&self) -> usize {
        run_git(&self.repository, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count()
    }
}

fn git_output(repository: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(repository)
        .args([
            "-c",
            "user.name=Codex Worktree Test",
            "-c",
            "user.email=codex-worktree-test@example.invalid",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?} in {repository:?}: {error}"))
}

fn run_git(repository: &Path, args: &[&str]) -> String {
    let output = git_output(repository, args);
    assert!(
        output.status.success(),
        "git {args:?} failed in {repository:?}: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn external_provisioner_owns_the_checkout_lifecycle() {
    let fixture = Fixture::new(&LINKED);
    let manager = fixture.manager();
    let head = run_git(&fixture.repository, &["rev-parse", "HEAD"]);

    let checkout = fixture.create(&manager).expect("create via program");
    assert_eq!(checkout.head_sha, head);
    assert_eq!(
        fs::read_to_string(checkout.root.join("tracked.txt")).expect("read checkout file"),
        "second\n"
    );
    assert_eq!(
        manager
            .list(&fixture.repository)
            .expect("list")
            .into_iter()
            .map(|listed| listed.root)
            .collect::<Vec<_>>(),
        vec![checkout.root.clone()]
    );

    // Ignored paths (dependency clones, build output) belong to the
    // provisioner, so they must not block removal the way they do for Git.
    fs::write(fixture.repository.join(".git/info/exclude"), "deps/\n").expect("ignore deps");
    fs::create_dir(checkout.root.join("deps")).expect("create ignored dir");
    fs::write(checkout.root.join("deps/pkg"), "cloned\n").expect("write ignored file");

    // Real local changes still block removal, and the program is not consulted.
    fs::write(checkout.root.join("unsaved.txt"), "local work\n").expect("write untracked file");
    assert!(manager.remove(&fixture.repository, &checkout.root).is_err());
    assert!(checkout.root.join("unsaved.txt").exists());
    fs::remove_file(checkout.root.join("unsaved.txt")).expect("remove untracked file");
    fs::write(checkout.root.join("tracked.txt"), "edited\n").expect("edit tracked file");
    assert!(manager.remove(&fixture.repository, &checkout.root).is_err());
    run_git(&checkout.root, &["checkout", "--", "tracked.txt"]);
    assert_eq!(fixture.calls().len(), 1, "program ran only for create");

    manager
        .remove(&fixture.repository, &checkout.root)
        .expect("remove clean checkout via program");
    assert!(!checkout.root.exists());
    assert_eq!(fixture.registered_worktrees(), 1);
    assert_eq!(
        fixture.calls(),
        vec![
            format!(
                "create {} {} {head}",
                fixture.repository.display(),
                checkout.root.display()
            ),
            format!(
                "remove {} {}",
                fixture.repository.display(),
                checkout.root.display()
            ),
        ]
    );
}

#[test]
fn program_that_fails_partway_leaves_no_worktree_behind() {
    let fixture = Fixture::new(&Program {
        create: r#"git -C "$2" worktree add --detach "$3" "$4" >/dev/null; echo 'disk quota exceeded' >&2; exit 1"#,
        ..LINKED
    });
    let manager = fixture.manager();

    let error = fixture.create(&manager).expect_err("program failure is an error");
    assert!(
        format!("{error:#}").contains("disk quota exceeded"),
        "stderr was not reported: {error:#}"
    );
    assert_eq!(fixture.registered_worktrees(), 1, "registration was rolled back");
    assert!(fixture.calls().iter().any(|call| call.starts_with("remove ")));
}

#[test]
fn program_that_checks_out_the_wrong_commit_is_rejected_and_rolled_back() {
    let fixture = Fixture::new(&Program {
        create: r#"git -C "$2" worktree add --detach "$3" "$4~1" >/dev/null"#,
        ..LINKED
    });
    let manager = fixture.manager();

    let error = fixture.create(&manager).expect_err("wrong HEAD must be rejected");
    assert!(format!("{error:#}").contains("invalid worktree"), "{error:#}");
    assert_eq!(fixture.registered_worktrees(), 1);
}

#[test]
fn program_that_creates_an_unrelated_repository_is_rejected() {
    let fixture = Fixture::new(&Program {
        create: r#"git init --quiet "$3""#,
        remove: r#"rm -rf "$3""#,
    });
    let manager = fixture.manager();

    let error = fixture.create(&manager).expect_err("unrelated repo must be rejected");
    assert!(format!("{error:#}").contains("not a linked worktree"), "{error:#}");
    assert_eq!(fixture.registered_worktrees(), 1);
}
