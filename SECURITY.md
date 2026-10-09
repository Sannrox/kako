# Security policy

kako runs untrusted transform code. A way for a job to escape its sandbox,
reach the network, read data it was not given, or tamper with another job's
outputs or receipt is a security vulnerability.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub private vulnerability
reporting: <https://github.com/Sannrox/kako/security/advisories/new>.
Do not open a public issue, pull request, or discussion.

Please include the kako version, the engine and job source used, and a
minimal reproduction. Escapes caused by the underlying sandbox may also be
reported to [kekkai](https://github.com/Sannrox/kekkai/security).

We aim to acknowledge reports within a week and to publish a fix and an
advisory before the details become public.

## Supported versions

kako has no releases yet. Once it does, only the latest release receives
security fixes.
