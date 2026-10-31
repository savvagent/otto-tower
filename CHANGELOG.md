# Changelog

## [0.17.0](https://github.com/savvagent/otto-tower/compare/v0.16.1...v0.17.0) (2026-10-31)


### ⚠ BREAKING CHANGES

* switch to tower.savvagent.com and the savvagent-tower MCP name ([#284](https://github.com/savvagent/otto-tower/issues/284))

### Features

* switch to tower.savvagent.com and the savvagent-tower MCP name ([#284](https://github.com/savvagent/otto-tower/issues/284)) ([e963029](https://github.com/savvagent/otto-tower/commit/e963029058b6cc87f43626200a8d1e83be94f65d)), closes [#279](https://github.com/savvagent/otto-tower/issues/279)

## [0.16.1](https://github.com/savvagent/otto-tower/compare/v0.16.0...v0.16.1) (2026-10-31)


### Bug Fixes

* point the platform URL at savvagent.com ([#282](https://github.com/savvagent/otto-tower/issues/282)) ([70fc63d](https://github.com/savvagent/otto-tower/commit/70fc63df4c40c4f362a45bb2df1164d3b57c5e73))

## [0.16.0](https://github.com/savvagent/otto-tower/compare/v0.15.0...v0.16.0) (2026-10-30)


### Features

* **of-mcp:** address jobs by their issue, and flag drift from it ([#260](https://github.com/savvagent/otto-tower/issues/260)) ([5449924](https://github.com/savvagent/otto-tower/commit/5449924d22ab4ec6a2fcb1b24d75deb79364185c))


### Bug Fixes

* **of-core:** clear ticket_changed_at when link_ticket changes the ticket ([#267](https://github.com/savvagent/otto-tower/issues/267)) ([dbf9a38](https://github.com/savvagent/otto-tower/commit/dbf9a38690a49ce8f98a5951a4fb1ff74f898262))
* **of-trackers:** guard comment webhooks by ticket_changed_at ([#263](https://github.com/savvagent/otto-tower/issues/263)) ([77dc932](https://github.com/savvagent/otto-tower/commit/77dc932f83db51d74e03f634a6f1847ae07b934c))
* **of-web:** deduplicate GitHub webhook deliveries ([#268](https://github.com/savvagent/otto-tower/issues/268)) ([bb7c0b3](https://github.com/savvagent/otto-tower/commit/bb7c0b387c88e727d2a1035ecf578569412569fa))
* **of-web:** harden tracker webhook dedupe ([#276](https://github.com/savvagent/otto-tower/issues/276)) ([641cc4d](https://github.com/savvagent/otto-tower/commit/641cc4df336932a576189f178cc4b6958bf5150c))
* **of-web:** redact remote_taken for team-limited callers on repo writes ([#269](https://github.com/savvagent/otto-tower/issues/269)) ([bd2d222](https://github.com/savvagent/otto-tower/commit/bd2d222ae581e654ee800d332c971a5703fc5d26))

## [0.15.0](https://github.com/savvagent/otto-tower/compare/v0.14.1...v0.15.0) (2026-10-24)


### ⚠ BREAKING CHANGES

* **of-mcp:** send_message no longer takes `to`; send_message, inbox, ack_messages and unread_count each require exactly one channel: repo (or remote) or team. ack_messages refuses an upTo that is not 0 and not a message the caller can read in that channel.
* **of-core:** register_repo refuses repo slugs outside [A-Za-z0-9][A-Za-z0-9._/-]{0,127} (invalid_repo_slug); RepoUnresolved quotes and caps the registered slug list.

### Features

* **of-billing:** re-declare the meter when the platform reports unknown_meter ([#242](https://github.com/savvagent/otto-tower/issues/242)) ([3acc11e](https://github.com/savvagent/otto-tower/commit/3acc11e43f9caf1354b42320c534af07550a495a))
* **of-mcp:** scope messages to a repo or team channel ([#254](https://github.com/savvagent/otto-tower/issues/254)) ([e432963](https://github.com/savvagent/otto-tower/commit/e432963f7a7c4a7ae96c15d84174dda422e1c714)), closes [#177](https://github.com/savvagent/otto-tower/issues/177)
* **of-server:** report a stalled meter declaration on /readyz ([#243](https://github.com/savvagent/otto-tower/issues/243)) ([71a494a](https://github.com/savvagent/otto-tower/commit/71a494a7054445f2baa6cb8b9a33410f1c3ec3a2))
* **of-web:** default new tracker bindings to the otto-tower label ([#245](https://github.com/savvagent/otto-tower/issues/245)) ([10e0fd1](https://github.com/savvagent/otto-tower/commit/10e0fd1d024a8fb5458978447513e949f837b4f9))


### Bug Fixes

* **of-core:** shape-check repo slugs and quote them in unresolved-repo errors ([#249](https://github.com/savvagent/otto-tower/issues/249)) ([b98761d](https://github.com/savvagent/otto-tower/commit/b98761d7b26fab3de371e5e2feac389905e5debd))
* **of-trackers:** do not treat a GitHub duplicate close as a completed job ([#240](https://github.com/savvagent/otto-tower/issues/240)) ([176e183](https://github.com/savvagent/otto-tower/commit/176e1836bf17c5d654725a89067b6efd8958df89))
* **web:** name the otto-tower GitHub App in the console and tests ([#244](https://github.com/savvagent/otto-tower/issues/244)) ([4d642f8](https://github.com/savvagent/otto-tower/commit/4d642f84b7ae6501c1c5dbc050309d155dcd52cc))

## [0.14.1](https://github.com/savvagent/otto-factory/compare/v0.14.0...v0.14.1) (2026-10-21)


### Bug Fixes

* **deploy:** serve otto-tower.savvagent.com as the canonical host ([#238](https://github.com/savvagent/otto-factory/issues/238)) ([8b334c1](https://github.com/savvagent/otto-factory/commit/8b334c1beb487c83a6f5c20dba3c1e3d0a818c36))

## [0.14.0](https://github.com/savvagent/otto-factory/compare/v0.13.0...v0.14.0) (2026-10-21)


### Features

* rename the product to otto-tower, with a deprecated legacy host ([#234](https://github.com/savvagent/otto-factory/issues/234)) ([4414245](https://github.com/savvagent/otto-factory/commit/44142458eda84f4d33100a6dd5b22d57708741ce)), closes [#230](https://github.com/savvagent/otto-factory/issues/230)

## [0.13.0](https://github.com/savvagent/otto-factory/compare/v0.12.0...v0.13.0) (2026-10-21)


### ⚠ BREAKING CHANGES

* **of-billing:** OF_ENFORCE_QUOTAS is retired. The platform's budget and payment block is now always enforced on complete_job, which gains a refusal (budget_exceeded or payment_required, not retriable). The quota_exceeded code is no longer produced, and whoami/usage `enforced` now always reports true.

### Features

* **of-billing:** bill completed jobs instead of coordination calls ([#223](https://github.com/savvagent/otto-factory/issues/223)) ([f4a0ce6](https://github.com/savvagent/otto-factory/commit/f4a0ce627e3587590411a6ea2700952c61b4bdfe)), closes [#222](https://github.com/savvagent/otto-factory/issues/222)

## [0.12.0](https://github.com/savvagent/otto-factory/compare/v0.11.0...v0.12.0) (2026-10-09)


### Features

* **web:** add a back-to-otto link to the console header ([#213](https://github.com/savvagent/otto-factory/issues/213)) ([2968164](https://github.com/savvagent/otto-factory/commit/29681641d48db67c141974c42dbbf0c7d3215ca5))


### Bug Fixes

* **web:** test the back link's address logic and apply review follow-ups ([#220](https://github.com/savvagent/otto-factory/issues/220)) ([4ecdd2e](https://github.com/savvagent/otto-factory/commit/4ecdd2e935538d2b616072823dfe139d3bfbd8dc))
* **web:** use one lease term in Spanish and one gender in Hindi ([#218](https://github.com/savvagent/otto-factory/issues/218)) ([b7156c8](https://github.com/savvagent/otto-factory/commit/b7156c84efb9582f5f448c866da9fe61683952e2))

## [0.11.0](https://github.com/savvagent/otto-factory/compare/v0.10.1...v0.11.0) (2026-10-09)


### ⚠ BREAKING CHANGES

* **of-core:** `claim_jobs`, `acquire_lease` and `send_message` refuse an `agent` label that is longer than 128 characters or contains control, line-break, non-space whitespace, invisible formatting or private-use characters (`invalid_agent_label`). A blank `agent` is now stored and returned as null instead of "". A stored label that fails this rule comes back as null in jobs, leases and messages. `AlreadyClaimed` and `LeaseHeld` now show the holder as `agent "<label>"` or `user <uuid>`.

### Bug Fixes

* **of-core:** bound and quote the agent label on claims, leases, and messages ([#208](https://github.com/savvagent/otto-factory/issues/208)) ([6a8c770](https://github.com/savvagent/otto-factory/commit/6a8c77065f7138d1e127d0090c903af1415a8b94)), closes [#163](https://github.com/savvagent/otto-factory/issues/163)
* **web:** use Lease instead of Sperre for lease in the German catalog ([#204](https://github.com/savvagent/otto-factory/issues/204)) ([4005409](https://github.com/savvagent/otto-factory/commit/40054094fd3b0089816607bf3237fb9ab8e78a19)), closes [#183](https://github.com/savvagent/otto-factory/issues/183)

## [0.10.1](https://github.com/savvagent/otto-factory/compare/v0.10.0...v0.10.1) (2026-10-08)


### Bug Fixes

* scope repo and job access to the caller's teams ([#200](https://github.com/savvagent/otto-factory/issues/200)) ([18587ea](https://github.com/savvagent/otto-factory/commit/18587ea68ef35a349df891969775eeba86676972))

## [0.10.0](https://github.com/savvagent/otto-factory/compare/v0.9.0...v0.10.0) (2026-10-08)


### ⚠ BREAKING CHANGES

* console signs in through otto-platform (OAuth code + PKCE) ([#199](https://github.com/savvagent/otto-factory/issues/199))
* become an otto-platform resource server ([#197](https://github.com/savvagent/otto-factory/issues/197))

### Features

* become an otto-platform resource server ([#197](https://github.com/savvagent/otto-factory/issues/197)) ([2562c67](https://github.com/savvagent/otto-factory/commit/2562c6791028ebcc84095d07b1940101d712de54))
* console signs in through otto-platform (OAuth code + PKCE) ([#199](https://github.com/savvagent/otto-factory/issues/199)) ([73c7fbf](https://github.com/savvagent/otto-factory/commit/73c7fbf753a5de01ff7305f2522cc18b2f7b60b2))


### Bug Fixes

* **of-web:** tighten authorization for member passkey resets ([#196](https://github.com/savvagent/otto-factory/issues/196)) ([394f4f8](https://github.com/savvagent/otto-factory/commit/394f4f85d1d73d5ac8d6b60b59bdb8c1de411d94))

## [0.9.0](https://github.com/savvagent/otto-factory/compare/v0.8.0...v0.9.0) (2026-10-07)


### ⚠ BREAKING CHANGES

* migration 0034 drops the claimed_domains (domain) primary key, so the previous release's ON CONFLICT (domain) fails. The deploy is forward-only; roll back by restoring the database.

### Code Refactoring

* run on otto-platform crates (platform cutover Phase 3) ([#194](https://github.com/savvagent/otto-factory/issues/194)) ([48aeafe](https://github.com/savvagent/otto-factory/commit/48aeafe17b6df6998a0aacee373688b20477f218)), closes [#191](https://github.com/savvagent/otto-factory/issues/191)

## [0.8.0](https://github.com/savvagent/otto-factory/compare/v0.7.0...v0.8.0) (2026-09-16)


### Features

* add enterprise OIDC federation ([#189](https://github.com/savvagent/otto-factory/issues/189)) ([ad1b916](https://github.com/savvagent/otto-factory/commit/ad1b91663ac5585a95dc63c41179da3fa7c40c96))


### Bug Fixes

* check passkey ceremony ownership before writing a credential or audit row ([#185](https://github.com/savvagent/otto-factory/issues/185)) ([aaa9a27](https://github.com/savvagent/otto-factory/commit/aaa9a273942894f064e23c489bd7729e24738fd7))

## [0.7.0](https://github.com/savvagent/otto-factory/compare/v0.6.0...v0.7.0) (2026-09-15)


### Features

* show at-a-glance lease presence and explain leases on the repos page ([#182](https://github.com/savvagent/otto-factory/issues/182)) ([9c6d879](https://github.com/savvagent/otto-factory/commit/9c6d879006d5116c886fe9dbffc8e91c6da1b75a))


### Bug Fixes

* **of-auth:** make passkey removal and clearing atomic with their audit writes ([#170](https://github.com/savvagent/otto-factory/issues/170)) ([83d9763](https://github.com/savvagent/otto-factory/commit/83d9763ddf6aaa4afb8982079f7858a90ad30d5c))

## [0.6.0](https://github.com/savvagent/otto-factory/compare/v0.5.0...v0.6.0) (2026-09-11)


### ⚠ BREAKING CHANGES

* **of-core:** fence complete_job/fail_job/cancel_job/renew_claim by claim generation ([#161](https://github.com/savvagent/otto-factory/issues/161))

### Features

* **web:** add a public marketing front page ([#175](https://github.com/savvagent/otto-factory/issues/175)) ([21476b7](https://github.com/savvagent/otto-factory/commit/21476b7ad3c45c1afc9988d94a890413d13e6b0b))
* **web:** add Otto CLI to the connect page's client list ([#153](https://github.com/savvagent/otto-factory/issues/153)) ([6cb3dd3](https://github.com/savvagent/otto-factory/commit/6cb3dd36adad8f5c8c3d432af3de0fd66866b5ce))


### Bug Fixes

* **of-auth:** make claim_finish's claim and ceremony consumption atomic with its writes ([#164](https://github.com/savvagent/otto-factory/issues/164)) ([546bc63](https://github.com/savvagent/otto-factory/commit/546bc6393e0926a8cb486b8d1c40046c19239707))
* **of-core:** fence complete_job/fail_job/cancel_job/renew_claim by claim generation ([#161](https://github.com/savvagent/otto-factory/issues/161)) ([c7c5192](https://github.com/savvagent/otto-factory/commit/c7c51925f46d8392a4ef604cce598ff993101a6b))

## [0.5.0](https://github.com/savvagent/otto-factory/compare/v0.4.0...v0.5.0) (2026-09-11)


### Features

* **web:** migrate the queue page to Poller, keyed on filters too ([#146](https://github.com/savvagent/otto-factory/issues/146)) ([867c10b](https://github.com/savvagent/otto-factory/commit/867c10b637a1d55a32d78c2f5da444e0007ff3e9))

## [0.4.0](https://github.com/savvagent/otto-factory/compare/v0.3.1...v0.4.0) (2026-09-11)


### Features

* **web:** show console version in footer ([#142](https://github.com/savvagent/otto-factory/issues/142)) ([4d5cedf](https://github.com/savvagent/otto-factory/commit/4d5cedf500603f7e0e33a209111d9ef774958c09))


### Bug Fixes

* **of-core:** describe both RLS outcomes in begin_unpinned's doc comment ([#141](https://github.com/savvagent/otto-factory/issues/141)) ([3d21fa3](https://github.com/savvagent/otto-factory/commit/3d21fa3e35e67b21a2c71d058447669d76dd30f3)), closes [#112](https://github.com/savvagent/otto-factory/issues/112)

## [0.3.1](https://github.com/savvagent/otto-factory/compare/v0.3.0...v0.3.1) (2026-09-10)


### Bug Fixes

* **of-auth:** make finish_registration's credential insert and audit write atomic ([#131](https://github.com/savvagent/otto-factory/issues/131)) ([692e795](https://github.com/savvagent/otto-factory/commit/692e795bc259b2d09ad64ee374b843088934490e))
* **of-core:** repeat lease-resource backfill using the documented per-org-loop pattern ([#135](https://github.com/savvagent/otto-factory/issues/135)) ([f85f086](https://github.com/savvagent/otto-factory/commit/f85f0861b1f2f969465042b48974ce375d5aaa9c))
* **of-core:** return a retriable RaceLost error when a unique-violation race is lost with no winner ([#138](https://github.com/savvagent/otto-factory/issues/138)) ([a86a807](https://github.com/savvagent/otto-factory/commit/a86a8079aa44a7452df2c881d36d7af37a6f842f))

## [0.3.0](https://github.com/savvagent/otto-factory/compare/v0.2.0...v0.3.0) (2026-09-10)


### Features

* filter jobs by agent_type in list_jobs and ready ([#79](https://github.com/savvagent/otto-factory/issues/79)) ([74d6062](https://github.com/savvagent/otto-factory/commit/74d60627a939eb12b42155496453ec564b5e4035)), closes [#66](https://github.com/savvagent/otto-factory/issues/66)


### Bug Fixes

* bound the two unbounded queries the overview poll repeats every 30s ([#81](https://github.com/savvagent/otto-factory/issues/81)) ([f32ae18](https://github.com/savvagent/otto-factory/commit/f32ae181daeeccdb306b61462bd7dbfe317bb00f)), closes [#61](https://github.com/savvagent/otto-factory/issues/61)
* **of-auth:** update passkeys.rs test for finish_registration's new signature ([f8a5f64](https://github.com/savvagent/otto-factory/commit/f8a5f64f847637dbda93c21b8c3155d0cfe542ba))
* **of-web:** validate and parse client_ip before storing it in audit_events.ip ([#122](https://github.com/savvagent/otto-factory/issues/122)) ([ac18376](https://github.com/savvagent/otto-factory/commit/ac183760c073cc720dc220c1e3fda7bec02cd07c)), closes [#110](https://github.com/savvagent/otto-factory/issues/110)
* remove four TOTP/email-era orphaned schemas from the OpenAPI doc ([#80](https://github.com/savvagent/otto-factory/issues/80)) ([659d64f](https://github.com/savvagent/otto-factory/commit/659d64fd06f2d8b5a21b9620e2a5c6990975625e)), closes [#72](https://github.com/savvagent/otto-factory/issues/72)
* TeamMember.email is nullable, matching users.email ([#78](https://github.com/savvagent/otto-factory/issues/78)) ([f525c42](https://github.com/savvagent/otto-factory/commit/f525c420483da107b37829e910817ac1992d5607)), closes [#64](https://github.com/savvagent/otto-factory/issues/64)

## [0.2.0](https://github.com/savvagent/otto-factory/compare/v0.1.0...v0.2.0) (2026-09-10)


### Features

* generalize repo leases from (repo, branch) to (repo, resource) ([#115](https://github.com/savvagent/otto-factory/issues/115)) ([6fe084e](https://github.com/savvagent/otto-factory/commit/6fe084e0c145c9d59d646e89f198a14398787db1))

## Changelog
