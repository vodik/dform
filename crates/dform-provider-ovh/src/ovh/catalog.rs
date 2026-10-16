//! What a region offers: its flavors and images, each listed once per region
//! and run (`api::Client::list`), an instance's or a volume's named by its name, and a flavor
//! compared with another (an instance resizes only to a larger one).

use super::*;

impl Ovh {
    /// A region's flavors or images (`what`), listed once per run.
    pub(super) async fn list(
        &self,
        a: &Account,
        p: &str,
        what: &str,
        region: &str,
    ) -> api::Result<Arc<Json>> {
        a.client
            .list(&format!(
                "/cloud/project/{p}/{what}?region={}",
                escape(region)
            ))
            .await
    }

    /// A region's flavors and images, both asked at once: an instance is
    /// checked or read against both, and the API is a round trip away. A
    /// failure is left for the one that asks.
    pub(super) async fn list_region(&self, a: &Account, p: &str, region: &str) {
        let _ = dform_sdk::join(
            self.list(a, p, "flavor", region),
            self.list(a, p, "image", region),
        )
        .await;
    }

    pub(super) async fn flavor_id(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        name: &str,
    ) -> Result<String> {
        let flavors = self.list(a, p, "flavor", region).await?;
        map::flavor_id(&flavors, name).ok_or_else(|| {
            anyhow!(
                "flavor {name:?} is not offered in region {region} (it offers {})",
                map::names(&flavors)
            )
        })
    }

    pub(super) async fn image_id(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        name: &str,
    ) -> Result<String> {
        let images = self.list(a, p, "image", region).await?;
        map::image_id(&images, name).ok_or_else(|| {
            anyhow!(
                "image {name:?} is not in region {region} (it has {})",
                map::names(&images)
            )
        })
    }

    /// A flavor's or an image's name by id, from the region's list, else
    /// asked. `None` only when the API says it has none (an image retired
    /// from the region); a failed call is an error, not an unknown name.
    pub(super) async fn name_of(
        &self,
        a: &Account,
        p: &str,
        what: &str,
        region: &str,
        id: &str,
    ) -> api::Result<Option<String>> {
        let by_id = |list: &Json| {
            list.as_array()?
                .iter()
                .find(|x| s(x, "id") == Some(id))
                .and_then(|x| s(x, "name"))
                .map(str::to_string)
        };
        if let Some(n) = by_id(&*self.list(a, p, what, region).await?) {
            return Ok(Some(n));
        }
        let one = a
            .client
            .get_opt(&format!("/cloud/project/{p}/{what}/{}", escape(id)))
            .await?;
        Ok(one.and_then(|o| s(&o, "name").map(str::to_string)))
    }

    /// Whether flavor `now` is smaller than `was` in `region` (fewer
    /// vCPUs, less RAM or less disk): the API resizes an instance only to
    /// a flavor no smaller. `None` when the region's flavors cannot be
    /// listed or either is not among them.
    pub(super) async fn smaller_flavor(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        was: &str,
        now: &str,
    ) -> Option<bool> {
        let flavors = self.list(a, p, "flavor", region).await.ok()?;
        let size = |name: &str| {
            let f = flavors
                .as_array()?
                .iter()
                .find(|f| s(f, "name") == Some(name))?;
            let n = |k: &str| f.get(k).and_then(Json::as_i64).unwrap_or(0);
            Some([n("vcpus"), n("ram"), n("disk")])
        };
        let (was, now) = (size(was)?, size(now)?);
        Some(was.iter().zip(&now).any(|(w, n)| n < w))
    }
}
