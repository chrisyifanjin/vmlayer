// Create a sandbox, run code in it, move a file out, clean up.
// Run from this directory after `npm install`: npx tsx quickstart.ts
import { Sandbox } from "vmlayer";

async function main() {
  await using sb = await Sandbox.create("python:3.12-alpine", { memoryMb: 256 });
  await sb.files.write("/work/main.py", "import sys; print(sys.version.split()[0])");
  const result = await sb.exec("python /work/main.py");
  console.log("python says:", result.stdout.trim(), "| exit", result.exitCode);
  await sb.files.download("/work/main.py", "./main.copy.py");
  console.log("done; sandbox killed by `await using`");
}

await main();
