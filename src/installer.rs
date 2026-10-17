//! A small wheel installer (the "install" half of the wheel spec), so runs don't need pip.
//!
//! Bundles are resolved at build time, so there's no dependency resolution here: every wheel
//! in the bundle is unpacked into the target layout.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use zip::ZipArchive;

use crate::wheel::{WheelFilename, normalize, parse_scripts};

/// Where the parts of a wheel go.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Nothing is written or removed outside this directory.
    pub root: PathBuf,
    pub purelib: PathBuf,
    pub platlib: PathBuf,
    pub data: PathBuf,
    /// `None` skips scripts entirely (pyrs launches entry points itself).
    pub scripts: Option<PathBuf>,
    pub headers: Option<PathBuf>,
    /// Interpreter for script shebangs; scripts are only written when this is set.
    pub python: Option<PathBuf>,
}

pub fn install_wheel<R: Read + Seek>(zip: &mut ZipArchive<R>, file_name: &str, layout: &Layout) -> Result<()> {
    let parsed = WheelFilename::parse(file_name)?;
    let dist_info = find_dist_info(zip, file_name)?;
    let data_dir = format!("{}.data", dist_info.trim_end_matches(".dist-info"));

    let wheel_meta = read_text(zip, &format!("{dist_info}/WHEEL"))?;
    let purelib_root = wheel_meta
        .lines()
        .filter_map(|l| l.split_once(':'))
        .any(|(k, v)| k.trim() == "Root-Is-Purelib" && v.trim().eq_ignore_ascii_case("true"));
    let lib = if purelib_root { &layout.purelib } else { &layout.platlib };

    uninstall(&parsed.distribution, layout)?;

    let mut installed = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let rel = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("{file_name} contains an unsafe path: {:?}", entry.name()))?;
        let mut parts = rel.components();
        let first = parts.next().map(|c| c.as_os_str().to_string_lossy().into_owned());

        let mut is_script = false;
        let dest = if first.as_deref() == Some(data_dir.as_str()) {
            let key = parts.next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
            let rest = parts.as_path();
            let base = match key.as_str() {
                "purelib" => Some(&layout.purelib),
                "platlib" => Some(&layout.platlib),
                "data" => Some(&layout.data),
                "scripts" => {
                    is_script = true;
                    layout.scripts.as_ref().filter(|_| layout.python.is_some())
                }
                "headers" => layout.headers.as_ref(),
                other => bail!("{file_name} uses an unknown .data directory {other:?}"),
            };
            match base {
                Some(base) => base.join(rest),
                None => continue,
            }
        } else {
            lib.join(&rel)
        };

        let executable = entry.unix_mode().is_some_and(|m| m & 0o111 != 0);
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut contents).with_context(|| format!("reading {} from {file_name}", rel.display()))?;
        if is_script {
            contents = rewrite_shebang(contents, layout.python.as_deref());
        }
        write_file(&dest, &contents, executable || is_script)?;
        installed.push(dest);
    }

    if let (Some(scripts), Some(python)) = (&layout.scripts, &layout.python) {
        if let Ok(text) = read_text(zip, &format!("{dist_info}/entry_points.txt")) {
            for (name, target) in parse_scripts(&text) {
                if cfg!(windows) {
                    // Windows needs .exe launchers; pyrs runs entry points itself instead.
                    continue;
                }
                let dest = scripts.join(&name);
                write_file(&dest, script_source(python, &target)?.as_bytes(), true)?;
                installed.push(dest);
            }
        }
    }

    let dist_info_dir = lib.join(&dist_info);
    let installer = dist_info_dir.join("INSTALLER");
    write_file(&installer, b"pyrs\n", false)?;
    installed.push(installer);
    write_record(&dist_info_dir, lib, &installed)
}

fn find_dist_info<R: Read + Seek>(zip: &ZipArchive<R>, file_name: &str) -> Result<String> {
    zip.file_names()
        .filter_map(|n| n.split_once('/'))
        .find(|(top, rest)| top.ends_with(".dist-info") && *rest == "WHEEL")
        .map(|(top, _)| top.to_string())
        .ok_or_else(|| anyhow!("{file_name} has no .dist-info/WHEEL"))
}

fn read_text<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Result<String> {
    let mut text = String::new();
    zip.by_name(name)?.read_to_string(&mut text)?;
    Ok(text)
}

fn write_file(dest: &Path, contents: &[u8], executable: bool) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    // Remove first: overwriting a hard-linked or read-only file in place could fail or leak.
    let _ = fs::remove_file(dest);
    let mut out = File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    out.write_all(contents)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// Wheel scripts start with `#!python`, which the installer points at the real interpreter.
fn rewrite_shebang(contents: Vec<u8>, python: Option<&Path>) -> Vec<u8> {
    let Some(python) = python else { return contents };
    if !(contents.starts_with(b"#!python") || contents.starts_with(b"#!pythonw")) {
        return contents;
    }
    let line_end = contents.iter().position(|&b| b == b'\n').unwrap_or(contents.len());
    let mut out = format!("#!{}", python.display()).into_bytes();
    out.extend_from_slice(&contents[line_end..]);
    out
}

