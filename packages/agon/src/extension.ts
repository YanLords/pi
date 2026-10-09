import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type {
	AgentSettledEvent,
	ExtensionAPI,
	ExtensionCommandContext,
	ExtensionContext,
	ToolCallEvent,
	ToolCallEventResult,
	ToolResultEvent,
	ToolResultEventResult,
	UIPromptStartEvent,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { AgonBridge, type AgonRepairResult, type AgonVerifyResult } from "./bridge.ts";

import { clearAlertNotification, sendAlertNotification } from "./notify.ts";

interface ToolActionMapping {
	action: "read" | "search" | "write" | "edit" | "shell";
	detail: string;
}

function mapToolCall(event: ToolCallEvent): ToolActionMapping | null {
	switch (event.toolName) {
		case "read": {
			const path = typeof event.input?.path === "string" ? event.input.path : "";
			return { action: "read", detail: path };
		}
		case "grep": {
			const pattern = typeof event.input?.pattern === "string" ? event.input.pattern : "";
			return { action: "search", detail: pattern };
		}
		case "find": {
			const pattern = typeof event.input?.pattern === "string" ? event.input.pattern : "";
			return { action: "search", detail: pattern };
		}
		case "ls": {
			const path = typeof event.input?.path === "string" ? event.input.path : "";
			return { action: "search", detail: path };
		}
		case "write": {
			const path = typeof event.input?.path === "string" ? event.input.path : "";
			return { action: "write", detail: path };
		}
		case "edit": {
			const path = typeof event.input?.path === "string" ? event.input.path : "";
			return { action: "edit", detail: path };
		}
		case "bash": {
			const command = typeof event.input?.command === "string" ? event.input.command : "";
			return { action: "shell", detail: command };
		}
		default:
			return null;
	}
}
const DEFAULT_CONFIG_TOML = `[form]
modules = []
tools = []
capabilities = []
checks = []

[jev]
base_url = "https://openrouter.ai/api"
model = "typesafe/jev-1.13"
api_key_env = "OPENROUTER_API_KEY"
max_retries = 3

[jev.thresholds]
noul_yes = 0.5
choice_min_confidence = 0.6
mutation_min_confidence = 0.75

[morph.budget]
max_mutations_per_task = 3
max_consecutive_failures = 3
max_same_form_retries = 1

[permissions]
read = "allow"
search = "allow"
write = "allow"
edit = "allow"
shell = "confirm"
git_commit = "confirm"
git_push = "deny"
shell_allow = [
	"npm test",
	"npm run",
	"cargo test",
	"cargo build",
	"cargo check",
	"git status",
	"git diff",
	"git log",
]
[runner]
timeout_secs = 300
env = []
`;

const DEFAULT_CHECKS_TOML = `# Checks: define verification commands
# [check."verify.build"]
# command = "cargo build"
# success = "exit_code == 0"
`;

const DEFAULT_EXTRACTORS_TOML = `# Extractors: observation -> signature
`;

const DEFAULT_CANDIDATES_TOML = `# Candidate mutation generation rules
`;

export function initAgon(cwd: string): { created: string[]; kept: string[] } {
	const agonDir = join(cwd, ".agon");
	mkdirSync(agonDir, { recursive: true });

	const files: [string, string][] = [
		["config.toml", DEFAULT_CONFIG_TOML],
		["checks.toml", DEFAULT_CHECKS_TOML],
		["extractors.toml", DEFAULT_EXTRACTORS_TOML],
		["candidates.toml", DEFAULT_CANDIDATES_TOML],
		[".gitignore", "sessions/\n"],
	];

	const created: string[] = [];
	const kept: string[] = [];

	for (const [name, content] of files) {
		const filePath = join(agonDir, name);
		if (existsSync(filePath)) {
			kept.push(name);
		} else {
			writeFileSync(filePath, content, "utf8");
			created.push(name);
		}
	}

	return { created, kept };
}

async function createBridge(ctx: ExtensionContext): Promise<AgonBridge> {
	const env: NodeJS.ProcessEnv = {};
	try {
		const openRouterKey = await ctx.modelRegistry?.getApiKeyForProvider("openrouter");
		if (openRouterKey) {
			env.OPENROUTER_API_KEY = openRouterKey;
		}
	} catch {
		// Ignore
	}
	try {
		const typesafeKey = await ctx.modelRegistry?.getApiKeyForProvider("typesafe");
		if (typesafeKey) {
			env.TYPESAFE_API_KEY = typesafeKey;
		}
	} catch {
		// Ignore
	}

	return new AgonBridge({
		cwd: ctx.cwd,
		env,
		onLog: (line) => {
			if (ctx.hasUI) {
				ctx.ui.notify(line.trimEnd(), "info");
			}
		},
	});
}

export function agonExtension(pi: ExtensionAPI): void {
	let bridge: AgonBridge | null = null;
	let hasAgon = false;
	let isRepairActive = false;

	pi.on("session_start", async (_event, ctx: ExtensionContext) => {
		const agonDir = join(ctx.cwd, ".agon");
		hasAgon = existsSync(agonDir);

		if (!hasAgon) {
			bridge = null;
			return;
		}

		try {
			bridge = await createBridge(ctx);
			await bridge.start();
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			if (ctx.hasUI) {
				ctx.ui.notify(`Failed to start Agon sidecar: ${message}`, "error");
			}
		}
	});

	pi.on("session_shutdown", async () => {
		if (bridge) {
			const active = bridge;
			bridge = null;
			await active.shutdown();
		}
	});
	pi.on("ui_prompt_start", (event: UIPromptStartEvent) => {
		const kindLabel = event.kind === "confirm" ? "Confirmation demandée" : "Saisie requise";
		sendAlertNotification("Pi", event.title ? `${kindLabel}: ${event.title}` : "Pi attend votre réponse");
	});

	pi.on("agent_settled", (event: AgentSettledEvent) => {
		if (!event.aborted) {
			sendAlertNotification("Pi", "Tâche terminée, en attente de vos instructions");
		}
	});
	pi.on("ui_prompt_end", () => {
		clearAlertNotification();
	});

	pi.on("turn_start", () => {
		clearAlertNotification();
	});

	pi.on("tool_call", async (event: ToolCallEvent, ctx: ExtensionContext): Promise<ToolCallEventResult | undefined> => {
		if (!hasAgon) {
			return undefined;
		}

		const mapped = mapToolCall(event);
		if (!mapped) {
			return undefined;
		}

		if (!bridge) {
			if (mapped.action === "read" || mapped.action === "search") {
				return undefined;
			}
			return {
				block: true,
				reason: "Agon daemon bridge unavailable: mutative action blocked for safety.",
			};
		}

		try {
			const auth = await bridge.authorize(mapped.action, mapped.detail);
			if (auth.verdict === "deny") {
				return {
					block: true,
					reason: auth.reason ?? `${mapped.action} is denied by Agon policy.`,
				};
			}

			if (auth.verdict === "confirm") {
				if (!ctx.hasUI) {
					return {
						block: true,
						reason: `Agon policy requires human confirmation (${auth.reason ?? "action requires confirmation"}), but running in non-interactive mode.`,
					};
				}

				sendAlertNotification(
					"Pi / Agon",
					auth.reason ?? `Confirmation requise: ${mapped.action} (${mapped.detail})`,
				);
				const confirmed = await ctx.ui.confirm(
					"Agon Policy Confirmation",
					auth.reason ?? `Confirm ${mapped.action}: ${mapped.detail}`,
				);
				clearAlertNotification();

				if (!confirmed) {
					return {
						block: true,
						reason: `Action blocked: user rejected Agon policy confirmation for ${mapped.action}.`,
					};
				}
			}

			return undefined;
		} catch (err) {
			if (mapped.action === "read" || mapped.action === "search") {
				return undefined;
			}
			return {
				block: true,
				reason: `Agon authorization error: ${err instanceof Error ? err.message : String(err)}`,
			};
		}
	});

	pi.on("tool_result", (event: ToolResultEvent): ToolResultEventResult | undefined => {
		if (event.toolName === "agon_verify") {
			return undefined;
		}
		return undefined;
	});

	pi.registerCommand("agon", {
		description: "Agon safety kernel commands: status, verify, repair, reset",
		handler: async (args: string, ctx: ExtensionCommandContext) => {
			const trimmed = args.trim();
			const [subcommand, ...rest] = trimmed.split(/\s+/).filter(Boolean);

			if (subcommand === "init") {
				const result = initAgon(ctx.cwd);
				hasAgon = true;
				try {
					if (!bridge) {
						bridge = await createBridge(ctx);
						await bridge.start();
					}
					ctx.ui.notify(
						`Initialized Agon project in .agon/\nCreated: ${result.created.join(", ") || "none"}\nKept: ${result.kept.join(", ") || "none"}\nNext: define checks in .agon/checks.toml and add them to [form] checks in .agon/config.toml`,
						"info",
					);
				} catch (e) {
					ctx.ui.notify(
						`Created .agon/ files, but sidecar failed to start: ${e instanceof Error ? e.message : String(e)}`,
						"warning",
					);
				}
				return;
			}

			if (!hasAgon) {
				ctx.ui.notify(
					"Agon is not configured in this directory (.agon missing). Run /agon init to initialize.",
					"warning",
				);
				return;
			}

			if (!bridge) {
				ctx.ui.notify("Agon bridge is not currently running", "error");
				return;
			}

			switch (subcommand) {
				case "status": {
					try {
						const status = await bridge.status();
						const violations =
							status.tampering_violations.length > 0
								? `\nTampering violations: ${status.tampering_violations.join(", ")}`
								: "";
						ctx.ui.notify(
							`Form: ${status.form_id}\nLock: ${status.lock_hash}\nActive checks: ${status.active_checks.join(", ") || "none"}\nRegistry mutations: ${status.registry_size}${violations}`,
							status.tampering_violations.length > 0 ? "warning" : "info",
						);
					} catch (e) {
						ctx.ui.notify(`Agon status failed: ${e instanceof Error ? e.message : String(e)}`, "error");
					}
					break;
				}

				case "verify": {
					const checkId = rest[0];
					try {
						const res = await bridge.verify(checkId, ctx.signal);
						const passed = res.passed ? "PASS" : "FAIL";
						const level = res.passed ? "info" : "error";
						ctx.ui.notify(
							`Check ${res.check_id}: ${passed} (exit code ${res.exit_code ?? "none"})\nSignature: ${res.signature.class}`,
							level,
						);
					} catch (e) {
						ctx.ui.notify(`Verification failed: ${e instanceof Error ? e.message : String(e)}`, "error");
					}
					break;
				}

				case "repair": {
					if (isRepairActive) {
						ctx.ui.notify("A repair operation is already in progress", "warning");
						return;
					}

					const checkId = rest[0];
					if (!checkId) {
						ctx.ui.notify("Usage: /agon repair <check_id>", "warning");
						return;
					}

					if (ctx.hasUI) {
						const confirmed = await ctx.ui.confirm(
							"Agon Repair Confirmation",
							`Launch automated Agon kernel repair for check "${checkId}"?`,
						);
						if (!confirmed) {
							ctx.ui.notify("Repair cancelled by user", "info");
							return;
						}
					}

					isRepairActive = true;
					try {
						ctx.ui.notify(`Starting Agon repair for ${checkId}...`, "info");
						const res = await bridge.repair(
							checkId,
							(event, _data) => {
								ctx.ui.notify(`[Agon] ${event}`, "info");
							},
							ctx.signal,
						);
						const level = res.outcome === "Fixed" || res.outcome === "Passed" ? "info" : "warning";
						ctx.ui.notify(`Repair outcome: ${res.outcome} ${res.why ? `(${res.why})` : ""}`, level);
					} catch (e) {
						ctx.ui.notify(`Repair failed: ${e instanceof Error ? e.message : String(e)}`, "error");
					} finally {
						isRepairActive = false;
					}
					break;
				}

				case "reset": {
					if (isRepairActive) {
						ctx.ui.notify("Cannot reset while a repair operation is in progress", "warning");
						return;
					}
					try {
						await bridge.shutdown();
						bridge = await createBridge(ctx);
						await bridge.start();
					} catch (e) {
						ctx.ui.notify(`Reset failed: ${e instanceof Error ? e.message : String(e)}`, "error");
					}
					break;
				}
				case "notify": {
					const soundName = rest[0];
					sendAlertNotification("Pi / Agon", "Test de notification : Pi attend vos instructions", soundName);
					ctx.ui.notify(`Notification sonore (${soundName || "défaut"}) et visuelle envoyée.`, "info");
					break;
				}

				default:
					ctx.ui.notify("Usage: /agon <init|status|verify|repair|reset|notify> [check_id]", "info");
					break;
			}
		},
	});
	pi.registerCommand("init", {
		description: "Initialize Agon project in .agon/",
		handler: async (_args: string, ctx: ExtensionCommandContext) => {
			const result = initAgon(ctx.cwd);
			hasAgon = true;
			try {
				if (!bridge) {
					bridge = await createBridge(ctx);
					await bridge.start();
				}
				ctx.ui.notify(
					`Initialized Agon project in .agon/\nCreated: ${result.created.join(", ") || "none"}\nKept: ${result.kept.join(", ") || "none"}\nNext: define checks in .agon/checks.toml and add them to [form] checks in .agon/config.toml`,
					"info",
				);
			} catch (e) {
				ctx.ui.notify(
					`Created .agon/ files, but sidecar failed to start: ${e instanceof Error ? e.message : String(e)}`,
					"warning",
				);
			}
		},
	});

	pi.registerTool({
		name: "agon_status",
		label: "Agon status",
		description: "Get current Agon safety status, active checks, lock hash, budget, and tampering violations.",
		parameters: Type.Object({}),
		execute: async () => {
			if (!hasAgon) {
				return {
					content: [{ type: "text", text: "Agon is not configured (.agon directory missing)." }],
					details: undefined,
					isError: false,
				};
			}
			if (!bridge) {
				return {
					content: [{ type: "text", text: "Agon daemon bridge is unavailable." }],
					details: undefined,
					isError: true,
				};
			}

			try {
				const status = await bridge.status();
				return {
					content: [{ type: "text", text: JSON.stringify(status, null, 2) }],
					details: status,
					isError: false,
				};
			} catch (e) {
				return {
					content: [
						{ type: "text", text: `Error fetching Agon status: ${e instanceof Error ? e.message : String(e)}` },
					],
					details: undefined,
					isError: true,
				};
			}
		},
	});

	pi.registerTool({
		name: "agon_verify",
		label: "Agon verify",
		description: "Run and verify an Agon check or all active checks, returning objective proof and signature.",
		parameters: Type.Object({
			check_id: Type.Optional(
				Type.String({ description: "Optional specific check identifier (e.g. 'verify.build')" }),
			),
		}),
		execute: async (_toolCallId, params) => {
			if (!hasAgon) {
				return {
					content: [{ type: "text", text: "Agon is not configured (.agon directory missing)." }],
					details: undefined,
					isError: false,
				};
			}
			if (!bridge) {
				return {
					content: [{ type: "text", text: "Agon daemon bridge is unavailable." }],
					details: undefined,
					isError: true,
				};
			}
			try {
				const res: AgonVerifyResult = await bridge.verify(params.check_id);
				const stderrSnippet = res.stderr ? res.stderr.split("\n").slice(0, 10).join("\n") : "";
				const summary = {
					check_id: res.check_id,
					passed: res.passed,
					exit_code: res.exit_code,
					timed_out: res.timed_out,
					signature_class: res.signature.class,
					signature_fields: res.signature.fields,
					stderr_preview: stderrSnippet,
					rerun_hint: `Use /agon verify ${res.check_id} to rerun interactively.`,
				};

				return {
					content: [{ type: "text", text: JSON.stringify(summary, null, 2) }],
					details: res,
					isError: !res.passed,
				};
			} catch (e) {
				return {
					content: [
						{
							type: "text",
							text: `Verification execution failed: ${e instanceof Error ? e.message : String(e)}`,
						},
					],
					details: undefined,
					isError: true,
				};
			}
		},
	});
	pi.registerTool({
		name: "agon_repair",
		label: "Agon repair",
		description:
			"Trigger an automated causal repair loop on a failing check. Agon reproduces the failure, detects the signature, evaluates candidate mutations (from candidates.toml), verifies causal proof, and commits the validated mutation to the Form and mutation registry.",
		parameters: Type.Object({
			check_id: Type.String({
				description: "Identifier of the check to repair (e.g. 'verify.test')",
			}),
		}),
		execute: async (_toolCallId, params, signal, _onUpdate, ctx) => {
			if (!hasAgon) {
				return {
					content: [{ type: "text", text: "Agon is not configured (.agon directory missing)." }],
					details: undefined,
					isError: false,
				};
			}
			if (!bridge) {
				return {
					content: [{ type: "text", text: "Agon daemon bridge is unavailable." }],
					details: undefined,
					isError: true,
				};
			}
			if (isRepairActive) {
				return {
					content: [{ type: "text", text: "Another Agon repair operation is already in progress." }],
					details: undefined,
					isError: true,
				};
			}

			isRepairActive = true;
			try {
				if (ctx.hasUI) {
					ctx.ui.notify(`[Agon] Starting autonomous repair loop for check "${params.check_id}"...`, "info");
				}
				const res: AgonRepairResult = await bridge.repair(
					params.check_id,
					(event, _data) => {
						if (ctx.hasUI) {
							ctx.ui.notify(`[Agon] ${event}`, "info");
						}
					},
					signal,
				);

				const isSuccess = res.outcome === "Fixed" || res.outcome === "Passed";
				return {
					content: [{ type: "text", text: JSON.stringify(res, null, 2) }],
					details: res,
					isError: !isSuccess,
				};
			} catch (e) {
				return {
					content: [
						{
							type: "text",
							text: `Repair execution failed: ${e instanceof Error ? e.message : String(e)}`,
						},
					],
					details: undefined,
					isError: true,
				};
			} finally {
				isRepairActive = false;
			}
		},
	});
}
