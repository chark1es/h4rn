{{/*
Chart name and fully qualified name.
*/}}
{{- define "h4rn-connect.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "h4rn-connect.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "h4rn-connect.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "h4rn-connect.labels" -}}
helm.sh/chart: {{ include "h4rn-connect.chart" . }}
{{ include "h4rn-connect.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "h4rn-connect.selectorLabels" -}}
app.kubernetes.io/name: {{ include "h4rn-connect.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "h4rn-connect.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "h4rn-connect.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "h4rn-connect.secretName" -}}
{{- if .Values.secrets.existingSecret }}
{{- .Values.secrets.existingSecret }}
{{- else }}
{{- printf "%s-secrets" (include "h4rn-connect.fullname" .) }}
{{- end }}
{{- end }}

{{- define "h4rn-connect.postgresServiceName" -}}
{{- printf "%s-postgres" (include "h4rn-connect.fullname" .) }}
{{- end }}

{{/*
The effective database URL. The chart-managed Secret computes and stores it;
this helper is only used by templates that need it for display or probes.
*/}}
{{- define "h4rn-connect.databaseUrl" -}}
{{- if .Values.secrets.databaseUrl }}
{{- .Values.secrets.databaseUrl }}
{{- else if .Values.postgres.enabled }}
{{- printf "postgres://%s:%s@%s:5432/%s" .Values.postgres.auth.username (urlquery (default "h4rn" .Values.postgres.auth.password)) (include "h4rn-connect.postgresServiceName" .) .Values.postgres.auth.database }}
{{- else if .Values.externalDatabase.url }}
{{- .Values.externalDatabase.url }}
{{- else }}
{{- printf "postgres://%s:%s@%s:%v/%s" .Values.externalDatabase.user (urlquery .Values.externalDatabase.password) .Values.externalDatabase.host .Values.externalDatabase.port .Values.externalDatabase.database }}
{{- end }}
{{- end }}
