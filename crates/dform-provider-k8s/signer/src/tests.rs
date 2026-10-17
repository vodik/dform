use super::*;
use alloc::collections::BTreeMap;
use dform_signer::Command;

/// A host as dform's performs a signer's needs, from what it is given:
/// the grant's value, the environment, files by path, and a command's
/// output by its program; each command it ran kept.
#[derive(Default)]
struct Host {
    given: Vec<(String, Vec<u8>)>,
    env: Vec<(String, String)>,
    files: BTreeMap<String, Vec<u8>>,
    exec: Option<fn(&Command) -> Vec<u8>>,
    ran: Vec<Command>,
}

impl Host {
    fn env(mut self, k: &str, v: &str) -> Host {
        self.env.push((k.into(), v.into()));
        self
    }

    fn file(mut self, path: &str, text: &str) -> Host {
        self.files.insert(path.into(), text.into());
        self
    }

    fn given(mut self, name: &str, text: &str) -> Host {
        self.given.push((name.into(), text.into()));
        self
    }

    fn perform(&mut self, need: &Need) -> Got {
        match need {
            Need::Given => Got::Given(self.given.clone()),
            Need::Env(vars) => Got::Env(
                self.env
                    .iter()
                    .filter(|(k, _)| vars.contains(k))
                    .cloned()
                    .collect(),
            ),
            Need::File(path) => Got::File(self.files.get(path).cloned()),
            Need::Exec(c) => {
                self.ran.push(c.clone());
                Got::Exec((self.exec.expect("no command"))(c))
            }
            Need::Fetch(_) => panic!("a kubeconfig fetches nothing"),
        }
    }

    /// The signer's material, each round of its needs performed; or its
    /// refusal.
    fn material(&mut self) -> Result<Material, String> {
        let mut needs = Kube.needs()?;
        let mut got = Vec::new();
        for _ in 0..8 {
            got.extend(needs.iter().map(|n| self.perform(n)));
            match Kube.step(&got)? {
                Outcome::Material(m) => return Ok(m),
                Outcome::More(more) => needs = more,
            }
        }
        panic!("it kept asking")
    }
}

const KC: &str = "current-context: c\ncontexts: [{name: c, context: {cluster: k, user: u, \
                  namespace: shop}}]\nclusters: [{name: k, cluster: {server: 'https://k:6443', \
                  certificate-authority: ca.pem}}]\nusers: [{name: u, user: {tokenFile: tok}}, \
                  {name: v, user: {}}]\n";

fn bearer(m: &Material) -> Option<&str> {
    m.recipe.iter().find_map(|r| match r {
        Recipe::Bearer(t) => Some(t.as_str()),
        _ => None,
    })
}

/// `~/.kube/config` with no `KUBECONFIG`; the files it names are its
/// directory's; its server and namespace are what the credential is for;
/// a user with nothing is anonymous.
#[test]
fn the_environments_kubeconfig_and_the_files_beside_it() {
    let mut host = Host::default()
        .env("HOME", "/h")
        .file("~/.kube/config", KC)
        .file("~/.kube/ca.pem", "CA")
        .file("~/.kube/tok", "t0k\n");
    let m = host.material().unwrap();
    assert_eq!(m.endpoint.as_deref(), Some("https://k:6443"));
    assert_eq!(m.namespace.as_deref(), Some("shop"));
    assert_eq!(
        m.recipe,
        [Recipe::Ca(b"CA".to_vec()), Recipe::Bearer("t0k".into())]
    );
    let anonymous = KC.replace("user: u,", "user: v,");
    let mut host = Host::default()
        .file("~/.kube/config", &anonymous)
        .file("~/.kube/ca.pem", "CA");
    assert_eq!(
        host.material().unwrap().recipe,
        [Recipe::Ca(b"CA".to_vec())]
    );
    let mut host = Host::default().file("~/.kube/config", KC);
    assert_eq!(
        host.material().unwrap_err(),
        "the kubeconfig at ~/.kube/config: certificate-authority ~/.kube/ca.pem cannot be read"
    );
}

