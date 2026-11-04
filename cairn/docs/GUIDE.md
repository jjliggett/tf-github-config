# Cairn user guide

Task-oriented documentation for running Cairn. The design rationale is in
[SPEC.md](SPEC.md); benchmarks are in [benchmark-report.md](benchmark-report.md).

## Identity and access

### Concepts

| Concept | Meaning |
|---|---|
| Space | A folder of stacks. Spaces nest (`--parent`). A role granted on a space applies to every space below it. |
| Role | `reader` < `planner` < `writer` < `admin` (TFC's read/plan/write/admin; Spacelift's read/write/admin plus "plan"). |
| User | Created on first OIDC login. Identified by issuer + `sub`; carries email, name and groups. |
| Team | An IdP group. A binding to team `platform` applies to every user whose `groups` claim contains `platform`. |
| Token | `user` tokens act as their user (the CLI gets one on `cairn login`). `service` tokens have their own role bindings. Both can expire; both are stored as SHA-256 hashes. |
| Site admin | The site token (`CAIRN_SITE_TOKEN`), members of `CAIRN_OIDC_ADMIN_GROUP`, and tokens created with `site_admin`. |

What each role allows:

| Action | reader | planner | writer | admin |
|---|---|---|---|---|
| See stacks, runs, logs, plans, non-sensitive outputs and variables | ✔ | ✔ | ✔ | ✔ |
| Proposed runs (plans), cancel proposed runs | | ✔ | ✔ | ✔ |
| Tracked runs, confirm/discard/cancel, variables, locks, state download/upload, sensitive outputs | | | ✔ | ✔ |
| Create/edit/delete stacks, child spaces, role bindings, service tokens for the space | | | | ✔ |

Nobody can grant a role higher than their own. Stacks and runs in spaces you
can't read answer `404`, so their existence isn't revealed. Every
non-GET request is written to the audit log (`cairn audit`, site admins).

### OIDC (Authentik, Keycloak, Dex, Google, …)

```sh
cairnd \
  --oidc-issuer https://auth.example.com/application/o/cairn/ \
  --oidc-client-id cairn --oidc-client-secret "$SECRET" \
  --oidc-admin-group cairn-admins \
  --external-url https://cairn.example.com
```

Register `https://cairn.example.com/auth/callback` as a redirect URI and
include a `groups` claim (`--oidc-groups-claim` to change its name).

* **Browser/CLI login**: `cairn login --host https://cairn.example.com` opens
  the IdP, then hands a session token (default 12 h, `--session-hours`) back to
  the CLI through a loopback redirect, like `tofu login`. Only loopback
  redirects are accepted.
* **Bearer ID tokens**: any request may present an ID token from the issuer
  (`Authorization: Bearer <jwt>`), verified against the issuer's JWKS. Add CI
  audiences with `--oidc-audiences`.

### Day-to-day

```sh
cairn space create platform                        # site admin: top-level space
cairn space create prod --parent platform          # admin on platform
cairn role grant platform planner --team developers
cairn role grant prod writer --user alice@example.com
cairn token create --service --role prod=writer --expires-hours 720 --description ci
cairn whoami                                       # your principal and roles per space
cairn audit --limit 20
```

## Storage and TLS

State versions, configuration tarballs and registry artifacts are blobs;
run metadata lives in SQLite (`<data>/cairn.db`). Choose the blob store with
`--blob-store` / `CAIRN_BLOB_STORE`:

| Backend | URL | Credentials |
|---|---|---|
| Local disk (default) | `file:///var/lib/cairn/blobs` | — |
| AWS S3 | `s3://bucket/prefix?region=eu-west-1` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional `AWS_SESSION_TOKEN` |
| MinIO, Garage, Ceph RGW, R2, B2 | `s3://bucket/prefix?endpoint=https://minio.lan:9000` (path-style by default with an endpoint) | same |
| Google Cloud Storage | `s3://bucket/prefix?endpoint=https://storage.googleapis.com&region=auto` (HMAC keys from the interoperability settings) | same |
| Azure Blob Storage | `azblob://account/container/prefix` | `AZURE_STORAGE_KEY` (base64 account key) |
| Azurite | `azblob://devstoreaccount1/container?endpoint=http://127.0.0.1:10000/devstoreaccount1` | same |

S3 requests are signed with AWS Signature Version 4 and Azure requests with
Shared Key, both implemented in Cairn (no SDKs). Blobs are content-addressed,
so identical state is stored once; deleting a stack deletes its blobs.

