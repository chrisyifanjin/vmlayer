"""A workspace that outlives any one sandbox, and checkpoints as templates."""

from vmlayer import Sandbox, Volume

repo = Volume.create(name="example-repo", size_mb=1024)
try:
    with Sandbox.create("python:3.12-alpine", workspace=repo) as sb:
        sb.exec("echo 'print(1+1)' > /work/app.py && pip install -q --root-user-action=ignore rich")
    ckpt = repo.snapshot(labels={"stage": "deps"})          # milliseconds, copy-on-write

    with Sandbox.create("python:3.12-alpine", workspace=repo) as sb:
        print("same files, new sandbox:", sb.exec("python /work/app.py").stdout.strip())
        sb.exec("echo broken > /work/app.py")

    fresh = Volume.from_snapshot(ckpt, name="example-repo-try2")   # back to the checkpoint
    try:
        with Sandbox.create("python:3.12-alpine", workspace=fresh) as sb:
            print("from checkpoint:", sb.exec("python /work/app.py").stdout.strip())
    finally:
        fresh.delete()
    ckpt.delete()
finally:
    repo.delete()
