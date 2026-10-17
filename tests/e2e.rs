//! End-to-end tests: build the fixture app into a bundle, then run it in every mode.
//!
//! Opt-in, because building needs a Python with pip and network access (for setuptools):
//!
//!     PYRS_E2E_PYTHON=/path/to/python-with-pip cargo test --test e2e
//!
//! Runs use normal interpreter discovery, so they exercise whatever `python3` is on PATH —
//! ideally a bare one without pip or ensurepip.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const PYRS: &str = env!("CARGO_BIN_EXE_pyrs");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn builder_python() -> Option<String> {
    std::env::var("PYRS_E2E_PYTHON").ok().filter(|p| !p.is_empty())
}

/// Builds the hello_app bundle once per test run and returns its path.
fn bundle() -> &'static Path {
    static BUNDLE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, path) = BUNDLE.get_or_init(|| {
        let python = builder_python().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let links = tmp.path().join("links");
        // Build from a copy: setuptools would leave build/ and *.egg-info in the fixture.
        let dep = tmp.path().join("greeter_dep");
        copy_dir(&fixtures().join("greeter_dep"), &dep);
        let status = Command::new(&python)
            .args(["-m", "pip", "wheel", "-q", "--no-deps", "--wheel-dir"])
            .arg(&links)
            .arg(&dep)
            .status()
            .unwrap();
        assert!(status.success(), "building the greeter_dep fixture failed");

        let out = tmp.path().join("hello.pyrs");
        let output = Command::new(PYRS)
            .args(["-q", "build"])
            .arg(fixtures().join("hello_app"))
            .args(["--python", &python, "-o"])
            .arg(&out)
            .env("PIP_FIND_LINKS", &links)
            .output()
            .unwrap();
        assert!(output.status.success(), "pyrs build failed: {}", String::from_utf8_lossy(&output.stderr));
        (tmp, out)
    });
    path
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dst = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dst);
        } else {
            std::fs::copy(entry.path(), dst).unwrap();
        }
    }
}

fn pyrs(args: &[&str]) -> Output {
    Command::new(PYRS).args(args).env_remove("PYRS_PYTHON").output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Returns early from a test when e2e tests are not enabled.
macro_rules! require_e2e {
    () => {
        if builder_python().is_none() {
            eprintln!("skipped: set PYRS_E2E_PYTHON to run end-to-end tests");
            return;
        }
    };
}

#[test]
fn temp_mode_passes_args_and_exit_code() {
    require_e2e!();
    let bundle = bundle().to_str().unwrap();
    let out = pyrs(&["-q", "run", bundle, "--", "Alice", "--exit", "7"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(7), "{text}");
    assert!(text.contains("Hello, Alice!"), "{text}");
    assert!(text.contains("argv0=hello-app"), "{text}");

    // The temp workspace is gone once the run is over.
    let prefix = text.lines().find_map(|l| l.strip_prefix("prefix=")).unwrap();
    assert!(!Path::new(prefix).exists(), "{prefix} was left behind");
}

#[test]
fn global_mode_leaves_the_interpreter_untouched() {
    require_e2e!();
    let bundle = bundle().to_str().unwrap();
    let out = pyrs(&["-q", "run", "--env", "global", bundle, "--", "Bob"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout(&out).contains("Hello, Bob!"));

    let leaked = Command::new("python3").args(["-I", "-c", "import greeter_dep"]).output().unwrap();
    assert!(!leaked.status.success(), "greeter_dep ended up in the host interpreter");
}

#[test]
fn venv_mode_installs_once() {
    require_e2e!();
    let bundle = bundle().to_str().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let venv = tmp.path().join("venv");
    let created = Command::new("python3").args(["-m", "venv", "--without-pip"]).arg(&venv).status().unwrap();
    assert!(created.success());
    let venv = venv.to_str().unwrap();

    let first = pyrs(&["run", "--venv", venv, bundle, "--", "Carol"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(String::from_utf8_lossy(&first.stderr).contains("installing hello-app 1.0.0"));
    assert!(stdout(&first).contains(&format!("prefix={venv}")));

    let second = pyrs(&["run", "--venv", venv, bundle, "--", "Dave"]);
    assert!(second.status.success());
    assert!(!String::from_utf8_lossy(&second.stderr).contains("installing"), "reinstalled on the second run");
}

#[test]
fn tampered_bundle_is_rejected() {
    require_e2e!();
    let tmp = tempfile::tempdir().unwrap();
    let mut bytes = std::fs::read(bundle()).unwrap();
    // Flip a byte in the middle of the stored wheel data (past manifest.json at the start).
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    let bad = tmp.path().join("bad.pyrs");
    std::fs::write(&bad, bytes).unwrap();

    let out = pyrs(&["run", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("corrupted"), "{err}");
}

#[test]
fn inspect_shows_the_entry_point() {
    require_e2e!();
    let out = pyrs(&["inspect", bundle().to_str().unwrap()]);
    let text = stdout(&out);
    assert!(text.starts_with("hello-app 1.0.0"), "{text}");
    assert!(text.contains("hello-app -> hello_app.cli:main"), "{text}");
    assert!(text.contains("greeter_dep-0.2.0-py3-none-any.whl"), "{text}");
}
