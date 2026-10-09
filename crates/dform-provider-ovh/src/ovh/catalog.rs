//! What a region offers: its flavors and images, each listed once per region
//! (`Ovh::lists`), an instance's or a volume's named by its name, and a flavor
//! compared with another (an instance resizes only to a larger one).

use super::*;

impl Ovh {
    /// A region's flavors or images (`what`), listed once.
    pub(super) fn list(&self, a: &Account, p: &str, what: &str, region: &str) -> api::Result<Json> {
        let k = format!("{what}/{region}");
        if let Some(v) = self.lists.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
            return Ok(v.clone());
        }
        let v = a.client.get(&format!(
            "/cloud/project/{p}/{what}?region={}",
            escape(region)
        ))?;
        self.lists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(k, v.clone());
        Ok(v)
    }

    /// A region's flavors and images, both listed at once when either is
    /// not yet: an instance is checked or read against both, and the API
    /// is a round trip away. A failure is left for the one that asks.
    pub(super) fn list_region(&self, a: &Account, p: &str, region: &str) {
        let lists = self.lists.lock().unwrap_or_else(|e| e.into_inner());
        let missing: Vec<&str> = ["flavor", "image"]
            .into_iter()
            .filter(|w| !lists.contains_key(&format!("{w}/{region}")))
            .collect();
        drop(lists);
        if missing.len() < 2 {
            return;
        }
        std::thread::scope(|scope| {
            for what in missing {
                scope.spawn(move || self.list(a, p, what, region));
            }
        });
    }

    pub(super) fn flavor_id(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        name: &str,
    ) -> Result<String> {
        let flavors = self.list(a, p, "flavor", region)?;
        map::flavor_id(&flavors, name).ok_or_else(|| {
            anyhow!(
                "flavor {name:?} is not offered in region {region} (it offers {})",
                map::names(&flavors)
            )
        })
    }

    pub(super) fn image_id(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        name: &str,
    ) -> Result<String> {
        let images = self.list(a, p, "image", region)?;
        map::image_id(&images, name).ok_or_else(|| {
            anyhow!(
                "image {name:?} is not in region {region} (it has {})",
                map::names(&images)
            )
        })
    }

    /// A flavor's or an image's name by id, from the region's list, else
    /// asked.
    pub(super) fn name_of(
        &self,
        a: &Account,
        p: &str,
        what: &str,
        region: &str,
        id: &str,
    ) -> Option<String> {
        let by_id = |list: &Json| {
            list.as_array()?
                .iter()
                .find(|x| s(x, "id") == Some(id))
                .and_then(|x| s(x, "name"))
                .map(str::to_string)
        };
        if let Ok(list) = self.list(a, p, what, region)
            && let Some(n) = by_id(&list)
        {
            return Some(n);
        }
        let one = a
            .client
            .get_opt(&format!("/cloud/project/{p}/{what}/{}", escape(id)))
            .ok()??;
        s(&one, "name").map(str::to_string)
    }

    /// Whether flavor `now` is smaller than `was` in `region` (fewer
    /// vCPUs, less RAM or less disk): the API resizes an instance only to
    /// a flavor no smaller. `None` when the region's flavors cannot be
    /// listed or either is not among them.
    pub(super) fn smaller_flavor(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        was: &str,
        now: &str,
    ) -> Option<bool> {
        let flavors = self.list(a, p, "flavor", region).ok()?;
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
