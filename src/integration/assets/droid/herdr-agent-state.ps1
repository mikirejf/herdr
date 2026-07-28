# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=droid
# HERDR_INTEGRATION_VERSION=3

param([string]$Action = "")

if ($Action -ne "session") { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

if ($null -eq $payload -or [string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }

# A Droid subagent runs in its own session but fires SessionStart from the same
# pane as the agent that spawned it. Reporting it would re-anchor the pane onto
# a session the user is not driving.
if (-not [string]::IsNullOrWhiteSpace($payload.calling_session_id) -or -not [string]::IsNullOrWhiteSpace($payload.parent_session_id)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }
$sessionStartSource = $payload.source
try {
    if ([string]::IsNullOrWhiteSpace($sessionStartSource)) {
        & $herdr pane report-agent-session $env:HERDR_PANE_ID --source herdr:droid --agent droid --agent-session-id $payload.session_id --seq $seq 2>$null | Out-Null
    } else {
        & $herdr pane report-agent-session $env:HERDR_PANE_ID --source herdr:droid --agent droid --agent-session-id $payload.session_id --seq $seq --session-start-source $sessionStartSource 2>$null | Out-Null
    }
} catch {
}
