import importlib.util
import json
import os
import sys
import sysconfig

KEYS = ("purelib", "platlib", "scripts", "data", "include")


def venv_layout():
    """Install paths of a venv, relative to the venv directory."""
    scheme = "venv" if "venv" in sysconfig.get_scheme_names() else ("nt" if os.name == "nt" else "posix_prefix")
    marker = "@@"
    base = dict.fromkeys(("base", "platbase", "installed_base", "installed_platbase"), marker)
    return {k: sysconfig.get_path(k, scheme, base)[len(marker):].lstrip("/\\") for k in KEYS}


print(json.dumps({
    "version": list(sys.version_info[:3]),
    "platform": sysconfig.get_platform(),
    "has_venv": importlib.util.find_spec("venv") is not None,
    "has_pip": importlib.util.find_spec("pip") is not None,
    "base_executable": getattr(sys, "_base_executable", None) or sys.executable,
    "venv_layout": venv_layout(),
    "paths": {k: sysconfig.get_path(k) for k in KEYS},
}))
