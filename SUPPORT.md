# ArgosFS Support

Choose the channel that fits your request:

| Topic | Channel |
| --- | --- |
| Setup questions, troubleshooting, deployment advice, general discussion | [GitHub Discussions](https://github.com/fwerkor/ArgosFS/discussions) |
| Reproducible defects, incorrect behavior, data-integrity regressions | [Bug report](https://github.com/fwerkor/ArgosFS/issues/new/choose) |
| Enhancement proposals | [Feature request](https://github.com/fwerkor/ArgosFS/issues/new/choose) |
| Security vulnerabilities | [Private reporting instructions](SECURITY.md) |
| Code contributions | [Contribution guide](CONTRIBUTING.md) |

When reporting an issue, provide the ArgosFS version or commit, Linux kernel
and CPU architecture, backend (`host`, `loop`, or `raw`), layout (`k+m`),
minimal reproduction steps, expected/actual results, and sanitized logs or
relevant `fsck`/`scan` output. For corruption or crash reports, describe
whether a clean restart and recovery were attempted and whether the volume was
mounted read-only or read-write.

**Protect your data:** remove credentials, passphrases, private filenames,
hostnames, and other identifying information from shared logs. Never attach
real filesystem images or raw device dumps without checking their contents.
Use disposable test data for reproductions when possible. Raw-device `mkfs`,
repair, and fault-injection commands may overwrite data.

For operating instructions, see the [README](README.md),
[CLI reference](docs/cli.md), [rootfs guide](docs/rootfs.md), and
[testing guide](docs/testing.md). The project is maintained on a best-effort
basis; these channels do not imply a response-time guarantee.
