//! Which repositories of the default registry the controller pushed to for each `Build`.
//!
//! One `ConfigMap` per `Build` in the controller's namespace, written before the controller signs
//! a token that pushes to a repository. It names the `Build`, the repositories and when the last
//! such token expires. The controller deletes only recorded repositories, so images someone else
//! pushed to a path of the same shape stay. A record outlives its `Build` until those tokens have
//! expired, so a later sweep also deletes what a run pushed after the first cleanup.

use std::collections::{BTreeMap, BTreeSet};

use jiff::Timestamp;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, ListParams, ObjectMeta, PostParams};
use zeabur_builder_api::labels;

/// Selects the records among the controller's `ConfigMap`s.
const LABEL: &str = "builder.zeabur.com/repositories";
const NAMESPACE: &str = "namespace";
const NAME: &str = "name";
const REPOSITORIES: &str = "repositories";
const TOKENS_UNTIL: &str = "tokensUntil";

/// The repositories the controller pushed to for one `Build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) namespace: String,
    pub(crate) name: String,
    pub(crate) repositories: BTreeSet<String>,
    /// When the last token that pushes to them expires.
    pub(crate) tokens_until: Timestamp,
}

/// The record's `ConfigMap` name. A `Build` name may be 253 characters, so the name is a hash.
pub(crate) fn object_name(namespace: &str, name: &str) -> String {
    let hash = zeabur_builder_core::archive::sha256(format!("{namespace}/{name}").as_bytes());
    let hex: String = hash
        .strip_prefix("sha256:")
        .unwrap_or(&hash)
        .chars()
        .take(20)
        .collect();
    format!("zeabur-builder-repositories-{hex}")
}

impl Record {
    fn to_config_map(&self) -> ConfigMap {
        let repositories: Vec<_> = self.repositories.iter().map(String::as_str).collect();
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(object_name(&self.namespace, &self.name)),
                labels: Some(BTreeMap::from([
                    (LABEL.to_owned(), "true".to_owned()),
                    (labels::MANAGED_BY.to_owned(), labels::MANAGER.to_owned()),
                ])),
                ..ObjectMeta::default()
            },
            data: Some(BTreeMap::from([
                (NAMESPACE.to_owned(), self.namespace.clone()),
                (NAME.to_owned(), self.name.clone()),
                (REPOSITORIES.to_owned(), repositories.join("\n")),
                (TOKENS_UNTIL.to_owned(), self.tokens_until.to_string()),
            ])),
            ..ConfigMap::default()
        }
    }

    fn from_config_map(config_map: &ConfigMap) -> Result<Self, String> {
        let name = config_map.metadata.name.as_deref().unwrap_or_default();
        let data = config_map.data.as_ref();
        let field = |key: &str| {
            data.and_then(|d| d.get(key))
                .ok_or_else(|| format!("ConfigMap `{name}` has no {key}"))
        };
        let record = Self {
            namespace: field(NAMESPACE)?.clone(),
            name: field(NAME)?.clone(),
            repositories: field(REPOSITORIES)?
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            tokens_until: field(TOKENS_UNTIL)?
                .parse()
                .map_err(|e| format!("ConfigMap `{name}`: {TOKENS_UNTIL}: {e}"))?,
        };
        if object_name(&record.namespace, &record.name) != name {
            return Err(format!("ConfigMap `{name}` names another Build"));
        }
        Ok(record)
    }
}

/// Records that `repository` of the `Build` `namespace/name` gets a token that pushes to it
/// until `until`.
pub(crate) async fn add(
    api: &Api<ConfigMap>,
    namespace: &str,
    name: &str,
    repository: &str,
    until: Timestamp,
) -> Result<(), String> {
    let object = object_name(namespace, name);
    let existing = api.get_opt(&object).await.map_err(|e| e.to_string())?;
    let mut record = match &existing {
        Some(config_map) => Record::from_config_map(config_map)?,
        None => Record {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            repositories: BTreeSet::new(),
            tokens_until: until,
        },
    };
    if record.namespace != namespace || record.name != name {
        return Err(format!("ConfigMap `{object}` names another Build"));
    }
    let added = record.repositories.insert(repository.to_owned());
    if !added && record.tokens_until >= until {
        return Ok(());
    }
    record.tokens_until = record.tokens_until.max(until);
    let mut config_map = record.to_config_map();
    let result = match existing {
        Some(existing) => {
            config_map.metadata.resource_version = existing.metadata.resource_version;
            api.replace(&object, &PostParams::default(), &config_map)
                .await
        }
        None => api.create(&PostParams::default(), &config_map).await,
    };
    result.map(drop).map_err(|e| e.to_string())
}

/// The record of the `Build` `namespace/name`, if there is one.
pub(crate) async fn get(
    api: &Api<ConfigMap>,
    namespace: &str,
    name: &str,
) -> Result<Option<Record>, String> {
    let object = object_name(namespace, name);
    api.get_opt(&object)
        .await
        .map_err(|e| e.to_string())?
        .map(|c| Record::from_config_map(&c))
        .transpose()
}

/// Every record.
pub(crate) async fn list(api: &Api<ConfigMap>, page: u32) -> Result<Vec<Record>, String> {
    let mut params = ListParams::default()
        .labels(&format!("{LABEL}=true"))
        .limit(page);
    let mut all = Vec::new();
    loop {
        let list = api.list(&params).await.map_err(|e| e.to_string())?;
        for config_map in &list.items {
            match Record::from_config_map(config_map) {
                Ok(record) => all.push(record),
                Err(error) => tracing::warn!(%error, "skipping a repository record"),
            }
        }
        match list.metadata.continue_ {
            Some(next) if !next.is_empty() => params = params.continue_token(&next),
            _ => return Ok(all),
        }
    }
}

/// Deletes the record of the `Build` `namespace/name`.
pub(crate) async fn remove(
    api: &Api<ConfigMap>,
    namespace: &str,
    name: &str,
) -> Result<(), String> {
    match api
        .delete(
            &object_name(namespace, name),
            &kube::api::DeleteParams::default(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            namespace: "team-a".to_owned(),
            name: "api".to_owned(),
            repositories: BTreeSet::from(["team-a/api".to_owned(), "team-a/api/source".to_owned()]),
            tokens_until: "2026-11-01T09:10:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let record = record();
        let config_map = record.to_config_map();
        assert_eq!(Record::from_config_map(&config_map), Ok(record));
        assert_eq!(
            config_map.data.unwrap()[REPOSITORIES],
            "team-a/api\nteam-a/api/source"
        );
    }

    #[test]
    fn each_build_has_its_own_object() {
        assert_ne!(object_name("team-a", "api"), object_name("team-b", "api"));
        assert_ne!(object_name("team-a", "b-c"), object_name("team-a-b", "c"));
        let long = "a".repeat(253);
        assert!(object_name("team-a", &long).len() <= 63);
    }

    #[test]
    fn a_record_under_another_builds_name_is_refused() {
        let mut config_map = record().to_config_map();
        config_map.metadata.name = Some(object_name("team-b", "api"));
        assert!(Record::from_config_map(&config_map).is_err());
    }
}
