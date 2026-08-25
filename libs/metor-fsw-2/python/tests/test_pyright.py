"""Optional static-analysis gate: if pyright is available, the recorder core
and the generated fixture module must type-check clean. Skipped cleanly when
pyright is absent, and never a build dependency.

Enable the ``uv run --with pyright`` fallback (which downloads pyright) by
setting ``METOR_ENABLE_PYRIGHT=1``; otherwise only a ``pyright`` already on
PATH is used.
"""

import json
import os
import shutil
import subprocess
import unittest

PYTHON_DIR = os.path.join(os.path.dirname(__file__), "..")


def pyright_command():
    """A command that runs pyright, or ``None`` if none is available."""
    if shutil.which("pyright"):
        return ["pyright"]
    if os.environ.get("METOR_ENABLE_PYRIGHT") and shutil.which("uv"):
        return ["uv", "run", "--with", "pyright", "pyright"]
    return None


class PyrightTest(unittest.TestCase):
    def test_core_and_generated_module_type_check(self):
        cmd = pyright_command()
        if cmd is None:
            self.skipTest("pyright not available on PATH")
        result = subprocess.run(
            [
                *cmd,
                "--outputjson",
                os.path.join("metor-config", "metor_config"),
                os.path.join("tests", "data", "demo.py"),
            ],
            cwd=os.path.abspath(PYTHON_DIR),
            env={
                **os.environ,
                "PYTHONPATH": os.path.abspath(os.path.join(PYTHON_DIR, "metor-config")),
            },
            capture_output=True,
            text=True,
        )
        # pyright exits 1 on findings; parse the JSON summary either way.
        try:
            report = json.loads(result.stdout)
        except json.JSONDecodeError:
            self.fail(f"pyright produced no JSON report:\n{result.stdout}\n{result.stderr}")
        errors = report.get("summary", {}).get("errorCount", 0)
        self.assertEqual(
            errors,
            0,
            "pyright found type errors in the recorder core or generated module:\n"
            + json.dumps(report.get("generalDiagnostics", []), indent=2),
        )


if __name__ == "__main__":
    unittest.main()
