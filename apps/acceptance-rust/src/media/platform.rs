use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    routing::get,
};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use uuid::Uuid;

use crate::{audit, refresh::BrowserImport, store::Store, web::WebError};

use super::{
    BUS, Dataset, Industry, Medium, Options, Selection, UPLOAD_BYTES, database, date_month,
    optimize_snapshot, optimized_snapshot_variants,
    plan::bus_route_key,
    read_evidence, receive,
    schedule::Calendar,
    visuals::{Evidence, resource_key},
};

const FORBIDDEN: WebError = (
    StatusCode::FORBIDDEN,
    "本接口仅供当前数据平台取数进程使用。",
);
const MISSING: WebError = (StatusCode::CONFLICT, "本次取数尚未提交完整档期数据。");

pub(crate) struct EvidenceRefresh {
    pins: BTreeMap<String, Uuid>,
    staged: Mutex<Option<BTreeMap<Uuid, Evidence>>>,
}

impl EvidenceRefresh {
    pub(super) fn new(pins: BTreeMap<String, Uuid>) -> Self {
        Self {
            pins,
            staged: Mutex::new(Some(BTreeMap::new())),
        }
    }

    async fn stage(&self, store: Store, batch: Uuid, additions: Evidence) -> Result<(), WebError> {
        let pinned = database(move || {
            store
                .connect()?
                .query_row(
                    "SELECT payload FROM media_snapshots WHERE id=?1",
                    [batch.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|payload| serde_json::from_str::<Dataset>(&payload).map_err(decode_error))
                .transpose()
        })
        .await?
        .ok_or(MISSING)?;
        let mut staging = self.staged.lock().expect("evidence staging lock");
        staging
            .as_mut()
            .ok_or(MISSING)?
            .entry(batch)
            .or_default()
            .supplement(additions, &pinned.stock)
            .map_err(|reason| (StatusCode::UNPROCESSABLE_ENTITY, reason))
    }

    pub(super) async fn publish(
        &self,
        store: &Store,
        worker: Uuid,
        task: Uuid,
    ) -> Result<BTreeMap<String, Uuid>, WebError> {
        let staged = self
            .staged
            .lock()
            .expect("evidence staging lock")
            .take()
            .unwrap_or_default();
        if staged.is_empty() {
            return Ok(self.pins.clone());
        }
        let mut pins = self.pins.clone();
        let store = store.clone();
        database(move || {
            let mut db = store.connect()?;
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            for batch in pins.values_mut() {
                let Some(additions) = staged.get(batch) else {
                    continue;
                };
                let (base, root, prior) = tx.query_row(
                    "SELECT latest.id, coalesce(pinned.evidence_root,pinned.id), latest.payload
                     FROM media_snapshots AS pinned
                     JOIN media_snapshots AS latest ON latest.id=coalesce((
                         SELECT id FROM media_snapshots WHERE evidence_root=coalesce(pinned.evidence_root,pinned.id)
                         ORDER BY rowid DESC LIMIT 1
                     ),pinned.id)
                     WHERE pinned.id=?1 AND pinned.industry='food'",
                    [batch.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
                )?;
                let mut dataset: Dataset = serde_json::from_str(&prior).map_err(decode_error)?;
                let previous = serde_json::to_string(&dataset.evidence).map_err(encode_error)?;
                // Merge onto the latest published evidence, including unrelated manual additions.
                dataset.evidence
                    .supplement(additions.clone(), &dataset.stock)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                if serde_json::to_string(&dataset.evidence).map_err(encode_error)? == previous {
                    continue;
                }
                let updated = Uuid::new_v4();
                tx.execute(
                    "INSERT INTO media_snapshots (id,industry,starts,ends,observed_at,payload,source,refresh_id,media_mask,evidence_root)
                     SELECT ?1,industry,starts,ends,observed_at,?2,source,?3,media_mask,?4
                     FROM media_snapshots WHERE id=?5",
                    params![
                        updated.to_string(),
                        serde_json::to_string(&dataset).map_err(encode_error)?,
                        worker.to_string(),
                        root,
                        base,
                    ],
                )?;
                audit::write(&tx, "平台", "按清单保存新云证据", Some(task),
                    &format!("原库存快照 {batch} · 补证快照 {updated}"))?;
                *batch = updated;
            }
            tx.commit()?;
            Ok(pins)
        })
        .await
    }
}

fn decode_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn encode_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

#[derive(Clone, Serialize)]
pub(crate) struct EvidencePlan {
    batch: Uuid,
    starts: String,
    ends: String,
    routes: Vec<Vec<String>>,
    assets: Vec<Asset>,
    months: Vec<String>,
    available: bool,
}

#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Serialize)]
struct Asset {
    media: Medium,
    id: String,
    name: String,
    station: Option<String>,
    grade: String,
}

impl EvidencePlan {
    fn accepts(&self, evidence: &Evidence) -> bool {
        let assets: BTreeSet<_> = self
            .assets
            .iter()
            .flat_map(|asset| {
                [&asset.id, &asset.name].map(|name| (asset.media, resource_key(asset.media, name)))
            })
            .collect();
        let route_sets: BTreeSet<_> = self
            .routes
            .iter()
            .map(|set| {
                set.iter()
                    .map(|route| bus_route_key(route))
                    .collect::<BTreeSet<_>>()
            })
            .collect();
        let matches =
            |medium, resource: &str| assets.contains(&(medium, resource_key(medium, resource)));
        evidence
            .reach
            .iter()
            .all(|row| matches(row.medium, &row.resource))
            && evidence
                .shapes
                .iter()
                .all(|shape| matches(shape.medium, &shape.resource))
            && evidence
                .maps
                .iter()
                .all(|map| route_sets.contains(&map.route_keys()))
    }
}

async fn plan(
    State(store): State<Store>,
    worker: Option<Extension<BrowserImport>>,
) -> Result<Json<Vec<EvidencePlan>>, WebError> {
    let Extension(worker) = worker.ok_or(FORBIDDEN)?;
    let manifest = worker.request.evidence_plan.clone();
    if let Some(plans) = manifest.get() {
        return Ok(Json(plans.clone()));
    }
    let owner = worker.id.to_string();
    let proof = worker.request.proof.clone();
    let db_store = store.clone();
    let batches = database(move || {
        let db = db_store.connect()?;
        if let Some(proof) = proof {
            let mut query = db.prepare(
                "SELECT starts,ends,id FROM media_snapshots WHERE id=?1 AND starts=?2 AND industry='food'",
            )?;
            return proof
                .pins
                .iter()
                .map(|(starts, id)| {
                    query.query_row(params![id.to_string(), starts], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                })
                .collect();
        }
        db.prepare("SELECT starts, ends, id FROM media_snapshots WHERE refresh_id=?1 AND source='platform' ORDER BY starts")?
            .query_map([owner], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
    }).await?;
    let request = worker.request;
    if batches.is_empty() || (request.proof.is_some() && request.portfolio.get().is_none()) {
        return Err(MISSING);
    }
    let pins = batches
        .iter()
        .map(|(starts, _, id)| Ok((starts.clone(), id.parse::<Uuid>().map_err(|_| MISSING)?)))
        .collect::<Result<BTreeMap<_, _>, WebError>>()?;
    let options = Options {
        industry: Industry::parse(request.industry)
            .filter(|industry| *industry == Industry::Food)
            .ok_or(FORBIDDEN)?,
        budget: Some(request.budget),
        media: Some(match request.media {
            "bus" => Selection::Bus,
            "metro" => Selection::Metro,
            "both" => Selection::Both,
            _ => return Err(FORBIDDEN),
        }),
        starts: Some(request.starts),
        ends: Some(request.ends),
        required_routes: request.required_routes,
        target_centers: request.target_centers,
        source: super::tasks::Source::Platform,
        batch: (batches.len() == 1)
            .then(|| pins.values().next().copied())
            .flatten(),
        ..Options::default()
    };
    let budget = request.budget;
    let policy = request.policy;
    let locked = request.portfolio;
    let variants = if let Some(mut rolling) = request.rolling {
        rolling.batches = pins.clone();
        let calendar = Calendar::load(&store, &options, &rolling).await?;
        tokio::task::spawn_blocking(move || {
            let portfolio = locked.get_or_init(|| calendar.optimize(&options, budget, policy));
            calendar.variants(&options, portfolio)
        })
        .await
        .map_err(|_| MISSING)?
    } else {
        let snapshot = super::read_snapshot(store, &options)
            .await?
            .ok_or(MISSING)?;
        tokio::task::spawn_blocking(move || {
            let portfolio =
                locked.get_or_init(|| optimize_snapshot(&options, &snapshot, budget, policy));
            optimized_snapshot_variants(&options, &snapshot, portfolio)
        })
        .await
        .map_err(|_| MISSING)?
    };
    let mut output = Vec::with_capacity(batches.len());
    for (starts, ends, _) in batches {
        let mut route_sets = BTreeSet::new();
        let mut assets = BTreeSet::new();
        let periods = || {
            variants
                .iter()
                .flat_map(|variant| &variant.periods)
                .filter(|period| period.starts == starts && period.ends == ends)
        };
        let available =
            periods().count() == variants.len() && periods().all(|period| period.available);
        for period in periods() {
            let routes = period
                .rows
                .iter()
                .filter(|row| row.media == BUS)
                .map(|row| row.stock.name.clone())
                .collect::<BTreeSet<_>>();
            if !routes.is_empty() {
                route_sets.insert(routes.into_iter().collect::<Vec<_>>());
            }
            for row in &period.rows {
                assets.insert(Asset {
                    media: row.media,
                    id: if row.stock.code.is_empty() {
                        row.stock.name.clone()
                    } else {
                        row.stock.code.clone()
                    },
                    name: row.stock.name.clone(),
                    station: row.stock.station.clone(),
                    grade: row.stock.grade.clone(),
                });
            }
        }
        let first = date_month(&starts).ok_or(MISSING)?;
        let last = date_month(&ends).ok_or(MISSING)?;
        output.push(EvidencePlan {
            batch: pins[&starts],
            starts,
            ends,
            routes: route_sets.into_iter().collect(),
            assets: assets.into_iter().collect(),
            months: (first..=last)
                .map(|month| format!("2023-{month:02}"))
                .collect(),
            available,
        });
    }
    Ok(Json(manifest.get_or_init(|| output).clone()))
}

async fn submit(
    State(store): State<Store>,
    worker: Option<Extension<BrowserImport>>,
    body: Multipart,
) -> Result<StatusCode, WebError> {
    let Extension(worker) = worker.ok_or(FORBIDDEN)?;
    let (fields, files) = receive(body).await?;
    let batch = fields
        .get("batch")
        .and_then(|value| value.parse::<Uuid>().ok())
        .ok_or(FORBIDDEN)?;
    let plan = worker
        .request
        .evidence_plan
        .get()
        .and_then(|plans| plans.iter().find(|plan| plan.batch == batch))
        .ok_or(FORBIDDEN)?;
    if fields.len() != 1
        || files.is_empty()
        || files
            .keys()
            .any(|field| !matches!(field.as_str(), "official_maps" | "reach" | "route_geometry"))
    {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "取证文件缺失或格式无效。"));
    }
    let additions =
        read_evidence(files).map_err(|detail| (StatusCode::UNPROCESSABLE_ENTITY, detail))?;
    if !plan.accepts(&additions) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "取证文件包含本档期入选范围外的资源或线路组合。",
        ));
    }
    if let Some(proof) = &worker.request.proof {
        proof.stage(store, batch, additions).await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let result = database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let id = batch.to_string();
        let owner = worker.id.to_string();
        let before: Option<String> = tx.query_row(
            "SELECT payload FROM media_snapshots WHERE id=?1 AND refresh_id=?2 AND source='platform'",
            params![id, owner], |row| row.get(0),
        ).optional()?;
        let Some(before) = before else { return Ok(Err(FORBIDDEN)); };
        let mut dataset: Dataset = serde_json::from_str(&before).map_err(decode_error)?;
        if let Err(error) = dataset.evidence.supplement(additions, &dataset.stock) {
            return Ok(Err((StatusCode::UNPROCESSABLE_ENTITY, error)));
        }
        let payload = serde_json::to_string(&dataset).map_err(encode_error)?;
        if payload != before {
            tx.execute("UPDATE media_snapshots SET payload=?1 WHERE id=?2 AND refresh_id=?3", params![payload, id, owner])?;
            let verified = dataset.evidence.reach.iter().filter(|row| row.unit_verified).count();
            let historical = dataset.evidence.reach.len() - verified;
            audit::write(&tx, "平台", "保存新云证据", None, &format!(
                "快照 {id} · 新云线路图 {} 张 · 单位触达 {verified} 条 · 历史标签人流 {historical} 条", dataset.evidence.maps.len(),
            ))?;
        }
        tx.commit()?;
        Ok(Ok(()))
    }).await?;
    result?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn router() -> Router<Store> {
    Router::new().route(
        "/api/media-proposals/platform-evidence",
        get(plan)
            .post(submit)
            .layer(DefaultBodyLimit::max(6 * UPLOAD_BYTES + 64 * 1024)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            plan::Source,
            visuals::{OfficialMap, Reach},
        },
        *,
    };

    #[test]
    fn evidence_is_bound_to_the_selected_resources_and_exact_map_sets() {
        let plan = EvidencePlan {
            batch: Uuid::nil(),
            starts: "2027-07-01".into(),
            ends: "2027-07-31".into(),
            routes: vec![vec!["95路".into()], vec!["927路".into()]],
            assets: [("95", BUS), ("927", BUS), ("M-1", Medium::Metro)]
                .map(|(id, media)| Asset {
                    media,
                    id: id.into(),
                    name: id.into(),
                    station: None,
                    grade: String::new(),
                })
                .to_vec(),
            months: Vec::new(),
            available: true,
        };
        let mut evidence = Evidence {
            reach: vec![Reach {
                medium: Medium::Metro,
                resource: "m-1".into(),
                month: 5,
                daily: 20,
                reference: "fixture".into(),
                source: Source {
                    file: "fixture".into(),
                    row: 2,
                },
                baseline: false,
                unit_verified: true,
            }],
            maps: vec![OfficialMap {
                routes: vec!["95".into()],
                image: String::new(),
                captured_at: String::new(),
                replayed: false,
            }],
            ..Evidence::default()
        };
        assert!(plan.accepts(&evidence));
        evidence.maps[0].routes.push("927".into());
        assert!(!plan.accepts(&evidence));
        evidence.maps.clear();
        evidence.reach[0].resource = "M-2".into();
        assert!(!plan.accepts(&evidence));
    }
}
