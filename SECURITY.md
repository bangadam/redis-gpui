# Security Policy

Redis GPUI handles Redis credentials and can issue writes against live data. Security reports are taken seriously.

## Supported versions

Redis GPUI is pre-1.0 and has no published release. Only the `main` branch is supported, and security fixes are applied there. There are no maintained release branches.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**. Do not open a public issue for a vulnerability. No email is required.

Include, where possible:

- the `main` commit you tested;
- your operating system and how the app was built;
- the affected area (credential handling, TLS, SSH tunnel, read-only gate, console policy, saved metadata, logging);
- reproduction steps, preferably against a local test Redis;
- the impact you believe the issue has.

This is a small pre-1.0 project, so reports are handled on a best-effort basis. There is no bug bounty program. Good-faith reporters are credited in the advisory if they want to be.

## Scope

In scope:

- credentials written to app files, settings, or logs in plaintext;
- keychain or credential-store misuse;
- TLS verification bypasses, silent downgrades, or failures to check the chain, hostname, or validity period;
- SSH host-key verification failures, including a changed key that is not blocked;
- read-only gate bypasses, command-policy misclassification, or destructive commands that execute without confirmation;
- write outcomes that are retried automatically after a lost acknowledgment;
- console commands sent to the server after the client-side policy should have refused them;
- sensitive data (passwords, key contents, command results) appearing in logs.

Out of scope:

- a compromised local machine, including a malicious OS keychain or a debugger attached to the process;
- attacks that require control of the Redis or SSH server you deliberately connect to, beyond what TLS verification and SSH host-key checking are designed to detect;
- packaging and code signing, which are not shipped; no packages have been built or published;
- vulnerabilities in upstream dependencies such as GPUI, Zed, or Rust crates, which should be reported upstream (a heads-up is still welcome).

## Design commitments

These are the properties a report can hold the project to:

- **Secrets are never written to app files.** Passwords and passphrases go to the OS keychain, or are prompted per session and never persisted. Saved connection metadata contains non-secret fields and file paths only.
- **TLS verification is on by default.** The default mode is verify-full. The insecure override is explicit, per connection, and keeps a warning chip visible for as long as the connection is active.
- **SSH host keys use TOFU.** A first-seen key requires explicit confirmation. A changed key is a hard block with no override.
- **Read-only is the default.** Editing is opt-in per connection and resets on disconnect or app restart.
- **Destructive console commands confirm in the client.** The confirmation happens before any bytes are sent. Read-only mode refuses unknown commands and the EVAL family client-side.
- **Lost acknowledgments are not retried.** A write with an unknown outcome is surfaced to the user and is never re-sent automatically.
- **Logs are safe to share.** Passwords, key data, and command results never enter application logs.
