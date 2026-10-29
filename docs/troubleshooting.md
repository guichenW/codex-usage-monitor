# Troubleshooting Codex Usage

## Taskbar error labels

Codex Usage keeps authentication failures separate from transient service failures:

| Simplified Chinese | Other languages | Meaning | Recommended action |
|---|---|---|---|
| `!` | `!` | Enabled service credentials are missing or expired | Sign in with the relevant CLI/app, then refresh Codex Usage. Claude Desktop login does not count as Claude Code CLI login. |
| `网络` | `NET` | Network or TLS connection failed | Check connectivity, VPN, proxy, and firewall settings |
| `限流` | `429` | Provider rate limit | Wait for the provider retry window; Codex Usage retries with backoff |
| `服务` | `5XX` | Provider service failure | Wait and retry; check provider status if it persists |
| `错误` | `ERR` | Invalid or unsupported response | Enable diagnostics and inspect the log |

Authentication failures pause provider polling until the credential source changes, preventing repeated login notifications. Transient failures use exponential backoff up to the configured refresh interval.

## Diagnostic log

Run:

```powershell
codex-usage.exe --diagnose
```

The log is written to `%TEMP%\codex-usage.log`. It includes:

- application version and executable path
- install channel (direct or portable)
- provider failure category and retry delay
- window creation, taskbar placement, and relaunch events

The log does not include access tokens, refresh tokens, credential file contents, or API response bodies.

## Proxy settings

Open **Settings > Proxy settings...** from the taskbar widget or tray menu.

- **Use environment variables** preserves the existing `HTTP_PROXY`, `HTTPS_PROXY`, and `ALL_PROXY` behavior.
- **Use a custom HTTP proxy** saves an address such as `http://127.0.0.1:7897`. An HTTP proxy also tunnels HTTPS requests. A bare `127.0.0.1:7897` address is normalized automatically.
- **Connect directly** bypasses proxy environment variables for the monitor's requests and Windows token-refresh subprocesses.

The preference is saved under `proxy` in `%APPDATA%\CodexUsage\settings.json` and is restored at startup. It applies to usage requests, GitHub update checks/downloads, and proxy variables passed to local token-refresh commands. Saving triggers a manual refresh; an existing in-flight request can finish before its queued refresh uses the new setting. Existing server rate-limit deadlines still apply.

Only HTTP proxy addresses are accepted by the custom mode. Cancel leaves the saved preference unchanged. Diagnostic logs identify the selected mode without printing the proxy address or credentials.

## Weekly Codex warm-up

Open **Settings > Codex scheduled warm-up...**. The switch is off by default. Each weekday has its own list of local `HH:MM` times; a blank day is disabled. The common time field can populate all seven days at once. Leave the model blank to prefer a lightweight model from the signed-in account's catalog, or specify a model identifier.

While enabled and the monitor is running, a 15-second timer checks the current local minute. Due slots submit one small Responses request through the configured proxy, using the current Codex login. The response must finish with `response.completed`; HTTP 200 alone is not considered success. No tools are declared or executed.

Missed minutes are not replayed after startup, sleep or a clock jump. Each date/time slot is claimed on disk before network work in `%APPDATA%\CodexUsage\codex-warmup-history.json`, preventing restart duplicates. Failed or interrupted attempts are not retried for that same slot. Server Retry-After deadlines also block later slots. The settings dialog displays the latest attempt; diagnostic logs include its result and token count when reported.

This sends a real model request and consumes usage. It does not force a running server quota window to reset. The backend protocol can change; actual account access and activation require a live test. To test a schedule, temporarily set one time one or two minutes ahead, enable it, and inspect the history and quota display afterward.

## Update failures

Direct installations and portable copies download only the exact `codex-usage.exe` asset and verify it against `codex-usage.exe.sha256` from the same GitHub Release. The updater keeps the previous EXE until the downloaded version has been installed and restarted successfully. If restart fails, the old EXE is restored.

## Reset local position without deleting settings

Right-click the taskbar component and choose **Settings > Reset Position**. Settings are stored at `%APPDATA%\CodexUsage\settings.json`.

## Reinstall while preserving settings

Normal uninstall keeps the settings file. Reinstalling restores the saved language, refresh interval, provider selection, widget visibility, and taskbar position. Use `uninstall.ps1 -RemoveSettings` only when a full reset is intended.
