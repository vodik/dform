//! The answers the fake gives, as they were recorded: a file under
//! `cassettes/` per answer the API gives, naming its model and each call
//! that answers it, where its shape comes from (`schema`: the API's model
//! at eu.api.ovh.com/1.0, every field and no other, the values made up and
//! redacted; `observed`: a live call), what was seen on the live API
//! (`observed`) and what is not known yet (`guessed`), and the answer. The
//! fake fills a cassette's answer with the world's values ([`shaped`]);
//! a field the cassette lacks is the fake inventing a shape, so it
//! panics. docs/providers/ovh.md lists the guesses for the next session
//! on the real API to record.

use serde_json::{Map, Value as Json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The cassettes by name, each file's text.
const FILES: [(&str, &str); 28] = [
    ("container", include_str!("cassettes/container.json")),
    (
        "container-list",
        include_str!("cassettes/container-list.json"),
    ),
    ("credential", include_str!("cassettes/credential.json")),
    ("flavor", include_str!("cassettes/flavor.json")),
    ("image", include_str!("cassettes/image.json")),
    ("input-body", include_str!("cassettes/input-body.json")),
    ("instance", include_str!("cassettes/instance.json")),
    ("interface", include_str!("cassettes/interface.json")),
    ("network", include_str!("cassettes/network.json")),
    ("project", include_str!("cassettes/project.json")),
    ("project-list", include_str!("cassettes/project-list.json")),
    ("record", include_str!("cassettes/record.json")),
    ("record-ids", include_str!("cassettes/record-ids.json")),
    ("region", include_str!("cassettes/region.json")),
    ("region-list", include_str!("cassettes/region-list.json")),
    ("roles", include_str!("cassettes/roles.json")),
    (
        "s3-credential-made",
        include_str!("cassettes/s3-credential-made.json"),
    ),
    (
        "s3-credentials",
        include_str!("cassettes/s3-credentials.json"),
    ),
    ("s3-secret", include_str!("cassettes/s3-secret.json")),
    ("sshkey", include_str!("cassettes/sshkey.json")),
    ("sshkey-list", include_str!("cassettes/sshkey-list.json")),
    ("subnet", include_str!("cassettes/subnet.json")),
    ("token", include_str!("cassettes/token.json")),
    ("user", include_str!("cassettes/user.json")),
    ("user-detail", include_str!("cassettes/user-detail.json")),
    ("volume", include_str!("cassettes/volume.json")),
    ("vrack", include_str!("cassettes/vrack.json")),
    ("zone", include_str!("cassettes/zone.json")),
];

/// One recorded answer.
#[derive(Debug)]
pub struct Cassette {
    /// The API's model of the answer (`cloud.instance.Instance`).
    pub model: Option<String>,
    /// Each call that answers it, `METHOD /path/{param}`.
    pub calls: Vec<String>,
    /// Where its fields come from: `schema`, `observed` or `guessed`.
    pub shape: String,
    /// What was seen on the live API, dated.
    pub observed: Vec<String>,
    /// What the fake does here that no one has seen the API do.
    pub guessed: Vec<String>,
    /// The status it was answered with.
    pub status: u16,
    pub answer: Json,
}

impl Cassette {
    fn parse(name: &str, text: &str) -> Cassette {
        let j: Json =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("cassette {name}: not JSON: {e}"));
        let strings = |k: &str| -> Vec<String> {
            j.get(k)
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        };
        Cassette {
            model: j.get("model").and_then(Json::as_str).map(str::to_string),
            calls: strings("calls"),
            shape: j["shape"].as_str().unwrap_or_default().to_string(),
            observed: strings("observed"),
            guessed: strings("guessed"),
            status: j.get("status").and_then(Json::as_u64).unwrap_or(200) as u16,
            answer: j["answer"].clone(),
        }
    }
}

static ALL: LazyLock<BTreeMap<&'static str, Cassette>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|(name, text)| (*name, Cassette::parse(name, text)))
        .collect()
});

/// Every cassette, by name.
pub fn all() -> &'static BTreeMap<&'static str, Cassette> {
    &ALL
}

/// The cassette `name`.
pub fn get(name: &str) -> &'static Cassette {
    ALL.get(name)
        .unwrap_or_else(|| panic!("the fake has no cassette {name}"))
}

/// The recorded answer of `name` as it is: a refusal's status and body.
pub fn replay(name: &str) -> (u16, Json) {
    let c = get(name);
    (c.status, c.answer.clone())
}

/// The recorded object at `pointer` in cassette `name`'s answer, a list's
/// first element standing for each.
fn template(name: &str, pointer: &str) -> &'static Map<String, Json> {
    let answer = match &get(name).answer {
        Json::Array(xs) => xs.first().unwrap_or(&Json::Null),
        o => o,
    };
    answer
        .pointer(pointer)
        .and_then(Json::as_object)
        .unwrap_or_else(|| panic!("cassette {name} has no object at {pointer:?}"))
}

/// An answer of cassette `name`'s shape with `fields` in it: the
/// recorded object (a list's first element) with each of `fields` put
/// over the recorded value. A field the recording does not have panics.
pub fn shaped(name: &str, fields: Json) -> Json {
    shaped_at(name, "", fields)
}

/// [`shaped`] for the object at `pointer` in the recorded answer (an
/// element's of a list in it: `/ipAddresses/0`).
pub fn shaped_at(name: &str, pointer: &str, fields: Json) -> Json {
    let template = template(name, pointer);
    let mut out: Map<String, Json> = template.clone();
    for (k, v) in fields.as_object().into_iter().flatten() {
        assert!(
            template.contains_key(k),
            "the fake answers {k:?} in {name}{pointer}, which the API was never seen to answer"
        );
        out.insert(k.clone(), v.clone());
    }
    Json::Object(out)
}

/// `full` (an answer of a richer model) as cassette `name` has it: the
/// fields of `name`'s recording, each `full`'s value (a list answering
/// less of each element than a GET of one does).
pub fn fitted(name: &str, full: &Json) -> Json {
    let fields: Map<String, Json> = full
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| template(name, "").contains_key(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    shaped(name, Json::Object(fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cassette parses, says where its shape comes from and which
    /// calls answer it; a recorded refusal keeps its status.
    #[test]
    fn every_cassette_is_whole() {
        assert_eq!(all().len(), FILES.len());
        for (name, c) in all() {
            assert!(
                ["schema", "observed", "guessed"].contains(&c.shape.as_str()),
                "{name}: shape {:?}",
                c.shape
            );
            assert!(!c.calls.is_empty(), "{name} names no call");
            assert!(!c.answer.is_null(), "{name} has no answer");
        }
        assert_eq!(
            replay("input-body"),
            (
                400,
                serde_json::json!({"message": "You provided an input body while none was expected"})
            )
        );
    }

    #[test]
    #[should_panic(expected = "never seen to answer")]
    fn a_field_no_recording_has_is_refused() {
        shaped("vrack", serde_json::json!({"region": "BHS5"}));
    }
}
