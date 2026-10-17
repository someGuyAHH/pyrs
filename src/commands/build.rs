use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::bundle;
use crate::cli::BuildArgs;
use crate::envs::run_captured;
use crate::interpreter::{self, Interpreter};
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

    let python = interpreter::discover(args.python.as_deref(), false, |info| {
        if info.has_pip {
            Ok(())
        } else {
            Err("it has no pip module, which building needs (pick another Python with --python)".into())
        }
    })?;

    let tmp = tempfile::Builder::new().prefix("pyrs-build-").tempdir().context("creating a build directory")?;
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
            "{} wheels, {:.1} MiB, {}",
            manifest.wheels.len(),
            size as f64 / (1024.0 * 1024.0),
            if pure { "pure Python" } else { "has compiled packages (tied to this platform)" }
        ),
    );
    println!("{}", out.display());
    Ok(())
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
    fn names_normalize_like_pep_503() {
        assert_eq!(normalize("My_App.Core"), "my-app-core");
        assert_eq!(normalize("a--b"), "a-b");
        assert_eq!(safe_name("my app/1"), "my_app_1");
    }
}
