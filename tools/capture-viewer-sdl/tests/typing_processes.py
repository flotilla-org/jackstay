"""Typing policies exercise the real legacy source and captured SDL events."""
import os
import pathlib
import subprocess
import sys
import tempfile
import time

# Enumerate every mode: text must suppress all keys; physical must suppress all text.
for mode, counts in (("cooperative", "downs=2 repeats=1 releases=1 text_bytes=1031"),
                     ("text", "downs=0 repeats=0 releases=0 text_bytes=1031"),
                     ("physical", "downs=2 repeats=0 releases=1 text_bytes=0")):
    with tempfile.TemporaryDirectory(prefix="js-typing-", dir="/tmp") as directory:
        endpoint = str(pathlib.Path(directory) / "source")
        source = subprocess.Popen([sys.argv[1], endpoint], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not pathlib.Path(endpoint).exists():
                assert source.poll() is None and time.monotonic() < deadline
                time.sleep(.01)
            viewer = subprocess.run([sys.argv[2], "--source-socket", endpoint, "--typing", mode,
                                     "--input-self-test", "--frames", "30"],
                                    env={**os.environ, "SDL_VIDEODRIVER": "dummy"}, capture_output=True, text=True, timeout=10)
            assert viewer.returncode == 0, (viewer.stdout, viewer.stderr)
            out, err = source.communicate(timeout=6)
            assert source.returncode == 0, (out, err)
            assert counts in out, (mode, out, err)
            assert "held=0 buttons=0" in out and "input_cleanup=completed" in viewer.stdout, out
            print(mode, out)
        finally:
            if source.poll() is None:
                source.kill()
                source.communicate()
