# Security Policy

## Supported Versions

Holt is a work in progress with no stable releases. Only the latest commit on
`main` receives security fixes.

| Version | Supported |
| --- | --- |
| `main` | ✅ |
| anything else | ❌ |

## Reporting a Vulnerability

**Please do not open a public issue for a security vulnerability.**

Report vulnerabilities privately through GitHub Security Advisories:

https://github.com/Onion-L/holt/security/advisories/new

Include:

- a description of the issue and its impact;
- steps to reproduce, or a proof of concept;
- the commit or build you tested against.

We aim to acknowledge reports within a few days and to keep you updated while
we investigate. Please give us a reasonable amount of time to address the
issue before any public disclosure.

## Scope Notes

Holt runs locally with the user's own permissions, so its security boundary is
about what the app — and the agent loop inside it — may touch. We consider the
following security issues:

- disclosure of credentials (provider API keys, web search keys), which are
  stored under `~/.holt` with owner-only permissions;
- bypassing the permission-mode approval gate for mutating tool calls
  (write/edit/bash);
- escaping workspace containment in the file RPCs (root enforcement, `.git`
  exclusion, symlink-landing rules);
- reading or writing files outside the documented local surfaces;
- flaws in the RPC boundary between the UI and the engine.

By design, prompts and attached content are sent to the model providers and
search backends **you** configure — that data flow is expected behavior, not a
vulnerability. Bugs in third-party providers or in the vendored gpui snapshot
are best reported upstream, though feel free to tell us if Holt's integration
makes them exploitable.
