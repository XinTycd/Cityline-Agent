use std::collections::{BTreeMap, BTreeSet, HashSet};

use good_lp::{
    Expression, Solution, SolutionStatus, SolverModel, Variable, WithInitialSolution,
    WithTimeLimit, microlp, variable, variables,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::plan::{
    self, Candidates, Cost, Dataset, Exclusion, Item, Medium, Priority, Proposal, RouteCheck,
    Selection,
};
use super::{Snapshot, visuals::Strategy};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct Policy {
    pub(super) emphasis: Priority,
    pub(super) hard_cap: Option<u64>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Portfolio {
    plans: [BTreeMap<String, Frozen>; 2],
}

#[derive(Clone, Deserialize, Serialize)]
struct Frozen {
    batch: Option<String>,
    ends: String,
    target: u64,
    cap: Option<u64>,
    allocation: Allocation,
}

impl Portfolio {
    pub(super) fn with_evidence(
        mut self,
        replacements: &BTreeMap<String, (Uuid, Uuid)>,
    ) -> Option<Self> {
        for plan in &mut self.plans {
            if !plan.keys().eq(replacements.keys()) {
                return None;
            }
            for (entry, (before, after)) in plan.values_mut().zip(replacements.values()) {
                if entry.batch.as_deref()?.parse::<Uuid>().ok()? != *before {
                    return None;
                }
                entry.batch = Some(after.to_string());
            }
        }
        Some(self)
    }

    pub(super) fn record(&mut self, strategy: Strategy, snapshot: &Snapshot, request: Request<'_>) {
        self.plans[strategy.index()].insert(
            snapshot.starts.clone(),
            Frozen {
                batch: snapshot.id.clone(),
                ends: snapshot.ends.clone(),
                target: request.target,
                cap: request.hard_cap,
                allocation: request.solve(),
            },
        );
    }

    pub(super) fn proposal(
        &self,
        strategy: Strategy,
        snapshot: &Snapshot,
        media: Selection,
        months: u64,
        routes: &[String],
    ) -> Option<(u64, Proposal)> {
        let entry = self.plans[strategy.index()].get(&snapshot.starts)?;
        if entry.batch != snapshot.id || entry.ends != snapshot.ends {
            return None;
        }
        let request = Request {
            data: &snapshot.data,
            target: entry.target,
            hard_cap: entry.cap,
            media,
            priority: strategy.priority(),
            emphasis: Priority::Balanced,
            months,
            routes,
        };
        Some((entry.target, request.proposal(&entry.allocation)?))
    }

    pub(super) fn engine_counts(&self) -> [usize; 4] {
        let mut counts = [0; 4];
        for phase in self.plans.iter().flat_map(BTreeMap::values) {
            counts[match phase.allocation.engine {
                Engine::Optimal => 0,
                Engine::Feasible => 1,
                Engine::Rules => 2,
                Engine::Unavailable => 3,
            }] += 1;
        }
        counts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Engine {
    Optimal,
    Feasible,
    Rules,
    Unavailable,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Allocation {
    quantities: Vec<u32>,
    pub(super) engine: Engine,
}

pub(super) struct Request<'a> {
    pub(super) data: &'a Dataset,
    pub(super) target: u64,
    pub(super) hard_cap: Option<u64>,
    pub(super) media: Selection,
    pub(super) priority: Priority,
    pub(super) emphasis: Priority,
    pub(super) months: u64,
    pub(super) routes: &'a [String],
}

fn maximum(item: &Item) -> u32 {
    if item.stock.medium == Medium::Bus && !item.coverage.is_empty() {
        item.stock.free
    } else {
        1
    }
}

impl Request<'_> {
    fn candidates(&self) -> Candidates {
        plan::candidates(self.data, self.media, self.months, self.routes)
    }

    pub(super) fn solve(&self) -> Allocation {
        let Candidates { items, .. } = self.candidates();
        let fallback = self.fallback(&items);
        self.optimize(&items, &fallback).unwrap_or(fallback)
    }

    pub(super) fn spending_bounds(&self) -> (u64, u64) {
        let required: BTreeSet<_> = self
            .routes
            .iter()
            .map(|name| plan::bus_route_key(name))
            .collect();
        let candidates = self.candidates();
        let minimum_quote = plan::quote_floor(&candidates.items, self.media, &required).high;
        let maximum_quote = candidates
            .items
            .iter()
            .map(|item| item.total.high * u64::from(maximum(item)))
            .sum();
        (minimum_quote, maximum_quote)
    }

    fn optimize(&self, items: &[Item], fallback: &Allocation) -> Option<Allocation> {
        if items.is_empty() {
            return Some(Allocation {
                quantities: Vec::new(),
                engine: Engine::Unavailable,
            });
        }
        let mut vars = variables!();
        let units: Vec<_> = items
            .iter()
            .map(|item| vars.add(variable().integer().min(0).max(maximum(item))))
            .collect();
        let used: Vec<_> = items
            .iter()
            .map(|_| vars.add(variable().binary()))
            .collect();
        let scale = self.target.max(1) as f64;
        let quoted: Expression = items
            .iter()
            .zip(&units)
            .map(|(item, quantity)| *quantity * (item.total.high as f64 / scale))
            .sum();
        let gap = vars.add(variable().min(0));
        let primary = self
            .data
            .coverage
            .first()
            .map(|cover| cover.circle.as_str());
        let required: BTreeSet<_> = self
            .routes
            .iter()
            .map(|route| plan::bus_route_key(route))
            .collect();
        let mut circles: BTreeMap<&str, Vec<Variable>> = BTreeMap::new();
        let mut bus_lines: BTreeMap<String, Vec<Variable>> = BTreeMap::new();
        let mut media: BTreeMap<Medium, Vec<Variable>> = BTreeMap::new();
        let mut core_units = Vec::new();
        let mut nearby_metro = Vec::new();
        for (index, (item, &chosen)) in items.iter().zip(&used).enumerate() {
            for area in item
                .coverage
                .iter()
                .map(|cover| cover.circle.as_str())
                .collect::<BTreeSet<_>>()
            {
                circles.entry(area).or_default().push(chosen);
            }
            media.entry(item.stock.medium).or_default().push(chosen);
            let near = item.coverage.iter().any(|cover| {
                Some(cover.circle.as_str()) == primary
                    && cover.distance.is_some_and(|distance| distance <= 500)
            });
            match item.stock.medium {
                Medium::Bus => {
                    bus_lines
                        .entry(plan::bus_route_key(&item.stock.name))
                        .or_default()
                        .push(chosen);
                    if near {
                        core_units.push((index, vars.add(variable().min(0).max(3))));
                    }
                }
                Medium::Metro if near => nearby_metro.push(chosen),
                Medium::Metro => {}
            }
        }
        let coverage: Vec<_> = circles
            .values()
            .map(|group| (group, vars.add(variable().binary())))
            .collect();
        let routes: Vec<_> = bus_lines
            .values()
            .map(|group| (group, vars.add(variable().binary())))
            .collect();
        let distinct = |groups: &[(&Vec<Variable>, Variable)]| -> Expression {
            groups.iter().map(|(_, value)| *value).sum::<Expression>() / groups.len().max(1) as f64
        };
        let frequency = core_units
            .iter()
            .map(|(_, count)| *count)
            .sum::<Expression>()
            / (3 * core_units.len().max(1)) as f64;
        let nearby_exits =
            nearby_metro.iter().copied().sum::<Expression>() / nearby_metro.len().max(1) as f64;
        let coverage_reward = distinct(&coverage);
        let near_reward = 0.75 * frequency + 0.25 * nearby_exits;
        let benefit = match self.priority {
            Priority::KeyCircles => 0.08 * coverage_reward.clone() + 0.04 * distinct(&routes),
            Priority::CoreFrequency | Priority::Nearby => 0.12 * near_reward.clone(),
            Priority::Balanced => Expression::default(),
        };
        let extra = match self.emphasis {
            Priority::KeyCircles => 0.025 * coverage_reward,
            Priority::Nearby | Priority::CoreFrequency => 0.025 * near_reward,
            Priority::Balanced => Expression::default(),
        };
        let make_requirement = |group| {
            let missed = self.hard_cap.map(|_| vars.add(variable().binary()));
            (group, missed)
        };
        let requirements: Vec<_> = required
            .iter()
            .filter_map(|key| bus_lines.get(key))
            .chain(media.values())
            .map(make_requirement)
            .collect();
        let unmet = requirements
            .iter()
            .filter_map(|(_, missed)| *missed)
            .sum::<Expression>();
        let quantities = &fallback.quantities;
        let selected: HashSet<_> = used
            .iter()
            .zip(quantities)
            .filter_map(|(&choice, &quantity)| (quantity > 0).then_some(choice))
            .collect();
        let group_selected =
            |group: &[Variable]| group.iter().any(|choice| selected.contains(choice));
        let valid_seed = requirements
            .iter()
            .all(|(group, missed)| missed.is_some() || group_selected(group));
        let mut seed = Vec::new();
        if valid_seed {
            for ((&unit, &choice), &quantity) in units.iter().zip(&used).zip(quantities) {
                seed.push((unit, f64::from(quantity)));
                seed.push((choice, u8::from(quantity > 0).into()));
            }
            let quote = self.quote(items, fallback);
            let deviation = (quote as f64 / scale - 1.0).abs();
            seed.push((gap, deviation));
            for (group, indicator) in coverage.iter().chain(&routes) {
                seed.push((*indicator, u8::from(group_selected(group)).into()));
            }
            for &(index, score) in &core_units {
                seed.push((score, f64::from(quantities[index].min(3))));
            }
            for (group, missed) in &requirements {
                if let Some(missed) = missed {
                    seed.push((*missed, u8::from(!group_selected(group)).into()));
                }
            }
        }
        let mut model = vars
            .minimise(gap + 3.0 * unmet - benefit - extra)
            .using(microlp)
            .with_time_limit(0.7);
        if valid_seed {
            model = model.with_initial_solution(seed);
        }
        model.add_constraint((quoted.clone() - gap).leq(1));
        model.add_constraint((quoted.clone() + gap).geq(1));
        if let Some(cap) = self.hard_cap {
            model.add_constraint(quoted.leq(cap as f64 / scale));
        }
        for ((item, &quantity), &chosen) in items.iter().zip(&units).zip(&used) {
            model.add_constraint((quantity - chosen).geq(0));
            model.add_constraint((quantity - f64::from(maximum(item)) * chosen).leq(0));
        }
        for (group, indicator) in coverage.into_iter().chain(routes) {
            model.add_constraint((group.iter().copied().sum::<Expression>() - indicator).geq(0));
        }
        for (index, score) in core_units {
            model.add_constraint((units[index] - score).geq(0));
        }
        for (group, missed) in requirements {
            let mut members = group.iter().copied().sum::<Expression>();
            if let Some(missed) = missed {
                members += missed;
            }
            model.add_constraint(members.geq(1));
        }
        let solution = model.solve().ok()?;
        let quantities = units
            .iter()
            .map(|&unit| solution.value(unit).round() as u32)
            .collect();
        let engine = match solution.status() {
            SolutionStatus::Optimal => Engine::Optimal,
            _ => Engine::Feasible,
        };
        let allocated = Allocation { quantities, engine };
        self.valid(&allocated, items).then_some(allocated)
    }

    fn valid(&self, allocation: &Allocation, items: &[Item]) -> bool {
        allocation.quantities.len() == items.len()
            && items
                .iter()
                .zip(&allocation.quantities)
                .all(|(item, &quantity)| quantity <= maximum(item))
            && self
                .hard_cap
                .is_none_or(|cap| self.quote(items, allocation) <= cap)
    }

    fn quote(&self, items: &[Item], allocation: &Allocation) -> u64 {
        items
            .iter()
            .zip(&allocation.quantities)
            .map(|(item, &quantity)| item.total.high * u64::from(quantity))
            .sum()
    }

    fn fallback(&self, items: &[Item]) -> Allocation {
        let identity = |item: &Item| {
            (
                item.stock.medium,
                item.stock.source.file.clone(),
                item.stock.source.row,
            )
        };
        let rule = |limit| {
            plan::assemble(
                self.data,
                limit,
                self.media,
                self.priority,
                self.months,
                self.routes,
            )
        };
        let mut plan = rule(
            self.hard_cap
                .map_or(self.target, |cap| cap.min(self.target)),
        );
        if !plan.complete_media || !plan.complete_required() || plan.items.is_empty() {
            let limit = self
                .hard_cap
                .or_else(|| plan.quote_floor.verified_total())
                .unwrap_or(self.target);
            if limit
                > self
                    .hard_cap
                    .map_or(self.target, |cap| cap.min(self.target))
            {
                plan = rule(limit);
            }
        }
        let mut quantities: BTreeMap<_, _> = plan
            .items
            .iter()
            .map(|item| (identity(item), item.quantity))
            .collect();
        let allocation = Allocation {
            engine: Engine::Rules,
            quantities: items
                .iter()
                .map(|item| {
                    quantities
                        .remove(&identity(item))
                        .unwrap_or_default()
                        .min(maximum(item))
                })
                .collect(),
        };
        if self.valid(&allocation, items) {
            allocation
        } else {
            Allocation {
                quantities: vec![0; items.len()],
                engine: Engine::Rules,
            }
        }
    }

    pub(super) fn proposal(&self, allocation: &Allocation) -> Option<Proposal> {
        let Candidates {
            items,
            mut excluded,
        } = self.candidates();
        if !self.valid(allocation, &items) {
            return None;
        }
        let required: BTreeSet<_> = self
            .routes
            .iter()
            .map(|route| plan::bus_route_key(route))
            .collect();
        let qualified: BTreeSet<_> = items
            .iter()
            .filter(|item| item.stock.medium == Medium::Bus)
            .map(|item| plan::bus_route_key(&item.stock.name))
            .collect();
        let quote_floor = plan::quote_floor(&items, self.media, &required);
        let mut selected = Vec::new();
        let mut total = Cost { low: 0, high: 0 };
        let mut covered = BTreeSet::new();
        let mut kinds = BTreeSet::new();
        let mut selected_routes = BTreeSet::new();
        for (mut item, &quantity) in items.into_iter().zip(&allocation.quantities) {
            let is_bus = item.stock.medium == Medium::Bus;
            let route = is_bus.then(|| plan::bus_route_key(&item.stock.name));
            item.client_required = route.as_ref().is_some_and(|key| required.contains(key));
            if quantity == 0 {
                excluded.push(Exclusion {
                    name: item.stock.name.clone(),
                    why: "有可售记录，未入选当前预算与覆盖目标下的组合",
                    stock: Some(item.stock),
                    coverage: item.coverage,
                    rate: Some(item.rate),
                });
                continue;
            }
            if let Some(route) = route {
                selected_routes.insert(route);
            }
            item.quantity = quantity;
            item.total.low *= u64::from(quantity);
            item.total.high *= u64::from(quantity);
            total.low += item.total.low;
            total.high += item.total.high;
            covered.extend(item.coverage.iter().map(|cover| cover.circle.clone()));
            kinds.insert(item.stock.medium);
            selected.push(item);
        }
        let required_routes = self
            .routes
            .iter()
            .map(|name| {
                let key = plan::bus_route_key(name);
                let selected = selected_routes.contains(&key);
                let reason = if selected {
                    "已核对档期可售余量与刊例，已优先入选"
                } else if qualified.contains(&key) {
                    "有可售且可核价的记录，本次组合未能纳入，需确认采购条件"
                } else {
                    plan::unavailable_route(self.data, name)
                };
                RouteCheck {
                    name: name.clone(),
                    selected,
                    reason,
                }
            })
            .collect();
        Some(Proposal {
            items: selected,
            excluded,
            covered,
            total,
            complete_media: kinds.len() == if self.media == Selection::Both { 2 } else { 1 },
            required_routes,
            quote_floor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::plan::{Cover, Inventory, Production, Rate, Source};

    fn fixture(rows: &[(Medium, &str, u64, u32, &str, u32)]) -> Dataset {
        let mut data = Dataset {
            stock: Vec::new(),
            empty_inventory: BTreeMap::new(),
            coverage: Vec::new(),
            rates: Vec::new(),
            demo: true,
            supplemental_metro_center: None,
            evidence: Default::default(),
        };
        for (row, &(medium, name, price, free, circle, distance)) in rows.iter().enumerate() {
            let source = Source {
                file: "test".into(),
                row,
            };
            data.stock.push(Inventory {
                medium,
                name: name.into(),
                station: None,
                station_attested: false,
                code: name.into(),
                metro_spec: None,
                operator: String::new(),
                grade: String::new(),
                format: String::new(),
                free,
                area: None,
                source: source.clone(),
            });
            data.rates.push(Rate {
                medium,
                target: name.into(),
                fee: Cost {
                    low: price,
                    high: price,
                },
                production: Production::Fixed(Cost { low: 0, high: 0 }),
                unit: 1,
                monthly: true,
                source: source.clone(),
            });
            data.coverage.push(Cover {
                medium,
                name: name.into(),
                circle: circle.into(),
                radius: 1500,
                distance: Some(distance),
                station: None,
                metro_line: None,
                operator: String::new(),
                format: String::new(),
                center_point: None,
                media_point: None,
                source,
            });
        }
        data
    }

    #[test]
    fn target_allows_a_costlier_required_mix_and_explicit_cap_does_not() {
        let data = fixture(&[
            (Medium::Bus, "10", 1_050_000, 2, "核心圈", 200),
            (Medium::Metro, "中山站1号口", 999_000, 1, "核心圈", 150),
        ]);
        let routes = vec!["10路".into()];
        let mut request = Request {
            data: &data,
            target: 2_000_000,
            hard_cap: None,
            media: Selection::Both,
            priority: Priority::KeyCircles,
            emphasis: Priority::Balanced,
            months: 1,
            routes: &routes,
        };
        let proposal = request.proposal(&request.solve()).unwrap();
        assert!(proposal.complete_media && proposal.complete_required());
        assert_eq!(proposal.total.high, 2_049_000);
        assert_eq!(proposal.quantity(Medium::Bus), 1);
        assert_eq!(proposal.quote_floor.verified_total(), Some(2_049_000));
        assert_eq!(proposal.quote_floor.resources.len(), 2);
        request.hard_cap = Some(2_000_000);
        let limited = request.proposal(&request.solve()).unwrap();
        assert!(!limited.complete_media);
        assert!(limited.complete_required());
        assert!(limited.total.high <= 2_000_000);
        assert_eq!(limited.quote_floor.verified_total(), Some(2_049_000));
        let unmatched = ["11路".to_owned()];
        request.routes = &unmatched;
        let waiting = request.proposal(&request.solve()).unwrap();
        assert_eq!(waiting.quote_floor.verified_total(), None);
    }

    #[test]
    fn equal_quotes_can_choose_different_routes_for_the_two_objectives() {
        let data = fixture(&[
            (Medium::Metro, "中山站1号口", 200_000, 1, "核心圈", 150),
            (Medium::Bus, "10", 1_800_000, 1, "另一商圈", 200),
            (Medium::Bus, "11", 1_800_000, 1, "核心圈", 200),
        ]);
        let mut request = Request {
            data: &data,
            target: 2_000_000,
            hard_cap: None,
            media: Selection::Both,
            priority: Priority::KeyCircles,
            emphasis: Priority::Balanced,
            months: 1,
            routes: &[],
        };
        let a = request.proposal(&request.solve()).unwrap();
        request.priority = Priority::CoreFrequency;
        let b = request.proposal(&request.solve()).unwrap();
        assert_eq!(a.total.high, 2_000_000);
        assert_eq!(b.total.high, 2_000_000);
        assert!(a.items.iter().any(|item| item.stock.name == "10"));
        assert!(b.items.iter().any(|item| item.stock.name == "11"));
    }
}
