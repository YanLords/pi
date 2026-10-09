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
import type { AgonRepairResult, AgonStatusResult, AgonVerifyResult } from "../src/bridge.ts";
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

	const mockContext = (hasUI = false): ExtensionContext =>
		({
			cwd: fixtureDir,
			hasUI,
			ui: {
				notify: () => {},
				confirm: async () => false,
			},
		}) as unknown as ExtensionContext;

	const mockToolContext: ExtensionToolContext = {} as unknown as ExtensionToolContext;

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
		expect(tools.agon_repair).toBeDefined();
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
		const res = await tools.agon_status.execute("call-1", {}, undefined, undefined, mockToolContext);
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
			mockToolContext,
		);
		expect(res.isError).toBe(false);
		const verify = res.details as AgonVerifyResult;
		expect(verify.passed).toBe(true);
	});

	it("executes agon_verify tool on failing check", async () => {
		const res = await tools.agon_verify.execute(
			"call-3",
			{ check_id: "verify.fail" },
			undefined,
			undefined,
			mockToolContext,
		);
		expect(res.isError).toBe(true);
		const verify = res.details as AgonVerifyResult;
		expect(verify.passed).toBe(false);
	});
	it("executes agon_repair tool on already passing check", async () => {
		const res = await tools.agon_repair.execute(
			"call-4",
			{ check_id: "verify.ok" },
			undefined,
			undefined,
			mockToolContext,
		);
		expect(res.isError).toBe(false);
		expect(res.details).toBeDefined();
		const details = res.details as AgonRepairResult;
		expect(details.outcome).toBe("Passed");
	});

	it("executes /agon init in unconfigured directory", async () => {
		const emptyDir = join(tmpdir(), `agon-empty-init-${Date.now()}`);
		mkdirSync(emptyDir, { recursive: true });

		const notified: string[] = [];
		const ctx = {
			cwd: emptyDir,
			hasUI: false,
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
});
