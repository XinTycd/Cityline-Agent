use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    store::Store,
    web::{WebError, render},
};

use super::{
    super::{
        BUS, Medium, load_plan, optimized_snapshot_variants, plan::bus_route_key,
        platform::ReachCheck, schedule::Calendar, visuals,
    },
    Task, get_task, snapshot_pins,
};

const UNAVAILABLE: WebError = (StatusCode::CONFLICT, "请先生成已锁定采购清单的提案。");
const INVALID_FILTER: WebError = (
    StatusCode::BAD_REQUEST,
    "请选择本提案的核验范围、档期、方案或缺项类型。",
);

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Kind {
    Inventory,
    Map,
    Route,
    Exit,
    Reach,
    Operations,
}

impl Kind {
    const ALL: [Self; 6] = [
        Self::Inventory,
        Self::Map,
        Self::Route,
        Self::Exit,
        Self::Reach,
        Self::Operations,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Map => "map",
            Self::Route => "route",
            Self::Exit => "exit",
            Self::Reach => "reach",
            Self::Operations => "operations",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Inventory => "库存与组合",
            Self::Map => "新云线路原图",
            Self::Route => "公交轨迹",
            Self::Exit => "地铁出口实点",
            Self::Reach => "单位日触达",
            Self::Operations => "施工条件",
        }
    }
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct Gap {
    starts: String,
    ends: String,
    batch: Option<Uuid>,
    kind: Kind,
    resource: String,
    code: String,
    needed: String,
    existing: String,
}

#[derive(Template)]
#[template(path = "media-evidence-review.html")]
struct Checklist {
    heading: &'static str,
    section: &'static str,
    task: Task,
    gaps: Vec<(Gap, String)>,
    total_gaps: usize,
    periods: BTreeMap<String, String>,
    kinds: [Kind; 6],
    format: Format,
    reach_templates: BTreeMap<(String, String), Uuid>,
    exit_templates: BTreeMap<(String, String), Uuid>,
    overview_missing: Vec<&'static str>,
    refresh_inventory: bool,
    refresh_evidence: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Format {
    scope: Scope,
    download: Option<u8>,
    reach_template: Option<Uuid>,
    exit_template: Option<Uuid>,
    starts: String,
    variant: String,
    kind: String,
}

#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Scope {
    #[default]
    Internal,
    Publication,
}

impl Format {
    fn publication(&self) -> bool {
        self.scope == Scope::Publication
    }

    fn scope_name(&self) -> &'static str {
        if self.publication() {
            "publication"
        } else {
            "internal"
        }
    }

    fn matches(&self, gap: &Gap, variants: &str) -> bool {
        (self.starts.is_empty() || self.starts == gap.starts)
            && (self.kind.is_empty() || self.kind == gap.kind.id())
            && (self.variant.is_empty() || variants.split('／').any(|code| code == self.variant))
    }
}

