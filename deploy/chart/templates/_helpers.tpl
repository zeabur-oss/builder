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

{{- /* The Secret with the registry's CA, <namespace>/<name>, or nothing when the registry has a public CA. */ -}}
{{- define "builder.registryCA" -}}
{{- with .Values.registry.caSecret -}}
{{- if or .namespace .secret -}}
{{ required "registry.caSecret.namespace is required with registry.caSecret.secret" .namespace }}/{{ required "registry.caSecret.secret is required with registry.caSecret.namespace" .secret }}
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
