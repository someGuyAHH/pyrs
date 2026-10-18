use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};

use crate::bundle;
use crate::cli::BuildArgs;
use crate::envs::run_captured;
use crate::interpreter::{self, Interpreter, ProbeInfo};
use crate::manifest::{BuiltWith, Entry, EntryKind, FORMAT_VERSION, Manifest, WheelEntry};
use crate::ui::status;
use crate::hash::sha256_file;
use crate::wheel::{WheelFilename, WheelInfo, normalize};

pub fn run(args: BuildArgs, quiet: bool) -> Result<()> {
    let dir = args
        .dir
        .canonicalize()
        .with_context(|| format!("project directory {} not found", args.dir.display()))?;
    if !dir.join("pyproject.toml").is_file() && !dir.join("setup.py").is_file() {
        bail!(
            "{} has no pyproject.toml; pyrs build needs an installable Python project",
            dir.display()
        );
    }

    let tmp = tempfile::Builder::new().prefix("pyrs-build-").tempdir().context("creating a build directory")?;
    let python = build_python(args.python.as_deref(), &dir, tmp.path(), quiet)?;
    let own_dir = tmp.path().join("own");
    let wheels_dir = tmp.path().join("wheels");

    // Build from a clean copy: setuptools leaves build/ and *.egg-info behind, and stale files
    // in an existing build/ would otherwise end up in the wheel.
    let source = tmp.path().join("source");
    copy_project(&dir, &source, true).context("copying the project")?;

    // Build the project alone first, so its wheel can be told apart from the dependencies.
    status(quiet, format!("building {}", dir.display()));
    pip_wheel(&python, &own_dir, ["--no-deps".as_ref(), source.as_os_str()], quiet)?;
    let own_wheel = single_wheel(&own_dir)?;
    let info = WheelInfo::read(&own_wheel)?;
    let project_wheel = file_name(&own_wheel)?;

    // Then resolve everything it needs. pyrs installs wheels itself, so pip stays out of the bundle.
    status(quiet, format!("collecting dependencies of {} {}", info.name, info.version));
    pip_wheel(&python, &wheels_dir, [own_wheel.as_os_str()], quiet)?;
    let copied = wheels_dir.join(&project_wheel);
    if !copied.exists() {
        fs::copy(&own_wheel, &copied).context("copying the project wheel into the bundle")?;
    }

    let entry = choose_entry(&info, args.entry.as_deref())?;

    let mut wheels = Vec::new();
    let mut pure = true;
    for path in wheel_paths(&wheels_dir)? {
        let file = file_name(&path)?;
        let parsed = WheelFilename::parse(&file)?;
        pure &= parsed.is_pure();
        wheels.push(WheelEntry { sha256: sha256_file(&path)?, file });
    }

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        name: info.name.clone(),
        version: info.version.clone(),
        requires_python: info.requires_python.clone(),
        entry,
        project_wheel,
        wheels,
        pure,
        built_with: BuiltWith {
            pyrs: env!("CARGO_PKG_VERSION").to_string(),
            python: python.info.version_string(),
            platform: python.info.platform.clone(),
        },
    };

    let out = match args.output {
        Some(out) => out,
        None => PathBuf::from(format!("{}-{}.pyrs", safe_name(&info.name), safe_name(&info.version))),
    };
    bundle::write(&out, &manifest, &wheels_dir)?;

    let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    status(
        quiet,
        format!(
            "{} wheel{}, {}, {}",
            manifest.wheels.len(),
            if manifest.wheels.len() == 1 { "" } else { "s" },
            human_size(size),
            if pure { "pure Python" } else { "has compiled packages (tied to this platform)" }
        ),
    );
    println!("{}", out.display());
    Ok(())
}

