# Security Policy

## Supported Versions

Only the latest code in the default branch is supported for security updates.

## Reporting a Vulnerability

Please report security issues privately:

1. Preferred: GitHub Security Advisory (private report)
2. Fallback: open an issue titled `[SECURITY] ...` without exploit details

Please include:

- Affected version / commit
- Reproduction steps
- Impact assessment
- Suggested fix (if available)

## Secrets and API Keys

- Do not commit API keys, tokens, or local config files.
- This project stores user DeepSeek key locally in:
  - `~/.opcsoskills/config.json`
- Before opening a PR:
  - Check staged files for secrets
  - Confirm `.env*`, local config, and build artifacts are not tracked

## Disclosure Process

- We will acknowledge receipt as soon as possible.
- We will validate, patch, and publish a fix.
- Please avoid public disclosure until a patch is available.
