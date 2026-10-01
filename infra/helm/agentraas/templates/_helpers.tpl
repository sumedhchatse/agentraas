{{- define "agentraas.name" -}}{{ .Release.Name }}-agentraas{{- end }}
{{- define "agentraas.labels" -}}
app.kubernetes.io/name: agentraas
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end }}
{{- define "agentraas.secretName" -}}{{ .Values.existingSecret | default (include "agentraas.name" .) }}{{- end }}
{{- define "agentraas.redisUrl" -}}
{{- if .Values.redis.enabled -}}redis://{{ include "agentraas.name" . }}-redis:6379{{- else -}}{{ required "externalRedis.url is required when redis.enabled=false" .Values.externalRedis.url }}{{- end -}}
{{- end }}
