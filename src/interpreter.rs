use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{Result, bail};
use pep440_rs::{Version, VersionSpecifiers};
use serde::{Deserialize, Serialize};

use crate::hash::sha256_hex;

const PROBE_SCRIPT: &str = include_str!("py/probe.py");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeInfo {
    pub version: [u64; 3],
    pub platform: String,
    pub has_venv: bool,
    pub has_pip: bool,
    /// The real interpreter behind this one (differs from the program inside a venv).
    pub base_executable: PathBuf,
    /// Install paths of a venv made from this interpreter, relative to the venv directory.
    pub venv_layout: HashMap<String, String>,
    /// This interpreter's own install paths, absolute.
    pub paths: HashMap<String, PathBuf>,
}

impl ProbeInfo {
    pub fn version_string(&self) -> String {
        let [major, minor, micro] = self.version;
        format!("{major}.{minor}.{micro}")
    }
}

#[derive(Debug, Clone)]
pub struct Interpreter {
    pub program: PathBuf,
    /// Arguments that must precede the script, e.g. `-3` for the Windows `py` launcher.
    pub pre_args: Vec<OsString>,
    pub info: ProbeInfo,
}

impl Interpreter {
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.pre_args);
        cmd
    }

    pub fn from_venv(venv: &Path) -> Result<Self> {
        let program = venv_python(venv);
        match probe(&program, &[], true) {
            Ok(info) => Ok(Self { program, pre_args: Vec::new(), info }),
            Err(reason) => bail!("{} is not a usable virtual environment: {reason}", venv.display()),
        }
    }
}

pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// Probes an interpreter, reusing an earlier answer when the interpreter binary is unchanged.
///
/// Starting Python just to ask it questions costs tens of milliseconds, which would otherwise be
/// paid on every run. `has_pip` can change without the binary changing, so callers that care
/// (the build) pass `use_cache = false`.
fn probe(program: &Path, pre_args: &[OsString], use_cache: bool) -> Result<ProbeInfo, String> {
    let cache_file = use_cache.then(|| probe_cache_file(program, pre_args)).flatten();
    if let Some(info) = cache_file.as_ref().and_then(|f| fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(info);
    }
    let info = probe_uncached(program, pre_args)?;
    if let Some(file) = cache_file {
        if let (Some(dir), Ok(json)) = (file.parent(), serde_json::to_vec(&info)) {
            let _ = fs::create_dir_all(dir).and_then(|()| fs::write(&file, json));
        }
    }
    Ok(info)
}

/// Cache key: the program as invoked plus identity of the binary it resolves to. The `py`
/// launcher picks an interpreter at run time, so it is never cached.
fn probe_cache_file(program: &Path, pre_args: &[OsString]) -> Option<PathBuf> {
    if !pre_args.is_empty() {
        return None;
    }
    let real = fs::canonicalize(program).ok()?;
    let meta = fs::metadata(&real).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    let key = format!(
        "{}\0{}\0{}\0{mtime}\0{}\0{}",
        env!("CARGO_PKG_VERSION"),
        program.display(),
        real.display(),
        meta.len(),
        sha256_hex(PROBE_SCRIPT.as_bytes()),
    );
    let dirs = directories::ProjectDirs::from("", "", "pyrs")?;
    Some(dirs.cache_dir().join("probes").join(format!("{}.json", &sha256_hex(key.as_bytes())[..32])))
}

/// Runs the probe script in isolated mode (`-I`) so nothing from the caller's cwd or env leaks in.
fn probe_uncached(program: &Path, pre_args: &[OsString]) -> Result<ProbeInfo, String> {
    let output = Command::new(program)
        .args(pre_args)
        .args(["-I", "-c", PROBE_SCRIPT])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run it ({e})"))?;
    if !output.status.success() {
        // The Windows Store `python.exe` stub lands here: it exits non-zero without running anything.
        return Err(format!("it did not run as a Python interpreter ({})", output.status));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("unexpected probe output ({e})"))
}

/// Checks a Python version against a wheel's `Requires-Python` string.
pub fn check_requires_python(spec: Option<&str>, info: &ProbeInfo) -> Result<(), String> {
    let Some(spec) = spec else { return Ok(()) };
    let specifiers = VersionSpecifiers::from_str(spec)
        .map_err(|e| format!("bundle has an unreadable Requires-Python {spec:?} ({e})"))?;
    let version = Version::new(info.version);
    if specifiers.contains(&version) {
        Ok(())
    } else {
        Err(format!("Python {} does not satisfy the project's requirement {spec}", info.version_string()))
    }
}

struct Candidate {
    label: String,
    program: PathBuf,
    pre_args: Vec<OsString>,
}

