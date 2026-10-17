use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use tempfile::TempDir;

use crate::bundle::Bundle;
use crate::installer::{Layout, install_wheel};
use crate::interpreter::{Interpreter, ProbeInfo, venv_python};
use crate::wheel::normalize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvMode {
    Temp,
    Global,
    Venv(PathBuf),
}

impl EnvMode {
    fn cache_label(&self) -> Option<&'static str> {
        match self {
            EnvMode::Temp => Some("temp"),
            EnvMode::Global => Some("global"),
            EnvMode::Venv(_) => None,
        }
    }
}

/// A directory holding whatever environment was built for a run.
///
/// Normally it is a temp dir that disappears on drop. With `--cache` it lives in the user
/// cache, keyed by bundle content, and `ready` says whether an earlier run finished building it.
pub struct Workspace {
    pub root: PathBuf,
    pub ready: bool,
    _temp: Option<TempDir>,
}

impl Workspace {
    pub fn create(mode: &EnvMode, cache: bool, digest: &str, python: &Interpreter) -> Result<Self> {
        if let (true, Some(label)) = (cache, mode.cache_label()) {
            let dirs = ProjectDirs::from("", "", "pyrs")
                .ok_or_else(|| anyhow!("cannot work out a cache directory for this user"))?;
            let [major, minor, _] = python.info.version;
            let key = format!("{}-{label}-py{major}.{minor}", &digest[..16]);
            let root = dirs.cache_dir().join("envs").join(key);
            let ready = root.join(READY_MARKER).is_file();
            if !ready && root.exists() {
                // A previous run died half way through; start over.
                fs::remove_dir_all(&root).with_context(|| format!("clearing {}", root.display()))?;
            }
            fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
            return Ok(Self { root, ready, _temp: None });
        }
        let temp = tempfile::Builder::new().prefix("pyrs-").tempdir().context("creating a temporary workspace")?;
        Ok(Self { root: temp.path().to_path_buf(), ready: false, _temp: Some(temp) })
    }

    pub fn mark_ready(&self) -> Result<()> {
        fs::write(self.root.join(READY_MARKER), b"").context("marking the cached environment as ready")
    }
}

const READY_MARKER: &str = ".pyrs-ready";

/// Creates a virtual environment without pip, which pyrs doesn't need.
///
/// On Unix a venv is only a `pyvenv.cfg` and a symlink to the base interpreter, so pyrs writes
/// it directly instead of paying for a Python start-up. Windows venvs need a launcher .exe, so
/// there it asks Python's own `venv` module.
pub fn create_venv(host: &Interpreter, venv: &Path) -> Result<()> {
    if cfg!(unix) {
        create_venv_native(&host.info, venv)
    } else {
        let mut cmd = host.command();
        cmd.args(["-m", "venv", "--without-pip"]).arg(venv);
        if let Some(parent) = venv.parent() {
            cmd.current_dir(parent);
        }
        run_captured(cmd, "creating the virtual environment")
    }
}

fn create_venv_native(info: &ProbeInfo, venv: &Path) -> Result<()> {
    let base = &info.base_executable;
    let home = base.parent().ok_or_else(|| anyhow!("{} has no parent directory", base.display()))?;
    let bin = venv.join("bin");
    fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;

    let [major, minor, micro] = info.version;
    let cfg = format!(
        "home = {}\ninclude-system-site-packages = false\nversion = {major}.{minor}.{micro}\n\
         executable = {}\ncommand = pyrs\n",
        home.display(),
        base.display()
    );
    fs::write(venv.join("pyvenv.cfg"), cfg).context("writing pyvenv.cfg")?;

    #[cfg(unix)]
    for name in ["python".to_string(), "python3".to_string(), format!("python{major}.{minor}")] {
        std::os::unix::fs::symlink(base, bin.join(&name))
            .with_context(|| format!("linking {} into the venv", base.display()))?;
    }

    let purelib = venv.join(layout_path(info, "purelib")?);
    fs::create_dir_all(&purelib).with_context(|| format!("creating {}", purelib.display()))
}

fn layout_path<'a>(info: &'a ProbeInfo, key: &str) -> Result<&'a str> {
    info.venv_layout
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("the interpreter did not report its venv {key} path"))
}

/// The interpreter inside a venv made from `host`. It is the same Python, so no probe is needed.
pub fn venv_interpreter(host: &Interpreter, venv: &Path) -> Interpreter {
    Interpreter { program: venv_python(venv), pre_args: Vec::new(), info: host.info.clone() }
}

/// Layout of a venv that pyrs created. Scripts are skipped: pyrs launches the entry point itself.
pub fn temp_venv_layout(venv: &Path, info: &ProbeInfo) -> Result<Layout> {
    Ok(Layout {
        root: venv.to_path_buf(),
        purelib: venv.join(layout_path(info, "purelib")?),
        platlib: venv.join(layout_path(info, "platlib")?),
        data: venv.join(layout_path(info, "data")?),
        scripts: None,
        headers: None,
        python: None,
    })
}

