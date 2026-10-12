import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { AgonBridge, resolveAgonDaemon } from "../src/bridge.ts";
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
		expect(res.tampering_violations).toHaveLength(0);
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
		const results = await bridge.verify("verify.ok");
		expect(results).toHaveLength(1);
		const res = results[0];
		expect(res.check_id).toBe("verify.ok");
		expect(res.passed).toBe(true);
		expect(res.exit_code).toBe(0);
	});

	it("verifies failing check returning passed: false without crashing", async () => {
		const results = await bridge.verify("verify.fail");
		expect(results).toHaveLength(1);
		const res = results[0];
		expect(res.check_id).toBe("verify.fail");
		expect(res.passed).toBe(false);
		expect(res.exit_code).not.toBe(0);
	});

	it("deduplicates concurrent start() calls to a single daemon process", async () => {
		const b = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await Promise.all([b.start(), b.start(), b.start()]);
		expect(b.isRunning).toBe(true);
		await b.shutdown();
	});

	it("times out and closes bridge when an operation exceeds its limit", async () => {
		const b = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await b.start();
		const bAny = b as unknown as { process: { pid: number } };
		if (process.platform !== "win32") {
			process.kill(bAny.process.pid, "SIGSTOP");
		}
		await expect(b.request("authorize", { action: "read", detail: "test" }, undefined, 10)).rejects.toThrow(
			/timed out/,
		);
		expect(b.isRunning).toBe(false);
		await b.shutdown();
	});

	it("treats unknown response ID as a protocol violation and rejects all pending", async () => {
		const b = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await b.start();
		const p1 = b.status();
		// Inject a rogue response with unknown id
		const bAny = b as unknown as {
			handleResponse: (resp: { id: number; ok: boolean; result: unknown }) => void;
		};
		bAny.handleResponse({ id: 999999, ok: true, result: {} });
		await expect(p1).rejects.toThrow(/protocol violation/);
		expect(b.isRunning).toBe(false);
		await b.shutdown();
	});

	it("rejects frame exceeding 8 MiB and closes bridge", async () => {
		const b = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await b.start();
		const oversizedHeader = Buffer.alloc(4);
		oversizedHeader.writeUInt32BE(9 * 1024 * 1024, 0); // 9 MiB
		const bAny = b as unknown as { handleIncomingData: (chunk: Buffer) => void };
		bAny.handleIncomingData(oversizedHeader);
		expect(b.isRunning).toBe(false);
		await expect(b.status()).rejects.toThrow(/closed/);
		await b.shutdown();
	});

	it("aborts a pending request when AbortSignal triggers", async () => {
		const b = new AgonBridge({
			cwd: fixtureDir,
			daemonPath: getTestDaemon(),
		});
		await b.start();
		const ac = new AbortController();
		const req = b.request("status", {}, ac.signal);
		ac.abort();
		await expect(req).rejects.toThrow("Operation aborted");
		await b.shutdown();
	});

	it("respects AGON_DAEMON only when AGON_ALLOW_DAEMON_OVERRIDE=1", () => {
		const originalOverride = process.env.AGON_ALLOW_DAEMON_OVERRIDE;
		const originalDaemon = process.env.AGON_DAEMON;
		try {
			process.env.AGON_DAEMON = "/nonexistent/custom-agon";
			delete process.env.AGON_ALLOW_DAEMON_OVERRIDE;
			// Without override flag, it should not pick /nonexistent/custom-agon
			const resolvedWithoutFlag = resolveAgonDaemon();
			expect(resolvedWithoutFlag).not.toBe("/nonexistent/custom-agon");

			// With flag set to 1, if file doesn't exist, it still skips
			process.env.AGON_ALLOW_DAEMON_OVERRIDE = "1";
			expect(resolveAgonDaemon()).not.toBe("/nonexistent/custom-agon");

			// With flag set to 1 and existing binary, it picks it
			process.env.AGON_DAEMON = resolvedWithoutFlag;
			expect(resolveAgonDaemon()).toBe(resolvedWithoutFlag);
		} finally {
			if (originalOverride !== undefined) {
				process.env.AGON_ALLOW_DAEMON_OVERRIDE = originalOverride;
			} else {
				delete process.env.AGON_ALLOW_DAEMON_OVERRIDE;
			}
			if (originalDaemon !== undefined) {
				process.env.AGON_DAEMON = originalDaemon;
			} else {
				delete process.env.AGON_DAEMON;
			}
		}
	});
});
