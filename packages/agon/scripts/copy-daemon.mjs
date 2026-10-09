import { chmodSync, copyFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = join(fileURLToPath(new URL("..", import.meta.url)));

const platform = process.platform === "win32" ? "windows" : process.platform;
const arch = process.arch;
const exeName = platform === "windows" ? "agon.exe" : "agon";
const srcExe = platform === "windows" ? "agon-bridge.exe" : "agon-bridge";

const src = join(packageRoot, "daemon", "target", "release", srcExe);
const destDir = join(packageRoot, "bin", `agon-${platform}-${arch}`);
const dest = join(destDir, exeName);

if (!existsSync(src)) {
	console.error(`Binary not found at ${src}. Run "cargo build --release --locked --manifest-path daemon/Cargo.toml" first.`);
	process.exit(1);
}

mkdirSync(destDir, { recursive: true });
copyFileSync(src, dest);
if (platform !== "windows") {
	chmodSync(dest, 0o755);
}

console.log(`Copied ${src} to ${dest}`);
