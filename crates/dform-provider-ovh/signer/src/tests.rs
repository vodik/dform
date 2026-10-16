use super::*;
use dform_signer::Response;

// go-ovh's test credentials and signatures (ovh/ovh_test.go,
// TestAllAPIMethods): `/auth` on ovh-eu at 1457018875.
const SECRET: &str = "9ufkBmLaTQ9nz5yMUlg79taH0GNnzDjk";
const CONSUMER: &str = "5mBuy6SUQcRw2ZUxg0cG68BoDKpED4KY";
const URL: &str = "https://eu.api.ovh.com/1.0/auth";
const TIME: i64 = 1457018875;

fn s(v: &str) -> String {
    v.to_string()
}

#[test]
fn signs_as_ovhs_own_clients_do() {
    let sig = |method, body: &str| {
        signature(
            SECRET.as_bytes(),
            CONSUMER.as_bytes(),
            method,
            URL,
            body.as_bytes(),
            TIME,
        )
    };
    assert_eq!(
        sig("GET", ""),
        "$1$e9556054b6309771395efa467c22e627407461ad"
    );
    assert_eq!(
        sig("DELETE", ""),
        "$1$a1eecd00b3b02b6cf5708b84b9ff42059a950d85"
    );
    let body = r#"{"a":"b","c":"d"}"#;
    assert_eq!(
        sig("POST", body),
        "$1$ec2fb5c7a81f64723c77d2e5b609ae6f58a84fc1"
    );
    assert_eq!(
        sig("PUT", body),
        "$1$8a75a9e7c8e7296c9dbeda6a2a735eb6bd58ec4b"
    );
}

/// The environment's keys and endpoint: the endpoint's clock asked, then
/// the material, whose signature is at the API's time (the host's `now`
/// and the clock's drift): the four headers, the request's own kept.
#[test]
fn the_environment_signs_at_the_apis_time() {
    let env = Got::Env(vec![
        (s("OVH_APPLICATION_KEY"), s("ak")),
        (s("OVH_APPLICATION_SECRET"), s(SECRET)),
        (s("OVH_CONSUMER_KEY"), s(CONSUMER)),
        (s("OVH_ENDPOINT"), s("ovh-eu")),
    ]);
    let mut got = vec![env, Got::File(None)];
    let Ok(Outcome::More(more)) = Ovh.step(&got) else {
        panic!("the clock is asked")
    };
    assert_eq!(
        more,
        [Need::Fetch(Request {
            method: s("GET"),
            url: s("https://eu.api.ovh.com/1.0/auth/time"),
            ..Request::default()
        })]
    );
    // The API's clock is 75 s ahead of the host's.
    got.push(Got::Fetch(Response {
        status: 200,
        body: b"1457018875".to_vec(),
        at: 1457018800,
        ..Response::default()
    }));
    let Ok(Outcome::Material(m)) = Ovh.step(&got) else {
        panic!("the material")
    };
    assert!(m.recipe == Recipe::Signed && m.expires.is_none());
    let req = Request {
        method: s("get"),
        url: s(URL),
        headers: vec![(s("Accept"), s("application/json"))],
        ..Request::default()
    };
    let signed = Ovh.sign(req, &m, 1457018800).unwrap();
    assert_eq!(
        signed.headers,
        [
            ("Accept", "application/json"),
            ("X-Ovh-Application", "ak"),
            ("X-Ovh-Consumer", CONSUMER),
            ("X-Ovh-Timestamp", "1457018875"),
            (
                "X-Ovh-Signature",
                "$1$e9556054b6309771395efa467c22e627407461ad"
            ),
        ]
        .map(|(k, v)| (s(k), s(v)))
    );
}

/// No key in the environment: `~/.ovh.conf` as OVH's clients read it, the
/// section `[default] endpoint` names (or `OVH_ENDPOINT`), or a section's
/// three lines alone (the operator's own file); what is missing named.
#[test]
fn the_file_is_read_as_ovhs_clients_read_it() {
    let conf = b"; ovh's own\n[default]\nendpoint=ovh-ca\n\n[ovh-ca]\napplication_key=ak-ca\n\
                 application_secret=as-ca\nconsumer_key=ck-ca\n\n[ovh-eu]\napplication_key=ak-eu\n\
                 application_secret=as-eu\nconsumer_key=ck-eu\n";
    let keys = Keys::of_file(conf, None).unwrap();
    assert_eq!(
        (keys.get("application_key"), keys.endpoint.as_deref()),
        ("ak-ca", Some("ovh-ca"))
    );
    let keys = Keys::of_file(conf, Some(s("ovh-eu"))).unwrap();
    assert_eq!(keys.get("consumer_key"), "ck-eu");
    let lines = b"application_key=a\napplication_secret=b\nconsumer_key=c\n";
    let keys = Keys::of_file(lines, None).unwrap();
    assert_eq!(
        (keys.get("application_key"), keys.endpoint.as_deref()),
        ("a", None)
    );
    // No endpoint to ask the clock of: the host's is the API's.
    let got = [Got::Env(Vec::new()), Got::File(Some(lines.to_vec()))];
    assert!(matches!(Ovh.step(&got), Ok(Outcome::Material(_))));
    let e = Keys::of_file(b"[default]\nendpoint=ovh-ca\n", None).unwrap_err();
    assert_eq!(e, "it has no section [ovh-ca]");
    let e = Keys::of_file(b"[ovh-ca]\napplication_key=a\n", Some(s("ovh-ca"))).unwrap_err();
    assert_eq!(e, "no application_secret in [ovh-ca]");
    let none = [Got::Env(Vec::new()), Got::File(None)];
    assert_eq!(
        Ovh.step(&none).unwrap_err(),
        "no OVH keys: neither OVH_APPLICATION_KEY, OVH_APPLICATION_SECRET and OVH_CONSUMER_KEY \
         in the environment nor ~/.ovh.conf"
    );
    let half = [
        Got::Env(vec![(s("OVH_APPLICATION_KEY"), s("a"))]),
        Got::File(None),
    ];
    assert_eq!(
        Ovh.step(&half).unwrap_err(),
        "OVH_APPLICATION_SECRET is not set in the environment"
    );
}
