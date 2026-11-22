use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
pub(super) enum Medium {
    Bus,
    Metro,
}

impl Medium {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Bus => "公交车身",
            Self::Metro => "地铁出入口",
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Selection {
    Bus,
    Metro,
    Both,
}

impl Selection {
    pub(super) fn mask(self) -> u8 {
        match self {
            Self::Bus => 1,
            Self::Metro => 2,
            Self::Both => 3,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Bus => "公交车身",
            Self::Metro => "地铁出入口",
            Self::Both => "公交车身、地铁出入口",
        }
    }

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Bus => "bus",
            Self::Metro => "metro",
            Self::Both => "both",
        }
    }
    pub(super) fn accepts(self, medium: Medium) -> bool {
        matches!(
            (self, medium),
            (Self::Both, _) | (Self::Bus, Medium::Bus) | (Self::Metro, Medium::Metro)
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Source {
    pub(super) file: String,
    pub(super) row: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Inventory {
    pub(super) medium: Medium,
    pub(super) name: String,
    #[serde(default)]
    pub(super) station: Option<String>,
    #[serde(default)]
    pub(super) station_attested: bool,
    #[serde(default)]
    pub(super) code: String,
    #[serde(default)]
    pub(super) metro_spec: Option<MetroEvidence>,
    pub(super) operator: String,
    pub(super) grade: String,
    pub(super) format: String,
    pub(super) free: u32,
    pub(super) area: Option<u32>,
    pub(super) source: Source,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct MetroEvidence {
    pub(super) source: Source,
    pub(super) remark: String,
    pub(super) needs_confirmation: bool,
    pub(super) line: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Cover {
    pub(super) medium: Medium,
    pub(super) name: String,
    #[serde(default)]
    pub(super) station: Option<String>,
    #[serde(default)]
    pub(super) metro_line: Option<u32>,
    pub(super) operator: String,
    pub(super) format: String,
    pub(super) circle: String,
    pub(super) radius: u32,
    #[serde(default)]
    pub(super) distance: Option<u32>,
    #[serde(default)]
    pub(super) center_point: Option<super::visuals::Coordinate>,
    #[serde(default)]
    pub(super) media_point: Option<super::visuals::Coordinate>,
    pub(super) source: Source,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Cost {
    pub(super) low: u64,
    pub(super) high: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) enum Production {
    Fixed(Cost),
    ByArea(u64),
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Rate {
    pub(super) medium: Medium,
    pub(super) target: String,
    pub(super) fee: Cost,
    pub(super) production: Production,
    pub(super) unit: u32,
    pub(super) monthly: bool,
    pub(super) source: Source,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Dataset {
    #[serde(skip)]
    pub(super) confirmed_core: Option<String>,
    pub(super) stock: Vec<Inventory>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) empty_inventory: BTreeMap<Medium, Source>,
    pub(super) coverage: Vec<Cover>,
    pub(super) rates: Vec<Rate>,
    pub(super) demo: bool,
    #[serde(default)]
    pub(super) supplemental_metro_center: Option<String>,
    #[serde(default)]
    pub(super) evidence: super::visuals::Evidence,
}

impl Dataset {
    pub(super) fn core_circle(&self) -> Option<&str> {
        self.confirmed_core
            .as_deref()
            .or_else(|| self.coverage.first().map(|cover| cover.circle.as_str()))
    }
}

#[derive(Clone)]
pub(super) struct Item {
    pub(super) stock: Inventory,
    pub(super) coverage: Vec<Cover>,
    pub(super) rate: Rate,
    pub(super) production: Cost,
    pub(super) quantity: u32,
    pub(super) total: Cost,
    pub(super) client_required: bool,
}

#[derive(Clone)]
pub(super) struct Exclusion {
    pub(super) name: String,
    pub(super) why: &'static str,
    pub(super) stock: Option<Inventory>,
    pub(super) coverage: Vec<Cover>,
    pub(super) rate: Option<Rate>,
    pub(super) alternative_quote: Option<Cost>,
}

impl Exclusion {
    pub(super) fn unselected(item: Item, why: &'static str) -> Self {
        Self {
            name: item.stock.name.clone(),
            why,
            stock: Some(item.stock),
            coverage: item.coverage,
            rate: Some(item.rate),
            alternative_quote: Some(item.total),
        }
    }
}

pub(super) struct Proposal {
    pub(super) items: Vec<Item>,
    pub(super) excluded: Vec<Exclusion>,
    pub(super) covered: BTreeSet<String>,
    pub(super) total: Cost,
    pub(super) complete_media: bool,
    pub(super) required_routes: Vec<RouteCheck>,
    pub(super) quote_floor: QuoteFloor,
}

#[derive(Clone)]
pub(super) struct FloorResource {
    pub(super) name: String,
    pub(super) medium: Medium,
    pub(super) format: String,
    pub(super) high: u64,
}

#[derive(Clone)]
pub(super) struct QuoteFloor {
    pub(super) resources: Vec<FloorResource>,
    pub(super) complete: bool,
    pub(super) high: u64,
}

impl QuoteFloor {
    pub(super) fn verified_total(&self) -> Option<u64> {
        self.complete.then_some(self.high)
    }
}

pub(super) fn quote_floor(
    items: &[Item],
    selection: Selection,
    required: &BTreeSet<String>,
) -> QuoteFloor {
    let by_price = |item: &&Item| (item.total.high, item.total.low);
    let cheapest = |medium| {
        items
            .iter()
            .filter(|item| item.stock.medium == medium && !item.coverage.is_empty())
            .min_by_key(by_price)
    };
    let mut chosen: Vec<_> = required
        .iter()
        .filter_map(|route| {
            items
                .iter()
                .filter(|item| {
                    item.stock.medium == Medium::Bus && bus_route_key(&item.stock.name) == *route
                })
                .min_by_key(by_price)
        })
        .collect();
    let required_complete = chosen.len() == required.len();
    let bus = cheapest(Medium::Bus);
    let metro = cheapest(Medium::Metro);
    let has_bus = !chosen.is_empty() || bus.is_some();
    if chosen.is_empty() && selection.accepts(Medium::Bus) {
        chosen.extend(bus);
    }
    if selection.accepts(Medium::Metro) {
        chosen.extend(metro);
    }
    QuoteFloor {
        high: chosen.iter().map(|item| item.total.high).sum(),
        complete: required_complete
            && (!selection.accepts(Medium::Bus) || has_bus)
            && (!selection.accepts(Medium::Metro) || metro.is_some()),
        resources: chosen
            .into_iter()
            .map(|item| FloorResource {
                name: item.stock.name.clone(),
                medium: item.stock.medium,
                format: item.stock.format.clone(),
                high: item.total.high,
            })
            .collect(),
    }
}

#[derive(Clone)]
pub(super) struct RouteCheck {
    pub(super) name: String,
    pub(super) selected: bool,
    pub(super) reason: &'static str,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Priority {
    #[default]
    Balanced,
    KeyCircles,
    Nearby,
    CoreFrequency,
}

impl Priority {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Balanced => "优先贴近预算目标",
            Self::KeyCircles => "优先覆盖更多目标圈",
            Self::Nearby => "优先选择距目标更近的资源",
            Self::CoreFrequency => "优先增加门店近场线路的投放车辆",
        }
    }

    fn proximity(self, item: &Item, primary: Option<&str>) -> u32 {
        match self {
            Self::Nearby | Self::CoreFrequency => item
                .coverage
                .iter()
                .filter(|cover| primary.is_none_or(|primary| cover.circle == primary))
                .filter_map(|cover| cover.distance)
                .min()
                .unwrap_or(u32::MAX),
            _ => 0,
        }
    }
}

impl Proposal {
    pub(super) fn complete_required(&self) -> bool {
        self.required_routes.iter().all(|route| route.selected)
    }

    pub(super) fn quantity(&self, medium: Medium) -> u32 {
        self.items
            .iter()
            .filter(|item| item.stock.medium == medium)
            .map(|item| item.quantity)
            .sum()
    }

    pub(super) fn cost(&self, medium: Medium) -> Cost {
        self.items
            .iter()
            .filter(|item| item.stock.medium == medium)
            .fold(Cost { low: 0, high: 0 }, |total, item| Cost {
                low: total.low + item.total.low,
                high: total.high + item.total.high,
            })
    }
}

pub(super) fn key(name: &str) -> String {
    name.nfkc()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .replace(['（', '）'], "")
        .replace(['(', ')'], "")
}

fn same(a: &str, b: &str) -> bool {
    key(a) == key(b)
}

fn media_key(name: &str, medium: Medium) -> String {
    let cleaned = key(name);
    match medium {
        Medium::Bus => {
            if let Some(route) = cleaned.strip_suffix('路')
                && route.ends_with(|ch: char| ch.is_ascii_digit())
            {
                return route.into();
            }
            cleaned
        }
        Medium::Metro => cleaned
            .replace("地铁站", "")
            .replace("地铁", "")
            .replace("站", "")
            .replace("号出入口", "号口")
            .replace("号出口", "号口"),
    }
}

pub(super) fn bus_route_key(name: &str) -> String {
    media_key(name, Medium::Bus)
}

pub(super) fn parse_required_routes(text: &str) -> Result<Vec<String>, &'static str> {
    let mut seen = BTreeSet::new();
    let mut routes = Vec::new();
    for raw in text.split([',', '，', '、', ';', '；', '\n']) {
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        if name.chars().count() > 26
            || !name.chars().any(char::is_alphanumeric)
            || (!name.chars().any(|ch| ch.is_numeric()) && !name.ends_with(['线', '路']))
            || !name.chars().all(|ch| {
                ch.is_alphanumeric() || matches!(ch, '-' | '－' | '(' | ')' | '（' | '）')
            })
        {
            return Err("请填写明确的公交线路名称，并使用逗号分隔。");
        }
        if seen.insert(bus_route_key(name)) {
            routes.push(name.to_owned());
        }
    }
    if routes.len() > 12 {
        return Err("每项任务最多指定 12 条公交线路。");
    }
    Ok(routes)
}

fn compatible(area: &Cover, stock: &Inventory) -> bool {
    area.medium == stock.medium
        && media_key(&area.name, area.medium) == media_key(&stock.name, stock.medium)
        && (area.operator.is_empty() || same(&area.operator, &stock.operator))
        && (area.format.is_empty() || same(&area.format, &stock.format))
}

fn station_match(area: &Cover, stock: &Inventory) -> bool {
    area.medium == Medium::Metro
        && stock.medium == Medium::Metro
        && area
            .station
            .as_ref()
            .zip(stock.station.as_ref())
            .is_some_and(|(covered, available)| {
                media_key(covered, Medium::Metro) == media_key(available, Medium::Metro)
            })
        && stock
            .metro_spec
            .as_ref()
            .and_then(|spec| spec.line)
            .zip(area.metro_line)
            .is_none_or(|(stock_line, cover_line)| stock_line == cover_line)
}

pub(super) fn has_coverage(stock: &Inventory, coverage: &[Cover]) -> bool {
    coverage
        .iter()
        .any(|area| compatible(area, stock) || station_match(area, stock))
}

fn charge(rate: &Rate, stock: &Inventory) -> Option<Cost> {
    if !rate.monthly || rate.unit != 1 {
        return None;
    }
    match rate.production {
        Production::Fixed(cost) => Some(cost),
        Production::ByArea(per) => stock.area.filter(|area| *area > 0).map(|area| Cost {
            low: per * u64::from(area),
            high: per * u64::from(area),
        }),
        Production::Unknown => None,
    }
}

fn rate_for<'a>(stock: &Inventory, rates: &'a [Rate]) -> Option<&'a Rate> {
    let mut candidates: Vec<_> = rates
        .iter()
        .filter(|rate| {
            rate.medium == stock.medium
                && (media_key(&rate.target, stock.medium) == media_key(&stock.name, stock.medium)
                    || (stock.medium == Medium::Metro
                        && !stock.format.is_empty()
                        && media_key(&rate.target, stock.medium)
                            == media_key(&format!("{}{}", stock.name, stock.format), stock.medium)))
        })
        .collect();
    if candidates.is_empty() {
        candidates.extend(
            rates
                .iter()
                .filter(|rate| rate.medium == stock.medium && same(&rate.target, &stock.grade)),
        );
    }
    if let [rate] = candidates.as_slice() {
        Some(*rate)
    } else {
        None
    }
}

fn exclude(excluded: &mut Vec<Exclusion>, name: &str, why: &'static str) {
    excluded.push(Exclusion {
        name: name.into(),
        why,
        stock: None,
        coverage: Vec::new(),
        rate: None,
        alternative_quote: None,
    });
}

fn exclude_resource(
    excluded: &mut Vec<Exclusion>,
    stock: &Inventory,
    coverage: Vec<Cover>,
    rate: Option<&Rate>,
    why: &'static str,
) {
    excluded.push(Exclusion {
        name: stock.name.clone(),
        why,
        stock: Some(stock.clone()),
        coverage,
        rate: rate.cloned(),
        alternative_quote: None,
    });
}

pub(super) struct Candidates {
    pub(super) items: Vec<Item>,
    pub(super) excluded: Vec<Exclusion>,
}

pub(super) fn candidates(
    data: &Dataset,
    selection: Selection,
    months: u64,
    required_routes: &[String],
) -> Candidates {
    let mut links = BTreeMap::<usize, Vec<Cover>>::new();
    let mut excluded = Vec::new();
    for area in data
        .coverage
        .iter()
        .filter(|area| selection.accepts(area.medium))
    {
        if area.station.is_some() {
            let mut found = false;
            for (index, _) in data
                .stock
                .iter()
                .enumerate()
                .filter(|(_, stock)| station_match(area, stock))
            {
                links.entry(index).or_default().push(area.clone());
                found = true;
            }
            if !found {
                exclude(&mut excluded, &area.name, "所选档期库存表缺少对应站点");
            }
            continue;
        }
        let mut matches = data
            .stock
            .iter()
            .enumerate()
            .filter(|(_, stock)| compatible(area, stock));
        match (matches.next(), matches.next()) {
            (Some((index, _)), None) => links.entry(index).or_default().push(area.clone()),
            (None, _) => exclude(&mut excluded, &area.name, "所选档期库存表缺少对应资源"),
            _ => exclude(
                &mut excluded,
                &area.name,
                "匹配到多个资源，需补足线路、运营公司或媒体形式",
            ),
        }
    }
    let required_keys = required_routes
        .iter()
        .map(|name| bus_route_key(name))
        .collect::<BTreeSet<_>>();
    for (index, stock) in data.stock.iter().enumerate() {
        if selection.accepts(Medium::Bus)
            && stock.medium == Medium::Bus
            && required_keys.contains(&bus_route_key(&stock.name))
        {
            links.entry(index).or_default();
        }
    }
    let mut items = Vec::new();
    for (index, coverage) in links {
        let stock = &data.stock[index];
        if stock.free == 0 {
            exclude_resource(&mut excluded, stock, coverage, None, "所选档期无可售余量");
            continue;
        }
        if stock
            .metro_spec
            .as_ref()
            .is_some_and(|spec| spec.needs_confirmation)
        {
            exclude_resource(
                &mut excluded,
                stock,
                coverage,
                None,
                "资源规格标记为需确认，待业务批准",
            );
            continue;
        }
        let Some(rate) = rate_for(stock, &data.rates) else {
            exclude_resource(&mut excluded, stock, coverage, None, "未找到唯一刊例价格");
            continue;
        };
        let Some(production) = charge(rate, stock) else {
            exclude_resource(
                &mut excluded,
                stock,
                coverage,
                Some(rate),
                "制作面积、媒体计价单位或月度报价待核",
            );
            continue;
        };
        items.push(Item {
            stock: stock.clone(),
            coverage,
            rate: rate.clone(),
            production,
            quantity: 1,
            client_required: false,
            total: Cost {
                low: rate.fee.low * months + production.low,
                high: rate.fee.high * months + production.high,
            },
        });
    }
    Candidates { items, excluded }
}

pub(super) fn unavailable_route(data: &Dataset, route: &str) -> &'static str {
    if data.empty_inventory.contains_key(&Medium::Bus) {
        return "红豆库存 API 返回该档期的公交库存为 0 条";
    }
    let key = bus_route_key(route);
    let mut records = data
        .stock
        .iter()
        .filter(|stock| stock.medium == Medium::Bus && bus_route_key(&stock.name) == key)
        .peekable();
    if records.peek().is_none() {
        "红豆同档期快照无该线路记录，库存尚未核实"
    } else if records.all(|stock| stock.free == 0) {
        "红豆记录显示该档期无可售余量"
    } else {
        "有可售记录，媒体形式或刊例价格待核"
    }
}

pub(super) fn assemble(
    data: &Dataset,
    budget: u64,
    selection: Selection,
    priority: Priority,
    months: u64,
    required_routes: &[String],
) -> Proposal {
    let primary = data.core_circle();
    let proximity = |item: &Item| priority.proximity(item, primary);
    let Candidates {
        items: mut candidates,
        mut excluded,
    } = candidates(data, selection, months, required_routes);
    let required_keys = required_routes
        .iter()
        .map(|name| bus_route_key(name))
        .collect::<BTreeSet<_>>();
    let quote_floor = quote_floor(&candidates, selection, &required_keys);
    let mut items = Vec::new();
    let mut covered = BTreeSet::new();
    let mut total = Cost { low: 0, high: 0 };
    let mut selected_kinds = BTreeSet::new();
    let mut route_checks = Vec::with_capacity(required_routes.len());
    for route in required_routes {
        let key = bus_route_key(route);
        let matches_route =
            |stock: &Inventory| stock.medium == Medium::Bus && bus_route_key(&stock.name) == key;
        let available = candidates
            .iter()
            .enumerate()
            .filter(|(_, item)| matches_route(&item.stock))
            .min_by_key(|(_, item)| (item.total.high, item.coverage.is_empty(), proximity(item)));
        let (selected, reason) = match available {
            Some((index, item)) if total.high + item.total.high <= budget => {
                let mut item = candidates.remove(index);
                item.client_required = true;
                take(
                    item,
                    &mut items,
                    &mut total,
                    &mut covered,
                    &mut selected_kinds,
                );
                (true, "已核对档期可售余量与刊例，已优先入选")
            }
            Some(_) => (false, "按刊例上限合计超过当前预算"),
            None => (false, unavailable_route(data, route)),
        };
        route_checks.push(RouteCheck {
            name: route.clone(),
            selected,
            reason,
        });
    }
    if selection == Selection::Both && selected_kinds.contains(&Medium::Bus) {
        if let Some(index) = candidates
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.stock.medium == Medium::Metro && total.high + item.total.high <= budget
            })
            .min_by_key(|(_, item)| (proximity(item), item.total.high))
            .map(|(index, _)| index)
        {
            take(
                candidates.remove(index),
                &mut items,
                &mut total,
                &mut covered,
                &mut selected_kinds,
            );
        }
    } else if selection == Selection::Both {
        let pair = candidates
            .iter()
            .enumerate()
            .filter(|(_, item)| item.stock.medium == Medium::Bus)
            .flat_map(|(a, bus)| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(move |(_, item)| {
                        item.stock.medium == Medium::Metro
                            && bus.total.high + item.total.high <= budget
                    })
                    .map(move |(b, metro)| {
                        let circles = if matches!(priority, Priority::KeyCircles) {
                            bus.coverage
                                .iter()
                                .chain(&metro.coverage)
                                .map(|area| &area.circle)
                                .collect::<BTreeSet<_>>()
                                .len()
                        } else {
                            0
                        };
                        let distance = u64::from(proximity(bus)) + u64::from(proximity(metro));
                        (
                            std::cmp::Reverse(circles),
                            distance,
                            bus.total.high + metro.total.high,
                            a,
                            b,
                        )
                    })
            })
            .min();
        if let Some((_, _, _, a, b)) = pair {
            let second = candidates.remove(a.max(b));
            let first = candidates.remove(a.min(b));
            for item in [first, second] {
                take(
                    item,
                    &mut items,
                    &mut total,
                    &mut covered,
                    &mut selected_kinds,
                );
            }
        }
    }
    let core_frequency = |item: &Item| {
        matches!(priority, Priority::CoreFrequency)
            && item.stock.medium == Medium::Bus
            && proximity(item) <= 500
    };
    for item in items.iter_mut().filter(|item| core_frequency(item)) {
        add_bus_quantity(item, &mut total, budget, 3);
    }
    loop {
        let candidate = candidates
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                total.high + item.total.high <= budget
                    && (item.stock.medium != Medium::Bus || !item.coverage.is_empty())
            })
            .min_by_key(|(_, item)| {
                let new_circles = item
                    .coverage
                    .iter()
                    .map(|area| &area.circle)
                    .filter(|circle| !covered.contains(*circle))
                    .collect::<BTreeSet<_>>()
                    .len();
                (
                    if matches!(priority, Priority::Nearby | Priority::CoreFrequency) {
                        proximity(item)
                    } else {
                        0
                    },
                    std::cmp::Reverse(new_circles),
                    item.total.high,
                    item.stock.name.as_str(),
                )
            })
            .map(|(index, _)| index);
        let Some(index) = candidate else { break };
        let item = candidates.remove(index);
        let increase_frequency = core_frequency(&item);
        take(
            item,
            &mut items,
            &mut total,
            &mut covered,
            &mut selected_kinds,
        );
        if increase_frequency && let Some(item) = items.last_mut() {
            add_bus_quantity(item, &mut total, budget, 3);
        }
    }
    for item in candidates {
        let why = if item.coverage.is_empty() {
            "此库存未匹配目标圈，且已有可出价的首选规格"
        } else {
            "在当前组合中追加将超过预算上限"
        };
        excluded.push(Exclusion::unselected(item, why));
    }
    let mut bus_items = items
        .iter_mut()
        .filter(|item| item.stock.medium == Medium::Bus && item.total.high > 0)
        .collect::<Vec<_>>();
    bus_items.sort_by_key(|item| {
        (
            proximity(item),
            item.total.high,
            item.coverage
                .iter()
                .filter_map(|cover| cover.distance)
                .min(),
        )
    });
    for item in bus_items {
        add_bus_quantity(item, &mut total, budget, item.stock.free);
    }
    Proposal {
        items,
        excluded,
        covered,
        total,
        complete_media: selection != Selection::Both || selected_kinds.len() == 2,
        required_routes: route_checks,
        quote_floor,
    }
}

fn add_bus_quantity(item: &mut Item, total: &mut Cost, budget: u64, limit: u32) {
    let unit = Cost {
        low: item.total.low / u64::from(item.quantity),
        high: item.total.high / u64::from(item.quantity),
    };
    let remaining = limit.min(item.stock.free).saturating_sub(item.quantity);
    let added = budget
        .saturating_sub(total.high)
        .checked_div(unit.high)
        .unwrap_or(u64::from(remaining))
        .min(u64::from(remaining));
    total.low += unit.low * added;
    total.high += unit.high * added;
    item.total.low += unit.low * added;
    item.total.high += unit.high * added;
    item.quantity += added as u32;
}

fn take(
    item: Item,
    items: &mut Vec<Item>,
    total: &mut Cost,
    covered: &mut BTreeSet<String>,
    kinds: &mut BTreeSet<Medium>,
) {
    total.low += item.total.low;
    total.high += item.total.high;
    kinds.insert(item.stock.medium);
    covered.extend(item.coverage.iter().map(|area| area.circle.clone()));
    items.push(item);
}