fn script_source(python: &Path, target: &str) -> Result<String> {
    let (module, attr) = target
        .split_once(':')
        .ok_or_else(|| anyhow!("entry point {target:?} is not in module:function form"))?;
    let top = attr.split('.').next().unwrap_or(attr);
    Ok(format!(
        "#!{}\nimport sys\nfrom {module} import {top}\nif __name__ == \"__main__\":\n    sys.exit({attr}())\n",
        python.display()
    ))
}

/// Writes RECORD with paths relative to `lib`. Hashes are left empty, which the spec allows for
/// files an installer generated; it's only used to uninstall.
fn write_record(dist_info: &Path, lib: &Path, installed: &[PathBuf]) -> Result<()> {
    let record = dist_info.join("RECORD");
    let mut text = String::new();
    for path in installed.iter().chain(std::iter::once(&record)) {
        let rel = relative(path, lib).to_string_lossy().replace('\\', "/");
        if rel.contains([',', '"']) {
            text.push_str(&format!("\"{}\",,\n", rel.replace('"', "\"\"")));
        } else {
            text.push_str(&format!("{rel},,\n"));
        }
    }
    fs::write(&record, text).with_context(|| format!("writing {}", record.display()))
}

/// Removes any installed version of `distribution` (by its RECORD), like `pip uninstall`.
pub fn uninstall(distribution: &str, layout: &Layout) -> Result<()> {
    let wanted = normalize(distribution);
    let libs: BTreeSet<&PathBuf> = [&layout.purelib, &layout.platlib].into_iter().collect();
    for lib in libs {
        let Ok(entries) = fs::read_dir(lib) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".dist-info") else { continue };
            let Some((dist, _version)) = stem.rsplit_once('-') else { continue };
            if normalize(dist) != wanted {
                continue;
            }
            let dist_info = entry.path();
            let mut parents = BTreeSet::new();
            if let Ok(record) = fs::read_to_string(dist_info.join("RECORD")) {
                for line in record.lines() {
                    let Some(path) = record_path(line) else { continue };
                    let full = lexical(&lib.join(path));
                    if !full.starts_with(&layout.root) {
                        continue;
                    }
                    if fs::remove_file(&full).is_ok() {
                        remove_pycs(&full);
                        if let Some(parent) = full.parent() {
                            parents.insert(parent.to_path_buf());
                        }
                    }
                }
            }
            fs::remove_dir_all(&dist_info).with_context(|| format!("removing {}", dist_info.display()))?;
            // Deepest first, so emptied packages disappear from the bottom up.
            for dir in parents.iter().rev() {
                let mut dir = dir.as_path();
                while dir.starts_with(lib) && dir != lib.as_path() && fs::remove_dir(dir).is_ok() {
                    dir = dir.parent().unwrap_or(lib);
                }
            }
        }
    }
    Ok(())
}

fn record_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix('"') {
        let mut path = String::new();
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    path.push('"');
                } else {
                    return Some(path);
                }
            } else {
                path.push(c);
            }
        }
        None
    } else {
        line.split(',').next().filter(|p| !p.is_empty()).map(str::to_owned)
    }
}

fn remove_pycs(py_file: &Path) {
    let (Some(dir), Some(stem)) = (py_file.parent(), py_file.file_stem()) else { return };
    if py_file.extension().is_none_or(|e| e != "py") {
        return;
    }
    let cache = dir.join("__pycache__");
    let prefix = format!("{}.", stem.to_string_lossy());
    if let Ok(entries) = fs::read_dir(&cache) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(".pyc") {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&cache);
    }
}

