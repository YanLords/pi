#!/usr/bin/env node

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { initAgon } from "./extension.ts";

const packageRoot = fileURLToPath(new URL("..", import.meta.url));

function resolvePiCli(): string {
	try {
		const resolvedEntry = fileURLToPath(import.meta.resolve("@earendil-works/pi-coding-agent"));
		const distDir = dirname(resolvedEntry);
		const candidate1 = join(distDir, "cli.js");
		if (existsSync(candidate1)) return candidate1;

		const candidate2 = join(distDir, "bundle", "cli.js");
		if (existsSync(candidate2)) return candidate2;
	} catch {
		// Fall through
	}

	const monorepoCandidate = join(packageRoot, "..", "coding-agent", "dist", "cli.js");
	if (existsSync(monorepoCandidate)) {
		return monorepoCandidate;
	}

	const monorepoBundleCandidate = join(packageRoot, "..", "coding-agent", "dist", "bundle", "cli.js");
	if (existsSync(monorepoBundleCandidate)) {
		return monorepoBundleCandidate;
	}

	throw new Error("Could not resolve Pi CLI bundle from @earendil-works/pi-coding-agent.");
}

function resolveExtensionEntry(): string {
	const distEntry = join(packageRoot, "dist", "index.js");
	if (existsSync(distEntry)) {
		return distEntry;
	}

	const srcEntry = join(packageRoot, "src", "index.ts");
	if (existsSync(srcEntry)) {
		return srcEntry;
	}

	return distEntry;
}

const piCli = resolvePiCli();
const extensionEntry = resolveExtensionEntry();

const rawArgs = process.argv.slice(2);
if (rawArgs[0] === "init") {
	const result = initAgon(process.cwd());
	console.log("Initialized Agon project in .agon/");
	if (result.created.length > 0) {
		console.log(`Created: ${result.created.join(", ")}`);
	}
	if (result.kept.length > 0) {
		console.log(`Kept: ${result.kept.join(", ")}`);
	}
	console.log("Next: define checks in .agon/checks.toml and add them to [form] checks in .agon/config.toml");
	process.exit(0);
}

const args = ["--extension", extensionEntry, ...rawArgs];

const child = spawn(process.execPath, [piCli, ...args], {
	stdio: "inherit",
	shell: false,
});

child.on("exit", (code, signal) => {
	if (signal) {
		process.kill(process.pid, signal);
	} else {
		process.exit(code ?? 0);
	}
});
