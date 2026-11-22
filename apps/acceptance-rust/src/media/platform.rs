use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    routing::get,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
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
    "本接口仅供当前从平台获取数据的进程使用。",
);
const MISSING: WebError = (StatusCode::CONFLICT, "本次获取数据尚未提交完整档期数据。");
const POLICY_UNAVAILABLE: WebError = (
    StatusCode::SERVICE_UNAVAILABLE,
    "核算策略未能在取证时限内就绪。",
);

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ReachCheck {
    pub(super) starts: String,
    pub(super) media: Medium,
    pub(super) resource: String,
    pub(super) month: u32,
    pub(super) exact: ReachFailure,
    pub(super) baseline: Option<ReachFailure>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ReachFailure {
    MonthNotConfigured,
    ResourceNotConfirmed,
    StationOnly,
    UnitNotConfirmed,
    NoObservation,
    AuthorizationRejected,
    LabelQueryError,
    PlatformError,
    PausedAfterProviderErrors,
}

impl ReachFailure {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::MonthNotConfigured => "未配置查询该统计月",
            Self::ResourceNotConfirmed => "无法匹配唯一的新云资源标签",
            Self::StationOnly => "仅取得站级历史人流",
            Self::UnitNotConfirmed => "有历史人流，缺少对应的单位日触达口径",
            Self::NoObservation => "未返回可用的该月统计",
            Self::AuthorizationRejected => "新云拒绝查询授权，请重新认证",
            Self::LabelQueryError => "新云媒体标签读取失败，请重试",
            Self::PlatformError => "新云统计查询失败，请重试",
            Self::PausedAfterProviderErrors => "新云连续查询失败后暂停",
        }
    }
}

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
        additions
            .check_exit_points(&pinned.stock, &pinned.coverage)
            .map_err(|reason| (StatusCode::UNPROCESSABLE_ENTITY, reason))?;
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
pub(crate) struct EvidenceManifest {
    periods: Vec<EvidencePlan>,
    overview_routes: Vec<Vec<String>>,
}

impl EvidenceManifest {
    pub(super) fn accepts_overview(&self, map: &super::visuals::OfficialMap) -> bool {
        let routes = map.route_keys();
        self.overview_routes
            .iter()
            .any(|set| route_keys(set) == routes)
    }
}

fn route_keys(routes: &[String]) -> BTreeSet<String> {
    routes.iter().map(|route| bus_route_key(route)).collect()
}