**Encryption at rest.** State and configuration blobs are encrypted with
ChaCha20-Poly1305 using the server key (`CAIRN_SECRET_KEY` or
`<data>/secret.key`). Back up that key: without it the blobs are unreadable.
`--no-blob-encryption` stores plaintext (for example when the bucket already
uses KMS encryption and you want to read objects directly).

**TLS.** `--tls-cert fullchain.pem --tls-key key.pem` serves HTTPS directly
(rustls); otherwise put Cairn behind a TLS-terminating proxy. OpenTofu needs
HTTPS for the `cloud {}` block and for registries, so one of the two is
required for those features.

## Policies

Policies are [Rego](https://www.openpolicyagent.org/docs/latest/policy-language/)
modules in `package cairn`, evaluated in-process (no OPA sidecar). Each
policy has a **type** that decides when it runs, what `input` it gets and
which rules Cairn reads:

| Type | Runs | `input` | Rules read |
|---|---|---|---|
| `plan` | After every plan (proposed and tracked) | `plan` (`tofu show -json`), `run`, `stack` | `deny` (set of messages: the run errors), `warn` (logged) |
| `approval` | After a tracked plan with changes, and after each review | `run` (with plan summary), `stack`, `reviews` | `approve` (every policy must approve), `reject` (any one discards the run) |
| `login` | On every OIDC login or bearer ID token | `session` (`subject`, `email`, `name`, `groups`, `issuer`) | `deny`, `admin` (site admin), `space_role` (`{"space": "writer"}`) |
| `notification` | On run events (planned, applied, errored, discarded, canceled) | `event`, `run`, `stack` | `webhook` (set of `{"endpoint_id", "payload"}`) |
| `push`, `trigger`, `task` | VCS pushes, dependency triggers, task runs | see those sections | `track`/`propose`/`ignore`, `trigger`, `deny` |

**Where they apply.** A policy lives in a space and applies to stacks in
that space and its children when it is attached explicitly
(`cairn policy attach`) or carries a label `autoattach:<label>` that matches a
stack label (`autoattach:*` matches every stack). `login` policies are global
and only site admins can manage them. Space admins manage the other types.

```rego
package cairn
import rego.v1

# plan: no deletions in prod, and flag big changes
deny contains sprintf("%s would be deleted", [rc.address]) if {
  some rc in input.plan.resource_changes
  "delete" in rc.change.actions
  "prod" in input.stack.labels
}
warn contains "more than 20 changes" if count(input.plan.resource_changes) > 20
```

```rego
package cairn
import rego.v1

# approval: two approvals, any rejection wins
approve if count({r.principal | some r in input.reviews; r.decision == "approve"}) >= 2
reject if { some r in input.reviews; r.decision == "reject" }
```

```rego
package cairn
import rego.v1

# login: contractors only get read access, platform team gets admin
deny if not endswith(input.session.email, "@example.com")
admin if "platform" in input.session.groups
space_role := {"staging": "writer"} if "contractors" in input.session.groups
```

A tracked run held by approval policies stops in `planned` with
`approval: pending`; `confirm` is refused until the policies approve. When
they approve an auto-apply stack's run it applies by itself; otherwise it
waits for `confirm` as usual. Evaluations are stored with the run.

```sh
cairn policy create no-deletes --kind plan --file no-deletes.rego --space prod --label autoattach:*
cairn policy eval --kind approval --file two-approvals.rego --input review.json   # dry run
cairn policy attach <policy-id> network --space prod
cairn run review <run-id> --approve --note "checked the plan"
cairn run policies <run-id>                         # evaluations and reviews
```

### Notifications

Webhook endpoints belong to a space and receive events for stacks in it and
below:

```sh
cairn webhook create prod https://hooks.example.com/cairn --secret "$SECRET"
```

Each delivery is a JSON `POST` with `X-Cairn-Event` and, with a secret,
`X-Cairn-Signature: sha256=<HMAC-SHA256 of the body>`. Failed deliveries are
retried after 1, 4 and 16 seconds. Without `notification` policies every
event goes to every endpoint; with them, only what their `webhook` rule
returns is sent, so they filter and reshape payloads (for example into a
Matrix, ntfy or Slack message). URLs that resolve to private or loopback
addresses are refused unless cairnd runs with `--allow-private-webhooks`
(needed for homelab receivers on the LAN).
