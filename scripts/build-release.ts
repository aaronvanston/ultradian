// Builds the release archives: one ultradian-<version>-<target>.tar.gz per
// system, each holding a single 'ultradian' binary, and a SHA256SUMS file
// listing them. Tools that ship ultradian (and people installing by hand)
// download an archive, check it against SHA256SUMS, and run
// 'ultradian self install' from it.
import { createHash } from "node:crypto";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";

import packageJson from "../package.json" with { type: "json" };

const root = path.resolve(import.meta.dir, "..");
const dist = path.resolve(root, "dist/release");
const { version } = packageJson;
// The name each archive carries, and the Bun target that builds it. Linux
// x64 uses the baseline build so it runs on CPUs without AVX2.
const targets = [
  { bun: "bun-darwin-arm64", name: "darwin-arm64" },
  { bun: "bun-darwin-x64", name: "darwin-x64" },
  { bun: "bun-linux-arm64", name: "linux-arm64" },
  { bun: "bun-linux-x64-baseline", name: "linux-x64" },
] as const;

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

const buildTarget = async (
  target: (typeof targets)[number]
): Promise<string> => {
  const folder = path.resolve(dist, target.name);
  await mkdir(folder, { recursive: true });
  const binary = path.resolve(folder, "ultradian");
  const result = await Bun.build({
    bytecode: true,
    compile: {
      autoloadBunfig: false,
      autoloadDotenv: false,
      outfile: binary,
      target: target.bun,
    },
    define: {
      "process.env.ULTRADIAN_COMMIT": JSON.stringify(commit),
    },
    entrypoints: [path.resolve(root, "src/index.ts")],
    minify: true,
  });
  if (!result.success) {
    for (const log of result.logs) {
      console.error(log);
    }
    process.exit(1);
  }

  // The build for this machine must actually start, the same probe
  // scripts/build.ts makes, so a binary the OS refuses never ships.
  if (`${process.platform}-${process.arch}` === target.name) {
    const probe = Bun.spawnSync({ cmd: [binary, "version", "--json"] });
    if (probe.exitCode !== 0) {
      console.error(`${target.name} failed its execution probe`);
      process.exit(1);
    }
  }

  const archive = `ultradian-${version}-${target.name}.tar.gz`;
  // No owner names or macOS metadata in the archive; it holds one file.
  const tar = Bun.spawnSync({
    cmd: [
      "tar",
      "-czf",
      path.resolve(dist, archive),
      "-C",
      folder,
      ...(process.platform === "darwin"
        ? ["--no-mac-metadata", "--uname", "root", "--gname", "root"]
        : ["--owner=0", "--group=0", "--numeric-owner"]),
      "ultradian",
    ],
    env: { ...process.env, COPYFILE_DISABLE: "1" },
    stderr: "pipe",
  });
  if (tar.exitCode !== 0) {
    console.error(tar.stderr.toString());
    process.exit(1);
  }
  await rm(folder, { force: true, recursive: true });
  const digest = createHash("sha256")
    .update(await readFile(path.resolve(dist, archive)))
    .digest("hex");
  console.log(`built ${archive}`);
  return `${digest}  ${archive}`;
};

// Each target builds into its own folder, so they can build side by side.
const sums = await Promise.all(targets.map(buildTarget));

await writeFile(path.resolve(dist, "SHA256SUMS"), `${sums.join("\n")}\n`);
console.log(`wrote ${path.resolve(dist, "SHA256SUMS")}`);
