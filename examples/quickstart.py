"""Create a sandbox, run code in it, move a file out, clean up."""

from vmlayer import Sandbox

with Sandbox.create("python:3.12-alpine", memory_mb=256) as sb:
    sb.files.write("/work/main.py", "import sys; print(sys.version.split()[0])")
    result = sb.exec("python /work/main.py")
    print("python says:", result.stdout.strip(), "| exit", result.exit_code)
    sb.files.write("/work/report.txt", result.output)
    sb.files.download("/work/report.txt", "./report.txt")
print("done; sandbox killed on exit")