/// Resolves `.` and `..` without touching the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` relative to `base`; both absolute.
fn relative(path: &Path, base: &Path) -> PathBuf {
    let path: Vec<_> = path.components().collect();
    let base: Vec<_> = base.components().collect();
    let common = path.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..base.len() {
        out.push("..");
    }
    for part in &path[common..] {
        out.push(part);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::tests::wheel_bytes;
    use std::io::Cursor;

    const WHEEL: &str = "Wheel-Version: 1.0\nRoot-Is-Purelib: true\n";

    fn layout(root: &Path) -> Layout {
        let lib = root.join("lib/site-packages");
        Layout {
            root: root.to_path_buf(),
            purelib: lib.clone(),
            platlib: lib,
            data: root.to_path_buf(),
            scripts: Some(root.join("bin")),
            headers: None,
            python: Some(root.join("bin/python")),
        }
    }

    fn install(root: &Path, file: &str, entries: &[(&str, &str)]) {
        let mut zip = ZipArchive::new(Cursor::new(wheel_bytes(entries))).unwrap();
        install_wheel(&mut zip, file, &layout(root)).unwrap();
    }

    fn v1() -> Vec<(&'static str, &'static str)> {
        vec![
            ("demo/__init__.py", "VERSION = 1\n"),
            ("demo/old_module.py", "\n"),
            ("demo-1.0.dist-info/WHEEL", WHEEL),
            ("demo-1.0.dist-info/METADATA", "Name: demo\nVersion: 1.0\n"),
            ("demo-1.0.dist-info/entry_points.txt", "[console_scripts]\ndemo = demo.cli:main\n"),
            ("demo-1.0.data/scripts/tool", "#!python\nprint('hi')\n"),
            ("demo-1.0.data/data/share/demo.txt", "shared\n"),
        ]
    }

    #[test]
    fn installs_lib_data_and_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install(root, "demo-1.0-py3-none-any.whl", &v1());

        let lib = root.join("lib/site-packages");
        assert_eq!(fs::read_to_string(lib.join("demo/__init__.py")).unwrap(), "VERSION = 1\n");
        assert_eq!(fs::read_to_string(root.join("share/demo.txt")).unwrap(), "shared\n");
        assert_eq!(fs::read_to_string(lib.join("demo-1.0.dist-info/INSTALLER")).unwrap(), "pyrs\n");

        let python = root.join("bin/python");
        let tool = fs::read_to_string(root.join("bin/tool")).unwrap();
        assert_eq!(tool, format!("#!{}\nprint('hi')\n", python.display()));
        if cfg!(unix) {
            let script = fs::read_to_string(root.join("bin/demo")).unwrap();
            assert!(script.starts_with(&format!("#!{}\n", python.display())), "{script}");
            assert!(script.contains("from demo.cli import main"), "{script}");
        }

        let record = fs::read_to_string(lib.join("demo-1.0.dist-info/RECORD")).unwrap();
        assert!(record.contains("demo/__init__.py,,"), "{record}");
        assert!(record.contains("../../bin/tool,,"), "{record}");
        assert!(record.contains("demo-1.0.dist-info/RECORD,,"), "{record}");
    }

    #[test]
    fn upgrading_removes_the_old_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        install(root, "demo-1.0-py3-none-any.whl", &v1());
        let lib = root.join("lib/site-packages");
        fs::create_dir_all(lib.join("demo/__pycache__")).unwrap();
        fs::write(lib.join("demo/__pycache__/old_module.cpython-313.pyc"), "").unwrap();

        install(
            root,
            "demo-2.0-py3-none-any.whl",
            &[
                ("demo/__init__.py", "VERSION = 2\n"),
                ("demo-2.0.dist-info/WHEEL", WHEEL),
                ("demo-2.0.dist-info/METADATA", "Name: demo\nVersion: 2.0\n"),
            ],
        );
        assert_eq!(fs::read_to_string(lib.join("demo/__init__.py")).unwrap(), "VERSION = 2\n");
        assert!(!lib.join("demo/old_module.py").exists());
        assert!(!lib.join("demo/__pycache__/old_module.cpython-313.pyc").exists());
        assert!(!lib.join("demo-1.0.dist-info").exists());
        assert!(!root.join("bin/tool").exists(), "script from 1.0 should be gone");
        assert!(!root.join("share/demo.txt").exists());
    }

    #[test]
    fn uninstall_never_leaves_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("precious.txt");
        fs::write(&outside, "keep me").unwrap();
        let root = tmp.path().join("env");
        let lib = root.join("lib/site-packages");
        fs::create_dir_all(lib.join("evil-1.0.dist-info")).unwrap();
        fs::write(lib.join("evil-1.0.dist-info/RECORD"), "../../../precious.txt,,\n").unwrap();

        uninstall("evil", &layout(&root)).unwrap();
        assert!(outside.exists());
        assert!(!lib.join("evil-1.0.dist-info").exists());
    }

    #[test]
    fn rejects_unsafe_entry_names() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = wheel_bytes(&[("../escape.py", ""), ("x-1.dist-info/WHEEL", WHEEL)]);
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        let err = install_wheel(&mut zip, "x-1-py3-none-any.whl", &layout(tmp.path())).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "{err}");
        assert!(!tmp.path().parent().unwrap().join("escape.py").exists());
    }

    #[test]
    fn record_paths_and_helpers() {
        assert_eq!(record_path("a/b.py,sha256=x,10").as_deref(), Some("a/b.py"));
        assert_eq!(record_path("\"a,\"\"b\"\".py\",,").as_deref(), Some("a,\"b\".py"));
        assert_eq!(lexical(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(relative(Path::new("/r/bin/x"), Path::new("/r/lib/sp")), PathBuf::from("../../bin/x"));
        assert_eq!(rewrite_shebang(b"#!python\nx".to_vec(), Some(Path::new("/p"))), b"#!/p\nx");
        assert_eq!(rewrite_shebang(b"#!/bin/sh\nx".to_vec(), Some(Path::new("/p"))), b"#!/bin/sh\nx");
    }
}
