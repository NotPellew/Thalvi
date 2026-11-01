"""Launch the native resident interface with the system GTK runtime."""

import subprocess
import sys
from pathlib import Path


def launch(config_path: Path) -> int:
    try:
        return subprocess.call(
            [
                "/usr/bin/python3",
                str(Path(__file__).with_name("gtk_app.py")),
                sys.executable,
                str(config_path.resolve()),
            ]
        )
    except OSError as error:
        print(f"Could not start Thalvi desktop: {error}", file=sys.stderr)
        return 1
