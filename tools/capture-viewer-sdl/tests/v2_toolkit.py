"""The unmodified toolkit minimal example is the real v2 producer collaborator."""
import os
import pathlib
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="js-toolkit-") as runtime:
    env = {**os.environ, "XDG_RUNTIME_DIR": runtime, "SDL_VIDEODRIVER": "dummy"}
    source = subprocess.Popen([sys.argv[1], "run", "--locked", "-p", "jackstay-producer", "--example", "minimal"],
                              cwd=sys.argv[2], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        endpoint = pathlib.Path(runtime) / f"jackstay-{os.geteuid()}" / "minimal-producer.sock"
        deadline = time.monotonic() + 60
        while not endpoint.exists():
            if source.poll() is not None or time.monotonic() >= deadline:
                raise AssertionError(source.communicate(timeout=1))
            time.sleep(.02)
        viewer = subprocess.run([sys.argv[3], "--source-endpoint", "minimal-producer", "--observe",
                                 "--affordances", "required", "--log-affordances", "--frames", "10"],
                                env=env, capture_output=True, text=True, timeout=10)
        # The actual toolkit produces media and its window snapshot; the host closes independently.
        assert viewer.returncode == 0, (viewer.stdout, viewer.stderr)
        assert "acquired_frames=10" in viewer.stdout, viewer.stdout
        assert "domain=window" in viewer.stderr and "Minimal producer" in viewer.stderr, viewer.stderr
        assert "affordances_cleanup=completed" in viewer.stdout, viewer.stdout
        out, err = source.communicate(timeout=12)
        assert source.returncode == 0, (out, err)
        print(viewer.stdout, viewer.stderr)
    finally:
        if source.poll() is None:
            source.kill()
            source.communicate()
