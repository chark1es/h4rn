//! Real Claude Code skill pickup.
//!
//! Ignored by default because it invokes the installed `claude` binary and
//! spends a model turn. Run it with:
//!
//! ```text
//! cargo test -p h4rn-skills --test claude_pickup -- --ignored --nocapture
//! ```
//!
//! The test installs a uniquely named skill through the same installer the
//! daemon uses, then asks Claude Code to run it. A response containing the
//! skill's token proves Claude Code discovered the copied directory, not just
//! that h4rn wrote the files.

use std::path::{Path, PathBuf};
use std::process::Command;

use h4rn_protocol::{HarnessKind, HostId, SkillSource};
use h4rn_skills::SkillsService;
use h4rn_skills::install::SkillLayout;

const SKILL_NAME: &str = "h4rn-pickup-check";
const TOKEN: &str = "h4rn-skill-picked-up";

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
    })
}

fn write_skill(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("SKILL.md"),
        format!(
            "---\nname: {SKILL_NAME}\ndescription: Proves h4rn-installed skills are discovered by Claude Code\n---\n\nWhen this skill is invoked, reply with exactly: {TOKEN}\n"
        ),
    )
    .unwrap();
}

/// Copies the current login so a temporary config dir can run the real binary.
fn copy_credentials(source_home: &Path, config_dir: &Path) -> bool {
    let source = source_home.join(".claude").join(".credentials.json");
    if !source.is_file() {
        return false;
    }
    std::fs::create_dir_all(config_dir).unwrap();
    let destination = config_dir.join(".credentials.json");
    std::fs::copy(source, &destination).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(&destination).unwrap().permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&destination, permissions).unwrap();
    }
    true
}

fn run_claude(config_dir: &Path, workspace: &Path) -> std::process::Output {
    Command::new("claude")
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .current_dir(workspace)
        .args(["-p", &format!("/{SKILL_NAME}"), "--output-format", "text"])
        .output()
        .expect("run claude")
}

#[tokio::test]
#[ignore = "invokes the real claude binary and spends a model turn"]
async fn claude_code_discovers_an_h4rn_installed_skill() {
    if !command_exists("claude") {
        panic!("claude is not on PATH; install Claude Code before running this test");
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let config = tempfile::tempdir().unwrap();
    let claude_dir = config.path().join(".claude");
    if !copy_credentials(&home, &claude_dir) {
        panic!("no ~/.claude/.credentials.json; sign in to Claude Code first");
    }
    let workspace = tempfile::tempdir().unwrap();

    let data = tempfile::tempdir().unwrap();
    let layout = SkillLayout {
        harness: HarnessKind::ClaudeCode,
        user_root: Some(claude_dir.join("skills")),
        project_root: None,
        shared_project_root: false,
        detected: true,
    };
    let service = SkillsService::with_layouts(
        data.path(),
        HostId::new("host_claude_pickup"),
        vec![layout],
        None,
    )
    .await
    .unwrap();

    let source = tempfile::tempdir().unwrap();
    let skill_root = source.path().join(SKILL_NAME);
    write_skill(&skill_root);
    let summary = service
        .add(
            &SkillSource::LocalFolder {
                path: skill_root.display().to_string(),
            },
            &[HarnessKind::ClaudeCode],
            None,
        )
        .await
        .expect("install the skill");
    let installed = claude_dir.join("skills").join(SKILL_NAME);
    assert!(installed.join("SKILL.md").is_file());
    assert!(installed.join(".h4rn-managed").is_file());

    let output = tokio::task::spawn_blocking({
        let config_dir = claude_dir.clone();
        let workspace = workspace.path().to_path_buf();
        move || run_claude(&config_dir, &workspace)
    })
    .await
    .expect("join claude");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "claude failed: status {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(TOKEN),
        "Claude Code did not pick up the skill. stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    service.remove(&summary.id).await.expect("uninstall");
    assert!(
        !installed.exists(),
        "uninstall must remove the managed copy"
    );
}