/// `KUBECONFIG`'s files merged: the first to name a context wins, and the
/// first current context; each file's paths its own directory's; a path
/// under the home is shown as `~/`.
#[test]
fn kubeconfigs_merge_as_kubectl_merges_them() {
    let second = "current-context: other\ncontexts: [{name: c, context: {cluster: x}}, \
                  {name: other, context: {cluster: x}}]\nclusters: [{name: x, cluster: \
                  {server: 'https://x'}}]\n";
    let mut host = Host::default()
        .env("HOME", "/h")
        .env("KUBECONFIG", "/h/a/kc:/missing:/b/kc")
        .file("/h/a/kc", KC)
        .file("/h/a/ca.pem", "CA")
        .file("/h/a/tok", "t")
        .file("/b/kc", second);
    let m = host.material().unwrap();
    assert_eq!(m.endpoint.as_deref(), Some("https://k:6443"));
    assert_eq!(bearer(&m), Some("t"));
    let none = Host::default()
        .env("HOME", "/h")
        .env("KUBECONFIG", "/h/a/kc:/b/kc")
        .material()
        .unwrap_err();
    assert_eq!(none, "no kubeconfig at ~/a/kc, /b/kc and not in a cluster");
    let bad = Host::default()
        .env("HOME", "/h")
        .file("~/.kube/config", "s3cret: [")
        .material()
        .unwrap_err();
    assert_eq!(
        bad,
        "the kubeconfig at ~/.kube/config: ~/.kube/config is not a kubeconfig (YAML)"
    );
}

/// No kubeconfig, and in a pod: its service account.
#[test]
fn in_a_pod_its_service_account() {
    let sa = "/var/run/secrets/kubernetes.io/serviceaccount";
    let mut host = Host::default()
        .env("KUBERNETES_SERVICE_HOST", "fd00::1")
        .env("KUBERNETES_SERVICE_PORT", "443")
        .file(&format!("{sa}/namespace"), "apps\n")
        .file(&format!("{sa}/ca.crt"), "CA")
        .file(&format!("{sa}/token"), "sa-token");
    let m = host.material().unwrap();
    assert_eq!(m.endpoint.as_deref(), Some("https://[fd00::1]:443"));
    assert_eq!(m.namespace.as_deref(), Some("apps"));
    assert_eq!(
        m.recipe,
        [
            Recipe::Ca(b"CA".to_vec()),
            Recipe::Bearer("sa-token".into())
        ]
    );
    let e = Host::default().material().unwrap_err();
    assert_eq!(e, "no kubeconfig at ~/.kube/config and not in a cluster");
}

/// The grant's kubeconfig (the program's, the operator's file): no
/// variable and no `~/.kube/config` is read; one that is not YAML says
/// so and never quotes itself.
#[test]
fn the_grants_kubeconfig_quotes_nothing() {
    let kc = "current-context: c\ncontexts: [{name: c, context: {cluster: k, user: u}}]\n\
              clusters: [{name: k, cluster: {server: 'https://k', insecure-skip-tls-verify: \
              true}}]\nusers: [{name: u, user: {username: a, password: b, \
              client-certificate-data: Q0VSVA==, client-key-data: S0VZ}}]\n";
    let mut host = Host::default()
        .given("kubeconfig", kc)
        .file("~/.kube/config", "not this one");
    let m = host.material().unwrap();
    assert_eq!(
        m.recipe,
        [
            Recipe::Insecure,
            Recipe::Headers(vec![("Authorization".into(), "Basic YTpi".into())]),
            Recipe::TlsClient(TlsClient {
                cert: b"CERT".to_vec(),
                key: b"KEY".to_vec()
            }),
        ]
    );
    let quoted = "insecure-skip-tls-verify: \"true\"";
    let m = Host::default()
        .given(
            "kubeconfig",
            &kc.replace("insecure-skip-tls-verify: true", quoted),
        )
        .material()
        .unwrap();
    assert!(
        !m.recipe.contains(&Recipe::Insecure),
        "a string is not true"
    );
    let e = Host::default()
        .given("kubeconfig", "s3cret: [")
        .material()
        .unwrap_err();
    assert_eq!(e, "it is not a kubeconfig (YAML)");
}