/// A Python with pip to build with, looked for in this order:
///
/// 1. `--python` / `PYRS_PYTHON`, used as given;
/// 2. a venv: the activated one (`$VIRTUAL_ENV`), then the project's `.venv/` or `venv/`;
/// 3. `python3` / `python` on PATH;
/// 4. a throwaway `python -m venv` made from the PATH Python, which gets pip from `ensurepip`.
///    It runs the same Python, so the wheels it resolves are the same.
fn build_python(explicit: Option<&str>, project: &Path, tmp: &Path, quiet: bool) -> Result<Interpreter> {
    let auto = explicit.is_none() && std::env::var_os("PYRS_PYTHON").is_none_or(|v| v.is_empty());
    // Venvs that were found but had no pip, to mention if nothing else works either.
    let mut pipless = Vec::new();
    if auto {
        let active = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
        for venv in venv_candidates(active, project) {
            match Interpreter::from_venv(&venv, false) {
                Ok(py) if py.info.has_pip => {
                    status(quiet, format!("building with the venv at {}", venv.display()));
                    return Ok(py);
                }
                Ok(_) => pipless.push(venv),
                // Not a working venv (e.g. a stray directory named `venv`): ignore it.
                Err(_) => {}
            }
        }
        let with_pip = |info: &ProbeInfo| if info.has_pip { Ok(()) } else { Err(String::new()) };
        if let Ok(py) = interpreter::discover(None, false, with_pip) {
            return Ok(py);
        }
    }
    let host = interpreter::discover(explicit, false, |_| Ok(()))?;
    if host.info.has_pip {
        return Ok(host);
    }

    let mut no_pip_help = String::new();
    for venv in &pipless {
        no_pip_help.push_str(&format!("Found the venv {}, but it has no pip in it.\n", venv.display()));
    }
    no_pip_help.push_str(
        "Install pip for that Python (on Debian/Ubuntu: sudo apt install python3-pip python3-venv), \
         put pip in the project's venv, or pass --python with an interpreter that has pip.",
    );

    let env = tmp.join("buildenv");
    let base = &host.info.base_executable;
    if host.info.has_venv {
        status(quiet, format!("{} has no pip; creating a build environment", base.display()));
        let mut cmd = host.command();
        cmd.args(["-m", "venv"]).arg(&env).current_dir(tmp);
        run_captured(cmd, "creating a build environment").map_err(|e| anyhow!("{e:#}\n\n{no_pip_help}"))?;
    } else {
        bail!("{} has no pip, which building needs.\n{no_pip_help}", base.display());
    }

    let python = Interpreter::from_venv(&env, false)?;
    if !python.info.has_pip {
        bail!("the build environment ended up without pip.\n{no_pip_help}");
    }
    Ok(python)
}

/// Venvs worth checking for pip, most specific first, existing ones only, without duplicates.
fn venv_candidates(active: Option<PathBuf>, project: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let candidates = active.filter(|p| !p.as_os_str().is_empty()).into_iter();
    for venv in candidates.chain([project.join(".venv"), project.join("venv")]) {
        let real = venv.canonicalize().unwrap_or_else(|_| venv.clone());
        if venv.join("pyvenv.cfg").is_file() && !found.iter().any(|f| f.canonicalize().ok().as_ref() == Some(&real)) {
            found.push(venv);
        }
    }
    found
}

fn pip_wheel<'a>(
    python: &Interpreter,
    wheel_dir: &Path,
    targets: impl IntoIterator<Item = &'a OsStr>,
    quiet: bool,
) -> Result<()> {
    let mut cmd = python.command();
    cmd.args(["-m", "pip", "wheel", "--disable-pip-version-check", "--wheel-dir"]).arg(wheel_dir);
    cmd.args(targets);
    if quiet {
        cmd.arg("--quiet");
        return run_captured(cmd, "pip wheel");
    }
    // Let pip's progress through: compiling a dependency can take a while.
    let result = cmd.stdin(Stdio::null()).status().context("could not start Python")?;
    if !result.success() {
        bail!("pip wheel failed ({result}); see pip's output above");
    }
    Ok(())
}

/// Build output and environments, skipped when copying (at the top level only).
const SKIP_AT_ROOT: &[&str] = &["build", "dist", ".venv", "venv", ".tox", ".nox", "node_modules"];

fn copy_project(from: &Path, to: &Path, root: bool) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == "__pycache__"
            || name_str.ends_with(".egg-info")
            || (root && SKIP_AT_ROOT.contains(&name_str.as_ref()))
        {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_project(&src, &dst, false)?;
        } else if kind.is_file() || (kind.is_symlink() && src.is_file()) {
            fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
        // Symlinked directories are skipped so a link loop can't recurse forever.
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

fn wheel_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "whl") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn single_wheel(dir: &Path) -> Result<PathBuf> {
    let mut wheels = wheel_paths(dir)?;
    if wheels.len() != 1 {
        bail!("expected the build to produce one wheel in {}, found {}", dir.display(), wheels.len());
    }
    Ok(wheels.remove(0))
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
        .with_context(|| format!("{} has no usable file name", path.display()))
}

fn safe_name(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect()
}

