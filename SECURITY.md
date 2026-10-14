# Security

Bytecode is untrusted input. A decoder bug, resource-exhaustion issue, or
unsafe server behavior should be reported privately before public disclosure.

Use GitHub's private vulnerability reporting if it is enabled for the
repository. Otherwise, ask the maintainer for a private reporting channel
without posting exploit details or sensitive attachments in a public issue.

Include the affected revision, input format, expected behavior, observed
behavior, and a minimal sample you are authorized to share. Never include
credentials, authentication cookies, or third-party proprietary scripts.

Do not execute recovered code unless you trust its source and environment.
The decompiler's successful parsing or compilation checks do not make the
recovered program safe to run. For HTTP deployment, enforce authentication,
upload limits, concurrency limits, and timeouts outside and inside the service.

Only the current development snapshot is maintained at present. This document
does not promise a response deadline or support for historical versions.
