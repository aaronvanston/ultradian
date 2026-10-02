import { mkdir, rm, symlink } from "node:fs/promises";
import path from "node:path";

const root = path.resolve(import.meta.dir, "..");
const dist = path.resolve(root, "dist");
const outfile = path.resolve(dist, "ultradian");
const commitProcess = Bun.spawnSync({
  cmd: ["git", "rev-parse", "--short=12", "HEAD"],
  cwd: root,
  stderr: "ignore",
  stdout: "pipe",
});
const commit =
  commitProcess.exitCode === 0
    ? commitProcess.stdout.toString().trim()
    : "unknown";

await rm(dist, { force: true, recursive: true });
await mkdir(dist, { recursive: true });

const result = await Bun.build({
  bytecode: true,
  compile: {
    autoloadBunfig: false,
    autoloadDotenv: false,
    outfile,
  },
  define: {
    "process.env.ULTRADIAN_COMMIT": JSON.stringify(commit),
  },
  entrypoints: [path.resolve(root, "src/index.ts")],
  minify: true,
  sourcemap: "linked",
});

if (result.success) {
  // Tests run from source, so this probe is the only thing that ever
  // executes the compiled artifact. A binary the OS refuses to run (for
  // example bun 1.3.12 shipped truncated macOS code signatures, and the
  // kernel SIGKILLed every compiled binary) must fail the build here.
  const probe = Bun.spawnSync({
    cmd: [outfile, "--version"],
    stderr: "pipe",
    stdout: "pipe",
  });
  if (probe.exitCode === 0) {
    await symlink("ultradian", path.resolve(dist, "udian"));
    console.log(`built ${outfile} (alias: udian)`);
  } else {
    const reason =
      probe.signalCode === null
        ? `exit ${probe.exitCode}`
        : `signal ${probe.signalCode}`;
    console.error(
      `built binary failed its execution probe (${reason}); the artifact cannot be shipped`
    );
    process.exitCode = 1;
  }
} else {
  for (const log of result.logs) {
    console.error(log);
  }
  process.exitCode = 1;
}