fn choose_entry(info: &WheelInfo, wanted: Option<&str>) -> Result<Entry> {
    let script = |name: &str, target: &str| Entry {
        kind: EntryKind::ConsoleScript,
        name: name.to_string(),
        target: target.to_string(),
    };
    let module = |package: &str| Entry {
        kind: EntryKind::Module,
        name: package.to_string(),
        target: package.to_string(),
    };

    if let Some(wanted) = wanted {
        if let Some((name, target)) = info.scripts.iter().find(|(n, _)| n == wanted) {
            return Ok(script(name, target));
        }
        if info.runnable_packages.iter().any(|p| p == wanted) {
            return Ok(module(wanted));
        }
        bail!("--entry {wanted:?} matches no console script or runnable package{}", available(info));
    }

    match info.scripts.as_slice() {
        [(name, target)] => return Ok(script(name, target)),
        [] => {}
        many => {
            // A script named after the project is the obvious main one.
            if let Some((name, target)) = many.iter().find(|(n, _)| normalize(n) == normalize(&info.name)) {
                return Ok(script(name, target));
            }
            bail!("the project has several console scripts; pick one with --entry{}", available(info));
        }
    }
    match info.runnable_packages.as_slice() {
        [package] => Ok(module(package)),
        [] => bail!(
            "nothing to run: add a [project.scripts] entry to pyproject.toml, or a __main__.py to your package"
        ),
        _ => bail!("several packages have a __main__.py; pick one with --entry{}", available(info)),
    }
}

fn available(info: &WheelInfo) -> String {
    let mut names: Vec<&str> = info.scripts.iter().map(|(n, _)| n.as_str()).collect();
    names.extend(info.runnable_packages.iter().map(String::as_str));
    if names.is_empty() { String::new() } else { format!(" (available: {})", names.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(scripts: &[(&str, &str)], packages: &[&str]) -> WheelInfo {
        WheelInfo {
            name: "My_App".into(),
            version: "1.0".into(),
            requires_python: None,
            scripts: scripts.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect(),
            runnable_packages: packages.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn single_script_is_picked() {
        let e = choose_entry(&info(&[("app", "app.cli:main")], &[]), None).unwrap();
        assert_eq!((e.kind, e.name.as_str(), e.target.as_str()), (EntryKind::ConsoleScript, "app", "app.cli:main"));
    }

    #[test]
    fn script_named_after_the_project_wins() {
        let i = info(&[("helper", "a:b"), ("my-app", "my_app.cli:main")], &[]);
        assert_eq!(choose_entry(&i, None).unwrap().name, "my-app");
    }

    #[test]
    fn ambiguous_scripts_need_entry() {
        let i = info(&[("one", "a:b"), ("two", "a:c")], &[]);
        let err = choose_entry(&i, None).unwrap_err().to_string();
        assert!(err.contains("--entry") && err.contains("one, two"), "{err}");
        assert_eq!(choose_entry(&i, Some("two")).unwrap().target, "a:c");
    }

    #[test]
    fn falls_back_to_dunder_main_package() {
        let e = choose_entry(&info(&[], &["mytool"]), None).unwrap();
        assert_eq!((e.kind, e.target.as_str()), (EntryKind::Module, "mytool"));
        assert!(choose_entry(&info(&[], &[]), None).is_err());
        assert!(choose_entry(&info(&[], &["a", "b"]), None).is_err());
    }

    #[test]
    fn unknown_entry_lists_choices() {
        let err = choose_entry(&info(&[("app", "a:b")], &["pkg"]), Some("nope")).unwrap_err().to_string();
        assert!(err.contains("app, pkg"), "{err}");
    }

    #[test]
    fn venv_candidates_prefer_the_active_venv_and_skip_non_venvs() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let active = tmp.path().join("active");
        for venv in [&active, &project.join(".venv")] {
            fs::create_dir_all(venv).unwrap();
            fs::write(venv.join("pyvenv.cfg"), "").unwrap();
        }
        // A directory called venv that isn't one.
        fs::create_dir_all(project.join("venv")).unwrap();

        let found = venv_candidates(Some(active.clone()), &project);
        assert_eq!(found, vec![active, project.join(".venv")]);
        // Activating the project's own venv doesn't list it twice.
        assert_eq!(venv_candidates(Some(project.join(".venv")), &project), vec![project.join(".venv")]);
        assert_eq!(venv_candidates(None, &project), vec![project.join(".venv")]);
    }

    #[test]
    fn names_normalize_like_pep_503() {
        assert_eq!(normalize("My_App.Core"), "my-app-core");
        assert_eq!(normalize("a--b"), "a-b");
        assert_eq!(safe_name("my app/1"), "my_app_1");
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(4290), "4.2 KiB");
        assert_eq!(human_size(1_821_191), "1.7 MiB");
    }
}
