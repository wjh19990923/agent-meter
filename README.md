# Agent Meter

Agent Meter is a lightweight, local-first Windows desktop widget for monitoring Codex and Claude Code token usage and API-equivalent cost estimates.

## Features

- 3 MB standalone Windows executable built with Tauri 2
- Compact always-on-top widget with an expandable detail view
- Codex and Claude Code input, cache, output, and total token counts
- Estimated USD cost using a bundled LiteLLM pricing snapshot
- Seven-day activity charts with calendar dates
- System tray, manual refresh, and launch-at-startup support
- No account credentials, API keys, or cloud service required

## What it reads

- `%USERPROFILE%\.codex\sessions\**\*.jsonl`
- `%USERPROFILE%\.claude\projects\**\*.jsonl`

The application reads these files in place and does not modify them.

## Install

Download the latest portable executable or NSIS installer from the GitHub Releases page. The portable executable can be run directly without installation.

## Development

Prerequisites are Rust stable MSVC, Microsoft C++ Build Tools, WebView2, and Node.js.

```powershell
npm.cmd install
npm.cmd run dev
```

## Checks and Windows build

```powershell
npm.cmd run check
npm.cmd run build
```

The release executable is written to `src-tauri/target/release/agent-meter.exe`. The NSIS installer is written under `src-tauri/target/release/bundle/nsis/`.

## Current scope

- Incremental local-log parsing with a 15-second refresh
- Compact always-on-top widget with an expandable detail view
- System tray and launch-at-startup option
- Codex and Claude Code token breakdowns
- API-equivalent USD cost estimates from a bundled LiteLLM pricing snapshot
- Seven-day charts with explicit calendar-date labels
- No subscription quota scraping
- Costs are API-equivalent estimates, not ChatGPT Pro or Claude subscription bills

## Privacy

Agent Meter reads local JSONL token metadata. It does not send conversation content or usage data over the network.

## Pricing data

The bundled pricing snapshot is derived from the LiteLLM model pricing database, the same upstream pricing source used by ccusage. Refresh it with:

```powershell
npm.cmd run pricing:update -- path\to\model_prices_and_context_window.json
```

Model prices change. Cost values should always be treated as estimates.

## License

MIT. See [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
