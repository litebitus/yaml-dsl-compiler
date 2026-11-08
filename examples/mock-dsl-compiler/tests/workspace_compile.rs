//! Runs the tool over a copy of the example workspace, as a user would.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const WORKSPACE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/workspace");
const GENERATED_FILE: &str = "generated.tf";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_COPY: AtomicUsize = AtomicUsize::new(0);

/// A copy of the example workspace without its generated files, removed when the
/// test ends, whether it passed or failed.
struct WorkspaceCopy {
    root: PathBuf,
}

impl WorkspaceCopy {
    fn new() -> Self {
        let copy = NEXT_COPY.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("mock-dsl-compiler-{}-{copy}", std::process::id()));
        copy_documents(Path::new(WORKSPACE), &root);
        Self { root }
    }

    fn run_tool(&self) -> Output {
        let child = Command::new(env!("CARGO_BIN_EXE_mock-dsl-compiler"))
            .arg(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the tool starts");
        output_within_timeout(child)
    }
}

impl Drop for WorkspaceCopy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_documents(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("the copy's directory is created");
    for entry in fs::read_dir(from).expect("the workspace is readable") {
        let entry = entry.expect("a workspace entry is readable");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("an entry has a type").is_dir() {
            copy_documents(&entry.path(), &target);
        } else if entry.file_name() != GENERATED_FILE {
            fs::copy(entry.path(), &target).expect("a document is copied");
        }
    }
}

fn output_within_timeout(mut child: Child) -> Output {
    let started = Instant::now();
    while child.try_wait().expect("the child is waited on").is_none() {
        if started.elapsed() > COMMAND_TIMEOUT {
            let _ = child.kill();
            panic!("the command ran longer than {COMMAND_TIMEOUT:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child
        .wait_with_output()
        .expect("the child's output is read")
}

fn overlays() -> Vec<&'static str> {
    vec!["concern-1/dev", "concern-1/staging", "concern-1/production"]
}

#[test]
fn every_overlay_compiles_to_the_committed_file() {
    let workspace = WorkspaceCopy::new();
    let output = workspace.run_tool();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for overlay in overlays() {
        let expected =
            fs::read_to_string(Path::new(WORKSPACE).join(overlay).join(GENERATED_FILE)).unwrap();
        let written =
            fs::read_to_string(workspace.root.join(overlay).join(GENERATED_FILE)).unwrap();
        assert_eq!(written, expected, "{overlay}");
    }
}

#[test]
fn an_error_names_its_place_and_its_unit_writes_nothing() {
    let workspace = WorkspaceCopy::new();
    let overlay = workspace.root.join("concern-1/dev/mock-dsl.yml");
    fs::write(
        &overlay,
        "queue:\n  name mock-orders:\n    visibility: slow\n",
    )
    .unwrap();
    let output = workspace.run_tool();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected_place = format!(
        "{}:3:17: `visibility` must be `integer`",
        overlay.canonicalize().unwrap().display()
    );
    assert!(stderr.contains(&expected_place), "{stderr}");
    assert!(
        !workspace
            .root
            .join("concern-1/dev")
            .join(GENERATED_FILE)
            .exists()
    );
    assert!(
        workspace
            .root
            .join("concern-1/production")
            .join(GENERATED_FILE)
            .exists()
    );
}

#[test]
fn an_undeclared_reference_is_an_error() {
    let workspace = WorkspaceCopy::new();
    let overlay = workspace.root.join("concern-1/dev/mock-dsl.yml");
    fs::write(
        &overlay,
        "queue:\n  name mock-orders:\n    kms_key: ref key.mock_missing.arn\n",
    )
    .unwrap();
    let output = workspace.run_tool();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid reference: key.mock_missing"),
        "{stderr}"
    );
}

#[test]
fn terraform_fmt_leaves_the_written_files_unchanged() {
    let workspace = WorkspaceCopy::new();
    assert!(workspace.run_tool().status.success());
    let child = Command::new("terraform")
        .args(["fmt", "-check", "-recursive"])
        .arg(&workspace.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("terraform is installed");
    let output = output_within_timeout(child);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
