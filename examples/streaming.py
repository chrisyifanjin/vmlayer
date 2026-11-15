"""Long-running commands: stream output, feed stdin, re-attach by pid."""

from vmlayer import Sandbox

with Sandbox.create("python:3.12-alpine") as sb:
    proc = sb.spawn("python3 -u -c 'import sys\nfor line in sys.stdin: print(\"got\", line.strip())'")
    proc.stdin.write(b"one\n")
    proc.stdin.write(b"two\n")
    proc.stdin.close()
    for chunk in proc:
        print(chunk.stream, chunk.data.decode().rstrip())
    print("exit", proc.wait().exit_code)

    # Output is kept by the daemon, so a fresh handle sees it too.
    again = sb.process(proc.pid)
    print("replayed:", again.wait().stdout.strip().replace("\n", " | "))
