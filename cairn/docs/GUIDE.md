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

## VCS integrations

Cairn takes code straight from GitHub, GitLab, Bitbucket Cloud, Azure
DevOps, Gitea or Forgejo. Each event fetches an archive of the exact commit
over the provider's API (no `git` binary, no clones kept on disk), so a run
plans precisely what was pushed.

| Provider | `--provider` | `--url` | Token needs | Webhook authenticity |
|---|---|---|---|---|
| GitHub / GHES | `github` | default `https://api.github.com`; GHES: `https://ghe.example.com/api/v3` | contents read, commit statuses write, pull requests / issues write | `X-Hub-Signature-256` HMAC |
| GitLab | `gitlab` | default `https://gitlab.com/api/v4` | `api` scope (or `read_repository` + statuses) | `X-Gitlab-Token` |
| Bitbucket Cloud | `bitbucket` | default `https://api.bitbucket.org/2.0` | repository read, pull requests write (use `--username` with an app password / API token, or a workspace access token alone) | `X-Hub-Signature` HMAC |
| Azure DevOps | `azure_devops` | `https://dev.azure.com/<org>` | Code (read & status), pull request threads | service hook basic auth (password = secret) |
| Gitea | `gitea` | the server, e.g. `https://git.example.com` | `repository` read/write | `X-Gitea-Signature` HMAC |
| Forgejo | `forgejo` | the server, e.g. `https://codeberg.org` | `repository` read/write | `X-Forgejo-Signature` HMAC |

```sh
cairn vcs create forgejo --provider forgejo --url https://git.lan --access-token "$TOKEN" --space platform
#   prints the webhook URL and secret: add them to the repository (push, pull request and comment events)
cairn stack create network --space prod --vcs forgejo --repo infra/network --branch main --auto-apply
cairn stack sync -s network --space prod -f      # tracked run from the branch head, now
```

An integration lives in a space; stacks in that space and below can use it.
Azure DevOps repositories are written `project/repo`.

**What triggers what** (defaults, which `push` policies can override):

| Event | Default |
|---|---|
| Push to the stack's branch touching its project root | tracked run (applies when the stack auto-applies) |
| Push to another branch | ignored |
| Pull request opened or updated against the stack's branch | proposed run; older unfinished runs of that PR are stopped |
| `/cairn plan [stack…]` comment on a pull request | proposed run on the PR head |
| `/cairn apply [stack…]` comment | tracked, auto-applied run on the PR head — only for stacks created with `--pr-apply` (approval policies still apply) |
| Pull request closed or merged | its queued runs are canceled and planned runs discarded, releasing their stack locks |

The last row is the fix for Atlantis's lock leak: no lock can outlive the
pull request that took it.

**Reporting.** Every run from VCS sets a commit status named
`cairn/<space>/<stack>` (pending → success/failure, with the plan summary)
and keeps **one** comment per pull request up to date: a table with the
latest run of every stack the PR touched, its changes and errors.
Set `--external-url` so the links in statuses and comments point at your
Cairn.

**Push policies** receive `input.push` (`branch`, `sha`, `message`,
`author`, `affected_files`), `input.pull_request` (`number`, `head_sha`,
`head_branch`, `base_branch`, `title`, `author`) or `input.comment`
(`body`, `author`, `command`), plus `input.stack` (with `repo` and
`branch`). They answer `track`, `propose` or `ignore`; when any push policy
applies to a stack, the defaults above are off.

```rego
package cairn
import rego.v1

track if input.push.branch == input.stack.branch
propose if { input.push; input.push.branch != input.stack.branch }   # preview every branch
propose if input.pull_request
ignore if contains(input.push.message, "[skip cairn]")
```

## Module and provider registry

Cairn is a private registry speaking OpenTofu's `modules.v1` and
`providers.v1` protocols. Namespaces are spaces, so the same roles decide
who can read and publish: `reader` to use, `writer` to publish, `admin` to
delete. It needs HTTPS (OpenTofu only talks to registries over TLS) and
`--external-url` set to the address OpenTofu uses.

