//! The OVH API's request signature: `$1$` and the hex SHA-1 of the
//! application secret, the consumer key, the method, the full URL, the
//! body and the timestamp, joined by `+`
//! (https://help.ovhcloud.com/csm/en-gb-api-getting-started-ovhcloud-api).

use sha1::{Digest, Sha1};

/// The `X-Ovh-Signature` of one request. `url` is the whole URL, query
/// included; `body` is empty for a GET or a DELETE.
pub fn signature(
    secret: &str,
    consumer: &str,
    method: &str,
    url: &str,
    body: &str,
    timestamp: i64,
) -> String {
    let text = format!("{secret}+{consumer}+{method}+{url}+{body}+{timestamp}");
    let digest = Sha1::digest(text.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("$1${hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // go-ovh's test credentials and signatures (ovh/ovh_test.go,
    // TestAllAPIMethods): `/auth` on ovh-eu at 1457018875.
    const SECRET: &str = "9ufkBmLaTQ9nz5yMUlg79taH0GNnzDjk";
    const CONSUMER: &str = "5mBuy6SUQcRw2ZUxg0cG68BoDKpED4KY";
    const URL: &str = "https://eu.api.ovh.com/1.0/auth";
    const TIME: i64 = 1457018875;

    #[test]
    fn signs_as_ovhs_own_clients_do() {
        let sig = |method, body| signature(SECRET, CONSUMER, method, URL, body, TIME);
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
}
