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

struct Built {
    _tmp: tempfile::TempDir,
    bundle: PathBuf,
    /// find-links directory holding the greeter_dep wheel.
    links: PathBuf,
}

/// Builds the hello_app bundle once per test run and returns its path.
fn bundle() -> &'static Path {
    &built().bundle
}

fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
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
        Built { _tmp: tmp, bundle: out, links }
    })
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

/// A cache directory shared by the tests, so they never touch the user's real cache.
fn cache_home() -> &'static Path {
    static CACHE: OnceLock<tempfile::TempDir> = OnceLock::new();
    CACHE.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

fn pyrs(args: &[&str]) -> Output {
    Command::new(PYRS)
        .args(args)
        .env_remove("PYRS_PYTHON")
        .env("XDG_CACHE_HOME", cache_home())
        .output()
        .unwrap()
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
fn no_cache_run_passes_args_and_exit_code_and_cleans_up() {
    require_e2e!();
    let bundle = bundle().to_str().unwrap();
    let out = pyrs(&["-q", "run", "--no-cache", bundle, "--", "Alice", "--exit", "7"]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(7), "{text}");
    assert!(text.contains("Hello, Alice!"), "{text}");
    assert!(text.contains("argv0=hello-app"), "{text}");

    // The temp workspace is gone once the run is over.
    let prefix = text.lines().find_map(|l| l.strip_prefix("prefix=")).unwrap();
    assert!(!Path::new(prefix).exists(), "{prefix} was left behind");
}

#[test]
fn default_run_reuses_the_cached_environment() {
    require_e2e!();
    let bundle = bundle().to_str().unwrap();
    let first = pyrs(&["run", bundle, "--", "One"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let second = pyrs(&["run", bundle, "--", "Two"]);
    assert!(stdout(&second).contains("Hello, Two!"));
    assert!(!String::from_utf8_lossy(&second.stderr).contains("setting up"), "rebuilt instead of reusing");
    let prefix = stdout(&second).lines().find_map(|l| l.strip_prefix("prefix=")).unwrap().to_string();
    assert!(Path::new(&prefix).starts_with(cache_home()), "{prefix}");
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

    // --no-cache: a cache hit would skip reading the wheels. That's safe (the cached environment
    // was built from wheels that matched the same manifest), but it wouldn't exercise the check.
    let out = pyrs(&["run", "--no-cache", bad.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("corrupted"), "{err}");
}

#[test]
fn builds_with_a_python_that_has_no_pip() {
    require_e2e!();
    let imports = |module: &str| {
        Command::new("python3").args(["-I", "-c", &format!("import {module}")]).output().is_ok_and(|o| o.status.success())
    };
    if imports("pip") {
        eprintln!("skipped: needs a python3 without pip");
        return;
    }
    let has_ensurepip = imports("ensurepip");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("nopip.pyrs");
    let output = Command::new(PYRS)
        .arg("build")
        .arg(fixtures().join("hello_app"))
        .args(["--python", "python3", "-o"])
        .arg(&out)
        .env("PIP_FIND_LINKS", &built().links)
        .env("XDG_CACHE_HOME", cache_home())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    if !has_ensurepip {
        // e.g. Debian without python3-venv: there's no way to get pip, so say how to fix it.
        assert_eq!(output.status.code(), Some(2), "{err}");
        assert!(err.contains("sudo apt install python3-pip python3-venv"), "{err}");
        return;
    }
    assert!(output.status.success(), "{err}");
    assert!(err.contains("creating a build environment"), "{err}");

    let run = pyrs(&["-q", "run", "--no-cache", out.to_str().unwrap(), "--", "NoPip"]);
    assert!(stdout(&run).contains("Hello, NoPip!"));
}

#[test]
#[cfg(unix)]
fn build_finds_the_projects_own_venv() {
    require_e2e!();
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("hello_app");
    copy_dir(&fixtures().join("hello_app"), &project);
    // The builder Python lives in a venv with pip; make it the project's .venv.
    let builder_venv = Path::new(&builder_python().unwrap()).parent().unwrap().parent().unwrap().to_path_buf();
    std::os::unix::fs::symlink(&builder_venv, project.join(".venv")).unwrap();

    let out = tmp.path().join("venv.pyrs");
    let output = Command::new(PYRS)
        .arg("build")
        .arg(&project)
        .arg("-o")
        .arg(&out)
        .env_remove("PYRS_PYTHON")
        .env_remove("VIRTUAL_ENV")
        .env("PIP_FIND_LINKS", &built().links)
        .env("XDG_CACHE_HOME", cache_home())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{err}");
    assert!(err.contains("building with the venv at") && err.contains(".venv"), "{err}");
    assert!(out.is_file());
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