```hcl
module "vpc" {
  source  = "cairn.example.com/platform/vpc/aws"   # host/space/name/system
  version = "~> 1.2"
}

terraform {
  required_providers {
    internal = { source = "cairn.example.com/platform/internal" }   # host/space/type
  }
}
```

**Publishing.**

```sh
cairn module publish platform/vpc/aws 1.2.0 --dir ./modules/vpc
cairn provider publish platform/internal 0.4.0 --dir dist --gpg-key signing-key.asc
cairn module list
cairn provider list
```

Provider releases use the public registry's layout, which goreleaser's
standard Terraform provider config produces: one
`terraform-provider-<type>_<version>_<os>_<arch>.zip` per platform,
`terraform-provider-<type>_<version>_SHA256SUMS` and its detached GPG
signature `…_SHA256SUMS.sig`. Cairn checks every zip against
`SHA256SUMS`, stores the signature and public key as published, and hands
them to OpenTofu, which verifies the signature itself during `tofu init`
(a bad signature fails the install). Signing keys never touch the server.

**Using it.** Locally, create a token (`cairn token create`) and give it to
OpenTofu with `TF_TOKEN_cairn_example_com=<token>` or a `credentials
"cairn.example.com" { token = "…" }` block in your CLI config (`tofu login`
is not supported yet). Runs inside Cairn need nothing: each run gets a short-lived token,
written to its own CLI config, that can read the registry namespaces of its
stack's space and that space's ancestors — nothing else — and is revoked
when the run finishes. Download URLs given to OpenTofu are pre-signed and
expire after an hour.

## Drift detection, schedules, tasks and destroy

**Run kinds.** Besides `proposed` and `tracked` runs, a stack can run:

| Kind | What it does | Needs |
|---|---|---|
| `drift` | Plans the deployed configuration (the last successful tracked run's) without a lock. Changes mean reality drifted; the result is stored on the stack (`drift: {detected, run_id, at}`) and published as a `stack.drift` event. With `reconcile`, a tracked auto-applied run of the same configuration follows | planner (writer to reconcile) |
| `destroy` | Plans and applies `-destroy` against the deployed configuration; confirm as usual or auto-apply | writer |
| `task` | One tofu command against the stack's state, holding the stack lock: `state list`, `state rm …`, `import …`, `apply -replace=…`, `force-unlock …` … State changes are saved as a new version. Ends `finished` or `errored` | writer, and every `task` policy |

```sh
cairn stack drift -s network --space prod              # exit 4 if drift was found
cairn stack drift -s network --space prod --reconcile
cairn stack task -s network --space prod -- state rm 'aws_instance.legacy'
cairn stack destroy -s preview-123 --auto-confirm
```

**Task policies** see `input.task.command` (an array) and `.line`,
`input.stack`, and `input.principal` (`kind`, `name`, `groups`,
`site_admin`); any `deny` refuses the task before it is queued:

```rego
package cairn
import rego.v1

deny contains "state surgery is for site admins" if {
  input.task.command[0] == "state"
  input.task.command[1] in {"rm", "mv", "push"}
  not input.principal.site_admin
}
```

**Schedules** run any of these on a cron expression (5 fields, UTC, or
`@hourly`/`@daily`/`@weekly`/`@monthly`) or once at a given time:

```sh
cairn schedule create -s network --space prod --kind drift --cron "*/30 * * * *" --reconcile
cairn schedule create -s network --space prod --kind task --cron "@daily" --command "state list"
cairn schedule create -s preview-123 --kind destroy --at 2026-12-01T18:00:00Z   # tear down a demo
cairn schedule create -s network --space prod --kind tracked --cron "0 6 * * mon"
cairn schedule list -s network --space prod
```

Destroy schedules need admin on the space; the others need writer. The
timer sleeps until the next due schedule (no polling); a schedule that was
due while the server was down fires once at startup. A drift schedule is
skipped while its previous check is still running. Each schedule records
`last_run_id` and `last_error`.
