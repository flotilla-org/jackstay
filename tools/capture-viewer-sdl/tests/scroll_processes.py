"""Actual toolkit producer and SDL viewer, separated by the bootstrap socket."""
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import time

def run(read_only):
    with tempfile.TemporaryDirectory(prefix="js-scroll-") as runtime:
        env = {**os.environ, "XDG_RUNTIME_DIR": runtime, "SDL_VIDEODRIVER": "dummy"}
        command = [sys.argv[1], "run", "--locked", "--offline", "-p", "jackstay-producer", "--example", "scroll"]
        if sys.platform == "darwin":
            command += ["--features", "jackstay/backend-macos"]
        if read_only:
            command += ["--", "--read-only"]
        source = subprocess.Popen(command, cwd=sys.argv[2], env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True)
        try:
            endpoint = pathlib.Path(runtime) / f"jackstay-{os.geteuid()}" / "scroll-producer.sock"
            deadline = time.monotonic() + 60
            while not endpoint.exists():
                if source.poll() is not None or time.monotonic() >= deadline:
                    raise AssertionError(source.communicate(timeout=1))
                time.sleep(.02)
            viewer = subprocess.run([sys.argv[3], "--source-socket", str(endpoint), "--affordances", "required",
                                     "--log-affordances", "--scroll-self-test", "--frames", "80"],
                                    env=env, capture_output=True, text=True, timeout=15)
            # Normal viewer routing must consume all overlay pointer events and
            # leave wheel delivery on the input channel; both axes use producer units.
            assert viewer.returncode == 0, (viewer.stdout, viewer.stderr)
            assert "acquired_frames=80" in viewer.stdout, viewer.stdout
            out, err = source.communicate(timeout=12)
            assert source.returncode == 0, (out, err)
            assert "unexpected pointer/key input" not in err, err
            verbs = re.findall(r'verb=(\w+) axis="([xy])" position=([\d.]+)', err)
            if read_only:
                # Capability flags forbid either verb, while the wheel remains input.
                assert verbs == [], err
                assert "wheel x=50 y=140" in err, err
                assert "x=50/1000 y=140/1000" in viewer.stderr, viewer.stderr
            else:
                assert [(verb, axis) for verb, axis, _ in verbs] == [
                    ("set_position", "y"), ("scroll_by_step", "y"),
                    ("set_position", "x"), ("scroll_by_step", "x")], err
                for (_, _, position), expected in zip(verbs, [566.6666666667, 366.6666666667, 525., 325.]):
                    assert abs(float(position) - expected) < .001, err
                # Wheel changes published scroll state, which the viewer observes.
                assert "wheel x=325 y=406.666" in err, err
                assert "x=325/1000 y=406.667/1000" in viewer.stderr, viewer.stderr
            assert "affordances_cleanup=completed" in viewer.stdout, viewer.stdout
            print(viewer.stdout, viewer.stderr, err)
        finally:
            if source.poll() is None:
                source.kill()
                source.communicate()

# macOS regression evidence: fail immediately if any writable/read-only pair
# misses a verb, and require 20 consecutive pairs in the existing CI job.
for iteration in range(20 if sys.platform == "darwin" else 1):
    print(f"scroll process iteration {iteration + 1}", flush=True)
    run(False)
    run(True)
