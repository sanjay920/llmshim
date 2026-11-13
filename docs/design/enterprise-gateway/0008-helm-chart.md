# 0008: Helm chart

Status: implemented as `deploy/helm/llmshim` (chart 0.1.0). In development, like
the `serve` feature it deploys.

## What it deploys

A Deployment running `llmshim serve --config /etc/llmshim/gateway.toml`, a
ConfigMap holding that file, a Service for the gateway port and a headless
Service for the admin port, a ServiceAccount, and optionally a
PodDisruptionBudget, a HorizontalPodAutoscaler and a NetworkPolicy.

The chart writes the parts of the configuration that must agree with the
Kubernetes objects (`[server]` and its port, `[server.tls]` and the mounted
Secret, `[admin]` and the probe port, `[state]` and the receipts volume,
`[fleet]`) from its values. Everything else (providers, caller keys, limits,
routes) is the operator's `config` TOML, appended. A probe port and a config
port therefore cannot disagree.

## Decisions

- **Probes use the admin port.** `/ready` and `/health` are open on it by
  design (0005), so the kubelet needs no credential, while `/metrics` requires
  a token. The chart requires one: `admin.tokenEnv` names the variable in the
  Secret (default `GATEWAY_ADMIN_TOKEN`), and the render fails unless it is set
  or `admin.allowUnauthenticated=true` opts out, matching the server's own rule
  that an `[admin]` listener must say how it is protected. The startup probe
  uses `/health`, not `/ready`: in fleet mode `/ready` is 503 while Redis is
  down, and a startup probe waiting on it would turn a Redis outage into a
  crash loop. The port binds `0.0.0.0` because the kubelet reaches the pod IP; the
  admin port is on a separate headless Service, never on the ClusterIP one, and
  the NetworkPolicy restricts who can reach it.
- **Secrets stay out of the chart.** Every secret the configuration names
  (`{ env = "..." }`) comes from one Secret the operator creates
  (`existingSecret`), mounted as environment variables. The chart does not render
  or store a credential.
- **TLS from a `kubernetes.io/tls` Secret**, mounted read-only. A renewed Secret
  updates the mounted files and the server reloads them (0007); no rollout.
- **A configuration change rolls the pods** (a checksum annotation). A change to
  a value in the Secret does not, because the pods read the Secret as
  environment variables and nothing in the pod spec changes; the operator bumps
  `secretRevision` (an annotation) to roll them, and until then pods that
  restart later differ from their peers. Key reload (0012) changes caller keys
  without a restart, but the chart still does not rely on it. The server has no
  reload of anything else, so a rolling update is how settings change. `maxUnavailable: 0`
  keeps capacity during it, and a PodDisruptionBudget guards voluntary
  disruption.
- **Hardened pod:** non-root (65532, matching the distroless image),
  `readOnlyRootFilesystem`, all capabilities dropped, no privilege escalation,
  default seccomp, no service-account token. Only the receipts directory is
  writable.
- **No image is published.** The chart's default repository is a placeholder; the
  comment in `values.yaml` says how to build one (`FEATURES=serve-redis` for fleet
  mode). Publishing, signing and an SBOM are a separate roadmap item.

## What is proven

`deploy/helm/validate.sh` (run by the `Helm chart` CI job) lints the chart,
renders it with defaults and with every option on, validates both against the
Kubernetes 1.30 schemas with `kubeconform -strict`, and feeds each rendered
`gateway.toml` through `llmshim serve --check`. A render that enables TLS
without a Secret name must fail.

The image was also run by hand with exactly the pod's constraints (`--user
65532 --read-only --cap-drop ALL --security-opt no-new-privileges`, a writable
tmpfs for receipts) using the rendered configuration: it started, `/ready` and
`/health` answered on the admin port, the gateway port refused a request without
a key, and the operational paths were 404 on the gateway port.

## Not done

- `[admin.tls]`. The probes are plain HTTP on the admin port, so the chart
  refuses a config that adds it rather than render a pod that never becomes
  ready.
- A `preStop` hook, and a grace period sized for long streams. The server stops
  accepting at once on SIGTERM and drains in-flight requests; the default
  `terminationGracePeriodSeconds: 60` is shorter than a long LLM stream, and a
  rolling update can briefly send a new connection to a terminating pod. Set the
  grace period for your longest stream.
- `appVersion` in `Chart.yaml` and the default image tag are not in the release
  checklist's version lockstep; bump them by hand at each release.
- A Service name for the admin port can exceed 63 characters when the release
  name is long; keep release names short.
- Ingress, ServiceMonitor/PodMonitor and Gateway API objects. They depend on
  what the cluster runs and are easy for an operator to add; none is needed to
  run the gateway.
- A chart repository or OCI publication, and chart signing.
- Running it in a real cluster in CI. Rendering and schema validation catch
  malformed manifests; they do not catch a scheduling or admission failure.
- `ReadWriteMany` receipts storage is the operator's to provide
  (`state.receiptsClaimName`). With the default `emptyDir`, a client that replays
  through a different replica loses its receipts.
