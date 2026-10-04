"""Real toolkit producer and viewer communicate across a bootstrap socket."""
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="js-nav-", dir="/tmp") as runtime:
    env = {**os.environ, "XDG_RUNTIME_DIR": runtime, "SDL_VIDEODRIVER": "dummy"}
    command = [sys.argv[1], "run", "--locked", "--offline", "-p", "jackstay-producer", "--example", "navigation"]
    if sys.platform == "darwin":
        command += ["--features", "jackstay/backend-macos"]
    source = subprocess.Popen(command, cwd=sys.argv[2], env=env, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
    try:
        endpoint = pathlib.Path(runtime) / f"jackstay-{os.geteuid()}" / "navigation-producer.sock"
        deadline = time.monotonic() + 60
        while not endpoint.exists():
            if source.poll() is not None or time.monotonic() >= deadline:
                raise AssertionError(source.communicate(timeout=1))
            time.sleep(.02)
        viewer = subprocess.run([sys.argv[3], "--source-socket", str(endpoint), "--affordances", "required",
                                 "--log-affordances", "--navigation-self-test", "--frames", "80"],
                                env=env, capture_output=True, text=True, timeout=15)
        # Normal toolbar routing sends verbs and captures keyboard/pointer input.
        assert viewer.returncode == 0, (viewer.stdout, viewer.stderr)
        out, err = source.communicate(timeout=12)
        assert source.returncode == 0, (out, err)
        assert re.findall(r"navigation verb=(\w+)", err) == ["back", "forward", "reload", "stop", "load"], err
        assert "navigation verb=load url=https://example.test/typed" in err, err
        assert "unexpected pointer/key input" not in err, err
        # Presentation hints exclude chrome to keep producer frames fixed.
        assert "presentation frame=640x480 scale=1" in err, err
        assert "frame=640x508" not in err, err
        assert "affordances_cleanup=completed" in viewer.stdout, viewer.stdout
        print(viewer.stdout, viewer.stderr, err)
    finally:
        if source.poll() is None:
            source.kill()
            source.communicate()
