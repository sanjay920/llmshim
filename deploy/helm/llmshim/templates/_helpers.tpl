{{- define "llmshim.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "llmshim.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "llmshim.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "llmshim.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "llmshim.selectorLabels" -}}
app.kubernetes.io/name: {{ include "llmshim.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "llmshim.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "llmshim.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* The claim the file media store mounts: the operator's, or the one the chart creates. */}}
{{- define "llmshim.mediaClaimName" -}}
{{- if .Values.mediaStore.file.claimName -}}
{{- .Values.mediaStore.file.claimName -}}
{{- else -}}
{{- printf "%s-media" (include "llmshim.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/* The [media_store] section (note 0050), or nothing. Every refusal is a render failure, so a
setting that would be ignored or would fail at start never reaches the cluster. */}}
{{- define "llmshim.mediaStore" -}}
{{- $m := .Values.mediaStore -}}
{{- if $m.enabled }}
{{- if not .Values.fleet.enabled }}
{{- fail "mediaStore needs fleet.enabled: a single instance keeps its media in memory and would ignore the store" }}
{{- end }}
{{- if not (has $m.kind (list "file" "s3" "gcs" "azure-blob")) }}
{{- fail "mediaStore.kind must be one of file, s3, gcs, azure-blob" }}
{{- end }}

[media_store]
kind = {{ $m.kind | quote }}
{{- if $m.maxObjectBytes }}
{{- if or (lt ($m.maxObjectBytes | int64) 1024) (gt ($m.maxObjectBytes | int64) 268435456) }}
{{- fail "mediaStore.maxObjectBytes must be 1024 to 268435456" }}
{{- end }}
max_object_bytes = {{ $m.maxObjectBytes | int64 }}
{{- end }}
{{- if $m.ttlSecs }}
{{- if or (lt ($m.ttlSecs | int64) 600) (gt ($m.ttlSecs | int64) 604800) }}
{{- fail "mediaStore.ttlSecs must be 600 to 604800" }}
{{- end }}
ttl_secs = {{ $m.ttlSecs | int64 }}
{{- end }}
{{- if contains "[media_store]" $.Values.config }}
{{- fail "the chart writes [media_store] from mediaStore: remove it from config (or set mediaStore.enabled=false)" }}
{{- end }}
{{- if and $m.prefix (eq $m.kind "file") }}
{{- fail "mediaStore.prefix does not apply to kind file (the claim is the namespace)" }}
{{- end }}
{{- if $m.prefix }}
{{- if not (hasSuffix "/" $m.prefix) }}
{{- fail "mediaStore.prefix must end in /" }}
{{- end }}
prefix = {{ $m.prefix | quote }}
{{- end }}
{{- if $m.caFile }}
{{- if eq $m.kind "file" }}
{{- fail "mediaStore.caFile does not apply to kind file" }}
{{- end }}
ca_file = {{ $m.caFile | quote }}
{{- end }}
{{- if eq $m.kind "file" }}
{{- if not (or $m.file.claimName $m.file.persistence.create) }}
{{- fail "mediaStore.file needs mediaStore.file.claimName (a ReadWriteMany claim) or mediaStore.file.persistence.create=true" }}
{{- end }}
path = "/var/lib/llmshim/media"
{{- else if eq $m.kind "s3" }}
{{- $s := $m.s3 }}
{{- if not (and $s.bucket $s.region) }}
{{- fail "mediaStore.s3 needs bucket and region" }}
{{- end }}
bucket = {{ $s.bucket | quote }}
region = {{ $s.region | quote }}
{{- if $s.endpoint }}
endpoint = {{ $s.endpoint | quote }}
{{- end }}
{{- /* A YAML boolean false is a value, not an absence: only an unset or empty string leaves the default. */}}
{{- if and (not (kindIs "invalid" $s.pathStyle)) (ne (toString $s.pathStyle) "") }}
path_style = {{ if eq (toString $s.pathStyle) "true" }}true{{ else }}false{{ end }}
{{- end }}
{{- if $s.allowInsecureHttp }}
{{- if not $s.endpoint }}
{{- fail "mediaStore.s3.allowInsecureHttp needs an endpoint" }}
{{- end }}
allow_insecure_http = true
{{- end }}
{{- if eq $s.credentials "aws" }}
credentials = "aws"
{{- else if eq $s.credentials "keys" }}
{{- if not .Values.existingSecret }}
{{- fail "mediaStore.s3 with credentials: keys reads its keys from existingSecret, which is not set (or use credentials: aws)" }}
{{- end }}
access_key_id = { env = {{ $s.accessKeyIdEnv | quote }} }
secret_access_key = { env = {{ $s.secretAccessKeyEnv | quote }} }
{{- if $s.sessionTokenEnv }}
session_token = { env = {{ $s.sessionTokenEnv | quote }} }
{{- end }}
{{- else }}
{{- fail "mediaStore.s3.credentials must be keys or aws" }}
{{- end }}
{{- else if eq $m.kind "gcs" }}
{{- $g := $m.gcs }}
{{- if not $g.bucket }}
{{- fail "mediaStore.gcs needs bucket" }}
{{- end }}
bucket = {{ $g.bucket | quote }}
{{- if $g.endpoint }}
endpoint = {{ $g.endpoint | quote }}
{{- end }}
{{- if $g.allowInsecureHttp }}
{{- if not $g.endpoint }}
{{- fail "mediaStore.gcs.allowInsecureHttp needs an endpoint" }}
{{- end }}
allow_insecure_http = true
{{- end }}
{{- if eq $g.credentials "metadata" }}
credentials = "metadata"
{{- if $g.metadataEndpoint }}
metadata_endpoint = {{ $g.metadataEndpoint | quote }}
{{- end }}
{{- else if eq $g.credentials "service-account" }}
{{- if not $g.serviceAccountSecret }}
{{- fail "mediaStore.gcs with credentials: service-account needs serviceAccountSecret (a Secret holding the key file)" }}
{{- end }}
{{- if or (not $g.serviceAccountKey) (contains "/" $g.serviceAccountKey) (contains ".." $g.serviceAccountKey) }}
{{- fail "mediaStore.gcs.serviceAccountKey must be a file name in the Secret (no slash)" }}
{{- end }}
credentials = "service-account"
service_account_file = {{ printf "/etc/llmshim-gcp/%s" $g.serviceAccountKey | quote }}
{{- else if eq $g.credentials "none" }}
{{- if not $g.endpoint }}
{{- fail "mediaStore.gcs with credentials: none needs an endpoint (an emulator): it is never for Google's own" }}
{{- end }}
credentials = "none"
{{- else }}
{{- fail "mediaStore.gcs.credentials must be metadata, service-account or none" }}
{{- end }}
{{- else if eq $m.kind "azure-blob" }}
{{- $a := $m.azureBlob }}
{{- if not (and $a.account $a.container) }}
{{- fail "mediaStore.azureBlob needs account and container" }}
{{- end }}
account = {{ $a.account | quote }}
container = {{ $a.container | quote }}
{{- if $a.endpoint }}
endpoint = {{ $a.endpoint | quote }}
{{- end }}
{{- if $a.allowInsecureHttp }}
{{- if not $a.endpoint }}
{{- fail "mediaStore.azureBlob.allowInsecureHttp needs an endpoint" }}
{{- end }}
allow_insecure_http = true
{{- end }}
{{- if eq $a.credentials "workload-identity" }}
{{- if not (and $a.tenantId $a.clientId) }}
{{- fail "mediaStore.azureBlob with credentials: workload-identity needs tenantId and clientId" }}
{{- end }}
credentials = "workload-identity"
tenant_id = {{ $a.tenantId | quote }}
client_id = {{ $a.clientId | quote }}
federated_token_file = {{ $a.federatedTokenFile | quote }}
{{- if $a.authorityHost }}
authority_host = {{ $a.authorityHost | quote }}
{{- end }}
{{- else if eq $a.credentials "managed-identity" }}
credentials = "managed-identity"
{{- if $a.clientId }}
client_id = {{ $a.clientId | quote }}
{{- end }}
{{- if $a.identityEndpoint }}
identity_endpoint = {{ $a.identityEndpoint | quote }}
{{- end }}
{{- else if or (eq $a.credentials "sas") (eq $a.credentials "account-key") }}
{{- if not .Values.existingSecret }}
{{- fail "mediaStore.azureBlob with credentials: sas or account-key reads its secret from existingSecret, which is not set" }}
{{- end }}
credentials = {{ $a.credentials | quote }}
{{- if eq $a.credentials "sas" }}
sas_token = { env = {{ $a.sasEnv | quote }} }
{{- else }}
account_key = { env = {{ $a.accountKeyEnv | quote }} }
{{- end }}
{{- else }}
{{- fail "mediaStore.azureBlob.credentials must be workload-identity, managed-identity, sas or account-key" }}
{{- end }}
{{- end }}
{{- end }}
{{- end -}}

{{/* Whether the pods carry the Azure Workload Identity label (the webhook projects the token). */}}
{{- define "llmshim.azureWorkloadIdentity" -}}
{{- if and .Values.mediaStore.enabled (eq .Values.mediaStore.kind "azure-blob") (eq .Values.mediaStore.azureBlob.credentials "workload-identity") -}}true{{- end -}}
{{- end -}}

{{/* The gateway configuration the pod runs. The sections that must agree with
the probes, ports and mounts are written here from the values; everything else
is the operator's `config`. */}}
{{- define "llmshim.config" -}}
version = 1

[server]
host = "0.0.0.0"
port = {{ .Values.server.port }}
{{- if .Values.server.tls.enabled }}

[server.tls]
cert_file = "/etc/llmshim/tls/tls.crt"
key_file = "/etc/llmshim/tls/tls.key"
reload_interval_secs = {{ .Values.server.tls.reloadIntervalSecs }}
{{- if .Values.server.tls.requireClientCertificate }}
client_ca_file = "/etc/llmshim/tls/ca.crt"
{{- end }}
{{- end }}

[admin]
host = "0.0.0.0"
port = {{ .Values.admin.port }}
{{- if and .Values.admin.tokenEnv .Values.admin.allowUnauthenticated }}
{{- fail "admin.tokenEnv and admin.allowUnauthenticated are mutually exclusive" }}
{{- else if .Values.admin.tokenEnv }}
token = { env = {{ .Values.admin.tokenEnv | quote }} }
{{- else if .Values.admin.allowUnauthenticated }}
allow_unauthenticated = true
{{- else }}
{{- fail "set admin.tokenEnv, or set admin.allowUnauthenticated=true to serve /metrics and the stats without a token" }}
{{- end }}

[state]
receipts_dir = "/var/lib/llmshim/receipts"
idempotency_ttl_secs = {{ .Values.state.idempotencyTtlSecs }}
{{- if or .Values.logging.accessLog .Values.logging.usageLog }}

[logging]
{{- if .Values.logging.accessLog }}
access_log = {{ .Values.logging.accessLog | quote }}
{{- end }}
{{- if .Values.logging.usageLog }}
usage_log = {{ .Values.logging.usageLog | quote }}
{{- end }}
{{- end }}
{{- if .Values.fleet.enabled }}

[fleet]
redis_url = { env = {{ .Values.fleet.redisUrlEnv | quote }} }
{{- end }}

{{- include "llmshim.mediaStore" . }}

{{- if contains "admin.tls" .Values.config }}
{{- fail "the chart does not support [admin.tls]: the probes use plain HTTP on the admin port" }}
{{- end }}

{{ .Values.config }}
{{- end -}}
