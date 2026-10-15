"""The viewer selects and plays a playback track offered at attach or added later, over real processes."""
import os
import re
import subprocess
import sys
import uuid

env = {**os.environ, "SDL_VIDEODRIVER": "dummy", "SDL_AUDIODRIVER": "dummy"}
for source_flags in ([], ["--late"]):
    name = "audio-test-" + uuid.uuid4().hex[:12]
    source = subprocess.Popen([sys.argv[1], name, *source_flags], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
    try:
        assert source.stdout.readline().strip() == "ready"
        viewer = subprocess.run([sys.argv[2], "--source-endpoint", name, "--observe", "--affordances", "none",
                                 "--audio", "tone", "--frames", "60"],
                                env=env, capture_output=True, text=True, timeout=20)
        out, err = source.communicate(timeout=5)
        assert viewer.returncode == 0 and "acquired_frames=60" in viewer.stdout, (viewer.stdout, viewer.stderr)
        # The offer is listed by label, selected and played to the end.
        assert "audio track id=2 label=tone rate=48000 channels=2" in viewer.stderr, viewer.stderr
        stats = re.search(r"audio track=2 underruns=(\d+) gaps=(\d+)", viewer.stdout)
        assert stats, viewer.stdout
        assert source.returncode == 0 and "audio_blocks=" in out, (out, err)
        print(source_flags, viewer.stdout.strip())
    finally:
        if source.poll() is None:
            source.kill()
            source.communicate()

# Audio needs a handshake source and a valid latency.
for flags in (["--audio", "tone"], ["--audio"], ["--source-endpoint", "unused", "--audio-latency-ms", "x"]):
    result = subprocess.run([sys.argv[2], *flags], env=env, capture_output=True, text=True, timeout=5)
    assert result.returncode != 0, flags