#[derive(Clone, Serialize)]
pub(crate) struct EvidencePlan {
    batch: Uuid,
    starts: String,
    ends: String,
    routes: Vec<Vec<String>>,
    geometry_routes: Vec<String>,
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
    exact_reach: Vec<String>,
    baseline_reach: Vec<String>,
    exit_verified: bool,
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
        let route_sets: BTreeSet<_> = self.routes.iter().map(|set| route_keys(set)).collect();
        let matches =
            |medium, resource: &str| assets.contains(&(medium, resource_key(medium, resource)));
        evidence.reach.iter().all(|row| {
            matches(row.medium, &row.resource)
                && (self.months.contains(&format!("2023-{:02}", row.month))
                    || (row.month == 5 && (row.baseline || !row.unit_verified)))
        }) && evidence
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
) -> Result<Json<EvidenceManifest>, WebError> {
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
        core_circle: request.core_circle,
        source: super::tasks::Source::Platform,
        batch: (batches.len() == 1)
            .then(|| pins.values().next().copied())
            .flatten(),
        ..Options::default()
    };
    let budget = request.budget;
    let mut policy = request.policy;
    let policy = *tokio::time::timeout(Duration::from_secs(100), policy.wait_for(Option::is_some))
        .await
        .map_err(|_| POLICY_UNAVAILABLE)?
        .map_err(|_| POLICY_UNAVAILABLE)?;
    let policy = policy.ok_or(POLICY_UNAVAILABLE)?;
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
    let overview_routes = variants
        .iter()
        .filter(|variant| variant.available && !variant.verified_route_overview())
        .map(super::visuals::Variant::route_names)
        .filter_map(|routes| {
            let keys = route_keys(&routes);
            (!keys.is_empty() && !request.saved_overview_routes.contains(&keys))
                .then_some((keys, routes))
        })
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    let mut output = Vec::with_capacity(batches.len());
    for (starts, ends, _) in batches {
        let mut route_sets = BTreeSet::new();
        let mut geometry_routes = BTreeSet::new();
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
                let routes = routes.into_iter().collect::<Vec<_>>();
                route_sets.insert(routes);
            }
            for row in &period.rows {
                if row.media == BUS && !period.has_geometry(row) {
                    geometry_routes.insert(row.stock.name.clone());
                }
                let (exact, baseline): (Vec<_>, Vec<_>) = row
                    .evidence
                    .iter()
                    .filter(|entry| entry.row.platform_verified)
                    .partition(|entry| entry.row.month == entry.applies_to);
                let months = |entries: Vec<&super::visuals::Observation>| {
                    entries
                        .into_iter()
                        .map(|entry| format!("2023-{:02}", entry.applies_to))
                        .collect()
                };
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
                    exact_reach: months(exact),
                    baseline_reach: months(baseline),
                    exit_verified: row.media == Medium::Metro && row.has_platform_exit(),
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
            geometry_routes: geometry_routes.into_iter().collect(),
            assets: assets.into_iter().collect(),
            months: (first..=last)
                .map(|month| format!("2023-{month:02}"))
                .collect(),
            available,
        });
    }
    Ok(Json(
        manifest
            .get_or_init(|| EvidenceManifest {
                periods: output,
                overview_routes,
            })
            .clone(),
    ))
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
        .and_then(|manifest| manifest.periods.iter().find(|plan| plan.batch == batch))
        .ok_or(FORBIDDEN)?;
    if fields.len() != 1
        || files.is_empty()
        || files
            .keys()
            .any(|field| !matches!(field.as_str(), "official_maps" | "reach" | "route_geometry"))
    {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "取证文件缺失或格式无效。"));
    }
    let mut additions =
        read_evidence(files).map_err(|detail| (StatusCode::UNPROCESSABLE_ENTITY, detail))?;
    if !plan.accepts(&additions) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "取证文件包含本档期入选范围外的资源、线路组合或统计月。",
        ));
    }
    for row in &mut additions.reach {
        row.platform_verified = true;
    }
    for shape in &mut additions.shapes {
        shape.platform_verified = true;
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
        if let Err(error) = additions.check_exit_points(&dataset.stock, &dataset.coverage) {
            return Ok(Err((StatusCode::UNPROCESSABLE_ENTITY, error)));
        }
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
            geometry_routes: vec![],
            assets: [("95", BUS), ("927", BUS), ("M-1", Medium::Metro)]
                .map(|(id, media)| Asset {
                    media,
                    id: id.into(),
                    name: id.into(),
                    station: None,
                    grade: String::new(),
                    exact_reach: vec![],
                    baseline_reach: vec![],
                    exit_verified: false,
                })
                .to_vec(),
            months: vec!["2023-07".into()],
            available: true,
        };
        let mut evidence = Evidence {
            reach: vec![Reach {
                medium: Medium::Metro,
                resource: "m-1".into(),
                month: 7,
                daily: 20,
                reference: "fixture".into(),
                source: Source {
                    file: "fixture".into(),
                    row: 2,
                },
                baseline: false,
                unit_verified: true,
                platform_verified: true,
            }],
            maps: vec![OfficialMap {
                routes: vec!["95".into()],
                image: String::new(),
                captured_at: String::new(),
                replayed: false,
                basemap_verified: true,
            }],
            ..Evidence::default()
        };
        assert!(plan.accepts(&evidence));
        evidence.reach[0].month = 6;
        assert!(!plan.accepts(&evidence));
        evidence.reach[0].month = 5;
        assert!(!plan.accepts(&evidence));
        evidence.reach[0].baseline = true;
        assert!(plan.accepts(&evidence));
        evidence.reach[0].baseline = false;
        evidence.reach[0].unit_verified = false;
        assert!(plan.accepts(&evidence));
        evidence.maps[0].routes.push("927".into());
        assert!(!plan.accepts(&evidence));
        evidence.maps.clear();
        evidence.reach[0].resource = "M-2".into();
        assert!(!plan.accepts(&evidence));
    }
}
