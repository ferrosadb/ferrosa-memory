# Security Policy

## Supported Versions

Ferrosa Memory is in developer preview. Only the latest tagged release receives
security updates.

| Version          | Supported          |
| ---------------- | ------------------ |
| latest tag       | :white_check_mark: |
| anything older   | :x:                |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

Report privately via one of these channels:

1. **GitHub Security Advisories** (preferred): https://github.com/ferrosadb/ferrosa-memory/security/advisories/new
2. **Email**: security@ferrosadb.com

Please include:

- A description of the issue and the impact
- Steps to reproduce, ideally with a minimal proof of concept
- The version, commit hash, or release tag where you observed the issue
- Whether the issue requires a specific MCP client, transport (stdio / HTTP), or
  ferrosa configuration to trigger
- Any logs or trace output (sanitize secrets first)

## Response Process

We aim to:

- Acknowledge the report within **3 business days**
- Provide an initial assessment within **7 business days**
- Coordinate disclosure timing with the reporter once a fix is available

If you do not receive an acknowledgement within 3 business days, follow up via
email.

## Disclosure Policy

Coordinated disclosure:

1. Reporter and maintainers agree on a disclosure timeline
2. Fix is prepared and tested privately
3. Patched release is published
4. Public security advisory is issued via GitHub Security Advisories with credit
   to the reporter (unless they prefer to remain anonymous)

We do not currently operate a paid bug bounty program. Public credit is offered
for valid reports.

## Scope

In scope:

- Ferrosa Memory's MCP server (`ferrosa-memory-mcp`) and its tool implementations
- Authentication and tenant-isolation logic
- Memory writes that propagate into the underlying Ferrosa keyspaces
- Released binaries and the install script

Out of scope:

- Vulnerabilities in upstream Ferrosa (report at
  https://github.com/ferrosadb/ferrosa/security/advisories/new)
- Issues in third-party MCP clients (Claude Desktop, Claude Code, etc.) — please
  report to the respective vendor
- Issues in third-party Rust dependencies (please report upstream; we'll track)
- Self-hosted deployments running modified or unsupported builds