fn candidates() -> Vec<Candidate> {
    let names: &[(&str, &[&str])] = if cfg!(windows) {
        &[("py", &["-3"]), ("python", &[]), ("python3", &[])]
    } else {
        &[("python3", &[]), ("python", &[])]
    };
    let mut found: Vec<Candidate> = Vec::new();
    for (name, args) in names {
        let Ok(program) = which::which(name) else { continue };
        if found.iter().any(|c| c.program == program && c.pre_args.len() == args.len()) {
            continue;
        }
        found.push(Candidate {
            label: if args.is_empty() { name.to_string() } else { format!("{name} {}", args.join(" ")) },
            program,
            pre_args: args.iter().map(OsString::from).collect(),
        });
    }
    found
}

/// Finds a Python that `accept` approves of.
///
/// An explicit choice (`--python` or `PYRS_PYTHON`) is used as given and never falls back to
/// auto-discovery, so a typo is reported instead of silently running a different Python.
pub fn discover(
    explicit: Option<&str>,
    use_cache: bool,
    accept: impl Fn(&ProbeInfo) -> Result<(), String>,
) -> Result<Interpreter> {
    let explicit = explicit.map(str::to_owned).or_else(|| std::env::var("PYRS_PYTHON").ok());
    if let Some(choice) = explicit.filter(|c| !c.is_empty()) {
        let program = if Path::new(&choice).components().count() > 1 || Path::new(&choice).is_file() {
            std::path::absolute(&choice).map_err(|e| anyhow::anyhow!("resolving {choice:?}: {e}"))?
        } else {
            which::which(&choice).map_err(|_| anyhow::anyhow!("Python {choice:?} was not found"))?
        };
        let info = probe(&program, &[], use_cache).map_err(|e| anyhow::anyhow!("{}: {e}", program.display()))?;
        accept(&info).map_err(|e| anyhow::anyhow!("{}: {e}", program.display()))?;
        return Ok(Interpreter { program, pre_args: Vec::new(), info });
    }

    let mut rejected = Vec::new();
    for candidate in candidates() {
        match probe(&candidate.program, &candidate.pre_args, use_cache) {
            Err(reason) => rejected.push(format!("{} ({}): {reason}", candidate.label, candidate.program.display())),
            Ok(info) => match accept(&info) {
                Ok(()) => {
                    return Ok(Interpreter { program: candidate.program, pre_args: candidate.pre_args, info });
                }
                Err(reason) => rejected.push(format!("{} ({}): {reason}", candidate.label, candidate.program.display())),
            },
        }
    }

    if rejected.is_empty() {
        bail!("Python was not found. {}", install_hint());
    }
    bail!(
        "no suitable Python found:\n  - {}\n{}",
        rejected.join("\n  - "),
        install_hint()
    )
}

fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Install Python from https://www.python.org/downloads/, or point pyrs at one with --python."
    } else if cfg!(target_os = "macos") {
        "Install Python (for example `brew install python`), or point pyrs at one with --python."
    } else {
        "Install Python (for example `sudo apt install python3`), or point pyrs at one with --python."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: [u64; 3]) -> ProbeInfo {
        ProbeInfo {
            version,
            platform: "linux-x86_64".into(),
            has_venv: true,
            has_pip: false,
            base_executable: "/usr/bin/python3".into(),
            venv_layout: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    #[test]
    fn requires_python_is_enforced() {
        assert!(check_requires_python(None, &info([3, 8, 0])).is_ok());
        assert!(check_requires_python(Some(">=3.10"), &info([3, 13, 5])).is_ok());
        assert!(check_requires_python(Some(">=3.10,<3.13"), &info([3, 13, 5])).is_err());
        let err = check_requires_python(Some(">=3.10"), &info([3, 9, 2])).unwrap_err();
        assert!(err.contains("3.9.2") && err.contains(">=3.10"), "{err}");
        assert!(check_requires_python(Some("not a specifier"), &info([3, 13, 5])).is_err());
    }

    #[test]
    fn probe_output_parses() {
        let json = r#"{"version":[3,13,5],"platform":"linux-x86_64","has_venv":true,"has_pip":false,
            "base_executable":"/usr/bin/python3","venv_layout":{"purelib":"lib/python3.13/site-packages"},
            "paths":{"purelib":"/usr/lib/python3/dist-packages"}}"#;
        let parsed: ProbeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.version_string(), "3.13.5");
        assert!(!parsed.has_pip);
    }

    #[test]
    fn venv_python_path_matches_platform() {
        let p = venv_python(Path::new("/tmp/v"));
        if cfg!(windows) {
            assert!(p.ends_with("Scripts/python.exe") || p.ends_with("Scripts\\python.exe"));
        } else {
            assert_eq!(p, PathBuf::from("/tmp/v/bin/python"));
        }
    }
}