/// Layout of a user's venv, from its own interpreter. Scripts are installed, as pip would.
pub fn user_venv_layout(venv: &Path, py: &Interpreter) -> Result<Layout> {
    let path = |key: &str| {
        py.info.paths.get(key).cloned().ok_or_else(|| anyhow!("the venv's Python did not report its {key} path"))
    };
    Ok(Layout {
        root: venv.to_path_buf(),
        purelib: path("purelib")?,
        platlib: path("platlib")?,
        data: path("data")?,
        scripts: Some(path("scripts")?),
        headers: None,
        python: Some(py.program.clone()),
    })
}

/// A flat directory for `PYTHONPATH`, like `pip install --target`.
pub fn target_layout(site: &Path) -> Layout {
    Layout {
        root: site.to_path_buf(),
        purelib: site.to_path_buf(),
        platlib: site.to_path_buf(),
        data: site.join("data"),
        scripts: None,
        headers: None,
        python: None,
    }
}

/// Unpacks every wheel of the bundle into `layout`, straight from the bundle file.
pub fn install_all(bundle: &mut Bundle, layout: &Layout) -> Result<()> {
    let files: Vec<String> = bundle.manifest.wheels.iter().map(|w| w.file.clone()).collect();
    for file in files {
        let mut wheel = bundle.open_wheel(&file)?;
        install_wheel(&mut wheel, &file, layout).with_context(|| format!("installing {file}"))?;
    }
    Ok(())
}

/// The installed version of `name` in `layout`, found by its `.dist-info` directory.
pub fn installed_version(layout: &Layout, name: &str) -> Option<String> {
    let wanted = normalize(name);
    for lib in [&layout.purelib, &layout.platlib] {
        for entry in fs::read_dir(lib).ok()?.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some((dist, version)) = file_name.strip_suffix(".dist-info").and_then(|s| s.rsplit_once('-')) else {
                continue;
            };
            if normalize(dist) == wanted {
                return Some(version.to_string());
            }
        }
    }
    None
}

/// Environment variables that make a child process behave as if the venv were activated.
pub fn venv_env(venv: &Path) -> Vec<(OsString, OsString)> {
    let bin = if cfg!(windows) { venv.join("Scripts") } else { venv.join("bin") };
    let mut paths = vec![bin];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let mut vars = vec![(OsString::from("VIRTUAL_ENV"), venv.as_os_str().to_owned())];
    if let Ok(joined) = std::env::join_paths(paths) {
        vars.push((OsString::from("PATH"), joined));
    }
    vars
}

/// `PYTHONPATH` with `dir` in front of whatever the caller already had.
pub fn prepend_pythonpath(dir: &Path) -> (OsString, OsString) {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PYTHONPATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(paths).unwrap_or_else(|_| dir.as_os_str().to_owned());
    (OsString::from("PYTHONPATH"), joined)
}

/// Runs a setup command with output captured, showing it only if the command fails.
pub fn run_captured(mut cmd: Command, what: &str) -> Result<()> {
    let output = cmd.stdin(Stdio::null()).output().with_context(|| format!("{what}: could not start Python"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut detail = String::new();
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        for line in &lines[lines.len().saturating_sub(20)..] {
            detail.push_str("    ");
            detail.push_str(line);
            detail.push('\n');
        }
    }
    bail!("{what} failed ({}):\n{}", output.status, detail.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn venv_env_puts_the_venv_first_on_path() {
        let vars = venv_env(Path::new("/tmp/v"));
        let virtual_env = vars.iter().find(|(k, _)| k == "VIRTUAL_ENV").unwrap();
        assert_eq!(virtual_env.1, "/tmp/v");
        let path = vars.iter().find(|(k, _)| k == "PATH").unwrap();
        let first = std::env::split_paths(&path.1).next().unwrap();
        assert!(first.starts_with("/tmp/v"));
    }

    #[test]
    fn installed_version_reads_dist_info_names() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = target_layout(tmp.path());
        assert_eq!(installed_version(&layout, "My-App"), None);
        fs::create_dir(tmp.path().join("my_app-1.2.0.dist-info")).unwrap();
        fs::create_dir(tmp.path().join("other-9.dist-info")).unwrap();
        assert_eq!(installed_version(&layout, "My-App").as_deref(), Some("1.2.0"));
    }

    #[cfg(unix)]
    #[test]
    fn native_venv_has_config_and_interpreter_links() {
        let tmp = tempfile::tempdir().unwrap();
        let venv = tmp.path().join("venv");
        let info = ProbeInfo {
            version: [3, 13, 5],
            platform: "linux-x86_64".into(),
            has_venv: true,
            has_pip: false,
            base_executable: "/usr/bin/python3".into(),
            venv_layout: HashMap::from([("purelib".to_string(), "lib/python3.13/site-packages".to_string())]),
            paths: HashMap::new(),
        };
        create_venv_native(&info, &venv).unwrap();
        let cfg = fs::read_to_string(venv.join("pyvenv.cfg")).unwrap();
        assert!(cfg.contains("home = /usr/bin\n") && cfg.contains("version = 3.13.5\n"), "{cfg}");
        assert_eq!(fs::read_link(venv.join("bin/python")).unwrap(), PathBuf::from("/usr/bin/python3"));
        assert!(venv.join("bin/python3.13").is_symlink());
        assert!(venv.join("lib/python3.13/site-packages").is_dir());
    }
}
