use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use super::{
    BUS, METRO, Snapshot, month_days,
    plan::{
        self, Cost, Inventory, Item, Medium, Priority, Proposal, QuoteFloor, RouteCheck, Source,
    },
};

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) struct Coordinate(f64, f64);

impl Coordinate {
    pub(super) fn shanghai(longitude: f64, latitude: f64) -> Option<Self> {
        ((120.7..=122.1).contains(&longitude) && (30.6..=32.0).contains(&latitude))
            .then_some(Self(longitude, latitude))
    }

    fn same_location(self, other: Self) -> bool {
        // Compare to the precision of a typical six-decimal coordinate export.
        (self.0 - other.0).abs() <= 0.000_001 && (self.1 - other.1).abs() <= 0.000_001
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(super) struct Evidence {
    pub(super) reach: Vec<Reach>,
    pub(super) shapes: Vec<Shape>,
    #[serde(default)]
    pub(super) maps: Vec<OfficialMap>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OfficialMap {
    pub(super) routes: Vec<String>,
    pub(super) image: String,
    pub(super) captured_at: String,
    #[serde(default)]
    pub(super) replayed: bool,
    #[serde(default)]
    pub(super) basemap_verified: bool,
}

impl OfficialMap {
    pub(super) fn route_keys(&self) -> BTreeSet<String> {
        self.routes
            .iter()
            .map(|route| plan::bus_route_key(route))
            .collect()
    }
}

pub(super) fn read_maps(bytes: &[u8]) -> Result<Vec<OfficialMap>, &'static str> {
    const INVALID: &str = "新云线路图文件无效，请使用工作台的取证脚本生成。";
    let maps: Vec<OfficialMap> = serde_json::from_slice(bytes).map_err(|_| INVALID)?;
    if maps.is_empty() || maps.len() > 3 {
        return Err(INVALID);
    }
    let mut seen = BTreeSet::new();
    for map in &maps {
        let routes = map.route_keys();
        if map.routes.is_empty()
            || map.routes.len() > 300
            || routes.len() != map.routes.len()
            || map.routes.iter().any(|route| !valid_resource(route))
            || !valid_reference(&map.captured_at)
            || map.captured_at.len() > 32
            || map.image.len() > 800_000
            || !seen.insert(routes)
        {
            return Err(INVALID);
        }
        let image = STANDARD.decode(&map.image).map_err(|_| INVALID)?;
        if !image.starts_with(&[0xff, 0xd8, 0xff]) || !image.ends_with(&[0xff, 0xd9]) {
            return Err(INVALID);
        }
    }
    Ok(maps)
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Reach {
    pub(super) medium: Medium,
    pub(super) resource: String,
    pub(super) month: u32,
    pub(super) daily: u32,
    pub(super) reference: String,
    pub(super) source: Source,
    #[serde(default)]
    pub(super) baseline: bool,
    #[serde(default = "verified_unit")]
    pub(super) unit_verified: bool,
    #[serde(default)]
    pub(super) platform_verified: bool,
}

fn verified_unit() -> bool {
    true
}

impl Reach {
    pub(super) fn formatted_daily(&self) -> String {
        format_count(&u64::from(self.daily))
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Shape {
    pub(super) medium: Medium,
    pub(super) resource: String,
    reference: String,
    source: Source,
    lines: Vec<Vec<Coordinate>>,
    #[serde(default)]
    pub(super) platform_verified: bool,
}

impl Shape {
    pub(super) fn metro_exit(
        resource: String,
        point: Coordinate,
        reference: String,
        source: Source,
    ) -> Self {
        Self {
            medium: METRO,
            resource,
            reference,
            source,
            lines: vec![vec![point]],
            platform_verified: false,
        }
    }

    fn repeats_station(&self, coverage: impl IntoIterator<Item = Coordinate>) -> bool {
        self.medium == METRO
            && coverage.into_iter().any(|station| {
                self.lines
                    .iter()
                    .flatten()
                    .any(|point| point.same_location(station))
            })
    }

    pub(super) fn repeats_stock_station(&self, item: &Inventory, coverage: &[plan::Cover]) -> bool {
        self.repeats_station(coverage.iter().filter_map(|area| {
            plan::has_coverage(item, std::slice::from_ref(area))
                .then_some(area.media_point)
                .flatten()
        }))
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum GeoJson {
    FeatureCollection { features: Vec<Feature> },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Feature {
    Feature {
        properties: Properties,
        geometry: Geometry,
    },
}

#[derive(Deserialize)]
struct Properties {
    media: Medium,
    resource: String,
    source_ref: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "coordinates")]
enum Geometry {
    Point([f64; 2]),
    LineString(Vec<[f64; 2]>),
    MultiLineString(Vec<Vec<[f64; 2]>>),
}

pub(super) fn valid_reference(value: &str) -> bool {
    (1..=180).contains(&value.trim().chars().count()) && !value.chars().any(char::is_control)
}

pub(super) fn valid_resource(value: &str) -> bool {
    !value.trim().is_empty() && value.chars().count() <= 128 && !value.chars().any(char::is_control)
}

pub(super) fn read_shapes(name: String, bytes: &[u8]) -> Result<Vec<Shape>, &'static str> {
    const FORMAT: &str = "线路图须为含媒体类型、资源标识与新云原始依据的 GeoJSON 要素集。";
    let GeoJson::FeatureCollection { features } =
        serde_json::from_slice(bytes).map_err(|_| FORMAT)?;
    if features.is_empty() || features.len() > 800 {
        return Err("线路图要素数量须为 1–800 项。");
    }
    let mut remaining_points = 18_000usize;
    let mut shapes = Vec::with_capacity(features.len());
    for (
        index,
        Feature::Feature {
            properties,
            geometry,
        },
    ) in features.into_iter().enumerate()
    {
        if !valid_reference(&properties.source_ref) || !valid_resource(&properties.resource) {
            return Err(FORMAT);
        }
        let lines = match (properties.media, geometry) {
            (BUS, Geometry::LineString(line)) => vec![line],
            (BUS, Geometry::MultiLineString(lines)) => lines,
            (METRO, Geometry::Point(point)) => vec![vec![point]],
            _ => return Err("公交须提供原始线路折线，地铁须提供具体出入口点。"),
        };
        if lines.is_empty() {
            return Err("线路图缺少几何坐标。");
        }
        let mut plotted = Vec::with_capacity(lines.len());
        for line in lines {
            if properties.media == BUS
                && (line.len() < 2 || line.windows(2).all(|pair| pair[0] == pair[1]))
            {
                return Err("公交轨迹需要至少两个不同的原始坐标。");
            }
            remaining_points = remaining_points
                .checked_sub(line.len())
                .ok_or("坐标总数超出 18000 个。")?;
            plotted.push(
                line.into_iter()
                    .map(|[longitude, latitude]| {
                        Coordinate::shanghai(longitude, latitude)
                            .ok_or("坐标超出上海地区，请检查坐标顺序和坐标系。")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        shapes.push(Shape {
            medium: properties.media,
            resource: properties.resource.trim().to_owned(),
            reference: properties.source_ref.trim().to_owned(),
            source: Source {
                file: name.clone(),
                row: index + 1,
            },
            lines: plotted,
            platform_verified: false,
        });
    }
    Ok(shapes)
}

pub(super) fn resource_key(medium: Medium, resource: &str) -> String {
    match medium {
        BUS => plan::bus_route_key(resource),
        METRO => plan::key(resource),
    }
}

fn matches(stock: &Inventory, medium: Medium, resource: &str) -> bool {
    stock.medium == medium && {
        let expected = resource_key(medium, resource);
        resource_key(medium, &stock.name) == expected
            || (!stock.code.is_empty() && plan::key(&stock.code) == expected)
    }
}

impl Evidence {
    pub(super) fn check_exit_points(
        &self,
        stock: &[Inventory],
        coverage: &[plan::Cover],
    ) -> Result<(), &'static str> {
        for shape in self.shapes.iter().filter(|shape| shape.medium == METRO) {
            if stock.iter().any(|item| {
                matches(item, METRO, &shape.resource) && shape.repeats_stock_station(item, coverage)
            }) {
                return Err("地铁出口坐标与新云站级落点重合，请提交具体出口的原始坐标和来源。");
            }
        }
        Ok(())
    }

    pub(super) fn supplement(
        &mut self,
        additions: Self,
        stock: &[Inventory],
    ) -> Result<(), &'static str> {
        additions.validate(stock)?;
        let assets = |medium, resource: &str| {
            stock
                .iter()
                .enumerate()
                .filter_map(|(index, item)| matches(item, medium, resource).then_some(index))
                .collect::<Vec<_>>()
        };
        merge_evidence(
            &mut self.reach,
            additions.reach,
            |row| {
                (
                    assets(row.medium, &row.resource),
                    row.month,
                    row.unit_verified,
                )
            },
            |row| row.platform_verified,
        );
        merge_evidence(
            &mut self.shapes,
            additions.shapes,
            |shape| assets(shape.medium, &shape.resource),
            |shape| shape.platform_verified,
        );
        merge_evidence(
            &mut self.maps,
            additions.maps,
            OfficialMap::route_keys,
            |map| map.basemap_verified,
        );
        self.validate(stock)
    }

    pub(super) fn validate(&self, stock: &[Inventory]) -> Result<(), &'static str> {
        let mut reached = BTreeSet::new();
        for row in &self.reach {
            let assets = stock
                .iter()
                .filter(|item| matches(item, row.medium, &row.resource));
            let mut found = false;
            for item in assets {
                found = true;
                if !reached.insert((
                    item.medium,
                    &item.name,
                    &item.code,
                    &item.format,
                    row.month,
                    row.unit_verified,
                )) {
                    return Err("同月同一资源存在重复触达记录。");
                }
            }
            if !found {
                return Err("触达记录中的线路或点位编号未匹配本批红豆库存。");
            }
        }
        let mut shaped = BTreeSet::new();
        for shape in &self.shapes {
            let mut found = false;
            for item in stock
                .iter()
                .filter(|item| matches(item, shape.medium, &shape.resource))
            {
                found = true;
                if !shaped.insert((item.medium, &item.name, &item.code, &item.format)) {
                    return Err("同一资源存在多份线路图要素。公交上下行请合为 MultiLineString。");
                }
            }
            if !found {
                return Err("线路图要素中的线路或点位编号未匹配本批红豆库存。");
            }
        }
        for map in &self.maps {
            if map
                .routes
                .iter()
                .any(|name| !stock.iter().any(|item| matches(item, BUS, name)))
            {
                return Err("线路图中的公交线路未匹配本批红豆库存。");
            }
        }
        Ok(())
    }
}

fn merge_evidence<T, K: Ord>(
    existing: &mut Vec<T>,
    additions: Vec<T>,
    key: impl Fn(&T) -> K,
    trusted: impl Fn(&T) -> bool,
) {
    if additions.is_empty() {
        return;
    }
    let mut additions: BTreeMap<_, _> = additions
        .into_iter()
        .map(|item| (key(&item), item))
        .collect();
    for item in existing.iter_mut() {
        if let Some(updated) = additions.remove(&key(item))
            && (!trusted(item) || trusted(&updated))
        {
            *item = updated;
        }
    }
    existing.extend(additions.into_values());
}

#[cfg(test)]
mod tests {
    use super::merge_evidence;

    #[test]
    fn platform_evidence_survives_manual_replacement_and_accepts_new_platform_data() {
        let mut rows = vec![("已有平台记录", 100, true), ("已有本地记录", 200, false)];
        merge_evidence(
            &mut rows,
            vec![
                ("已有平台记录", 300, false),
                ("已有本地记录", 400, true),
                ("新增本地记录", 500, false),
            ],
            |row| row.0,
            |row| row.2,
        );
        assert_eq!(
            rows,
            vec![
                ("已有平台记录", 100, true),
                ("已有本地记录", 400, true),
                ("新增本地记录", 500, false)
            ]
        );
        merge_evidence(
            &mut rows,
            vec![("已有平台记录", 600, true)],
            |row| row.0,
            |row| row.2,
        );
        assert_eq!(rows[0], ("已有平台记录", 600, true));
    }
}

#[derive(Clone, Copy)]
pub(super) enum Strategy {
    CommerceCoverage,
    StoreParticipation,
}

impl Strategy {
    pub(super) const ALL: [Self; 2] = [Self::CommerceCoverage, Self::StoreParticipation];

    pub(super) fn index(self) -> usize {
        match self {
            Self::CommerceCoverage => 0,
            Self::StoreParticipation => 1,
        }
    }

    pub(super) fn priority(self) -> Priority {
        match self {
            Self::CommerceCoverage => Priority::KeyCircles,
            Self::StoreParticipation => Priority::CoreFrequency,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::CommerceCoverage => "A",
            Self::StoreParticipation => "B",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::CommerceCoverage => "多商圈覆盖",
            Self::StoreParticipation => "核心门店触达",
        }
    }

    fn rationale(self) -> &'static str {
        match self {
            Self::CommerceCoverage => {
                "以预算目标为基准，优先覆盖多个目标商圈并选择更多不同的公交线路。"
            }
            Self::StoreParticipation => {
                "在预算目标附近优先配置核心商圈 500 米内的公交和地铁资源。每条近场公交线路的前三辆提高比选得分。"
            }
        }
    }
}

pub(super) struct Variant {
    pub(super) code: &'static str,
    pub(super) name: &'static str,
    pub(super) advantage: &'static str,
    pub(super) limit: u32,
    pub(super) hard_cap: Option<u64>,
    pub(super) available: bool,
    pub(super) total: Cost,
    pub(super) circles: usize,
    pub(super) target_difference: u64,
    pub(super) above_target: bool,
    pub(super) bus_lines: usize,
    pub(super) buses: u32,
    pub(super) exits: u32,
    pub(super) map: Map,
    pub(super) route_overview: Option<OfficialMap>,
    pub(super) periods: Vec<Period>,
    pub(super) metrics: Vec<Metric>,
    pub(super) same_as: Option<&'static str>,
}

pub(super) struct BudgetBasis {
    pub(super) minimum: u64,
    pub(super) target_gap: u64,
    pub(super) cap_gap: u64,
}

impl Variant {
    pub(super) fn route_names(&self) -> Vec<String> {
        bus_routes(&self.periods).into_values().collect()
    }

    pub(super) fn set_route_overview(&mut self, maps: &[OfficialMap]) {
        let routes = bus_routes(&self.periods)
            .into_keys()
            .collect::<BTreeSet<_>>();
        if let Some(map) = maps
            .iter()
            .filter(|map| map.route_keys() == routes)
            .max_by_key(|map| map.basemap_verified)
            .filter(|map| map.basemap_verified || !self.verified_route_overview())
        {
            self.route_overview = Some(map.clone());
        }
    }

    pub(super) fn verified_route_overview(&self) -> bool {
        self.route_overview
            .as_ref()
            .is_some_and(|map| map.basemap_verified)
    }

    pub(super) fn overview_pending(&self) -> bool {
        self.bus_lines > 0
            && !self.verified_route_overview()
            && self
                .periods
                .iter()
                .any(|period| period.official_map_required)
    }

    pub(super) fn has_phase_maps(&self) -> bool {
        self.periods
            .iter()
            .any(|period| period.official_map.is_some())
    }

    pub(super) fn bus_map_ready(&self) -> bool {
        self.verified_route_overview()
            || (!self.overview_pending() && self.periods.iter().all(Period::bus_map_ready))
    }

    pub(super) fn budget_gap_needs_confirmation(&self) -> bool {
        self.available && super::budget_gap_needs_confirmation(self.limit, self.total.high)
    }

    pub(super) fn above_budget_band(&self) -> bool {
        self.total.high > super::budget_band(self.limit).1
    }

    pub(super) fn new(
        strategy: Strategy,
        limit: u32,
        hard_cap: Option<u64>,
        periods: Vec<Period>,
    ) -> Self {
        let map = Map::new(&periods);
        let routes = bus_routes(&periods).into_keys().collect::<BTreeSet<_>>();
        let route_overview = periods
            .iter()
            .filter_map(|period| period.official_map.as_ref())
            .filter(|map| map.route_keys() == routes)
            .max_by_key(|map| map.basemap_verified)
            .cloned();
        let available = periods.iter().all(|period| period.available);
        let total = periods
            .iter()
            .fold(Cost { low: 0, high: 0 }, |sum, period| Cost {
                low: sum.low + period.total.low,
                high: sum.high + period.total.high,
            });
        let circles = periods
            .iter()
            .flat_map(|period| &period.circles)
            .collect::<BTreeSet<_>>()
            .len();
        let metrics = metrics(&periods);
        let quantity = |media| {
            periods
                .iter()
                .flat_map(|period| &period.rows)
                .filter(|row| row.media == media)
                .map(|row| row.quantity)
                .sum()
        };
        Self {
            code: strategy.code(),
            name: strategy.name(),
            advantage: strategy.rationale(),
            limit,
            hard_cap,
            available,
            total,
            circles,
            target_difference: (u64::from(limit) * 10_000).abs_diff(total.high),
            above_target: total.high > u64::from(limit) * 10_000,
            buses: quantity(BUS),
            exits: quantity(METRO),
            bus_lines: routes.len(),
            periods,
            map,
            route_overview,
            metrics,
            same_as: None,
        }
    }

    pub(super) fn budget_basis(&self) -> Option<BudgetBasis> {
        if self.periods.is_empty() {
            return None;
        }
        let minimum = self
            .periods
            .iter()
            .map(|period| period.quote_floor.as_ref()?.verified_total())
            .sum::<Option<u64>>()?;
        Some(BudgetBasis {
            minimum,
            target_gap: minimum.saturating_sub(super::budget_band(self.limit).1),
            cap_gap: self.hard_cap.map_or(0, |cap| minimum.saturating_sub(cap)),
        })
    }

    pub(super) fn media_cost(&self, medium: Medium) -> Cost {
        self.periods
            .iter()
            .flat_map(|period| &period.rows)
            .filter(|row| row.media == medium)
            .fold(Cost { low: 0, high: 0 }, |sum, row| Cost {
                low: sum.low + row.total.low,
                high: sum.high + row.total.high,
            })
    }

    pub(super) fn high_quote_share(&self, amount: &u64) -> String {
        format!(
            "{:.1}%",
            *amount as f64 * 100.0 / self.total.high.max(1) as f64
        )
    }

    pub(super) fn production_total(&self) -> Cost {
        self.periods.iter().flat_map(|period| &period.rows).fold(
            Cost { low: 0, high: 0 },
            |sum, row| Cost {
                low: sum.low + row.production.low * u64::from(row.quantity),
                high: sum.high + row.production.high * u64::from(row.quantity),
            },
        )
    }

    pub(super) fn publication_total(&self) -> Cost {
        let production = self.production_total();
        Cost {
            low: self.total.low - production.low,
            high: self.total.high - production.high,
        }
    }

    pub(super) fn mark_duplicates(variants: &mut [Self]) {
        let mut configurations = BTreeMap::new();
        for variant in variants {
            if variant.available {
                let key = variant
                    .periods
                    .iter()
                    .map(|period| {
                        period
                            .rows
                            .iter()
                            .map(|row| (row.media, row.resource.clone(), row.quantity))
                            .collect::<BTreeSet<_>>()
                    })
                    .collect::<Vec<_>>();
                variant.same_as = match configurations.entry(key) {
                    std::collections::btree_map::Entry::Occupied(entry) => Some(*entry.get()),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(variant.name);
                        None
                    }
                };
            }
        }
    }

    pub(super) fn has_exposure_metrics(&self) -> bool {
        self.metrics.iter().any(|metric| metric.known)
    }

    pub(super) fn has_full_evidence(&self) -> bool {
        self.available
            && self.bus_map_ready()
            && self.periods.iter().all(|period| {
                !period.rows.is_empty()
                    && period.metro_map_ready()
                    && period.rows.iter().all(Line::reach_complete)
            })
    }

    pub(super) fn core_bus_days(&self) -> Option<u64> {
        if !self.available {
            return None;
        }
        let mut days = 0;
        for period in &self.periods {
            for row in period.rows.iter().filter(|row| row.media == BUS) {
                let core = period.primary_circle.as_deref()?;
                let mut distances = row
                    .coverage
                    .iter()
                    .filter(|edge| edge.circle == core)
                    .map(|edge| edge.distance);
                let first = distances.next().flatten()?;
                let complete = distances.try_fold(first, |nearest, distance| {
                    distance.map(|distance| nearest.min(distance))
                });
                if complete? <= 500 {
                    days += u64::from(row.quantity) * u64::from(period.days);
                }
            }
        }
        Some(days)
    }

    pub(super) fn operations_ready(&self) -> bool {
        !self.periods.is_empty()
            && self.periods.iter().all(|period| {
                period
                    .operations
                    .as_ref()
                    .is_some_and(super::operations::Phase::ready)
            })
    }
}

fn bus_routes(periods: &[Period]) -> BTreeMap<String, String> {
    periods
        .iter()
        .flat_map(|period| &period.rows)
        .filter(|row| row.media == BUS)
        .map(|row| (plan::bus_route_key(&row.stock.name), row.stock.name.clone()))
        .collect()
}

pub(super) struct Period {
    pub(super) starts: String,
    pub(super) ends: String,
    pub(super) days: u32,
    pub(super) reason: String,
    pub(super) observed: Option<String>,
    pub(super) platform_inventory: bool,
    pub(super) empty_inventory: BTreeMap<Medium, Source>,
    pub(super) limit: u64,
    pub(super) available: bool,
    pub(super) required_complete: bool,
    pub(super) primary_circle: Option<String>,
    pub(super) circles: BTreeSet<String>,
    pub(super) total: Cost,
    pub(super) quote_floor: Option<QuoteFloor>,
    pub(super) operations: Option<super::operations::Phase>,
    pub(super) operations_required: bool,
    pub(super) rows: Vec<Line>,
    pub(super) official_map: Option<OfficialMap>,
    pub(super) screenshot_pending: bool,
    official_map_required: bool,
    pub(super) missing_routes: Vec<RouteCheck>,
    pub(super) alternatives: Vec<Alternative>,
}

pub(super) struct Alternative {
    pub(super) stock: Inventory,
    pub(super) quote: Cost,
    pub(super) circles: Vec<String>,
    pub(super) rate_source: Source,
}

impl Alternative {
    fn for_period(snapshot: &Snapshot, proposal: &Proposal) -> Vec<Self> {
        let required: BTreeSet<_> = proposal
            .required_routes
            .iter()
            .map(|route| plan::bus_route_key(&route.name))
            .chain(
                proposal
                    .items
                    .iter()
                    .filter(|item| item.stock.medium == BUS)
                    .map(|item| plan::bus_route_key(&item.stock.name)),
            )
            .collect();
        let mut candidates: Vec<_> = proposal
            .excluded
            .iter()
            .filter_map(|entry| {
                let stock = entry.stock.as_ref()?;
                let quote = entry.alternative_quote?;
                let rate = entry.rate.as_ref()?;
                if stock.medium != BUS
                    || stock.free == 0
                    || entry.coverage.is_empty()
                    || required.contains(&plan::bus_route_key(&stock.name))
                {
                    return None;
                }
                let primary_distance = entry
                    .coverage
                    .iter()
                    .filter(|cover| Some(cover.circle.as_str()) == snapshot.data.core_circle())
                    .filter_map(|cover| cover.distance)
                    .min()
                    .unwrap_or(u32::MAX);
                let circles = entry
                    .coverage
                    .iter()
                    .map(|cover| cover.circle.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                Some((
                    primary_distance,
                    Self {
                        stock: stock.clone(),
                        quote,
                        circles,
                        rate_source: rate.source.clone(),
                    },
                ))
            })
            .collect();
        candidates.sort_by_key(|(distance, alternative)| {
            (
                *distance,
                std::cmp::Reverse(alternative.circles.len()),
                alternative.quote.high,
            )
        });
        let mut seen = BTreeSet::new();
        candidates
            .into_iter()
            .map(|(_, candidate)| candidate)
            .filter(|candidate| seen.insert(plan::bus_route_key(&candidate.stock.name)))
            .take(3)
            .collect()
    }
}

impl Period {
    pub(super) fn has_exposure_metrics(&self) -> bool {
        self.rows.iter().any(|row| !row.evidence.is_empty())
    }

    pub(super) fn exposure_metrics(&self) -> Vec<Metric> {
        metrics(std::slice::from_ref(self))
    }

    pub(super) fn has_geometry(&self, row: &Line) -> bool {
        row.shape.is_some() || (row.media == BUS && self.official_map.is_some())
    }

    pub(super) fn has_platform_evidence(&self) -> bool {
        self.rows.iter().all(|row| {
            row.missing_platform_reach_months().next().is_none()
                && row
                    .historical_reach
                    .iter()
                    .all(|entry| entry.platform_verified)
                && (row.media != METRO || row.has_platform_exit())
        })
    }

    pub(super) fn bus_map_ready(&self) -> bool {
        !self.screenshot_pending
            && self
                .rows
                .iter()
                .filter(|row| row.media == BUS)
                .all(|row| self.has_geometry(row))
    }

    pub(super) fn metro_map_ready(&self) -> bool {
        self.rows
            .iter()
            .filter(|row| row.media == METRO)
            .all(|row| self.has_geometry(row))
    }

    pub(super) fn new(
        starts: &str,
        ends: &str,
        reason: String,
        limit: u64,
        verified: Option<(&Snapshot, &Proposal)>,
    ) -> Self {
        let months = month_spans(starts, ends);
        let days = months.iter().map(|(_, days)| days).sum();
        let rows: Vec<Line> = verified.map_or_else(Vec::new, |(snapshot, proposal)| {
            proposal
                .items
                .iter()
                .map(|item| Line::new(&snapshot.data.evidence, item, &months))
                .collect()
        });
        let route_keys = rows
            .iter()
            .filter(|row| row.media == BUS)
            .map(|row| plan::bus_route_key(&row.stock.name))
            .collect::<BTreeSet<_>>();
        let official_map = verified.and_then(|(snapshot, _)| {
            snapshot
                .data
                .evidence
                .maps
                .iter()
                .find(|map| route_keys == map.route_keys())
                .cloned()
        });
        let platform_inventory = verified
            .is_some_and(|(snapshot, _)| snapshot.source == super::source_label("platform"));
        let official_map_required = platform_inventory && !route_keys.is_empty();
        let screenshot_pending = official_map_required
            && !official_map
                .as_ref()
                .is_some_and(|map| map.basemap_verified);
        let missing_routes = verified
            .map(|(_, proposal)| {
                proposal
                    .required_routes
                    .iter()
                    .filter(|route| !route.selected)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let alternatives = verified
            .filter(|_| !missing_routes.is_empty())
            .map(|(snapshot, proposal)| Alternative::for_period(snapshot, proposal))
            .unwrap_or_default();
        Self {
            starts: starts.into(),
            ends: ends.into(),
            days,
            reason,
            limit,
            rows,
            official_map,
            screenshot_pending,
            official_map_required,
            missing_routes,
            alternatives,
            observed: verified
                .map(|(snapshot, _)| format!("{} · {}", snapshot.source, snapshot.observed)),
            platform_inventory,
            empty_inventory: verified
                .map(|(snapshot, _)| snapshot.data.empty_inventory.clone())
                .unwrap_or_default(),
            required_complete: verified.is_some_and(|(_, proposal)| proposal.complete_required()),
            primary_circle: verified
                .and_then(|(snapshot, _)| snapshot.data.core_circle().map(str::to_owned)),
            available: verified.is_some_and(|(_, proposal)| {
                proposal.complete_required()
                    && proposal.complete_media
                    && !proposal.items.is_empty()
            }),
            circles: verified
                .map(|(_, proposal)| proposal.covered.clone())
                .unwrap_or_default(),
            total: verified
                .map(|(_, proposal)| proposal.total)
                .unwrap_or(Cost { low: 0, high: 0 }),
            quote_floor: verified.map(|(_, proposal)| proposal.quote_floor.clone()),
            operations: None,
            operations_required: false,
        }
    }
}

fn month_spans(starts: &str, ends: &str) -> Vec<(u32, u32)> {
    let segment = |date: &str| {
        let month = date[5..7].parse::<u32>().expect("validated month");
        let day = date[8..10].parse::<u32>().expect("validated day");
        (month, day)
    };
    let (start_month, start_day) = segment(starts);
    let (end_month, end_day) = segment(ends);
    (start_month..=end_month)
        .map(|month| {
            let first = if month == start_month { start_day } else { 1 };
            let last = if month == end_month {
                end_day
            } else {
                u32::from(month_days(&format!("{month:02}")).expect("validated month"))
            };
            (month, last - first + 1)
        })
        .collect()
}

pub(super) struct Line {
    pub(super) stock: Inventory,
    pub(super) media: Medium,
    pub(super) resource: String,
    pub(super) quantity: u32,
    pub(super) fee: Cost,
    pub(super) production: Cost,
    pub(super) total: Cost,
    pub(super) rate_source: Source,
    pub(super) coverage: Vec<plan::Cover>,
    pub(super) client_required: bool,
    pub(super) evidence: Vec<Observation>,
    pub(super) historical_reach: Vec<Reach>,
    pub(super) missing_reach_months: Vec<u32>,
    pub(super) exposures: u64,
    shape: Option<Shape>,
}

pub(super) struct Observation {
    pub(super) row: Reach,
    pub(super) days: u32,
    pub(super) applies_to: u32,
}

impl Line {
    pub(super) fn missing_platform_reach_months(&self) -> impl Iterator<Item = u32> + '_ {
        self.missing_reach_months.iter().copied().chain(
            self.evidence
                .iter()
                .filter(|entry| !entry.row.platform_verified)
                .map(|entry| entry.applies_to),
        )
    }

    pub(super) fn has_platform_exit(&self) -> bool {
        self.shape
            .as_ref()
            .is_some_and(|point| point.platform_verified)
    }

    pub(super) fn reach_complete(&self) -> bool {
        self.missing_reach_months.is_empty()
    }

    fn new(evidence: &Evidence, item: &Item, months: &[(u32, u32)]) -> Self {
        let matched = |medium, resource: &str| matches(&item.stock, medium, resource);
        let coverage = item.coverage.clone();
        let shape = evidence
            .shapes
            .iter()
            .find(|row| {
                matched(row.medium, &row.resource)
                    && !row.repeats_station(coverage.iter().filter_map(|area| area.media_point))
            })
            .cloned();
        let mut historical_reach: Vec<_> = evidence
            .reach
            .iter()
            .filter(|row| {
                !row.unit_verified
                    && matched(row.medium, &row.resource)
                    && (row.month == 5 || months.iter().any(|(month, _)| *month == row.month))
            })
            .cloned()
            .collect();
        historical_reach.sort_by_key(|row| row.month);
        let mut missing_reach_months = Vec::new();
        let evidence: Vec<_> = months
            .iter()
            .filter_map(|(month, days)| {
                let observation = evidence
                    .reach
                    .iter()
                    .filter(|row| {
                        row.unit_verified
                            && matched(row.medium, &row.resource)
                            && (row.month == *month || row.baseline)
                    })
                    .min_by_key(|row| (!row.platform_verified, row.month != *month))
                    .map(|row| Observation {
                        row: row.clone(),
                        days: *days,
                        applies_to: *month,
                    });
                if observation.is_none() {
                    missing_reach_months.push(*month);
                }
                observation
            })
            .collect();
        let exposures = evidence
            .iter()
            .map(|entry| {
                u64::from(entry.row.daily) * u64::from(entry.days) * u64::from(item.quantity)
            })
            .sum();
        Self {
            stock: item.stock.clone(),
            media: item.stock.medium,
            resource: format!(
                "{} · {} · {}",
                item.stock.name, item.stock.code, item.stock.format
            ),
            quantity: item.quantity,
            fee: item.rate.fee,
            production: item.production,
            total: item.total,
            rate_source: item.rate.source.clone(),
            coverage,
            client_required: item.client_required,
            historical_reach,
            missing_reach_months,
            evidence,
            exposures,
            shape,
        }
    }
}

pub(super) struct Metric {
    pub(super) name: &'static str,
    pub(super) status: &'static str,
    pub(super) known: bool,
    pub(super) total: String,
    pub(super) daily: String,
    pub(super) missing: usize,
    pub(super) baseline_months: usize,
}

fn metrics(periods: &[Period]) -> Vec<Metric> {
    let days: u64 = periods.iter().map(|period| u64::from(period.days)).sum();
    let incomplete_schedule = periods.iter().any(|period| !period.available);
    [
        ("公交车身", Some(BUS)),
        ("地铁出入口", Some(METRO)),
        ("组合曝光", None),
    ]
    .into_iter()
    .map(|(name, media)| {
        let rows = periods
            .iter()
            .flat_map(|period| &period.rows)
            .filter(|row| media.is_none_or(|media| media == row.media))
            .collect::<Vec<_>>();
        let known = rows.iter().any(|row| !row.evidence.is_empty());
        let missing = rows.iter().filter(|row| !row.reach_complete()).count();
        let baseline_months = rows
            .iter()
            .flat_map(|row| &row.evidence)
            .filter(|observation| observation.row.month != observation.applies_to)
            .count();
        let sum: u64 = rows.iter().map(|row| row.exposures).sum();
        let complete = known && !incomplete_schedule && missing == 0;
        let status = if complete {
            "完整估算"
        } else {
            "部分可核验人次"
        };
        Metric {
            name,
            status,
            known,
            total: format_count(&sum),
            daily: if complete {
                format_count(&((sum + days / 2) / days))
            } else {
                String::new()
            },
            missing,
            baseline_months,
        }
    })
    .collect()
}

pub(super) fn format_count(count: &u64) -> String {
    let entered = count.to_string();
    entered
        .chars()
        .enumerate()
        .flat_map(|(index, digit)| {
            (index > 0 && (entered.len() - index).is_multiple_of(3))
                .then_some(',')
                .into_iter()
                .chain(std::iter::once(digit))
        })
        .collect()
}

pub(super) struct Map {
    pub(super) paths: Vec<Path>,
    pub(super) points: Vec<Point>,
    pub(super) buses: usize,
    pub(super) exits: usize,
    pub(super) missing: usize,
    pub(super) coverage_only: usize,
    pub(super) has_data: bool,
}

struct FeatureDraw {
    label: String,
    class: &'static str,
    source: String,
    lines: Vec<Vec<Coordinate>>,
}

pub(super) struct Path {
    pub(super) coords: String,
    pub(super) label: String,
    pub(super) source: String,
}

pub(super) struct Point {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) class: &'static str,
    pub(super) label: String,
    pub(super) source: String,
}

impl Map {
    fn new(periods: &[Period]) -> Self {
        let mut additions = BTreeSet::new();
        let mut features = Vec::new();
        let (mut buses, mut exits, mut missing, mut coverage_only) = (0, 0, 0, 0);
        for period in periods {
            for row in &period.rows {
                let mut point_available = false;
                if let Some(shape) = &row.shape {
                    match row.media {
                        BUS => buses += 1,
                        METRO => exits += 1,
                    }
                    let class = if shape.medium == BUS {
                        "route-map-path"
                    } else {
                        "route-map-exit"
                    };
                    let source = format!(
                        "{} · {} 要素 {} · 操作员核对导入",
                        shape.reference, shape.source.file, shape.source.row
                    );
                    if additions.insert((row.resource.clone(), source.clone())) {
                        features.push(FeatureDraw {
                            label: row.stock.name.clone(),
                            class,
                            source,
                            lines: shape.lines.clone(),
                        });
                    }
                }
                for cover in &row.coverage {
                    let source = format!("{} · 第 {} 行", cover.source.file, cover.source.row);
                    if let Some(point) = cover.center_point
                        && additions.insert(("目标圈".into(), cover.circle.clone()))
                    {
                        features.push(FeatureDraw {
                            label: cover.circle.clone(),
                            class: "route-map-center",
                            source: source.clone(),
                            lines: vec![vec![point]],
                        });
                    }
                    if row.shape.is_none()
                        && let Some(point) = cover.media_point
                    {
                        point_available = true;
                        if additions.insert((row.resource.clone(), source.clone())) {
                            let label = if row.media == BUS {
                                "最近轨迹点"
                            } else {
                                "地铁站级落点"
                            };
                            features.push(FeatureDraw {
                                label: format!("{} · {}", row.stock.name, label),
                                class: "route-map-evidence",
                                source,
                                lines: vec![vec![point]],
                            });
                        }
                    }
                }
                if row.shape.is_none() {
                    if point_available {
                        coverage_only += 1;
                    } else {
                        missing += 1;
                    }
                }
            }
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for Coordinate(x, y) in features
            .iter()
            .flat_map(|feature| feature.lines.iter().flatten().copied())
        {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let has_data = !features.is_empty();
        let longitude_ratio = ((min_y + max_y) / 2.0).to_radians().cos();
        let scale = (680.0 / ((max_x - min_x) * longitude_ratio).max(0.002))
            .min(290.0 / (max_y - min_y).max(0.002));
        let project = |Coordinate(x, y)| {
            let screen_x = 380.0 + (x - (min_x + max_x) / 2.0) * longitude_ratio * scale;
            let screen_y = 180.0 - (y - (min_y + max_y) / 2.0) * scale;
            (screen_x.round(), screen_y.round())
        };
        let mut paths = Vec::new();
        let mut points = Vec::new();
        for feature in features {
            for line in feature.lines {
                if let [coordinate] = line.as_slice() {
                    let (x, y) = project(*coordinate);
                    points.push(Point {
                        x,
                        y,
                        class: feature.class,
                        label: feature.label.clone(),
                        source: feature.source.clone(),
                    });
                } else {
                    let coords = line
                        .into_iter()
                        .map(|coordinate| {
                            let (x, y) = project(coordinate);
                            format!("{x},{y}")
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    paths.push(Path {
                        coords,
                        label: feature.label.clone(),
                        source: feature.source.clone(),
                    });
                }
            }
        }
        Self {
            paths,
            points,
            buses,
            exits,
            missing,
            coverage_only,
            has_data,
        }
    }
}
