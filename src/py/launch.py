"""Runs a bundled project's entry point. Usage: launch.py KIND NAME TARGET [ARGS...]"""
import importlib
import runpy
import sys

kind, name, target = sys.argv[1:4]
sys.argv = [name, *sys.argv[4:]]

if kind == "console_script":
    module_name, _, attr = target.partition(":")
    func = importlib.import_module(module_name)
    for part in attr.split("."):
        func = getattr(func, part)
    sys.exit(func())
elif kind == "module":
    runpy.run_module(target, run_name="__main__", alter_sys=True)
else:
    sys.exit(f"pyrs: unknown entry kind {kind!r}")
