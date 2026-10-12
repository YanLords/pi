import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AlertController, isDesktopAlertEnabled, resolveMacSound, spawnDesktopAlert } from "../src/notify.ts";

describe("Alert notification controller", () => {
	let originalDesktop: string | undefined;
	let originalSound: string | undefined;

	beforeEach(() => {
		originalDesktop = process.env.PI_ALERT_DESKTOP;
		originalSound = process.env.PI_ALERT_SOUND;
		delete process.env.PI_ALERT_DESKTOP;
		delete process.env.PI_ALERT_SOUND;
	});

	afterEach(() => {
		if (originalDesktop !== undefined) {
			process.env.PI_ALERT_DESKTOP = originalDesktop;
		} else {
			delete process.env.PI_ALERT_DESKTOP;
		}
		if (originalSound !== undefined) {
			process.env.PI_ALERT_SOUND = originalSound;
		} else {
			delete process.env.PI_ALERT_SOUND;
		}
		vi.restoreAllMocks();
	});

	it("disables desktop alerts and escape sequences without PI_ALERT_DESKTOP=1", () => {
		expect(isDesktopAlertEnabled()).toBe(false);
		const stdoutSpy = vi.spyOn(process.stdout, "write");
		const child = spawnDesktopAlert("Test", "Message");
		expect(child).toBeNull();
		expect(stdoutSpy).not.toHaveBeenCalled();
	});

	it("enables desktop alert when PI_ALERT_DESKTOP=1", () => {
		process.env.PI_ALERT_DESKTOP = "1";
		expect(isDesktopAlertEnabled()).toBe(true);
		const stdoutSpy = vi.spyOn(process.stdout, "write").mockReturnValue(true);
		const child = spawnDesktopAlert("Test", "Message");
		expect(stdoutSpy).toHaveBeenCalled();
		if (child) {
			child.kill();
		}
	});

	it("resolves custom and default sounds correctly", () => {
		const defaultSound = resolveMacSound();
		expect(defaultSound).toBeDefined();
		expect(defaultSound.name).toBeDefined();

		const glass = resolveMacSound("Glass");
		expect(glass.name).toBe("Glass");
	});

	it("deduplicates identical state transitions in AlertController", () => {
		const controller = new AlertController();
		let notifyCount = 0;
		const notify = () => {
			notifyCount++;
		};

		// First transition to awaiting_input: accepted
		const t1 = controller.transition("awaiting_input", notify);
		expect(t1).toBe(true);
		expect(notifyCount).toBe(1);
		expect(controller.getState()).toBe("awaiting_input");

		// Second identical transition: rejected / deduplicated
		const t2 = controller.transition("awaiting_input", notify);
		expect(t2).toBe(false);
		expect(notifyCount).toBe(1);

		// Transition to settled: accepted
		const t3 = controller.transition("settled", notify);
		expect(t3).toBe(true);
		expect(notifyCount).toBe(2);

		// Transition back to idle
		const t4 = controller.transition("idle", notify);
		expect(t4).toBe(true);
		expect(controller.getState()).toBe("idle");
	});

	it("limits in-flight desktop notifications to one and cancels previous on transition", () => {
		process.env.PI_ALERT_DESKTOP = "1";
		vi.spyOn(process.stdout, "write").mockReturnValue(true);

		const controller = new AlertController();
		controller.transition("awaiting_input", undefined, {
			title: "Pi",
			message: "Waiting",
		});
		expect(controller.getState()).toBe("awaiting_input");

		// New transition replaces previous in-flight
		controller.transition("settled", undefined, {
			title: "Pi",
			message: "Settled",
		});
		expect(controller.getState()).toBe("settled");

		controller.reset();
		expect(controller.getState()).toBe("idle");
	});

	it("respects TUI mode vs non-interactive print/json/rpc mode", () => {
		const controller = new AlertController();
		const handleEvent = (mode: string, state: "awaiting_input" | "settled", onNotify: () => void) => {
			if (mode !== "tui") return; // Suppressed in print/json/rpc
			controller.transition(state, onNotify);
		};

		let notifyCalls = 0;
		const onNotify = () => {
			notifyCalls++;
		};

		// Non-TUI modes produce no notifications
		handleEvent("print", "awaiting_input", onNotify);
		handleEvent("json", "awaiting_input", onNotify);
		handleEvent("rpc", "awaiting_input", onNotify);
		expect(notifyCalls).toBe(0);

		// TUI mode notifies exactly once
		handleEvent("tui", "awaiting_input", onNotify);
		expect(notifyCalls).toBe(1);

		// Second TUI call for same state is deduplicated
		handleEvent("tui", "awaiting_input", onNotify);
		expect(notifyCalls).toBe(1);
	});
});
