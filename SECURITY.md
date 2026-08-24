# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately to the repository maintainers before
opening a public issue. Include affected versions, reproduction steps, expected
impact, and any suggested mitigation. Do not include real credentials, private
screenshots, or personal desktop data.

## Integration requirements

- Run the sidecar with the same or lower integrity level as its host.
- Keep its stdio/RPC channel private to the spawning host process.
- Place `alice-computer-use-broker` in front of every model-originated action.
- Require explicit approval for destructive, security-sensitive, or ambiguous actions.
- Never log screenshot bytes, text from password controls, access tokens, or raw
  Windows security structures.
- Invalidate leases and observations after user interference, display changes,
  foreground-window changes, or process restarts.
- Treat an unverified action as uncertain: observe again instead of repeating it.

## Scope

Security issues include policy bypasses, stale-reference acceptance, protected
surface access, password disclosure, unbounded evidence retention, process-channel
spoofing, and input injection outside the approved target.
