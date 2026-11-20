//! Unions as Kubernetes writes them: a `type` field and one member field per type
//! ([KEP-1027]).
//!
//! A union is a Rust enum with `#[serde(tag = "type")]` and `#[schemars(transform =
//! union::tagged)]`. Each variant is a unit variant or has named member fields. The API server
//! takes no `oneOf` with properties, so [`tagged`] turns the `oneOf` schemars writes into one
//! object: `type` as an enum, every member as an optional property, and a CEL rule per member
//! that it is set for exactly the types that have it.
//!
//! [KEP-1027]: https://github.com/kubernetes/enhancements/tree/master/keps/sig-api-machinery/1027-api-unions

use schemars::Schema;
use serde_json::{Map, Value, json};

/// A member and the types that have it.
struct Member {
    name: String,
    schema: Value,
    allowed: Vec<String>,
    required: Vec<String>,
}

/// Rewrites the `oneOf` of a `#[serde(tag = "type")]` enum into one object. A member keeps no
/// `default`.
pub(crate) fn tagged(schema: &mut Schema) {
    let Some(Value::Array(variants)) = schema.remove("oneOf") else {
        return;
    };
    let mut tags = Vec::new();
    let mut docs = Vec::new();
    let mut members: Vec<Member> = Vec::new();
    for variant in variants {
        let Value::Object(mut variant) = variant else {
            continue;
        };
        let Some(Value::Object(mut properties)) = variant.remove("properties") else {
            continue;
        };
        let Some(tag) = properties
            .remove("type")
            .and_then(|kind| kind.get("const").and_then(Value::as_str).map(str::to_owned))
        else {
            continue;
        };
        if let Some(doc) = variant.get("description").and_then(Value::as_str) {
            docs.push(format!("`{tag}`: {doc}"));
        }
        let needed: Vec<&str> = variant
            .get("required")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |names| {
                names.iter().filter_map(Value::as_str).collect()
            });
        for (name, mut member_schema) in properties {
            // The API server would fill a default in for every type, not only this one.
            if let Some(member_schema) = member_schema.as_object_mut() {
                member_schema.remove("default");
            }
            let requires = if needed.contains(&name.as_str()) {
                vec![tag.clone()]
            } else {
                Vec::new()
            };
            if let Some(member) = members.iter_mut().find(|member| member.name == name) {
                member.allowed.push(tag.clone());
                member.required.extend(requires);
            } else {
                members.push(Member {
                    name,
                    schema: member_schema,
                    allowed: vec![tag.clone()],
                    required: requires,
                });
            }
        }
        tags.push(tag);
    }

    let mut kind = Map::from_iter([
        ("type".to_owned(), json!("string")),
        ("enum".to_owned(), json!(tags)),
    ]);
    if !docs.is_empty() {
        kind.insert("description".to_owned(), Value::String(docs.join("\n\n")));
    }
    let mut properties = Map::from_iter([("type".to_owned(), Value::Object(kind))]);
    let rules: Vec<Value> = members.iter().map(rule).collect();
    for member in members {
        properties.insert(member.name, member.schema);
    }
    schema.insert("type".to_owned(), json!("object"));
    schema.insert("properties".to_owned(), Value::Object(properties));
    schema.insert("required".to_owned(), json!(["type"]));
    if !rules.is_empty() {
        schema.insert("x-kubernetes-validations".to_owned(), Value::Array(rules));
    }
}

/// The rule that `member` is set only for the types that have it, and always for those that
/// require it.
fn rule(member: &Member) -> Value {
    let name = &member.name;
    let allowed = one_of(&member.allowed);
    let (rule, message) = if member.required.is_empty() {
        (
            format!("!has(self.{name}) || {}", is(&member.allowed)),
            format!("{name} is only for type {allowed}"),
        )
    } else if member.required == member.allowed {
        (
            format!("has(self.{name}) == ({})", is(&member.allowed)),
            format!("{name} is required for type {allowed}, and only for it"),
        )
    } else {
        (
            format!(
                "has(self.{name}) ? {} : !({})",
                is(&member.allowed),
                is(&member.required)
            ),
            format!(
                "{name} is only for type {allowed}, and required for {}",
                one_of(&member.required)
            ),
        )
    };
    json!({"rule": rule, "message": message})
}

/// `self.type == 'a'` or `self.type in ['a', 'b']`.
fn is(tags: &[String]) -> String {
    if let [tag] = tags {
        return format!("self.type == '{tag}'");
    }
    let quoted: Vec<String> = tags.iter().map(|tag| format!("'{tag}'")).collect();
    format!("self.type in [{}]", quoted.join(", "))
}

fn one_of(tags: &[String]) -> String {
    tags.join(" or ")
}

#[cfg(test)]
mod tests {
    use schemars::{JsonSchema, SchemaGenerator};
    use serde::{Deserialize, Serialize};

    /// A pet.
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(tag = "type", rename_all = "camelCase")]
    #[schemars(transform = super::tagged)]
    enum Pet {
        /// No pet.
        None,
        /// A cat.
        Cat {
            /// Its name.
            name: String,
        },
        /// A dog.
        Dog {
            /// Its name.
            name: String,
            /// Its collar.
            #[serde(default = "red")]
            collar: String,
        },
    }

    fn red() -> String {
        "red".to_owned()
    }

    #[test]
    fn a_union_is_one_object_with_a_rule_per_member() {
        let schema = SchemaGenerator::default().into_root_schema_for::<Pet>();
        let schema = serde_json::to_value(schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["type"]));
        assert_eq!(
            schema["properties"]["type"]["enum"],
            serde_json::json!(["none", "cat", "dog"])
        );
        assert_eq!(
            schema["properties"]["type"]["description"],
            "`none`: No pet.\n\n`cat`: A cat.\n\n`dog`: A dog."
        );
        assert_eq!(schema["properties"]["name"]["description"], "Its name.");
        assert!(schema["properties"]["collar"].get("default").is_none());
        assert!(schema.get("oneOf").is_none());
        assert_eq!(
            schema["x-kubernetes-validations"],
            serde_json::json!([
                {
                    "rule": "has(self.name) == (self.type in ['cat', 'dog'])",
                    "message": "name is required for type cat or dog, and only for it",
                },
                {
                    "rule": "!has(self.collar) || self.type == 'dog'",
                    "message": "collar is only for type dog",
                },
            ])
        );
    }
}
