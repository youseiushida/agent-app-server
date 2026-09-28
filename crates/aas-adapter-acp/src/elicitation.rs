//! `elicitation/create` (stable in ACP v1 since schema 1.21.0) as a `question` interaction, and
//! the user's answer as a `CreateElicitationResponse`. Pure mapping; the table is documented in
//! `docs/adapters/acp.md` §7.2.
//!
//! * **Form mode**: one question per property of `requestedSchema` (strings, numbers, integers,
//!   booleans, single and multi selects). The answer is checked against the schema (required
//!   properties, enum membership, number bounds, string lengths, item counts) before it is sent;
//!   an invalid answer is refused and the question stays open. A schema with a property type
//!   this adapter does not know (the schema reserves custom `_…` and future types) is shown as a
//!   single free-text question asking for a JSON object, so no field is rendered as a control it
//!   is not.
//! * **URL mode**: one question with the message and the URL and the choices "continue" and
//!   "decline".
//! * **Other modes** (custom `_…` or future ones) are not rendered (the schema forbids showing
//!   them as a known mode); the session answers `cancel`.

use aas_harness::protocol::{
    InteractionRequest, InteractionResolution, Question, QuestionAnswer, QuestionChoice,
};
use serde_json::{Map, Value, json};

use crate::wire::CreateElicitationParams;

/// Choice ids of the URL-mode question and of a form without fields.
pub const CHOICE_ACCEPT: &str = "accept";
pub const CHOICE_DECLINE: &str = "decline";
/// Question id of the URL-mode question.
const URL_QUESTION: &str = "url";
/// Question id of a form without fields.
const CONFIRM_QUESTION: &str = "confirm";
/// Question id of the free-form JSON answer.
const JSON_QUESTION: &str = "json";

/// How the answer to an elicitation is turned into its response.
#[derive(Debug, Clone, PartialEq)]
pub enum Form {
    /// One question per property.
    Fields(Vec<Field>),
    /// A form without properties: accept or decline.
    Confirm,
    /// A schema with unknown property types: the answer is a JSON object typed by the user.
    RawJson,
    /// URL mode.
    Url,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
    pub required: bool,
    /// The schema's default, used when the user leaves the field empty.
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text {
        min_length: Option<u64>,
        max_length: Option<u64>,
    },
    Number {
        integer: bool,
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean,
    Single {
        values: Vec<String>,
    },
    Multi {
        values: Vec<String>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
}

/// Interpretation of an `elicitation/create` request.
#[derive(Debug, Clone, PartialEq)]
pub enum Elicitation {
    Ask {
        request: InteractionRequest,
        form: Form,
    },
    /// A mode this adapter cannot show.
    UnknownMode(String),
}

/// `{"action": "cancel"}`.
pub fn cancelled() -> Value {
    json!({ "action": "cancel" })
}

/// `{"action": "accept"}` without content (URL mode).
pub fn accepted() -> Value {
    json!({ "action": "accept" })
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `(value, label)` pairs of a titled (`oneOf` / `anyOf` of `{const, title}`) or untitled
/// (`enum`) option list.
fn options(schema: &Value, titled_key: &str) -> Option<Vec<(String, String)>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|v| (v.to_owned(), v.to_owned()))
                .collect(),
        );
    }
    let titled = schema.get(titled_key).and_then(Value::as_array)?;
    Some(
        titled
            .iter()
            .filter_map(|o| {
                let value = o.get("const")?.as_str()?.to_owned();
                let label = str_field(o, "title").unwrap_or(&value).to_owned();
                Some((value, label))
            })
            .collect(),
    )
}

/// The field kind of a property schema and its choices; `None` for an unknown type.
fn field_kind(schema: &Value) -> Option<(FieldKind, Vec<(String, String)>)> {
    let u = |key: &str| schema.get(key).and_then(Value::as_u64);
    let f = |key: &str| schema.get(key).and_then(Value::as_f64);
    match schema.get("type").and_then(Value::as_str)? {
        "string" => Some(match options(schema, "oneOf") {
            Some(values) => (
                FieldKind::Single {
                    values: values.iter().map(|(v, _)| v.clone()).collect(),
                },
                values,
            ),
            None => (
                FieldKind::Text {
                    min_length: u("minLength"),
                    max_length: u("maxLength"),
                },
                Vec::new(),
            ),
        }),
        "number" => Some((
            FieldKind::Number {
                integer: false,
                minimum: f("minimum"),
                maximum: f("maximum"),
            },
            Vec::new(),
        )),
        "integer" => Some((
            FieldKind::Number {
                integer: true,
                minimum: f("minimum"),
                maximum: f("maximum"),
            },
            Vec::new(),
        )),
        "boolean" => Some((
            FieldKind::Boolean,
            vec![("true".into(), "Yes".into()), ("false".into(), "No".into())],
        )),
        "array" => {
            let values = options(schema.get("items")?, "anyOf")?;
            let kind = FieldKind::Multi {
                values: values.iter().map(|(v, _)| v.clone()).collect(),
                min_items: u("minItems"),
                max_items: u("maxItems"),
            };
            Some((kind, values))
        }
        _ => None,
    }
}

