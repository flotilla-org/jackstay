"""V2 optional/required channel negotiation and independent teardown over real processes."""
import os
import subprocess
import sys
import uuid

env = {**os.environ, "SDL_VIDEODRIVER": "dummy"}
# Covers offered, unrequested, optional absence, and required refusal.
for offered, policy, session in (("offered", "required", False), ("offered", "none", False),
                                 ("absent", "optional", False), ("absent", "required", False),
                                 ("offered", "default", True)):
    name = "v2-test-" + uuid.uuid4().hex[:12]
    source = subprocess.Popen([sys.argv[1], name, offered, *(["session"] if session else [])], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True)
    try:
        assert source.stdout.readline().strip() == "ready"
        flags = ([] if policy == "default" else ["--affordances", policy]) + (["--session-scope"] if session else [])
        viewer = subprocess.run([sys.argv[2], "--source-endpoint", name, "--observe",
                                 *flags, "--log-affordances", "--frames", "10"],
                                env=env, capture_output=True, text=True, timeout=12)
        out, err = source.communicate(timeout=5)
        # A required unavailable channel fails bootstrap clearly before media admission.
        if policy == "required" and offered == "absent":
            assert viewer.returncode != 0 and "required affordances" in viewer.stderr, viewer
            assert "acquired_frames=" not in viewer.stdout, viewer.stdout
        else:
            # Media remains available when affordances are optional or unrequested.
            assert viewer.returncode == 0 and "acquired_frames=10" in viewer.stdout, (viewer.stdout, viewer.stderr)
            if policy != "none" and offered == "offered":
                # Presentation arrives at the producer and both peers observe channel closure.
                assert "affordances domain=window" in viewer.stderr, viewer.stderr
                assert "affordances_cleanup=completed" in viewer.stdout, viewer.stdout
                assert "presentation=1 affordances_closed=1" in out, (out, err)
        assert source.returncode == 0, (out, err)
        print(policy, offered, viewer.stdout, out)
    finally:
        if source.poll() is None:
            source.kill()
            source.communicate()

# Invalid flag values/combinations must fail without connecting to a source.
for flags in (["--typing", "bad"], ["--typing"], ["--affordances", "bad"],
              ["--affordances"], ["--source-endpoint"], ["--session-scope"],
              ["--source-socket", "/unused", "--affordances", "required"],
              ["--source-endpoint", "unused", "--source-socket", "/unused"]):
    result = subprocess.run([sys.argv[2], *flags], env=env, capture_output=True, text=True, timeout=5)
    assert result.returncode != 0, flags
