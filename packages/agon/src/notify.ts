import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

function resolveMacSound(override?: string): { name: string; file: string } {
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

export function sendAlertNotification(title: string, message: string, soundOverride?: string): void {
	// 1. Terminal / Editor notifications for Zed and VS Code:
	// - BEL (\x07): Zed uses this directly to trigger its in-editor visual pop-up toast and audio chime
	//   when the terminal pane is not focused (Zed Terminal Threads notifications).
	//   VS Code uses it for the terminal tab bell icon, audio chime, and activity badge.
	// - OSC 0 (\x1b]0;...\x07): Changes the terminal tab title in Zed / VS Code to show a bell icon and alert.
	// - OSC 9 & OSC 777: Standard terminal notification sequences parsed by terminal integrations.
	// - OSC 633;D: VS Code Shell Integration command finished/alert signal.
	try {
		const sequences = [
			"\x07", // BEL: Zed pop-up + VS Code tab alert badge
			`\x1b]0;🔔 [Pi attend] ${title}\x07`, // Tab title in Zed & VS Code
			`\x1b]9;${title}: ${message}\x07`, // OSC 9
			`\x1b]777;notify;${title};${message}\x1b\\`, // OSC 777
			"\x1b]633;D\x07", // VS Code shell integration alert
		];
		process.stdout.write(sequences.join(""));
	} catch {
		// Ignore
	}

	// 2. Native OS desktop notification fallback
	if (process.platform === "darwin") {
		const sound = resolveMacSound(soundOverride);
		const safeMessage = message.replace(/[\\"]/g, "\\$&");
		const safeTitle = title.replace(/[\\"]/g, "\\$&");
		const script = `display notification "${safeMessage}" with title "${safeTitle}" sound name "${sound.name}"`;
		execFile("osascript", ["-e", script], () => {});
		execFile("afplay", [sound.file], () => {});
	} else if (process.platform === "linux") {
		execFile("notify-send", [title, message], () => {});
	} else if (process.platform === "win32") {
		const psScript = `
$title = "${title.replace(/"/g, '`"')}";
$msg = "${message.replace(/"/g, '`"')}";
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02);
$text = $template.GetElementsByTagName("text");
$text[0].AppendChild($template.CreateTextNode($title)) | Out-Null;
$text[1].AppendChild($template.CreateTextNode($msg)) | Out-Null;
$toast = [Windows.UI.Notifications.ToastNotification]::new($template);
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("Pi").Show($toast);
`;
		execFile("powershell", ["-NoProfile", "-Command", psScript], () => {});
	}
}

export function clearAlertNotification(): void {
	try {
		// Restore terminal tab title
		process.stdout.write("\x1b]0;Pi\x07");
	} catch {
		// Ignore
	}
}