fn describe_default(default: &Value) -> String {
    match default {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn form_question(params: &CreateElicitationParams, schema: &Value) -> (Vec<Question>, Form) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return (Vec::new(), Form::Confirm);
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for (name, property) in properties {
        let Some((kind, choices)) = field_kind(property) else {
            return (Vec::new(), Form::RawJson);
        };
        let title = str_field(property, "title");
        let description = str_field(property, "description");
        let default = property.get("default").filter(|d| !d.is_null()).cloned();
        let is_required = required.contains(&name.as_str());
        let mut prompt = description.or(title).unwrap_or(name).to_owned();
        if !is_required {
            prompt.push_str(" (optional)");
        }
        if let Some(d) = &default {
            prompt.push_str(&format!(" [default: {}]", describe_default(d)));
        }
        questions.push(Question {
            id: name.clone(),
            header: title.map(str::to_owned),
            prompt,
            choices: choices
                .into_iter()
                .map(|(id, label)| QuestionChoice {
                    id,
                    label,
                    description: None,
                })
                .collect(),
            multi_select: matches!(kind, FieldKind::Multi { .. }),
            allow_free_text: matches!(kind, FieldKind::Text { .. } | FieldKind::Number { .. }),
            placeholder: match &kind {
                FieldKind::Number { integer: true, .. } => Some("integer".into()),
                FieldKind::Number { integer: false, .. } => Some("number".into()),
                FieldKind::Text { .. } => str_field(property, "format").map(str::to_owned),
                _ => None,
            },
        });
        fields.push(Field {
            name: name.clone(),
            kind,
            required: is_required,
            default,
        });
    }
    if fields.is_empty() {
        return (Vec::new(), Form::Confirm);
    }
    if !params.message.is_empty()
        && let Some(first) = questions.first_mut()
    {
        first.prompt = format!("{}\n{}", params.message, first.prompt);
    }
    (questions, Form::Fields(fields))
}

/// Maps an `elicitation/create` request to a question.
pub fn request(params: &CreateElicitationParams) -> Elicitation {
    let title_of = |schema: Option<&Value>| {
        schema
            .and_then(|s| str_field(s, "title"))
            .map(str::to_owned)
            .unwrap_or_else(|| "The agent needs your input".to_owned())
    };
    match params.mode.as_str() {
        "url" => {
            let url = params.url.clone().unwrap_or_default();
            let question = Question {
                id: URL_QUESTION.into(),
                header: None,
                prompt: format!("{}\n{url}", params.message).trim().to_owned(),
                choices: vec![
                    QuestionChoice {
                        id: CHOICE_ACCEPT.into(),
                        label: "Done — continue".into(),
                        description: None,
                    },
                    QuestionChoice {
                        id: CHOICE_DECLINE.into(),
                        label: "Decline".into(),
                        description: None,
                    },
                ],
                multi_select: false,
                allow_free_text: false,
                placeholder: None,
            };
            Elicitation::Ask {
                request: InteractionRequest::Question {
                    title: title_of(None),
                    questions: vec![question],
                },
                form: Form::Url,
            }
        }
        "form" => {
            let schema = params.requested_schema.clone().unwrap_or(Value::Null);
            let (questions, form) = form_question(params, &schema);
            let questions = match form {
                Form::Fields(_) => questions,
                Form::Confirm => vec![Question {
                    id: CONFIRM_QUESTION.into(),
                    header: None,
                    prompt: params.message.clone(),
                    choices: vec![
                        QuestionChoice {
                            id: CHOICE_ACCEPT.into(),
                            label: "Accept".into(),
                            description: None,
                        },
                        QuestionChoice {
                            id: CHOICE_DECLINE.into(),
                            label: "Decline".into(),
                            description: None,
                        },
                    ],
                    multi_select: false,
                    allow_free_text: false,
                    placeholder: None,
                }],
                Form::RawJson | Form::Url => vec![Question {
                    id: JSON_QUESTION.into(),
                    header: None,
                    prompt: format!(
                        "{}\nAnswer with a JSON object matching:\n{schema}",
                        params.message
                    )
                    .trim()
                    .to_owned(),
                    choices: Vec::new(),
                    multi_select: false,
                    allow_free_text: true,
                    placeholder: Some("{ … }".into()),
                }],
            };
            Elicitation::Ask {
                request: InteractionRequest::Question {
                    title: title_of(Some(&schema)),
                    questions,
                },
                form,
            }
        }
        other => Elicitation::UnknownMode(other.to_owned()),
    }
}

fn find<'a>(answers: &'a [QuestionAnswer], id: &str) -> Option<&'a QuestionAnswer> {
    answers.iter().find(|a| a.question_id == id)
}

