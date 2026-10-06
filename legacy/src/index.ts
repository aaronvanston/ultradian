#!/usr/bin/env bun

import { app } from "./app.ts";

const handleBrokenPipe = (error: NodeJS.ErrnoException): void => {
  if (error.code === "EPIPE") {
    process.exitCode = 0;
    return;
  }
  throw error;
};

process.stdout.on("error", handleBrokenPipe);
process.stderr.on("error", handleBrokenPipe);

const main = async (): Promise<void> => {
  process.exitCode = await app.run();
};

void main();
