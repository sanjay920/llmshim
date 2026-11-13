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
{{- end }}

[admin]
host = "0.0.0.0"
port = {{ .Values.admin.port }}
{{- if .Values.admin.tokenEnv }}
token = { env = {{ .Values.admin.tokenEnv | quote }} }
{{- end }}

[state]
receipts_dir = "/var/lib/llmshim/receipts"
idempotency_ttl_secs = {{ .Values.state.idempotencyTtlSecs }}
{{- if .Values.fleet.enabled }}

[fleet]
redis_url = { env = {{ .Values.fleet.redisUrlEnv | quote }} }
{{- end }}

{{ .Values.config }}
{{- end -}}