fn choice<'a>(answers: &'a [QuestionAnswer], id: &str) -> Option<&'a str> {
    find(answers, id)
        .and_then(|a| a.choice_ids.first())
        .map(String::as_str)
}

/// The value of one field, `None` when the user left it empty (and it has no default).
fn field_value(field: &Field, answer: Option<&QuestionAnswer>) -> Result<Option<Value>, String> {
    let name = &field.name;
    let text = answer
        .and_then(|a| a.text.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let choices: &[String] = answer.map(|a| a.choice_ids.as_slice()).unwrap_or_default();
    let value = match &field.kind {
        FieldKind::Text {
            min_length,
            max_length,
        } => match text {
            None => None,
            Some(t) => {
                let len = t.chars().count() as u64;
                if min_length.is_some_and(|m| len < m) || max_length.is_some_and(|m| len > m) {
                    return Err(format!(
                        "{name}: the text must be {}–{} characters long",
                        min_length.unwrap_or(0),
                        max_length.map_or("∞".to_owned(), |m| m.to_string())
                    ));
                }
                Some(Value::String(t.to_owned()))
            }
        },
        FieldKind::Number {
            integer,
            minimum,
            maximum,
        } => match text {
            None => None,
            Some(t) => {
                let (value, n) = if *integer {
                    let n = t
                        .parse::<i64>()
                        .map_err(|_| format!("{name}: `{t}` is not an integer"))?;
                    (json!(n), n as f64)
                } else {
                    let n = t
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .ok_or_else(|| format!("{name}: `{t}` is not a number"))?;
                    (json!(n), n)
                };
                if minimum.is_some_and(|m| n < m) || maximum.is_some_and(|m| n > m) {
                    return Err(format!(
                        "{name}: the value must be between {} and {}",
                        minimum.map_or("-∞".to_owned(), |m| m.to_string()),
                        maximum.map_or("∞".to_owned(), |m| m.to_string())
                    ));
                }
                Some(value)
            }
        },
        FieldKind::Boolean => match choices.first().map(String::as_str) {
            Some("true") => Some(Value::Bool(true)),
            Some("false") => Some(Value::Bool(false)),
            Some(other) => return Err(format!("{name}: unknown choice {other}")),
            None => None,
        },
        FieldKind::Single { values } => match choices.first() {
            Some(v) if values.contains(v) => Some(Value::String(v.clone())),
            Some(v) => return Err(format!("{name}: unknown choice {v}")),
            None => None,
        },
        FieldKind::Multi {
            values,
            min_items,
            max_items,
        } => {
            if let Some(unknown) = choices.iter().find(|c| !values.contains(c)) {
                return Err(format!("{name}: unknown choice {unknown}"));
            }
            if choices.is_empty() && answer.is_none() {
                None
            } else {
                let n = choices.len() as u64;
                if min_items.is_some_and(|m| n < m) || max_items.is_some_and(|m| n > m) {
                    return Err(format!(
                        "{name}: choose between {} and {} options",
                        min_items.unwrap_or(0),
                        max_items.map_or("any number of".to_owned(), |m| m.to_string())
                    ));
                }
                Some(json!(choices))
            }
        }
    };
    Ok(value.or_else(|| field.default.clone()))
}

/// The `CreateElicitationResponse` for an answer. An error leaves the request open.
pub fn response(form: &Form, resolution: &InteractionResolution) -> Result<Value, String> {
    let answers = match resolution {
        InteractionResolution::Dismissed => return Ok(cancelled()),
        InteractionResolution::Approval { .. } => {
            return Err("an elicitation needs answers to its questions".into());
        }
        InteractionResolution::Question { answers } => answers,
    };
    match form {
        Form::Url => match choice(answers, URL_QUESTION) {
            Some(CHOICE_ACCEPT) => Ok(accepted()),
            Some(CHOICE_DECLINE) => Ok(json!({ "action": "decline" })),
            other => Err(format!("unknown choice {other:?}")),
        },
        Form::Confirm => match choice(answers, CONFIRM_QUESTION) {
            Some(CHOICE_ACCEPT) => Ok(json!({ "action": "accept", "content": {} })),
            Some(CHOICE_DECLINE) => Ok(json!({ "action": "decline" })),
            other => Err(format!("unknown choice {other:?}")),
        },
        Form::RawJson => {
            let text = find(answers, JSON_QUESTION)
                .and_then(|a| a.text.clone())
                .unwrap_or_default();
            let content: Value = serde_json::from_str(&text)
                .map_err(|e| format!("the answer is not valid JSON: {e}"))?;
            if !content.is_object() {
                return Err("the answer must be a JSON object".into());
            }
            Ok(json!({ "action": "accept", "content": content }))
        }
        Form::Fields(fields) => {
            let mut content = Map::new();
            for field in fields {
                match field_value(field, find(answers, &field.name))? {
                    Some(value) => {
                        content.insert(field.name.clone(), value);
                    }
                    None if field.required => {
                        return Err(format!("{}: an answer is required", field.name));
                    }
                    None => {}
                }
            }
            Ok(json!({ "action": "accept", "content": content }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn params(v: Value) -> CreateElicitationParams {
        serde_json::from_value(v).unwrap()
    }

    fn answer(id: &str, choices: &[&str], text: Option<&str>) -> QuestionAnswer {
        QuestionAnswer {
            question_id: id.into(),
            choice_ids: choices.iter().map(|c| (*c).to_owned()).collect(),
            text: text.map(str::to_owned),
        }
    }

    fn form_request() -> CreateElicitationParams {
        params(json!({
            "sessionId": "s1", "toolCallId": "call_1", "mode": "form", "message": "Configure the deploy",
            "requestedSchema": {
                "type": "object", "title": "Deploy",
                "properties": {
                    "env": {"type": "string", "title": "Environment", "oneOf": [
                        {"const": "prod", "title": "Production"}, {"const": "dev", "title": "Development"}]},
                    "name": {"type": "string", "description": "Release name", "minLength": 2, "maxLength": 8},
                    "replicas": {"type": "integer", "minimum": 1, "maximum": 5, "default": 2},
                    "ratio": {"type": "number"},
                    "notify": {"type": "boolean"},
                    "regions": {"type": "array", "items": {"type": "string", "enum": ["eu", "us"]}, "minItems": 1}
                },
                "required": ["env", "name", "regions"]
            }
        }))
    }

    #[test]
    fn form_properties_become_questions() {
        let Elicitation::Ask {
            request: InteractionRequest::Question { title, questions },
            form,
        } = request(&form_request())
        else {
            panic!()
        };
        assert_eq!(title, "Deploy");
        let ids: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
        // Properties in the order serde_json keeps them (sorted by name).
        assert_eq!(
            ids,
            ["env", "name", "notify", "ratio", "regions", "replicas"]
        );
        assert!(questions[0].prompt.starts_with("Configure the deploy\n"));
        assert_eq!(
            questions[0]
                .choices
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Production", "Development"]
        );
        assert!(!questions[0].allow_free_text);
        assert!(questions[1].allow_free_text);
        assert_eq!(questions[2].choices.len(), 2, "booleans are Yes/No choices");
        assert!(questions[4].multi_select);
        assert_eq!(questions[5].placeholder.as_deref(), Some("integer"));
        assert!(
            questions[5].prompt.contains("(optional)")
                && questions[5].prompt.contains("[default: 2]")
        );
        let Form::Fields(fields) = form else { panic!() };
        assert_eq!(fields.len(), 6);
    }

    #[test]
    fn answers_are_checked_against_the_schema() {
        let Elicitation::Ask { form, .. } = request(&form_request()) else {
            panic!()
        };
        let resolve = |answers: Vec<QuestionAnswer>| {
            response(&form, &InteractionResolution::Question { answers })
        };
        let ok = resolve(vec![
            answer("env", &["prod"], None),
            answer("name", &[], Some(" v1.2 ")),
            answer("notify", &["true"], None),
            answer("ratio", &[], Some("0.5")),
            answer("regions", &["eu", "us"], None),
        ])
        .unwrap();
        assert_eq!(
            ok,
            json!({"action": "accept", "content": {
                "env": "prod", "name": "v1.2", "notify": true, "ratio": 0.5, "regions": ["eu", "us"], "replicas": 2
            }})
        );
        // Missing required field, too short text, out-of-range integer, unknown choice, empty multi.
        assert!(
            resolve(vec![
                answer("env", &["prod"], None),
                answer("regions", &["eu"], None)
            ])
            .unwrap_err()
            .contains("name")
        );
        let base = || {
            vec![
                answer("env", &["prod"], None),
                answer("regions", &["eu"], None),
            ]
        };
        let with = |extra: QuestionAnswer| {
            let mut a = base();
            a.push(answer("name", &[], Some("ok")));
            a.push(extra);
            a
        };
        assert!(
            resolve(with(answer("replicas", &[], Some("9"))))
                .unwrap_err()
                .contains("between")
        );
        assert!(
            resolve(with(answer("replicas", &[], Some("2.5"))))
                .unwrap_err()
                .contains("not an integer")
        );
        assert!(
            resolve(with(answer("ratio", &[], Some("NaN"))))
                .unwrap_err()
                .contains("not a number")
        );
        let mut too_short = base();
        too_short.push(answer("name", &[], Some("x")));
        assert!(resolve(too_short).unwrap_err().contains("characters"));
        let mut bad_choice = vec![
            answer("env", &["staging"], None),
            answer("name", &[], Some("ok")),
            answer("regions", &["eu"], None),
        ];
        assert!(
            resolve(bad_choice.clone())
                .unwrap_err()
                .contains("unknown choice")
        );
        bad_choice[0] = answer("env", &["dev"], None);
        bad_choice[2] = answer("regions", &[], None);
        assert!(resolve(bad_choice).unwrap_err().contains("choose between"));
        // Dismissing cancels; an approval answer is refused.
        assert_eq!(
            response(&form, &InteractionResolution::Dismissed).unwrap(),
            json!({"action": "cancel"})
        );
        assert!(
            response(
                &form,
                &InteractionResolution::Approval {
                    option_id: "x".into(),
                    feedback: None
                }
            )
            .is_err()
        );
    }

    #[test]
    fn url_mode_asks_to_continue_or_decline() {
        let p = params(json!({
            "sessionId": "s1", "mode": "url", "elicitationId": "e1",
            "url": "https://example.com/oauth", "message": "Sign in to the MCP server"
        }));
        let Elicitation::Ask {
            request: InteractionRequest::Question { questions, .. },
            form,
        } = request(&p)
        else {
            panic!()
        };
        assert_eq!(form, Form::Url);
        assert_eq!(
            questions[0].prompt,
            "Sign in to the MCP server\nhttps://example.com/oauth"
        );
        let pick = |c: &str| {
            response(
                &form,
                &InteractionResolution::Question {
                    answers: vec![answer("url", &[c], None)],
                },
            )
        };
        assert_eq!(pick("accept").unwrap(), json!({"action": "accept"}));
        assert_eq!(pick("decline").unwrap(), json!({"action": "decline"}));
        assert!(pick("other").is_err());
    }

    #[test]
    fn empty_forms_confirm_and_unknown_types_ask_for_json() {
        let empty = params(
            json!({"sessionId": "s1", "mode": "form", "message": "Proceed?", "requestedSchema": {"type": "object"}}),
        );
        let Elicitation::Ask {
            form,
            request: InteractionRequest::Question { questions, .. },
        } = request(&empty)
        else {
            panic!()
        };
        assert_eq!(form, Form::Confirm);
        assert_eq!(questions[0].prompt, "Proceed?");
        let accept = InteractionResolution::Question {
            answers: vec![answer("confirm", &["accept"], None)],
        };
        assert_eq!(
            response(&form, &accept).unwrap(),
            json!({"action": "accept", "content": {}})
        );

        let custom = params(
            json!({"sessionId": "s1", "mode": "form", "message": "Pick a file",
            "requestedSchema": {"type": "object", "properties": {"file": {"type": "_x.file"}}}}),
        );
        let Elicitation::Ask {
            form,
            request: InteractionRequest::Question { questions, .. },
        } = request(&custom)
        else {
            panic!()
        };
        assert_eq!(form, Form::RawJson);
        assert!(questions[0].prompt.contains("_x.file"));
        let typed = |t: &str| {
            response(
                &form,
                &InteractionResolution::Question {
                    answers: vec![answer("json", &[], Some(t))],
                },
            )
        };
        assert_eq!(
            typed(r#"{"file": "a.txt"}"#).unwrap(),
            json!({"action": "accept", "content": {"file": "a.txt"}})
        );
        assert!(typed("[1]").is_err());
        assert!(typed("nope").is_err());

        assert_eq!(
            request(&params(
                json!({"sessionId": "s1", "mode": "_vendor.thing", "message": "x"})
            )),
            Elicitation::UnknownMode("_vendor.thing".into())
        );
    }
}
