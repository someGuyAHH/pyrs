use std::fs;
use std::process::ExitStatus;

use anyhow::{Context, Result, anyhow, bail};

use crate::bundle::Bundle;
use crate::cli::{EnvKind, RunArgs};
use crate::envs::{self, EnvMode, Workspace};
use crate::interpreter::{self, Interpreter, ProbeInfo, check_requires_python};
use crate::manifest::{EntryKind, Manifest};
use crate::ui::{status, warn};
use crate::wheel::WheelFilename;

const LAUNCHER: &str = include_str!("../py/launch.py");

pub fn run(args: RunArgs, quiet: bool) -> Result<i32> {
    let mode = resolve_mode(&args)?;
    let mut bundle = Bundle::open(&args.bundle)?;
    let manifest = bundle.manifest.clone();
    let requires = manifest.requires_python.as_deref();

    let host = match &mode {
        EnvMode::Venv(dir) => {
            let py = Interpreter::from_venv(dir, true)?;
            check_requires_python(requires, &py.info).map_err(|e| anyhow!("{}: {e}", dir.display()))?;
            py
        }
        _ => interpreter::discover(args.python.as_deref(), true, |info: &ProbeInfo| {
            check_requires_python(requires, info)
        })?,
    };
    check_compatible(&manifest, &host.info)?;

    let ws = Workspace::create(&mode, !args.no_cache, &bundle.digest, &host)?;
    let label = format!("{} {}", manifest.name, manifest.version);

    let (python, env_vars) = match &mode {
        EnvMode::Temp => {
            let venv = ws.root.join("venv");
            if !ws.ready {
                status(quiet, format!("setting up {label}"));
                envs::create_venv(&host, &venv)?;
                envs::install_all(&mut bundle, &envs::temp_venv_layout(&venv, &host.info)?)?;
                ws.mark_ready()?;
            }
            (envs::venv_interpreter(&host, &venv), envs::venv_env(&venv))
        }
        EnvMode::Global => {
            let site = ws.root.join("site");
            if !ws.ready {
                status(quiet, format!("setting up {label}"));
                envs::install_all(&mut bundle, &envs::target_layout(&site))?;
                ws.mark_ready()?;
            }
            (host, vec![envs::prepend_pythonpath(&site)])
        }
        EnvMode::Venv(dir) => {
            let layout = envs::user_venv_layout(dir, &host)?;
            if envs::installed_version(&layout, &manifest.name).as_deref() != Some(manifest.version.as_str()) {
                if args.no_install {
                    warn(format!("{label} is not installed in {}; running anyway (--no-install)", dir.display()));
                } else {
                    status(quiet, format!("installing {label} into {}", dir.display()));
                    envs::install_all(&mut bundle, &layout)?;
                }
            }
            (host, envs::venv_env(dir))
        }
    };

    let launcher_dir = ws.root.join("launcher");
    fs::create_dir_all(&launcher_dir).context("creating the launcher directory")?;
    // A directory of its own, so the only thing next to the launcher on sys.path[0] is the launcher.
    let launcher = launcher_dir.join("_pyrs_launch.py");
    fs::write(&launcher, LAUNCHER).context("writing the launcher")?;

    let kind = match manifest.entry.kind {
        EntryKind::ConsoleScript => "console_script",
        EntryKind::Module => "module",
    };
    let mut cmd = python.command();
    cmd.arg(&launcher).arg(kind).arg(&manifest.entry.name).arg(&manifest.entry.target).args(&args.args);
    for (key, value) in &env_vars {
        cmd.env(key, value);
    }

    // The terminal sends Ctrl-C to the whole foreground group, child included. pyrs must
    // outlive it so the workspace is removed once the child has exited.
    let _ = ctrlc::set_handler(|| {});
    let exit = cmd.status().with_context(|| format!("starting {}", python.program.display()))?;
    Ok(exit_code(exit))
}

fn resolve_mode(args: &RunArgs) -> Result<EnvMode> {
    Ok(match (args.env, &args.venv) {
        // Subprocesses run from other directories, so the path must not be relative.
        (None | Some(EnvKind::Venv), Some(dir)) => {
            EnvMode::Venv(std::path::absolute(dir).with_context(|| format!("resolving {}", dir.display()))?)
        }
        (Some(EnvKind::Venv), None) => bail!("--env venv needs --venv <DIR>"),
        (Some(_), Some(_)) => bail!("--venv cannot be combined with --env temp or --env global"),
        (None | Some(EnvKind::Temp), None) => EnvMode::Temp,
        (Some(EnvKind::Global), None) => EnvMode::Global,
    })
}

/// Compiled wheels only load on the platform and Python they were built for. pip used to refuse
/// them; unpacking them anyway would only fail later with a confusing ImportError.
fn check_compatible(manifest: &Manifest, host: &ProbeInfo) -> Result<()> {
    if manifest.pure {
        return Ok(());
    }
    let built = &manifest.built_with;
    let rebuild = "rebuild the bundle on a machine that matches this one";
    if os_arch(&built.platform) != os_arch(&host.platform) {
        bail!(
            "this bundle contains compiled packages built for {}, but this machine is {}; {rebuild}",
            built.platform,
            host.platform
        );
    }
    let [major, minor, _] = host.version;
    for wheel in &manifest.wheels {
        let parsed = WheelFilename::parse(&wheel.file)?;
        if !parsed.abi_compatible(major, minor) {
            bail!(
                "{} was compiled for a different Python (bundle built with {}, this is {}); {rebuild}",
                wheel.file,
                built.python,
                host.version_string()
            );
        }
    }
    Ok(())
}

/// `macosx-14.0-arm64` → `macosx-arm64`: the macOS version is a minimum, not an identity.
fn os_arch(platform: &str) -> String {
    let parts: Vec<&str> = platform.split('-').collect();
    match parts.as_slice() {
        ["macosx", _version, arch] => format!("macosx-{arch}"),
        _ => platform.to_string(),
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(env: Option<EnvKind>, venv: Option<&str>) -> RunArgs {
        RunArgs {
            bundle: PathBuf::from("x.pyrs"),
            env,
            venv: venv.map(PathBuf::from),
            python: None,
            no_cache: false,
            no_install: false,
            args: Vec::<OsString>::new(),
        }
    }

    #[test]
    fn mode_resolution() {
        assert_eq!(resolve_mode(&args(None, None)).unwrap(), EnvMode::Temp);
        assert_eq!(resolve_mode(&args(Some(EnvKind::Global), None)).unwrap(), EnvMode::Global);
        let abs = std::path::absolute("v").unwrap();
        assert_eq!(resolve_mode(&args(None, Some("v"))).unwrap(), EnvMode::Venv(abs.clone()));
        assert_eq!(resolve_mode(&args(Some(EnvKind::Venv), Some("v"))).unwrap(), EnvMode::Venv(abs));
        assert!(resolve_mode(&args(Some(EnvKind::Venv), None)).is_err());
        assert!(resolve_mode(&args(Some(EnvKind::Global), Some("v"))).is_err());
    }

    #[test]
    fn macos_versions_do_not_count_as_platform_differences() {
        assert_eq!(os_arch("macosx-14.0-arm64"), os_arch("macosx-11.0-arm64"));
        assert_ne!(os_arch("macosx-14.0-arm64"), os_arch("macosx-14.0-x86_64"));
        assert_eq!(os_arch("linux-x86_64"), "linux-x86_64");
    }
}
