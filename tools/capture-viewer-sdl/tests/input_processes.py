"""Real C-ABI producer/viewer processes; SDL dummy requires no desktop permission."""
import os
import pathlib
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="js-input-", dir="/tmp") as directory:
    media = str(pathlib.Path(directory) / "source")
    source = subprocess.Popen([sys.argv[1], media], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        deadline = time.monotonic() + 5
        while not pathlib.Path(media).exists():
            if source.poll() is not None or time.monotonic() >= deadline:
                raise AssertionError("source failed to listen")
            time.sleep(0.01)
        viewer = subprocess.run([sys.argv[2], "--source-socket", media,
                                 "--input-self-test", "--frames", "30"],
                                env={**os.environ, "SDL_VIDEODRIVER": "dummy"}, capture_output=True, text=True, timeout=10)
        assert viewer.returncode == 0, (viewer.returncode, viewer.stdout, viewer.stderr, source.poll())
        assert "input_cleanup=completed" in viewer.stdout, viewer.stdout
        out, err = source.communicate(timeout=6)
        assert source.returncode == 0, (out, err)
        assert "downs=2 repeats=1 releases=1 text_bytes=1031" in out, (out, err, viewer.stderr)
        assert "held=0 buttons=0" in out, out
        print(viewer.stdout, out)
    finally:
        if source.poll() is None:
            source.kill()
            source.communicate()

# Kill a real viewer only after the source reports both a held key and button.
# The surviving source must execute cleanup despite loss of the controller process.
with tempfile.TemporaryDirectory(prefix="js-exit-", dir="/tmp") as directory:
    media = str(pathlib.Path(directory) / "source")
    report = pathlib.Path(directory) / "source.log"
    with report.open("w") as log:
        source = subprocess.Popen([sys.argv[1], media, "--report-state"], stdout=log, stderr=subprocess.PIPE, text=True)
        viewer = None
        try:
            deadline = time.monotonic() + 5
            while not pathlib.Path(media).exists():
                assert source.poll() is None and time.monotonic() < deadline
                time.sleep(0.01)
            viewer = subprocess.Popen([sys.argv[2], "--source-socket", media, "--input-self-test"],
                                      env={**os.environ, "SDL_VIDEODRIVER": "dummy"}, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            while "text_bytes=1031 held=1 buttons=1" not in report.read_text():
                assert viewer.poll() is None and time.monotonic() < deadline, report.read_text()
                time.sleep(0.01)
            viewer.kill()
            viewer.communicate(timeout=3)
            _, err = source.communicate(timeout=6)
            assert source.returncode == 0, (report.read_text(), err)
            assert "held=0 buttons=0" in report.read_text().splitlines()[-1], report.read_text()
            print("viewer process death: executor cleanup confirmed")
        finally:
            for process in (viewer, source):
                if process is not None and process.poll() is None:
                    process.kill()
                    process.communicate()

# Observation is explicit, and optional input absence preserves media.
for source_flags, viewer_flags in (([], ["--observe"]), (["--observe-only"], [])):
    with tempfile.TemporaryDirectory(prefix="js-observe-", dir="/tmp") as directory:
        endpoint = str(pathlib.Path(directory) / "source")
        source = subprocess.Popen([sys.argv[1], endpoint, *source_flags], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not pathlib.Path(endpoint).exists():
                assert source.poll() is None and time.monotonic() < deadline
                time.sleep(.01)
            viewer = subprocess.run([sys.argv[2], "--source-socket", endpoint, *viewer_flags, "--frames", "3"],
                                    env={**os.environ, "SDL_VIDEODRIVER": "dummy"}, capture_output=True, text=True, timeout=10)
            assert viewer.returncode == 0, (viewer.stdout, viewer.stderr)
            assert "acquired_frames=3" in viewer.stdout, viewer.stdout
            out, err = source.communicate(timeout=6)
            assert source.returncode == 0, (out, err)
            assert "downs=0" in out and "cleanup=0" in out, out
            print("one-endpoint observation: no input controller admitted")
        finally:
            if source.poll() is None:
                source.kill(); source.communicate()
