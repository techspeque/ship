//! What Docker reports about an image Ship built: its id, the ports it
//! declares with `EXPOSE` and the paths it declares with `VOLUME`.

use std::collections::BTreeMap;

use serde_json::Value;
use ship_core::deploy::ExposedPort;

use crate::argv::image_inspect;
use crate::error::DockerError;
use crate::inspect::{inspect_json, label_map};
use crate::labels::ShipLabels;
use crate::runner::Runner;

/// One image as `image inspect` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// The content-addressed id, `sha256:<hex>`.
    pub id: String,
    /// `Config.ExposedPorts`, sorted; entries Ship cannot read (a range,
    /// `sctp`) are left out.
    pub exposed: Vec<ExposedPort>,
    /// `Config.Volumes`: the container paths, sorted.
    pub volumes: Vec<String>,
    /// Ship's labels on the image, when it carries all four.
    pub labels: Option<ShipLabels>,
}

/// `image inspect` of `reference`, read into an [`ImageInfo`].
pub async fn inspect_image(runner: &Runner, reference: &str) -> Result<ImageInfo, DockerError> {
    let json = inspect_json(runner, &image_inspect(reference)?).await?;
    parse_image(&json)
}

/// Parses `image inspect` output holding exactly one image.
pub fn parse_image(json: &[u8]) -> Result<ImageInfo, DockerError> {
    let malformed = |what: &str| DockerError::Malformed(format!("image inspect: {what}"));
    let items: Vec<Value> = serde_json::from_slice(json).map_err(|e| malformed(&e.to_string()))?;
    let [item] = items.as_slice() else {
        return Err(malformed("expected exactly one image"));
    };
    let id = item
        .get("Id")
        .and_then(Value::as_str)
        .filter(|id| is_image_id(id))
        .ok_or_else(|| malformed("missing or malformed Id"))?
        .to_owned();
    let keys = |pointer: &str| -> Vec<String> {
        item.pointer(pointer)
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut exposed: Vec<ExposedPort> = keys("/Config/ExposedPorts")
        .iter()
        .filter_map(|k| ExposedPort::parse(k))
        .collect();
    exposed.sort();
    exposed.dedup();
    let mut volumes = keys("/Config/Volumes");
    volumes.sort();
    let raw: BTreeMap<String, String> = label_map(item.pointer("/Config/Labels"));
    Ok(ImageInfo {
        id,
        exposed,
        volumes,
        labels: ShipLabels::parse(&raw),
    })
}

/// `sha256:` and 64 lowercase hex digits.
fn is_image_id(id: &str) -> bool {
    id.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        && !id.bytes().any(|b| b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ship_core::deploy::Protocol;

    fn id() -> String {
        format!("sha256:{}", "ab".repeat(32))
    }

    #[test]
    fn ports_volumes_and_labels_are_read() {
        let (p, e, s, r) = crate::labels::tests::ids();
        let doc = json!([{
            "Id": id(),
            "Config": {
                "ExposedPorts": {
                    "8080/tcp": {}, "3000/tcp": {}, "53/udp": {}, "9000-9010/tcp": {},
                },
                "Volumes": { "/var/lib/data": {}, "/cache": {} },
                "Labels": {
                    "dev.ship.project": p.to_string(),
                    "dev.ship.environment": e.to_string(),
                    "dev.ship.service": s.to_string(),
                    "dev.ship.release": r.to_string(),
                },
            },
        }]);
        let info = parse_image(doc.to_string().as_bytes()).unwrap();
        assert_eq!(info.id, id());
        let ports: Vec<String> = info.exposed.iter().map(ToString::to_string).collect();
        assert_eq!(ports, ["53/udp", "3000/tcp", "8080/tcp"]);
        assert_eq!(info.exposed[0].protocol, Protocol::Udp);
        assert_eq!(info.volumes, ["/cache", "/var/lib/data"]);
        let labels = info.labels.unwrap();
        assert_eq!((labels.environment, labels.release), (e, r));
    }

    #[test]
    fn nulls_are_empty() {
        let doc = json!([{
            "Id": id(),
            "Config": { "ExposedPorts": null, "Volumes": null, "Labels": null },
        }]);
        let info = parse_image(doc.to_string().as_bytes()).unwrap();
        assert!(info.exposed.is_empty() && info.volumes.is_empty());
        assert_eq!(info.labels, None);
        let bare = json!([{ "Id": id() }]);
        assert!(parse_image(bare.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn malformed_output_is_refused() {
        for bad in [
            json!([]),
            json!([{ "Id": id() }, { "Id": id() }]),
            json!([{ "Id": "abc" }]),
            json!([{ "Id": format!("sha256:{}", "AB".repeat(32)) }]),
            json!([{}]),
            json!({ "Id": id() }),
        ] {
            assert!(
                matches!(
                    parse_image(bad.to_string().as_bytes()),
                    Err(DockerError::Malformed(_))
                ),
                "{bad}"
            );
        }
        assert!(parse_image(b"not json").is_err());
    }
}
