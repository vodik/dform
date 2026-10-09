//! The plugin protocol over the provider (`Handler`): each call answered by
//! the method for it, its documents converted on the way in and out.

use super::*;

impl Handler for Ovh {
    fn handle(
        &self,
        call: backend::Call,
        progress: backend::Progress,
    ) -> std::result::Result<Reply, CallError> {
        use backend::Call as C;
        Ok(match call {
            C::Handshake(req) => Reply::Handshake(handshake(&req)?),
            C::Configure(req) => {
                let config = doc_of(req.config.as_ref())?.unwrap_or(json!({}));
                Reply::Configure(self.configure(&config).map_err(invalid)?)
            }
            C::Schema(req) => Reply::Schema(self.schema_reply(&req)?),
            C::Query(q) => Reply::Query(self.query_reply(&q)?),
            C::Read(r) => Reply::Read(self.read_reply(&r)?),
            C::Plan(r) => Reply::Plan(self.plan_reply(&r)?),
            C::Apply(r) => Reply::Apply(self.apply(&r, progress)?),
            C::Import(r) => Reply::Import(self.import_reply(r)?),
            C::Reveal(r) => Reply::Reveal(self.reveal(r)?),
            C::Health(r) => Reply::Health(pb::HealthResponse {
                answers: self.health(&r.objects).map_err(invalid)?,
            }),
        })
    }
}

/// The provider's name, protocol, capabilities and settings, to a host
/// that speaks its protocol version.
fn handshake(req: &pb::HandshakeRequest) -> std::result::Result<pb::HandshakeResponse, CallError> {
    let v = req.protocol_version;
    if v != VERSION {
        return Err(CallError::Refused(format!(
            "this provider speaks protocol version {VERSION}, not {v}"
        )));
    }
    Ok(pb::HandshakeResponse {
        protocol_version: VERSION,
        name: PROVIDER.into(),
        // `offline`: its Plan with no credentials checks the
        // schema and the networks, all but what the account
        // offers (a flavor, an image, a vRack) (R-188).
        capabilities: ["resource", "managed", "keep", "offline"]
            .map(String::from)
            .to_vec(),
        // R-203: each judged from its status word.
        health: health::TYPES.map(String::from).to_vec(),
        version: backend::BUILD.into(),
        // What names the account; the keys are the OVH SDK's
        // configuration's, never a setting (R-44).
        settings: ["endpoint", "project"]
            .map(|name| pb::SettingDecl {
                name: name.into(),
                sensitive: false,
            })
            .to_vec(),
    })
}

impl Ovh {
    fn schema_reply(
        &self,
        req: &pb::SchemaRequest,
    ) -> std::result::Result<pb::SchemaResponse, CallError> {
        Ok(pb::SchemaResponse {
            facts: wire::schema_facts(&self.schema, req).map_err(invalid)?,
            externs: EXTERNS
                .iter()
                .map(|(pred, input)| pb::ExternDecl {
                    pred: pred.to_string(),
                    arity: input.len() as u32,
                    input: input.to_vec(),
                })
                .collect(),
            checks_refinements: false,
            examples: self.examples(),
        })
    }

    fn query_reply(&self, q: &pb::QueryRequest) -> std::result::Result<Vec<pb::Row>, CallError> {
        let inputs = q
            .inputs
            .iter()
            .map(Value::try_from)
            .collect::<Result<Vec<_>>>()
            .map_err(invalid)?;
        let rows = self.query(&q.pred, &q.input, &inputs).map_err(invalid)?;
        Ok(rows
            .iter()
            .map(|row| pb::Row {
                values: row.iter().map(pb::Value::from).collect(),
            })
            .collect())
    }

    fn read_reply(&self, r: &pb::ReadRequest) -> std::result::Result<pb::ReadResponse, CallError> {
        Ok(
            match self.read(&r.r#type, &r.remote, &r.name).map_err(invalid)? {
                Some((attrs, computed)) => pb::ReadResponse {
                    found: true,
                    attrs: Some(wire::doc(&attrs)),
                    computed: Some(wire::doc(&self.outward(&r.r#type, &r.name, &computed))),
                },
                None => pb::ReadResponse::default(),
            },
        )
    }

    fn plan_reply(&self, r: &pb::PlanRequest) -> std::result::Result<pb::PlanResponse, CallError> {
        let prior = doc_of(r.prior.as_ref())?;
        let desired = doc_of(r.desired.as_ref())?;
        let (changes, requires_replace) = self
            .plan(
                &r.r#type,
                &r.name,
                &r.remote,
                prior.as_ref(),
                desired.as_ref(),
            )
            .map_err(invalid)?;
        Ok(pb::PlanResponse {
            changes: changes.iter().map(pb::Change::from).collect(),
            requires_replace,
        })
    }

    fn import_reply(
        &self,
        r: pb::ImportRequest,
    ) -> std::result::Result<pb::ImportResponse, CallError> {
        Ok(
            match self.read(&r.r#type, &r.remote, "").map_err(invalid)? {
                Some((attrs, computed)) => pb::ImportResponse {
                    found: true,
                    name: s(&attrs, "name").unwrap_or(&r.remote).to_string(),
                    computed: Some(wire::doc(&self.outward(
                        &r.r#type,
                        s(&attrs, "name").unwrap_or(&r.remote),
                        &computed,
                    ))),
                    r#type: r.r#type,
                    attrs: Some(wire::doc(&attrs)),
                },
                None => pb::ImportResponse::default(),
            },
        )
    }
}

fn doc_of(v: Option<&pb::Value>) -> std::result::Result<Option<Json>, CallError> {
    v.map(wire::from_doc).transpose().map_err(invalid)
}
