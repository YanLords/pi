import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { AgonBridge } from "../src/bridge.ts";
import { getTestDaemon } from "./daemon.ts";

function setupFixture(name: string): string {
	const dir = join(tmpdir(), `agon-test-${name}-${Date.now()}`);
	mkdirSync(join(dir, ".agon"), { recursive: true });

	const config = `
[form]
tools = ["cargo"]
checks = ["verify.ok"]

[permissions]
shell_allow = ["cargo test"]
`;

	const checks = `
[check."verify.ok"]
command = "true"
success = "exit_code == 0"

[check."verify.fail"]
command = "false"
success = "exit_code == 0"
`;

	writeFileSync(join(dir, ".agon/config.toml"), config);
	writeFileSync(join(dir, ".agon/checks.toml"), checks);
	return dir;
}

describe("AgonBridge", () => {
	let fixtureDir: string;
	let bridge: AgonBridge;

	beforeAll(async () => {
		fixtureDir = setupFixture("bridge");
		bridge = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await bridge.start();
	});

	afterAll(async () => {
		await bridge.shutdown();
		rmSync(fixtureDir, { recursive: true, force: true });
	});

	it("runs hello and returns protocol version", async () => {
		const res = await bridge.hello();
		expect(res.protocol).toBe(1);
		expect(res.capabilities).toContain("status");
		expect(res.capabilities).toContain("authorize");
		expect(res.capabilities).toContain("verify");
	});

	it("runs status and reports active checks and no tampering", async () => {
		const res = await bridge.status();
		expect(res.active_checks).toContain("verify.ok");
		expect(res.tampering_violations).toEqual([]);
		expect(typeof res.form_id).toBe("string");
		expect(typeof res.lock_hash).toBe("string");
	});

	it("authorizes read operations with allow", async () => {
		const res = await bridge.authorize("read", "src/index.ts");
		expect(res.verdict).toBe("allow");
	});

	it("authorizes allowlisted shell commands with allow", async () => {
		const res = await bridge.authorize("shell", "cargo test");
		expect(res.verdict).toBe("allow");
	});

	it("authorizes git push commands with deny", async () => {
		const res = await bridge.authorize("shell", "git push origin main");
		expect(res.verdict).toBe("deny");
	});

	it("authorizes write commands with confirm", async () => {
		const res = await bridge.authorize("write", "foo.txt");
		expect(res.verdict).toBe("confirm");
	});

	it("verifies passing check returning passed: true", async () => {
		const res = await bridge.verify("verify.ok");
		expect(res.check_id).toBe("verify.ok");
		expect(res.passed).toBe(true);
		expect(res.exit_code).toBe(0);
	});

	it("verifies failing check returning passed: false without crashing", async () => {
		const res = await bridge.verify("verify.fail");
		expect(res.check_id).toBe("verify.fail");
		expect(res.passed).toBe(false);
		expect(res.exit_code).not.toBe(0);
	});
});
