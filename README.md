# python.rs

`pyrs` turns a Python project into one `.pyrs` file that anyone with Python installed can run with a single command.

It doesn't bundle a Python runtime the way PyInstaller does. The file holds your project as a normal wheel plus every dependency as a wheel, and nothing else, so it's about the size of your code. A hello-world app is about 4 KB. The only thing a friend needs is Python: python.org or the `.msi` on Windows, `sudo apt install python3` on Linux, or whatever comes preinstalled. They don't need pip, `python3-venv`, or network access.

## Compared with PyInstaller

| | PyInstaller (`--onefile`) | pyrs |
|---|---|---|
| What's in the file | Interpreter + stdlib + your code + deps, frozen | Your wheel + dependency wheels |
| Hello-world size | several MB (it carries the interpreter) | ~4 KB |
| Startup | Unpacks the whole archive to a temp dir on *every* run | ~50 ms; ~30 ms with `--cache` (measured on Linux) |
| Needs on the target | Nothing | Any Python 3 |
| One file per OS? | Always (it freezes the interpreter) | Only if you use compiled packages; pure Python runs anywhere |
| Your code inside | Frozen bytecode | Ordinary installed packages, so tracebacks and `importlib.metadata` work normally |

## Usage

Build (on your machine, with a Python that has pip):

```sh
pyrs build path/to/project            # writes ./<name>-<version>.pyrs
pyrs build . -o myapp.pyrs --entry mycli
```

Run (on any machine with Python):

```sh
pyrs run myapp.pyrs -- --some --args       # fresh venv in a temp dir, removed afterwards
pyrs run --cache myapp.pyrs                # keep that venv and reuse it next time (much faster)
pyrs run --env global myapp.pyrs           # use the interpreter's own environment
pyrs run --venv ~/envs/mine myapp.pyrs     # use (and install into) an existing venv
pyrs inspect myapp.pyrs                    # what's inside, what it was built for
```

Pick the interpreter with `--python PATH_OR_NAME` or `PYRS_PYTHON`. Otherwise pyrs tries `python3` and then `python` (on Windows, `py -3` and then `python`) and uses the first one that satisfies the project's `requires-python`. Add `-q` to silence pyrs's own status lines. The program's own output always goes straight through.

## What a project needs

- A `pyproject.toml` (or `setup.py`) that pip can build.
- Something to run: a `[project.scripts]` entry, or a package with a `__main__.py`. If there are several, choose one with `--entry`.

## How it works

`pyrs build` copies the project to a temp dir, leaving out `build/`, `dist/`, `*.egg-info` and venvs, so nothing stale gets in and nothing is left in your tree. It then runs `pip wheel` to build your wheel and resolve every dependency to a wheel, and zips them all with a `manifest.json`. The manifest records the entry point, `requires-python`, the build platform, and a SHA-256 for each wheel.

`pyrs run` doesn't use pip. Bundles are fully resolved at build time, so installing is just unpacking wheels, and pyrs does that itself in Rust. It reads each wheel directly out of the `.pyrs` file and checks its hash first. On Unix it also writes the venv itself, which is only a `pyvenv.cfg` and a symlink, so it works even where `ensurepip`/`python3-venv` is missing. The interpreter probe is cached per Python binary. A run therefore starts Python exactly once: to run your program. Arguments, exit codes and Ctrl-C pass through.

`--env global` unpacks into a throwaway directory put on `PYTHONPATH`, so the system site-packages are never touched. That also avoids PEP 668 "externally managed" errors. Installs into `--venv` follow the wheel spec: they upgrade older versions, write `RECORD`, and create console scripts on Unix, so pip can list and uninstall them afterwards.

### Startup tips

A fresh temp environment has no compiled `.pyc` files, so Python compiles every module it imports, on every run. That's barely noticeable for small apps. It adds up for big dependencies: an app using `rich` takes ~220 ms in temp mode against ~100 ms with `--cache`. If startup matters, use `--cache`.

## Limits

- **Dependencies are resolved for the machine that built the bundle.** That means compiled packages (numpy and the like) and platform-conditional dependencies. A bundle with compiled wheels runs only on the same OS/architecture and Python minor version. `pyrs inspect` shows which kind you have. Even a pure-Python bundle can miss a dependency that is only required on another OS, e.g. `colorama` on Windows. If so, build it on that OS.
- A bundle is code you run with your own permissions. There is no sandbox or signing yet, so only run bundles from people you trust.
- On Windows, installing into `--venv` doesn't create console-script `.exe` launchers. `pyrs run` doesn't need them.
- Only SIGINT (Ctrl-C) is handled gracefully. If pyrs itself is killed, a temp workspace can be left in the system temp dir.

## Development

```sh
cargo test                                         # unit tests (e2e tests skip themselves)
PYRS_E2E_PYTHON=/path/to/python-with-pip cargo test   # also build and run tests/fixtures/hello_app
```

The e2e build step needs network access to fetch setuptools. The run steps are offline and use the `python3` on `PATH`, so ideally use a bare one.

## License

MIT. See [LICENSE](LICENSE).
