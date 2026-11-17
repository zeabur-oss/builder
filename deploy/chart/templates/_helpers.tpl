{{- define "builder.labels" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "builder.selector" -}}
app.kubernetes.io/name: zeabur-builder
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "builder.webhookPath" -}}
{{ .Values.webhooks.path | trimSuffix "/" }}
{{- end }}

{{- /* An image reference: repository@digest, else repository:tag, else repository:chart version. */ -}}
{{- define "builder.image" -}}
{{- $image := index . 0 -}}
{{- $chart := index . 1 -}}
{{- if $image.digest -}}
{{ $image.repository }}@{{ $image.digest }}
{{- else -}}
{{ $image.repository }}:{{ $image.tag | default $chart.Version }}
{{- end -}}
{{- end }}

{{- /* The registry's CA: registry.ca, else the key of registry.caFrom's Secret, else nothing. */ -}}
{{- define "builder.registryCA" -}}
{{- $registry := .Values.registry -}}
{{- if $registry.ca -}}
{{ $registry.ca }}
{{- else if and $registry.caFrom.namespace $registry.caFrom.secret -}}
{{- $secret := lookup "v1" "Secret" $registry.caFrom.namespace $registry.caFrom.secret -}}
{{- if $secret -}}
{{ index $secret.data $registry.caFrom.key | default "" | b64dec }}
{{- end -}}
{{- end -}}
{{- end }}

{{- /* A list of strings as a CEL list literal: ['a', 'b']. */ -}}
{{- define "builder.celList" -}}
{{- $items := list -}}
{{- range . }}{{ $items = append $items (printf "'%s'" .) }}{{ end -}}
[{{ join ", " $items }}]
{{- end }}

{{- /* The registry token key as namespace/secret. The chart fails without both fields. */ -}}
{{- define "builder.tokenKey" -}}
{{- $key := .Values.registry.tokenKey -}}
{{ required "registry.tokenKey.namespace is required: the Secret whose tls.key signs registry tokens" $key.namespace }}/{{ required "registry.tokenKey.secret is required: the Secret whose tls.key signs registry tokens" $key.secret }}
{{- end }}
