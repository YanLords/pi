import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type {
	AgentSettledEvent,
	ExtensionAPI,
	ExtensionCommandContext,
	ExtensionContext,
	SessionShutdownEvent,
	ToolCallEvent,
	ToolCallEventResult,
	UIPromptStartEvent,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { AgonBridge, type AgonVerifyResult } from "./bridge.ts";

import { AlertController, isDesktopAlertEnabled } from "./notify.ts";

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
		case "powershell": {
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
	if (ctx.modelRegistry) {
		const [openRouterKey, typesafeKey] = await Promise.all([
			ctx.modelRegistry.getApiKeyForProvider("openrouter").catch(() => undefined),
			ctx.modelRegistry.getApiKeyForProvider("typesafe").catch(() => undefined),
		]);
		if (openRouterKey) {
			env.OPENROUTER_API_KEY = openRouterKey;
		}
		if (typesafeKey) {
			env.TYPESAFE_API_KEY = typesafeKey;
		}
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
	let bridgeStartPromise: Promise<AgonBridge> | null = null;
	let hasAgon = false;
	let isRepairActive = false;
	const alertController = new AlertController();

	async function ensureBridge(ctx: ExtensionContext): Promise<AgonBridge> {
		const agonDir = join(ctx.cwd, ".agon");
		hasAgon = existsSync(agonDir);
		if (!hasAgon) {
			throw new Error("Agon is not configured in this directory (.agon missing).");
		}
		if (!ctx.isProjectTrusted()) {
			throw new Error("Project is not trusted: Agon sidecar disabled.");
		}

		if (bridge) {
			if (bridge.cwd === ctx.cwd && bridge.isRunning) {
				return bridge;
			}
			const oldBridge = bridge;
			bridge = null;
			bridgeStartPromise = null;
			await oldBridge.shutdown().catch(() => {});
		}

		if (bridgeStartPromise) {
			return bridgeStartPromise;
		}

		bridgeStartPromise = (async () => {
			try {
				const newBridge = await createBridge(ctx);
				await newBridge.start();
				bridge = newBridge;
				return newBridge;
			} finally {
				bridgeStartPromise = null;
			}
		})();

		return bridgeStartPromise;
	}

	pi.on("session_start", async (_event, ctx: ExtensionContext) => {
		const agonDir = join(ctx.cwd, ".agon");
		hasAgon = existsSync(agonDir);

		if (!hasAgon) {
			if (bridge) {
				const old = bridge;
				bridge = null;
				bridgeStartPromise = null;
				void old.shutdown().catch(() => {});
			}
			return;
		}

		if (!ctx.isProjectTrusted()) {
			if (bridge) {
				const old = bridge;
				bridge = null;
				bridgeStartPromise = null;
				void old.shutdown().catch(() => {});
			}
			if (ctx.hasUI) {
				ctx.ui.notify("Agon sidecar not started: project is not trusted.", "warning");
			}
			return;
		}

		void ensureBridge(ctx).catch((err) => {
			const message = err instanceof Error ? err.message : String(err);
			if (ctx.hasUI) {
				ctx.ui.notify(`Failed to start Agon sidecar: ${message}`, "error");
			}
		});
	});

	pi.on("session_shutdown", async (event: SessionShutdownEvent) => {
		if (event.reason === "quit") {
			if (bridge) {
				const active = bridge;
				bridge = null;
				bridgeStartPromise = null;
				await active.shutdown();
			}
		}
	});
	pi.on("ui_prompt_start", (event: UIPromptStartEvent, ctx: ExtensionContext) => {
		const isTui = ctx.mode === "tui";
		const kindLabel = event.kind === "confirm" ? "Confirmation demandée" : "Saisie requise";
		const title = event.title ? `${kindLabel}: ${event.title}` : "Pi attend votre réponse";

		alertController.transition(
			"awaiting_input",
			() => {
				if (isTui) {
					ctx.ui.notify(title, "info");
				}
			},
			{ title: "Pi", message: title },
		);
	});

	pi.on("agent_settled", (event: AgentSettledEvent, ctx: ExtensionContext) => {
		if (event.aborted) return;
		const isTui = ctx.mode === "tui";
		const message = "Tâche terminée, en attente de vos instructions";

		alertController.transition(
			"settled",
			() => {
				if (isTui) {
					ctx.ui.notify(message, "info");
				}
			},
			{ title: "Pi", message },
		);
	});

	pi.on("ui_prompt_end", () => {
		alertController.transition("idle");
	});

	pi.on("turn_start", () => {
		alertController.transition("idle");
	});
	pi.on("tool_call", async (event: ToolCallEvent, ctx: ExtensionContext): Promise<ToolCallEventResult | undefined> => {
		if (!hasAgon) {
			return undefined;
		}

		const mapped = mapToolCall(event);
		if (!mapped) {
			return undefined;
		}

		if (!ctx.isProjectTrusted()) {
			if (mapped.action === "read" || mapped.action === "search") {
				return undefined;
			}
			return {
				block: true,
				reason: "Project is not trusted: mutative action blocked for safety.",
			};
		}

		let activeBridge: AgonBridge;
		try {
			activeBridge = await ensureBridge(ctx);
		} catch (err) {
			if (mapped.action === "read" || mapped.action === "search") {
				return undefined;
			}
			return {
				block: true,
				reason: `Agon daemon bridge unavailable: mutative action blocked for safety (${err instanceof Error ? err.message : String(err)}).`,
			};
		}

		try {
			const auth = await activeBridge.authorize(mapped.action, mapped.detail);
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

				if (isDesktopAlertEnabled()) {
					alertController.sendDesktop(
						"Pi / Agon",
						auth.reason ?? `Confirmation requise: ${mapped.action} (${mapped.detail})`,
					);
				}
				const confirmed = await ctx.ui.confirm(
					"Agon Policy Confirmation",
					auth.reason ?? `Confirm ${mapped.action}: ${mapped.detail}`,
				);
				alertController.reset();

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

	pi.registerCommand("agon", {
		description: "Agon safety kernel commands: status, verify, repair, reset",
		handler: async (args: string, ctx: ExtensionCommandContext) => {
			const trimmed = args.trim();
			const [subcommand, ...rest] = trimmed.split(/\s+/).filter(Boolean);

			if (subcommand === "init") {
				const result = initAgon(ctx.cwd);
				hasAgon = true;
				try {
					await ensureBridge(ctx);
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

			let activeBridge: AgonBridge;
			try {
				activeBridge = await ensureBridge(ctx);
			} catch (e) {
				ctx.ui.notify(
					`Agon bridge is not currently running: ${e instanceof Error ? e.message : String(e)}`,
					"error",
				);
				return;
			}

			switch (subcommand) {
				case "status": {
					try {
						const status = await activeBridge.status();
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
						const results = await activeBridge.verify(checkId, ctx.signal);
						for (const res of results) {
							const passed = res.passed ? "PASS" : "FAIL";
							const level = res.passed ? "info" : "error";
							ctx.ui.notify(
								`Check ${res.check_id}: ${passed} (exit code ${res.exit_code ?? "none"})\nSignature: ${res.signature.class}`,
								level,
							);
						}
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

					if (!ctx.hasUI) {
						ctx.ui.notify("Agon repair requires interactive UI confirmation", "error");
						return;
					}
					const confirmed = await ctx.ui.confirm(
						"Agon Repair Confirmation",
						`Launch automated Agon kernel repair for check "${checkId}"?`,
					);
					if (!confirmed) {
						ctx.ui.notify("Repair cancelled by user", "info");
						return;
					}

					isRepairActive = true;
					try {
						ctx.ui.notify(`Starting Agon repair for ${checkId}...`, "info");
						const res = await activeBridge.repair(
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
						if (bridge) {
							const old = bridge;
							bridge = null;
							bridgeStartPromise = null;
							await old.shutdown().catch(() => {});
						}
						await ensureBridge(ctx);
						ctx.ui.notify("Agon sidecar restarted successfully", "info");
					} catch (e) {
						ctx.ui.notify(`Reset failed: ${e instanceof Error ? e.message : String(e)}`, "error");
					}
					break;
				}
				case "notify": {
					const soundName = rest[0];
					if (isDesktopAlertEnabled()) {
						alertController.sendDesktop(
							"Pi / Agon",
							"Test de notification : Pi attend vos instructions",
							soundName,
						);
						ctx.ui.notify(`Notification sonore (${soundName || "défaut"}) et visuelle envoyée.`, "info");
					} else {
						ctx.ui.notify(
							`Voie desktop/sonore inactive (activer avec PI_ALERT_DESKTOP=1). Notification UI standard émise: test`,
							"info",
						);
					}
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
				await ensureBridge(ctx);
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
		execute: async (_toolCallId, _params, _signal, _onUpdate, ctx) => {
			if (!hasAgon) {
				return {
					content: [{ type: "text", text: "Agon is not configured (.agon directory missing)." }],
					details: undefined,
					isError: false,
				};
			}

			let activeBridge: AgonBridge;
			try {
				activeBridge = await ensureBridge(ctx);
			} catch (err) {
				return {
					content: [
						{
							type: "text",
							text: `Agon daemon bridge is unavailable: ${err instanceof Error ? err.message : String(err)}`,
						},
					],
					details: undefined,
					isError: true,
				};
			}

			try {
				const status = await activeBridge.status();
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
		execute: async (_toolCallId, params, signal, _onUpdate, ctx) => {
			if (!hasAgon) {
				return {
					content: [{ type: "text", text: "Agon is not configured (.agon directory missing)." }],
					details: undefined,
					isError: false,
				};
			}
			let activeBridge: AgonBridge;
			try {
				activeBridge = await ensureBridge(ctx);
			} catch (err) {
				return {
					content: [
						{
							type: "text",
							text: `Agon daemon bridge is unavailable: ${err instanceof Error ? err.message : String(err)}`,
						},
					],
					details: undefined,
					isError: true,
				};
			}
			try {
				const results: AgonVerifyResult[] = await activeBridge.verify(params.check_id, signal);
				const summary = results.map((res) => {
					const stderrSnippet = res.stderr ? res.stderr.split("\n").slice(0, 10).join("\n") : "";
					return {
						check_id: res.check_id,
						passed: res.passed,
						exit_code: res.exit_code,
						timed_out: res.timed_out,
						signature_class: res.signature.class,
						signature_fields: res.signature.fields,
						stderr_preview: stderrSnippet,
						rerun_hint: `Use /agon verify ${res.check_id} to rerun interactively.`,
					};
				});

				const allPassed = results.length > 0 && results.every((r) => r.passed);
				return {
					content: [{ type: "text", text: JSON.stringify(summary, null, 2) }],
					details: results,
					isError: !allPassed,
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
}
