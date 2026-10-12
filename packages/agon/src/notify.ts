import { type ChildProcess, execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export type AlertState = "idle" | "awaiting_input" | "settled";

export function isDesktopAlertEnabled(): boolean {
	return process.env.PI_ALERT_DESKTOP === "1";
}

export function resolveMacSound(override?: string): { name: string; file: string } {
	const chosen = override || process.env.PI_ALERT_SOUND || process.env.AGON_ALERT_SOUND || "mario";

	// 1. Direct path
	if (existsSync(chosen)) {
		return { name: "Glass", file: chosen };
	}

	// 2. User sounds in ~/Library/Sounds/
	const userSoundsDir = join(homedir(), "Library", "Sounds");
	const userCandidate = join(userSoundsDir, chosen);
	if (existsSync(userCandidate)) {
		return { name: "Glass", file: userCandidate };
	}
	for (const ext of [".wav", ".aiff", ".mp3", ".m4a"]) {
		const withExt = `${userCandidate}${ext}`;
		if (existsSync(withExt)) {
			return { name: "Glass", file: withExt };
		}
	}

	// 3. System sounds in /System/Library/Sounds/
	const sysCandidate = `/System/Library/Sounds/${chosen}.aiff`;
	if (existsSync(sysCandidate)) {
		return { name: chosen, file: sysCandidate };
	}

	return { name: "Glass", file: "/System/Library/Sounds/Glass.aiff" };
}

export function spawnDesktopAlert(title: string, message: string, soundOverride?: string): ChildProcess | null {
	if (!isDesktopAlertEnabled()) {
		return null;
	}

	// Write terminal escape sequences only when desktop alert is explicitly enabled
	try {
		const sequences = [
			"\x07", // BEL
			`\x1b]0;[Pi] ${title}\x07`, // OSC 0
			`\x1b]9;${title}: ${message}\x07`, // OSC 9
			`\x1b]777;notify;${title};${message}\x1b\\`, // OSC 777
			"\x1b]633;D\x07", // OSC 633;D
		];
		process.stdout.write(sequences.join(""));
	} catch {
		// Ignore
	}

	if (process.platform === "darwin") {
		const sound = resolveMacSound(soundOverride);
		// Safe AppleScript with argv - no string interpolation into code
		const script =
			"on run argv\n display notification (item 2 of argv) with title (item 1 of argv) sound name (item 3 of argv)\nend run";
		const child = execFile("osascript", ["-e", script, title, message, sound.name], () => {});
		execFile("afplay", [sound.file], () => {});
		return child;
	}

	if (process.platform === "linux") {
		return execFile("notify-send", [title, message], () => {});
	}

	if (process.platform === "win32") {
		// Safe PowerShell script using parameters
		const psScript = `
param([string]$Title, [string]$Msg)
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$text = $template.GetElementsByTagName("text")
$text[0].AppendChild($template.CreateTextNode($Title)) | Out-Null
$text[1].AppendChild($template.CreateTextNode($Msg)) | Out-Null
$toast = [Windows.UI.Notifications.ToastNotification]::new($template)
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("Pi").Show($toast)
`;
		return execFile("powershell", ["-NoProfile", "-Command", psScript, "-Title", title, "-Msg", message], () => {});
	}

	return null;
}

export class AlertController {
	private currentState: AlertState = "idle";
	private activeChild: ChildProcess | null = null;

	getState(): AlertState {
		return this.currentState;
	}

	transition(
		newState: AlertState,
		onNotify?: () => void,
		desktopPayload?: { title: string; message: string; soundOverride?: string },
	): boolean {
		if (this.currentState === newState) {
			return false; // Deduplicated
		}

		this.currentState = newState;
		if (newState === "idle") {
			this.cancelInFlightDesktop();
			return true;
		}

		onNotify?.();

		if (isDesktopAlertEnabled() && desktopPayload) {
			this.sendDesktop(desktopPayload.title, desktopPayload.message, desktopPayload.soundOverride);
		}

		return true;
	}

	reset(): void {
		this.currentState = "idle";
		this.cancelInFlightDesktop();
	}

	cancelInFlightDesktop(): void {
		if (this.activeChild && !this.activeChild.killed) {
			try {
				this.activeChild.kill();
			} catch {
				// Ignore
			}
			this.activeChild = null;
		}
	}

	sendDesktop(title: string, message: string, soundOverride?: string): void {
		this.cancelInFlightDesktop();
		this.activeChild = spawnDesktopAlert(title, message, soundOverride);
	}
}

export const defaultAlertController = new AlertController();

export function sendAlertNotification(title: string, message: string, soundOverride?: string): void {
	if (isDesktopAlertEnabled()) {
		defaultAlertController.sendDesktop(title, message, soundOverride);
	}
}

export function clearAlertNotification(): void {
	defaultAlertController.reset();
}
