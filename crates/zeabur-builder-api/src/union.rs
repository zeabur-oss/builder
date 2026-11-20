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
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// A `oneOf` entry that schemars writes for one variant.
#[derive(Deserialize)]
struct Variant {
    description: Option<String>,
    /// The `type` tag and the variant's fields.
    properties: Map<String, Value>,
    #[serde(default)]
    required: Vec<String>,
}

/// The schema of the `type` tag of a variant.
#[derive(Deserialize)]
struct Tag {
    #[serde(rename = "const")]
    value: String,
}

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
        let Ok(Variant {
            description,
            mut properties,
            required,
        }) = serde_json::from_value(variant)
        else {
            continue;
        };
        let Some(Ok(Tag { value: tag })) = properties.remove("type").map(serde_json::from_value)
        else {
            continue;
        };
        if let Some(doc) = description {
            docs.push(format!("`{tag}`: {doc}"));
        }
        for (name, mut member_schema) in properties {
            // The API server would fill a default in for every type, not only this one.
            if let Value::Object(member_schema) = &mut member_schema {
                member_schema.remove("default");
            }
            let requires = if required.contains(&name) {
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
        assert_eq!(
            schema,
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Pet",
                "description": "A pet.",
                "type": "object",
                "properties": {
                    "type": {
                        "type": "string",
                        "enum": ["none", "cat", "dog"],
                        "description": "`none`: No pet.\n\n`cat`: A cat.\n\n`dog`: A dog.",
                    },
                    "name": {"type": "string", "description": "Its name."},
                    "collar": {"type": "string", "description": "Its collar."},
                },
                "required": ["type"],
                "x-kubernetes-validations": [
                    {
                        "rule": "has(self.name) == (self.type in ['cat', 'dog'])",
                        "message": "name is required for type cat or dog, and only for it",
                    },
                    {
                        "rule": "!has(self.collar) || self.type == 'dog'",
                        "message": "collar is only for type dog",
                    },
                ],
            })
        );
    }
}
