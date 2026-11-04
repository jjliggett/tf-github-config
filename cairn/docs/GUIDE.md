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
