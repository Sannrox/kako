"""Load user transform source as data and run it against staged frames.

The runner never imports user code as a package. This wrapper is the only
Python the worker executes on purpose: it loads `user.py` from the scratch
directory, calls `transform(inputs)`, and writes returned frames under
`outputs/`.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path


def _load_user(path: Path):
    spec = importlib.util.spec_from_file_location("kako_user_transform", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load user transform from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _read_frame(path: Path):
    import polars as pl

    suffix = path.suffix.lower()
    if suffix == ".parquet":
        return pl.read_parquet(path)
    if suffix in {".arrow", ".feather", ".ipc"}:
        return pl.read_ipc(path)
    if suffix == ".tsv":
        return pl.read_csv(path, separator="\t")
    return pl.read_csv(path)


def _as_polars(frame):
    import polars as pl

    if isinstance(frame, pl.DataFrame):
        return frame
    if hasattr(frame, "to_pandas") and type(frame).__module__.startswith("polars"):
        return frame
    try:
        import pandas as pd
    except ImportError:
        pd = None
    if pd is not None and isinstance(frame, pd.DataFrame):
        return pl.from_pandas(frame)
    raise TypeError(
        "transform must return polars or pandas DataFrames, got "
        f"{type(frame)!r}"
    )


def _write_frame(frame, path: Path) -> None:
    frame = _as_polars(frame)
    path.parent.mkdir(parents=True, exist_ok=True)
    suffix = path.suffix.lower()
    if suffix == ".parquet":
        frame.write_parquet(path)
        return
    if suffix in {".arrow", ".feather", ".ipc"}:
        frame.write_ipc(path)
        return
    frame.write_csv(path)


def main() -> int:
    scratch = Path(".")
    manifest = json.loads((scratch / "manifest.json").read_text())
    user_path = scratch / "user.py"
    if not user_path.is_file():
        print("python engine: user.py is missing", file=sys.stderr)
        return 1
    try:
        module = _load_user(user_path)
    except Exception as error:  # noqa: BLE001 — surface user load errors as job failures
        print(f"python engine: load user transform: {error}", file=sys.stderr)
        return 1
    transform = getattr(module, "transform", None)
    if transform is None or not callable(transform):
        print("python engine: user code must define transform(inputs)", file=sys.stderr)
        return 1
    try:
        inputs = {
            item["name"]: _read_frame(Path(item["relative"]))
            for item in manifest.get("inputs", [])
        }
    except Exception as error:  # noqa: BLE001
        print(f"python engine: read inputs: {error}", file=sys.stderr)
        return 1
    # User prints must not become engine_version on stdout.
    sys.stdout = sys.stderr
    try:
        outputs = transform(inputs)
    except Exception as error:  # noqa: BLE001
        print(f"python engine: transform: {error}", file=sys.stderr)
        return 1
    finally:
        sys.stdout = sys.__stdout__
    if not isinstance(outputs, dict):
        print("python engine: transform must return a dict of name -> frame", file=sys.stderr)
        return 1
    try:
        for name, frame in outputs.items():
            dest = Path("outputs") / name
            if dest.parent != Path("outputs"):
                print(
                    f"python engine: output `{name}` must be a file name under outputs/",
                    file=sys.stderr,
                )
                return 1
            _write_frame(frame, dest)
    except Exception as error:  # noqa: BLE001
        print(f"python engine: write outputs: {error}", file=sys.stderr)
        return 1
    import polars as pl

    print(f"python {sys.version.split()[0]} polars {pl.__version__}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
