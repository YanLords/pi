import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type {
	ExtensionAPI,
	ExtensionCommandContext,
	ExtensionContext,
	ExtensionToolContext,
	RegisteredCommand,
	ToolCallEvent,
	ToolCallEventResult,
	ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { AgonStatusResult, AgonVerifyResult } from "../src/bridge.ts";
import { agonExtension } from "../src/extension.ts";

function setupFixture(name: string): string {
	const dir = join(tmpdir(), `agon-ext-test-${name}-${Date.now()}`);
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

type EventHandler = (event: unknown, ctx: ExtensionContext) => Promise<unknown> | unknown;

describe("agonExtension", () => {
	let fixtureDir: string;
	const handlers: Record<string, EventHandler[]> = {};
	const commands: Record<string, Omit<RegisteredCommand, "name" | "sourceInfo">> = {};
	const tools: Record<string, ToolDefinition> = {};

	const mockApi = {
		on(event: string, handler: EventHandler) {
			if (!handlers[event]) handlers[event] = [];
			handlers[event].push(handler);
		},
		registerCommand(name: string, options: Omit<RegisteredCommand, "name" | "sourceInfo">) {
			commands[name] = options;
		},
		registerTool(tool: ToolDefinition) {
			tools[tool.name] = tool;
		},
	} as unknown as ExtensionAPI;

	const mockContext = (hasUI = false, isTrusted = true): ExtensionContext =>
		({
			cwd: fixtureDir,
			hasUI,
			isProjectTrusted: () => isTrusted,
			ui: {
				notify: () => {},
				confirm: async () => false,
			},
		}) as unknown as ExtensionContext;

	const getToolContext = (): ExtensionToolContext => mockContext() as unknown as ExtensionToolContext;

	beforeAll(async () => {
		fixtureDir = setupFixture("ext");
		agonExtension(mockApi);

		const startHandlers = handlers.session_start ?? [];
		for (const h of startHandlers) {
			await h({ type: "session_start", reason: "startup" }, mockContext());
		}
	});

	afterAll(async () => {
		const shutdownHandlers = handlers.session_shutdown ?? [];
		for (const h of shutdownHandlers) {
			await h({ type: "session_shutdown", reason: "quit" }, mockContext());
		}
		rmSync(fixtureDir, { recursive: true, force: true });
	});

	it("registers commands and tools", () => {
		expect(commands.agon).toBeDefined();
		expect(tools.agon_status).toBeDefined();
		expect(tools.agon_verify).toBeDefined();
		expect(tools.agon_repair).toBeUndefined();
	});

	it("allows read tool calls without blocking", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "1",
			toolName: "read",
			input: { path: "src/main.rs" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext())) as ToolCallEventResult | undefined;
			expect(res).toBeUndefined();
		}
	});

	it("allows allowlisted bash tool call without blocking", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "2",
			toolName: "bash",
			input: { command: "cargo test" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext())) as ToolCallEventResult | undefined;
			expect(res).toBeUndefined();
		}
	});

	it("blocks git push bash tool call by policy", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "3",
			toolName: "bash",
			input: { command: "git push origin main" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext())) as ToolCallEventResult | undefined;
			expect(res?.block).toBe(true);
			expect(res?.reason).toContain("denied");
		}
	});

	it("blocks git push powershell tool call by policy", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "3-ps",
			toolName: "powershell",
			input: { command: "git push origin main" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext())) as ToolCallEventResult | undefined;
			expect(res?.block).toBe(true);
			expect(res?.reason).toContain("denied");
		}
	});

	it("allows allowlisted powershell tool call without blocking", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "2-ps",
			toolName: "powershell",
			input: { command: "cargo test" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext())) as ToolCallEventResult | undefined;
			expect(res).toBeUndefined();
		}
	});

	it("blocks write tool call when running without UI", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const event: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "4",
			toolName: "write",
			input: { path: "hello.txt", content: "hi" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(event, mockContext(false))) as ToolCallEventResult | undefined;
			expect(res?.block).toBe(true);
			expect(res?.reason).toContain("requires human confirmation");
		}
	});

	it("executes agon_status tool and returns valid status", async () => {
		const res = await tools.agon_status.execute("call-1", {}, undefined, undefined, getToolContext());
		expect(res.isError).toBe(false);
		expect(res.details).toBeDefined();
		const status = res.details as AgonStatusResult;
		expect(status.active_checks).toContain("verify.ok");
	});

	it("executes agon_verify tool on passing check", async () => {
		const res = await tools.agon_verify.execute(
			"call-2",
			{ check_id: "verify.ok" },
			undefined,
			undefined,
			getToolContext(),
		);
		expect(res.isError).toBe(false);
		const verifyList = res.details as AgonVerifyResult[];
		expect(verifyList[0].passed).toBe(true);
	});

	it("executes agon_verify tool on failing check", async () => {
		const res = await tools.agon_verify.execute(
			"call-3",
			{ check_id: "verify.fail" },
			undefined,
			undefined,
			getToolContext(),
		);
		expect(res.isError).toBe(true);
		const verifyList = res.details as AgonVerifyResult[];
		expect(verifyList[0].passed).toBe(false);
	});

	it("does not expose agon_repair tool to models", () => {
		expect(tools.agon_repair).toBeUndefined();
	});

	it("/agon repair refuses without interactive UI confirmation", async () => {
		const notified: string[] = [];
		const ctx = {
			cwd: fixtureDir,
			hasUI: false,
			isProjectTrusted: () => true,
			ui: {
				notify: (msg: string) => {
					notified.push(msg);
				},
				confirm: async () => false,
			},
		} as unknown as ExtensionCommandContext;

		await commands.agon.handler("repair verify.ok", ctx);
		expect(notified.some((m) => m.includes("requires interactive UI confirmation"))).toBe(true);
	});

	it("executes /agon init in unconfigured directory", async () => {
		const emptyDir = join(tmpdir(), `agon-empty-init-${Date.now()}`);
		mkdirSync(emptyDir, { recursive: true });

		const notified: string[] = [];
		const ctx = {
			cwd: emptyDir,
			hasUI: false,
			isProjectTrusted: () => true,
			ui: {
				notify: (msg: string) => {
					notified.push(msg);
				},
				confirm: async () => false,
			},
		} as unknown as ExtensionCommandContext;

		await commands.agon.handler("init", ctx);
		expect(notified.some((m) => m.includes("Initialized Agon project in .agon/"))).toBe(true);
		rmSync(emptyDir, { recursive: true, force: true });
	});

	it("blocks mutations but allows read when project is untrusted", async () => {
		const toolCallHandlers = handlers.tool_call ?? [];
		const readEvent: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "untrusted-read",
			toolName: "read",
			input: { path: "src/main.rs" },
		};
		const untrustedCtx = mockContext(false, false);
		for (const h of toolCallHandlers) {
			const res = (await h(readEvent, untrustedCtx)) as ToolCallEventResult | undefined;
			expect(res).toBeUndefined();
		}

		const writeEvent: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "untrusted-write",
			toolName: "write",
			input: { path: "foo.txt", content: "bar" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(writeEvent, untrustedCtx)) as ToolCallEventResult | undefined;
			expect(res?.block).toBe(true);
			expect(res?.reason).toContain("Project is not trusted");
		}
	});

	it("allows read and blocks mutations when bridge is unavailable in configured project", async () => {
		const brokenDir = join(tmpdir(), `agon-broken-${Date.now()}`);
		mkdirSync(join(brokenDir, ".agon"), { recursive: true });
		writeFileSync(join(brokenDir, ".agon/config.toml"), "invalid toml {{{");
		const brokenCtx: ExtensionContext = {
			cwd: brokenDir,
			hasUI: false,
			isProjectTrusted: () => true,
			ui: { notify: () => {}, confirm: async () => false },
		} as unknown as ExtensionContext;

		const toolCallHandlers = handlers.tool_call ?? [];
		const readEvent: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "broken-read",
			toolName: "read",
			input: { path: "src/main.rs" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(readEvent, brokenCtx)) as ToolCallEventResult | undefined;
			expect(res).toBeUndefined();
		}

		const bashEvent: ToolCallEvent = {
			type: "tool_call",
			toolCallId: "broken-bash",
			toolName: "bash",
			input: { command: "cargo test" },
		};
		for (const h of toolCallHandlers) {
			const res = (await h(bashEvent, brokenCtx)) as ToolCallEventResult | undefined;
			expect(res?.block).toBe(true);
			expect(res?.reason).toContain("bridge unavailable");
		}
		rmSync(brokenDir, { recursive: true, force: true });
	});

	it("verifies multi-check when check_id is omitted", async () => {
		const res = await tools.agon_verify.execute("call-multi", {}, undefined, undefined, getToolContext());
		expect(res.details).toBeDefined();
		const results = res.details as AgonVerifyResult[];
		expect(Array.isArray(results)).toBe(true);
		expect(results.length).toBeGreaterThanOrEqual(1);
		for (const r of results) {
			expect(r.check_id).toBeDefined();
			expect(typeof r.passed).toBe("boolean");
		}
	});

	it("session_start executes asynchronously without blocking", async () => {
		const startHandlers = handlers.session_start ?? [];
		const start = performance.now();
		for (const h of startHandlers) {
			await h({ type: "session_start", reason: "startup" }, mockContext());
		}
		const elapsed = performance.now() - start;
		expect(elapsed).toBeLessThan(50);
	});
});