/// A program's parts: a host, a token; a host alone is refused.
#[test]
fn parts_make_one() {
    let m = Host::default()
        .given("host", "k.example:6443")
        .given("token", "t")
        .material()
        .unwrap();
    assert_eq!(m.endpoint.as_deref(), Some("https://k.example:6443"));
    assert_eq!(m.recipe, [Recipe::Bearer("t".into())]);
    let e = Host::default().given("host", "k").material().unwrap_err();
    assert_eq!(
        e,
        "host needs a token, or a client_certificate and a client_key"
    );
}

/// A user's exec plugin is run by the host with its argv and variables,
/// told it is not interactive; its token is applied, and its
/// `expirationTimestamp` is when the material expires. A cluster's
/// `tls-server-name` and `proxy-url` are the host's to apply.
#[test]
fn an_exec_plugins_answer_and_the_clusters_server_name_and_proxy() {
    let kc = "current-context: c\ncontexts: [{name: c, context: {cluster: k, user: u}}]\n\
              clusters: [{name: k, cluster: {server: 'https://10.0.0.1:6443', tls-server-name: \
              k.example, proxy-url: 'http://proxy:3128'}}]\nusers: [{name: u, user: {exec: \
              {apiVersion: client.authentication.k8s.io/v1beta1, command: plugin, args: [get, \
              token], env: [{name: T, value: x}]}}}]\n";
    let mut host = Host::default().given("kubeconfig", kc);
    host.exec = Some(|_| {
        br#"{"status": {"token": "from-exec", "expirationTimestamp": "2030-01-02T03:04:05Z"}}"#
            .to_vec()
    });
    let m = host.material().unwrap();
    assert_eq!(
        m.recipe,
        [
            Recipe::ServerName("k.example".into()),
            Recipe::Proxy("http://proxy:3128".into()),
            Recipe::Bearer("from-exec".into()),
        ]
    );
    assert_eq!(m.expires, Some(1_893_553_445));
    let ran = &host.ran[0];
    assert_eq!(ran.argv, ["plugin", "get", "token"]);
    assert_eq!(ran.env[0], ("T".into(), "x".into()));
    let (k, info) = &ran.env[1];
    let info: serde_json::Value = serde_json::from_str(info).unwrap();
    assert_eq!(k, "KUBERNETES_EXEC_INFO");
    assert_eq!(info["apiVersion"], "client.authentication.k8s.io/v1beta1");
    assert_eq!(info["spec"]["interactive"], false);
    // Renewed from the start: the kubeconfig read, the plugin run again.
    assert_eq!(Kube.refresh(&m, 0).unwrap(), Outcome::More(first()));
    host.exec = Some(|_| br#"{"status": {}}"#.to_vec());
    assert_eq!(
        host.material().unwrap_err(),
        "its exec plugin plugin answered neither a token nor a client certificate"
    );
}

/// An exec plugin whose command is a path is the kubeconfig's
/// directory's; a bare name is looked up on PATH.
#[test]
fn an_exec_plugins_path_is_the_kubeconfigs() {
    let kc = |command: &str| {
        format!(
            "current-context: c\ncontexts: [{{name: c, context: {{cluster: k, user: u}}}}]\n\
             clusters: [{{name: k, cluster: {{server: 'https://k'}}}}]\nusers: [{{name: u, user: \
             {{exec: {{command: {command}}}}}}}]\n"
        )
    };
    for (command, run) in [("bin/auth", "/etc/kube/bin/auth"), ("auth", "auth")] {
        let mut host = Host::default()
            .env("KUBECONFIG", "/etc/kube/config")
            .file("/etc/kube/config", &kc(command));
        host.exec = Some(|_| br#"{"status": {"token": "t"}}"#.to_vec());
        host.material().unwrap();
        assert_eq!(host.ran[0].argv, [run]);
    }
}

/// An auth-provider is refused, naming it.
#[test]
fn an_auth_provider_is_refused() {
    let kc = "current-context: c\ncontexts: [{name: c, context: {cluster: k, user: u}}]\n\
              clusters: [{name: k, cluster: {server: 'https://k'}}]\nusers: [{name: u, user: \
              {auth-provider: {name: gcp}}}]\n";
    let e = Host::default()
        .given("kubeconfig", kc)
        .material()
        .unwrap_err();
    assert_eq!(
        e,
        "its user's auth-provider (gcp) is not supported: use an exec plugin"
    );
}
