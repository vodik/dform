//! Configure: the session's account, from its credentials (`config`)
//! and its project (an id, or a description, [`resolve_project`]), and
//! what the credentials are and lack, as notes (`credential`).

use super::*;
use std::path::PathBuf;

/// What a Configure gives: the program's settings, when they came, and
/// dform's cache directory.
struct Given {
    /// The program's `use ovh { .. }` settings came.
    configured: bool,
    /// The program configures it, and has not yet.
    deferred: bool,
    endpoint: Option<String>,
    /// The program's token, the name dform holds it in the host by.
    token: Option<String>,
    /// The keys the host signs with, as the operator's grant names them
    /// (`signed:lab`).
    signature: Option<String>,
    project: Option<String>,
    cache: Option<PathBuf>,
}

impl Given {
    fn of(config: &Json) -> Result<Given> {
        let settings = config.get("settings").filter(|v| v.is_object());
        let setting = |k: &str| -> Result<Option<String>> {
            match settings.and_then(|s| s.get(k)) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(v)) => Ok(Some(v.clone())),
                Some(v) => bail!(
                    "provider ovh: {k} is {}, not a string",
                    provider::fmt_value(Some(v))
                ),
            }
        };
        Ok(Given {
            configured: settings.is_some(),
            deferred: config.get("deferred") == Some(&Json::Bool(true)),
            endpoint: setting("endpoint")?,
            token: setting("token")?,
            signature: config
                .pointer("/credentials/signed")
                .and_then(Json::as_str)
                .map(String::from),
            project: setting("project")?,
            cache: config
                .get("cache")
                .and_then(Json::as_str)
                .map(PathBuf::from),
        })
    }

    /// What the host holds for the provider, by name.
    fn held(&self) -> config::Held<'_> {
        config::Held {
            token: self.token.as_deref(),
            signature: self.signature.as_deref(),
        }
    }
}

impl Ovh {
    /// Configure from `config` (dform's, with the program's `settings` the
    /// second time): the account's project id, for `expect_account`, and
    /// what the credentials are and lack, as notes.
    pub async fn configure(&self, config: &Json) -> Result<pb::ConfigureResponse> {
        let env = |k: &str| std::env::var(k).ok();
        self.configure_with(config, &env, &config::default_files())
            .await
    }

    /// `configure` with the environment `env` and the configuration files
    /// `files` (a test's own, so no real `ovh.conf` is read).
    pub async fn configure_with(
        &self,
        config: &Json,
        env: &dyn Fn(&str) -> Option<String>,
        files: &[PathBuf],
    ) -> Result<pb::ConfigureResponse> {
        let given = Given::of(config)?;
        if !given.configured && given.deferred {
            let why = "the program configures it (`use ovh { .. }`) and has not yet";
            self.set_account(Err(why.into()), true);
            return Ok(pb::ConfigureResponse::default());
        }
        let creds = config::resolve(given.endpoint.as_deref(), given.held(), env, files);
        let creds = match creds {
            Ok(c) => c,
            // The program names the account: it cannot be reached.
            Err(e) if given.configured => return Err(e.context("provider ovh")),
            Err(e) => {
                self.set_account(Err(format!("{e:#}")), false);
                return Ok(pb::ConfigureResponse::default());
            }
        };
        // The host's refusal names the provider and the credential.
        let client = Client::opened(creds).map_err(|e| anyhow!("{e}"))?;
        // Run outside a program and a project, as `provider check` runs
        // it: which credentials, and what they may do.
        let full = !given.configured && given.cache.is_none();
        let cache = given.cache.as_deref();
        let notes = crate::credential::notes(&client, full, cache, unix_now()).await;
        let project = given
            .project
            .or_else(|| env("OVH_CLOUD_PROJECT_SERVICE").filter(|v| !v.is_empty()));
        let project = match project {
            Some(p) => Some(resolve_project(&client, &p, cache).await?),
            None => None,
        };
        let account = project.clone();
        let reached = Account {
            client,
            project,
            cache: given.cache,
            poll: poll_every(env),
        };
        self.set_account(Ok(Arc::new(reached)), false);
        Ok(pb::ConfigureResponse {
            account,
            notes,
            ..Default::default()
        })
    }

    /// The session configured: the account it reaches, or why none;
    /// `awaiting` the program's settings.
    fn set_account(&self, account: std::result::Result<Arc<Account>, String>, awaiting: bool) {
        let c = Configured { account, awaiting };
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(c));
    }
}