pub(super) async fn checklist(
    State(store): State<Store>,
    Path(id): Path<Uuid>,
    Query(format): Query<Format>,
) -> Result<Response, WebError> {
    let task = get_task(&store, id).await?;
    if !task.can_refresh_evidence() {
        return Err(UNAVAILABLE);
    }
    let portfolio = task.portfolio.as_ref().ok_or(UNAVAILABLE)?;
    let options = &task.options;
    let pins = snapshot_pins(options);
    if (format.reach_template.is_some() && format.exit_template.is_some())
        || (format.publication()
            && (format.reach_template.is_some() || format.exit_template.is_some()))
    {
        return Err(INVALID_FILTER);
    }
    let template_start = if let Some(batch) = format.reach_template.or(format.exit_template) {
        Some(
            pins.iter()
                .find_map(|(starts, id)| (*id == batch).then_some(starts.as_str()))
                .ok_or((StatusCode::NOT_FOUND, "该快照不属于本提案。"))?,
        )
    } else {
        None
    };
    let mut variants = if let Some(rolling) = &options.rolling {
        let calendar = Calendar::load(&store, options, rolling).await?;
        calendar.variants(options, portfolio)
    } else {
        let (_, snapshot) = load_plan(store, options.clone()).await?;
        optimized_snapshot_variants(options, &snapshot.ok_or(UNAVAILABLE)?, portfolio)
    };
    for variant in &mut variants {
        variant.set_route_overview(&task.route_overviews);
    }
    let overview_missing = variants
        .iter()
        .filter(|variant| {
            if format.publication() {
                variant.bus_lines > 0 && !variant.verified_route_overview()
            } else {
                variant.overview_pending()
            }
        })
        .map(|variant| variant.code)
        .collect();
    if let Some(starts) = template_start.filter(|_| format.reach_template.is_some()) {
        let csv = reach_csv(&variants, starts).ok_or((
            StatusCode::CONFLICT,
            "该档期没有入选资源需要补录单位日触达。",
        ))?;
        return Ok(csv_attachment(format!("cityline-reach-{starts}.csv"), csv));
    }
    let mut gaps = collect(&variants, &pins, &task.reach_checks, format.scope);
    if let Some(starts) = template_start {
        let csv = exit_csv(&gaps, starts)
            .ok_or((StatusCode::CONFLICT, "该档期没有入选资源需要补录具体出口。"))?;
        return Ok(csv_attachment(format!("cityline-exits-{starts}.csv"), csv));
    }
    let periods: BTreeMap<_, _> = variants
        .iter()
        .flat_map(|variant| &variant.periods)
        .map(|period| (period.starts.clone(), period.ends.clone()))
        .collect();
    if (!format.starts.is_empty() && !periods.contains_key(&format.starts))
        || !matches!(format.variant.as_str(), "" | "A" | "B")
        || (!format.kind.is_empty() && !Kind::ALL.iter().any(|kind| kind.id() == format.kind))
    {
        return Err(INVALID_FILTER);
    }
    let total_gaps = gaps.len();
    let refresh_inventory = format.publication()
        && variants.iter().any(|variant| {
            variant
                .periods
                .iter()
                .any(|period| !period.platform_inventory)
        });
    let refresh_evidence = format.publication()
        && gaps
            .iter()
            .any(|(gap, _)| matches!(gap.kind, Kind::Map | Kind::Exit | Kind::Reach));
    gaps.retain(|(gap, variants)| format.matches(gap, variants));
    if format.download == Some(1) {
        return Ok(csv_attachment(
            "cityline-evidence.csv".into(),
            to_csv(&gaps),
        ));
    }
    let templates = |kind| {
        gaps.iter()
            .filter_map(|(gap, _)| {
                let batch = gap.batch.filter(|_| gap.kind == kind)?;
                Some(((gap.starts.clone(), gap.ends.clone()), batch))
            })
            .collect()
    };
    let reach_templates = if format.publication() {
        BTreeMap::new()
    } else {
        templates(Kind::Reach)
    };
    let exit_templates = if format.publication() {
        BTreeMap::new()
    } else {
        templates(Kind::Exit)
    };
    Ok(render(Checklist {
        heading: if format.publication() {
            "正式报告核验清单"
        } else {
            "提案补证清单"
        },
        section: "tasks",
        task,
        gaps,
        total_gaps,
        periods,
        kinds: Kind::ALL,
        format,
        reach_templates,
        exit_templates,
        overview_missing,
        refresh_inventory,
        refresh_evidence,
    }))
}

fn csv_attachment(filename: String, contents: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".into()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename={filename}"),
            ),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        contents,
    )
        .into_response()
}

fn reach_csv(variants: &[visuals::Variant], starts: &str) -> Option<String> {
    let mut rows = BTreeMap::<(Medium, String, u32), &str>::new();
    for line in variants
        .iter()
        .flat_map(|variant| &variant.periods)
        .filter(|period| period.starts == starts)
        .flat_map(|period| &period.rows)
    {
        let resource = if line.media == BUS || line.stock.code.is_empty() {
            &line.stock.name
        } else {
            &line.stock.code
        };
        for &month in &line.missing_reach_months {
            rows.entry((
                line.media,
                visuals::resource_key(line.media, resource),
                month,
            ))
            .or_insert(resource);
        }
    }
    if rows.is_empty() {
        return None;
    }
    let mut csv =
        "\u{feff}媒体类型,资源编号或线路,统计月份,每日触达人次,计量单位,来源记录,估算基准\r\n"
            .to_owned();
    for ((medium, _, month), resource) in rows {
        let unit = if medium == BUS {
            "人次/辆/天"
        } else {
            "人次/点位/天"
        };
        write_csv_row(
            &mut csv,
            [
                medium.label(),
                resource,
                &format!("2023-{month:02}"),
                "",
                unit,
                "",
                "",
            ],
        );
    }
    Some(csv)
}

