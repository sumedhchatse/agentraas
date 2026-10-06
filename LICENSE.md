# Licensing

AgentRaaS is free to self-host, with every feature, for any purpose,
including commercial use inside your own business. Three licenses cover
different parts of this repository:

| Part | License | File |
|---|---|---|
| The server: everything under `src/api-gateway-rs/` except the files in the next row | GNU AGPL-3.0 | [LICENSE-AGPL](./LICENSE-AGPL) |
| Team/Enterprise features: `src/api-gateway-rs/crates/api/src/ee/`, `src/api-gateway-rs/crates/core/src/dlp.rs`, `src/api-gateway-rs/crates/core/src/hmac_verify.rs`, `compose.ee.yaml` | Functional Source License 1.1, Apache-2.0 future license (FSL-1.1-ALv2) | [LICENSE-FSL.md](./LICENSE-FSL.md) |
| Everything else: the SDKs (`src/sdk`, `src/sdk-js`), `src/chaos-action`, `integrations/`, `infra/`, `compose.yaml`, docs | MIT or Apache-2.0, your choice | [LICENSE-MIT](./LICENSE-MIT), [LICENSE-APACHE](./LICENSE-APACHE) |

In plain words (the license files are what counts):

- **AGPL-3.0 (server).** Use, modify and run it for anything. If you let
  other people use a modified version over a network, you must offer them
  your modified source.
- **FSL (Team/Enterprise features).** Use, modify and self-host it for
  anything except a competing product: you may not offer it to others as a
  commercial hosted service that substitutes for AgentRaaS. Internal use at
  a for-profit company is allowed. Each release becomes Apache-2.0 two years
  after it is published.
- **MIT/Apache-2.0 (SDKs and the rest).** No conditions beyond keeping the
  notice.

Releases up to and including server 0.9.2 keep the license they were
released under (MIT/Apache-2.0 for the core).

Questions: support@agentraas.io.
