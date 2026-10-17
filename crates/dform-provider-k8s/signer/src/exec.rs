//! A kubeconfig's exec plugin, as kubectl runs it: the command the host
//! runs for the signer, told it is not interactive
//! (`KUBERNETES_EXEC_INFO`), and its answer, an `ExecCredential`.

use crate::kubeconfig::Exec;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use dform_signer::Command;

impl Exec {
    /// The command the host runs: the plugin's argv, its variables, and
    /// `KUBERNETES_EXEC_INFO`, which says it is not run interactively.
    pub fn command(&self) -> Command {
        let info = serde_json::json!({
            "apiVersion": self.api_version,
            "kind": "ExecCredential",
            "spec": {"interactive": false},
        });
        let mut env = self.env.clone();
        env.push(("KUBERNETES_EXEC_INFO".into(), info.to_string()));
        Command {
            argv: core::iter::once(self.command.clone())
                .chain(self.args.iter().cloned())
                .collect(),
            env,
        }
    }

    /// What the plugin answered on its standard output: its
    /// `ExecCredential`'s status.
    pub fn answer(&self, out: &[u8]) -> Result<Answer, String> {
        let command = &self.command;
        let answer: serde_json::Value = serde_json::from_slice(out)
            .map_err(|_| format!("its exec plugin {command} answered no ExecCredential"))?;
        let status = &answer["status"];
        let s = |k: &str| status.get(k).and_then(serde_json::Value::as_str);
        let token = s("token").map(String::from);
        let client_cert = s("clientCertificateData")
            .zip(s("clientKeyData"))
            .map(|(c, k)| (c.as_bytes().to_vec(), k.as_bytes().to_vec()));
        if token.is_none() && client_cert.is_none() {
            return Err(format!(
                "its exec plugin {command} answered neither a token nor a client certificate"
            ));
        }
        let expires = match s("expirationTimestamp") {
            None => None,
            Some(t) => Some(dform_signer::parse_rfc3339(t).ok_or_else(|| {
                format!(
                    "its exec plugin {command} answered an expirationTimestamp that is not an \
                     RFC 3339 time in UTC"
                )
            })?),
        };
        Ok(Answer {
            token,
            client_cert,
            expires,
        })
    }
}

/// What an exec plugin answers: a token, or a client certificate and its
/// key (PEM), and when they expire, in seconds since the epoch. A user
/// with no plugin answers nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub token: Option<String>,
    pub client_cert: Option<(Vec<u8>, Vec<u8>)>,
    pub expires: Option<u64>,
}