fn exit_csv(gaps: &[(Gap, String)], starts: &str) -> Option<String> {
    let assets: BTreeSet<_> = gaps
        .iter()
        .filter(|(gap, _)| gap.kind == Kind::Exit && gap.starts == starts)
        .map(|(gap, _)| {
            let resource = if gap.code.is_empty() {
                &gap.resource
            } else {
                &gap.code
            };
            (resource.as_str(), gap.resource.as_str())
        })
        .collect();
    if assets.is_empty() {
        return None;
    }
    let mut csv = "\u{feff}点位编号或名称,点位名称,经度,纬度,坐标系,来源记录\r\n".to_owned();
    for (resource, name) in assets {
        write_csv_row(&mut csv, [resource, name, "", "", "BD-09", ""]);
    }
    Some(csv)
}

fn collect(
    variants: &[visuals::Variant],
    pins: &BTreeMap<String, Uuid>,
    checks: &[ReachCheck],
    scope: Scope,
) -> Vec<(Gap, String)> {
    let mut gaps = BTreeMap::<Gap, Vec<&str>>::new();
    let publication = scope == Scope::Publication;
    for variant in variants {
        let has_overview = if publication {
            variant.verified_route_overview()
        } else {
            variant.route_overview.is_some()
        };
        for period in &variant.periods {
            let batch = pins.get(&period.starts).copied();
            let mut add = |kind, resource, code, needed, existing| {
                gaps.entry(Gap {
                    starts: period.starts.clone(),
                    ends: period.ends.clone(),
                    batch,
                    kind,
                    resource,
                    code,
                    needed,
                    existing,
                })
                .or_default()
                .push(variant.code);
            };
            if publication && batch.is_some() && !period.platform_inventory {
                add(
                    Kind::Inventory,
                    String::new(),
                    String::new(),
                    "按本档期从红豆和新云重新获取数据，锁定有授权记录的库存与覆盖快照。".into(),
                    "当前库存来自本地或历史快照".into(),
                );
            } else if !period.available {
                let (needed, existing) = if batch.is_none() {
                    ("查询完整档期的红豆空车数及对应刊例。", "缺少同档期库存快照")
                } else if !period.required_complete {
                    (
                        "逐条复核客户指定线路的库存和刊例，确认可用替代。",
                        "已锁定库存快照",
                    )
                } else {
                    (
                        "核对所选媒体的可售量、点位规格、刊例及预算条件。",
                        "已锁定库存快照",
                    )
                };
                add(
                    Kind::Inventory,
                    String::new(),
                    String::new(),
                    needed.into(),
                    existing.into(),
                );
            }
            let needs_map = period.screenshot_pending
                || (publication && period.rows.iter().any(|row| row.media == BUS));
            if needs_map && !has_overview {
                let routes = period
                    .rows
                    .iter()
                    .filter(|row| row.media == BUS)
                    .map(|row| bus_route_key(&row.stock.name))
                    .collect::<BTreeSet<_>>();
                add(
                    Kind::Map,
                    routes.into_iter().collect::<Vec<_>>().join("、"),
                    String::new(),
                    "在工作台按采购清单重新取证。本段线路须出现在方案总图中。".into(),
                    String::new(),
                );
            }
            for row in &period.rows {
                let mut for_resource = |kind, needed, existing| {
                    add(
                        kind,
                        row.stock.name.clone(),
                        row.stock.code.clone(),
                        needed,
                        existing,
                    );
                };
                let missing_geometry = if publication && row.media != BUS {
                    !row.has_platform_exit()
                } else {
                    !period.has_geometry(row)
                };
                if missing_geometry && !(row.media == BUS && (needs_map || has_overview)) {
                    if row.media == BUS {
                        for_resource(
                            Kind::Route,
                            "提供本线路的新云轨迹，或本案完整线路组合的新云截图。".into(),
                            String::new(),
                        );
                    } else {
                        for_resource(
                            Kind::Exit,
                            if publication {
                                "从授权平台取得该点位编号对应的具体出口 BD-09 坐标。".into()
                            } else {
                                "提供该编号对应的具体出口 BD-09 坐标和官方来源。".into()
                            },
                            if publication && period.has_geometry(row) {
                                "当前坐标来自本地补录，可用于内部核验".into()
                            } else if row.coverage.iter().any(|cover| cover.media_point.is_some()) {
                                "已有新云站级落点".into()
                            } else {
                                String::new()
                            },
                        );
                    }
                }
                let missing_months: BTreeSet<_> = if publication {
                    row.missing_platform_reach_months().collect()
                } else {
                    row.missing_reach_months.iter().copied().collect()
                };
                if !missing_months.is_empty() {
                    let months = missing_months
                        .iter()
                        .map(|month| format!("2027-{month:02}"))
                        .collect::<Vec<_>>()
                        .join("、");
                    let unit = if row.media == BUS {
                        "人次/辆/天"
                    } else {
                        "人次/点位/天"
                    };
                    let mut existing = Vec::new();
                    if publication
                        && row
                            .evidence
                            .iter()
                            .any(|entry| !entry.row.platform_verified)
                    {
                        existing.push("部分月份采用本地触达记录，可用于内部核验".into());
                    }
                    if !row.historical_reach.is_empty() {
                        let sources = row
                            .historical_reach
                            .iter()
                            .map(|reach| reach.reference.as_str())
                            .collect::<Vec<_>>();
                        existing.push(format!("已有历史标签人流 · {}", sources.join("、")));
                    }
                    let resource = if row.stock.code.is_empty() {
                        &row.stock.name
                    } else {
                        &row.stock.code
                    };
                    let key = visuals::resource_key(row.media, resource);
                    for check in checks.iter().filter(|check| {
                        check.starts == period.starts
                            && check.media == row.media
                            && missing_months.contains(&check.month)
                            && visuals::resource_key(check.media, &check.resource) == key
                    }) {
                        let mut query =
                            format!("本次查询 2023-{:02} · {}", check.month, check.exact.label());
                        if let Some(baseline) = check.baseline {
                            query.push_str(&format!("。2023-05 基准查询 · {}", baseline.label()));
                        }
                        existing.push(query);
                    }
                    for_resource(
                        Kind::Reach,
                        if publication {
                            format!(
                                "{months} 缺少平台确认的 {unit}。请按入选资源及统计月份重新取证。"
                            )
                        } else {
                            format!(
                                "{months} 缺少 {unit}。请提供统计月份和来源，复用历史测算基准时注明口径。"
                            )
                        },
                        existing.join("。"),
                    );
                }
                if publication
                    && row
                        .historical_reach
                        .iter()
                        .any(|entry| !entry.platform_verified)
                {
                    for_resource(
                        Kind::Reach,
                        "正式报告使用可追溯到授权平台的历史标签记录。".into(),
                        "当前含本地历史标签记录".into(),
                    );
                }
            }
            if period.operations_required && !period.rows.is_empty() {
                if let Some(operations) = &period.operations {
                    for (label, work) in operations.steps() {
                        let mut reasons = Vec::new();
                        if work.shortage > 0 {
                            reasons.push(format!("含天气缓冲还缺 {} 晚", work.shortage));
                        }
                        if work.outside_period {
                            reasons.push("施工窗口与投放档期冲突".into());
                        }
                        if work.overlaps {
                            reasons.push("施工窗口重叠".into());
                        }
                        if !reasons.is_empty() {
                            add(
                                Kind::Operations,
                                label.into(),
                                String::new(),
                                "请运营复核施工窗口与产能。".into(),
                                reasons.join(" · "),
                            );
                        }
                    }
                } else {
                    add(
                        Kind::Operations,
                        "上刊与下刊".into(),
                        String::new(),
                        "请运营确认夜间窗口、单晚产能和天气缓冲。".into(),
                        String::new(),
                    );
                }
            }
        }
    }
    gaps.into_iter()
        .map(|(gap, variants)| (gap, variants.join("／")))
        .collect()
}

fn to_csv(gaps: &[(Gap, String)]) -> String {
    let mut csv =
        "\u{feff}投放开始,投放结束,方案,缺项,资源,资源编号,需要的依据,已有线索,库存快照\r\n"
            .to_owned();
    for (gap, variants) in gaps {
        let batch = gap.batch.map(|id| id.to_string()).unwrap_or_default();
        let cols = [
            &gap.starts,
            &gap.ends,
            variants,
            gap.kind.label(),
            &gap.resource,
            &gap.code,
            &gap.needed,
            &gap.existing,
            &batch,
        ];
        write_csv_row(&mut csv, cols);
    }
    csv
}

fn write_csv_row<'a>(csv: &mut String, columns: impl IntoIterator<Item = &'a str>) {
    csv.push_str(
        &columns
            .into_iter()
            .map(safe_cell)
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push_str("\r\n");
}

fn safe_cell(value: &str) -> String {
    // An external resource name must stay plain text when opened in a spreadsheet.
    let escaped = value.replace('"', "\"\"");
    let prefix = if value.trim_start().starts_with(['=', '+', '-', '@']) {
        "'"
    } else {
        ""
    };
    format!("\"{prefix}{escaped}\"")
}
