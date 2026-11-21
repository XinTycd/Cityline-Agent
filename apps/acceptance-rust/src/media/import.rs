use std::{collections::BTreeMap, io::Cursor, sync::LazyLock};

use calamine::{Reader, open_workbook_auto_from_rs};
use regex::Regex;

use super::plan::{Cost, Cover, Inventory, Medium, MetroEvidence, Production, Rate, Source, key};

pub(super) struct Table {
    name: String,
    rows: Vec<Vec<String>>,
}

fn alias(cell: &str) -> String {
    key(cell).trim_start_matches('\u{feff}').to_owned()
}

impl Table {
    pub(super) fn read(name: String, bytes: &[u8]) -> Result<Self, &'static str> {
        let rows = if name.to_ascii_lowercase().ends_with(".csv") {
            let line_ends: Vec<_> = bytes
                .iter()
                .enumerate()
                .filter_map(|(index, byte)| {
                    (*byte == b'\n' || (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n')))
                        .then_some(index)
                })
                .collect();
            let mut reader = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .from_reader(bytes);
            let mut rows = Vec::new();
            for row in reader.records().take(4001) {
                let row = row.map_err(|_| "CSV 无法读取，请使用 UTF-8 编码。")?;
                let line = row.position().map_or(rows.len() + 1, |position| {
                    // The CSV cursor can still point to a separator or a skipped blank line.
                    let start = position.byte() as usize;
                    let record_start = start
                        + bytes[start..]
                            .iter()
                            .take_while(|&&byte| matches!(byte, b'\r' | b'\n'))
                            .count();
                    line_ends.partition_point(|&end| end < record_start) + 1
                });
                if line > 4000 {
                    return Err("CSV 超过 4000 行，请缩小导出范围。");
                }
                rows.resize_with(line.saturating_sub(1).max(rows.len()), Vec::new);
                rows.push(row.iter().map(str::to_owned).collect());
            }
            rows
        } else if name.to_ascii_lowercase().ends_with(".xlsx")
            || name.to_ascii_lowercase().ends_with(".xls")
        {
            let mut workbook = open_workbook_auto_from_rs(Cursor::new(bytes))
                .map_err(|_| "Excel 工作簿无法读取。")?;
            let sheet = workbook
                .sheet_names()
                .first()
                .cloned()
                .ok_or("Excel 工作簿没有工作表。")?;
            let range = workbook
                .worksheet_range(&sheet)
                .map_err(|_| "无法读取 Excel 首个工作表。")?;
            let first_row = range.start().map_or(0, |(row, _)| row as usize);
            if first_row.saturating_add(range.height()) > 4000 || range.width() > 200 {
                return Err("工作表超过 4000 行或 200 列，请缩小导出范围。");
            }
            let mut rows = vec![Vec::new(); first_row];
            rows.extend(
                range
                    .rows()
                    .map(|row| row.iter().map(|cell| cell.to_string()).collect()),
            );
            rows
        } else {
            return Err("请上传 .xlsx、.xls 或 UTF-8 .csv 文件。");
        };
        if rows.is_empty() || rows.len() > 4000 || rows.iter().any(|r| r.len() > 200) {
            return Err("表格为空或超过 4000 行、200 列。");
        }
        Ok(Self { name, rows })
    }

    fn header(&self, required: &[&[&str]]) -> Result<(usize, Vec<String>), &'static str> {
        self.rows
            .iter()
            .take(12)
            .enumerate()
            .find_map(|(i, row)| {
                let headers: Vec<_> = row.iter().map(|name| alias(name)).collect();
                required
                    .iter()
                    .all(|keys| column(&headers, keys).is_some())
                    .then_some((i, headers))
            })
            .ok_or("未识别表头。请使用赛用系统的导出表，或对照导入说明调整列名。")
    }

    fn source(&self, index: usize) -> Source {
        Source {
            file: self.name.clone(),
            row: index + 1,
        }
    }
}

const BUS_NAME: &[&str] = &["线路名称", "公交线路", "公交线路名称", "线路", "线路号"];
const METRO_NAME: &[&str] = &[
    "完整点位",
    "点位名称",
    "点位",
    "媒体点位",
    "媒体位置",
    "位置",
];
const STATION: &[&str] = &[
    "地铁站点",
    "站点名称",
    "车站名称",
    "站点",
    "地铁站",
    "地铁站名",
    "车站",
    "站名",
    "站名称",
];
const EXIT: &[&str] = &[
    "出入口名称",
    "出入口",
    "出入口编号",
    "出入口号",
    "出口",
    "出口名称",
    "出口编号",
    "出口号",
];
const FREE: &[&str] = &[
    "空车数",
    "空位数",
    "可售数量",
    "空余数量",
    "空余点位",
    "可用数量",
    "剩余数量",
];
const OPERATOR: &[&str] = &["运营公司", "行车公司"];
const METRO_CODE: &[&str] = &["地铁编号"];
const FORMAT: &[&str] = &[
    "媒体形式",
    "媒体名称",
    "媒体类型",
    "广告形式",
    "规格",
    "粘贴形式",
];
const GRADE: &[&str] = &[
    "线路等级",
    "媒体等级",
    "等级",
    "点位等级",
    "车站等级",
    "站点等级",
    "级别",
];
const AREA: &[&str] = &[
    "面积",
    "可制作面积",
    "制作面积",
    "广告面积",
    "媒体面积",
    "面积㎡",
    "面积（㎡）",
    "面积(平方米)",
    "面积（m²）",
];

fn column(headers: &[String], options: &[&str]) -> Option<usize> {
    options
        .iter()
        .find_map(|option| headers.iter().position(|name| name == &alias(option)))
}

fn field(row: &[String], index: Option<usize>) -> &str {
    index
        .and_then(|index| row.get(index))
        .map_or("", |value| value.trim())
}

struct MediaColumns {
    name: Option<usize>,
    station: Option<usize>,
    exit: Option<usize>,
    operator: Option<usize>,
    grade: Option<usize>,
    format: Option<usize>,
}

impl MediaColumns {
    fn new(
        headers: &[String],
        medium: Medium,
        allow_station_lookup: bool,
    ) -> Result<Self, &'static str> {
        let name = column(
            headers,
            if medium == Medium::Bus {
                BUS_NAME
            } else {
                METRO_NAME
            },
        );
        let station = column(headers, STATION);
        let exit = column(headers, EXIT);
        if name.is_none()
            && (medium == Medium::Bus
                || (station.is_none() && !allow_station_lookup)
                || exit.is_none())
        {
            return Err("公交需要线路名称。地铁需要完整点位，或同时具备站点和出口列。");
        }
        Ok(Self {
            name,
            station,
            exit,
            operator: column(headers, OPERATOR),
            grade: column(headers, GRADE),
            format: column(headers, FORMAT),
        })
    }

    fn name(
        &self,
        row: &[String],
        medium: Medium,
        station_override: &str,
    ) -> Result<Option<String>, &'static str> {
        let value = field(row, self.name);
        let name = if medium == Medium::Bus {
            if value.is_empty() {
                return Ok(None);
            }
            value.to_owned()
        } else {
            let exit = field(row, self.exit);
            let station = self.station_name(row, station_override);
            if !station.is_empty() && !exit.is_empty() {
                let formatted = if exit.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                    format!("{exit}号口")
                } else {
                    exit.into()
                };
                format!("{station}{formatted}")
            } else if !value.is_empty() {
                value.to_owned()
            } else if station.is_empty() && exit.is_empty() {
                return Ok(None);
            } else {
                return Err("地铁数据存在缺少站点或出口的行。");
            }
        };
        if name.chars().count() > 128 {
            return Err("媒体标识超过 128 个字符。");
        }
        Ok(Some(name))
    }

    fn station_name<'a>(&self, row: &'a [String], station_override: &'a str) -> &'a str {
        self.station
            .map_or(station_override, |index| field(row, Some(index)))
    }
}

fn media_header(
    table: &Table,
    medium: Medium,
    inventory: bool,
    allow_station_lookup: bool,
) -> Result<(usize, Vec<String>), &'static str> {
    let extra = if inventory { vec![FREE] } else { vec![] };
    let mut named = extra.clone();
    named.push(if medium == Medium::Bus {
        BUS_NAME
    } else {
        METRO_NAME
    });
    table.header(&named).or_else(|error| {
        if medium == Medium::Bus {
            return Err(error);
        }
        let mut split = extra;
        split.extend([STATION, EXIT]);
        table.header(&split).or_else(|error| {
            if inventory && allow_station_lookup {
                table.header(&[FREE, METRO_CODE, EXIT])
            } else {
                Err(error)
            }
        })
    })
}

pub(super) struct MetroSpec {
    station: String,
    name: String,
    area: Option<u32>,
    grade: String,
    format: String,
    evidence: MetroEvidence,
}

fn parse_area(value: &str) -> Option<u32> {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| *value > 0.0 && *value <= 10_000.0)
        .map(|value| value.ceil() as u32)
}

fn metro_line(value: &str) -> Option<u32> {
    let value = key(value);
    value
        .strip_prefix('l')
        .or_else(|| {
            value
                .strip_prefix("地铁")
                .and_then(|value| value.strip_suffix("号线"))
        })?
        .parse::<u32>()
        .ok()
        .filter(|line| *line > 0)
}

pub(super) fn metro_specs(table: &Table) -> Result<BTreeMap<String, MetroSpec>, &'static str> {
    let (start, headers) = table.header(&[METRO_CODE, STATION, EXIT, AREA, GRADE])?;
    let cols = MediaColumns::new(&headers, Medium::Metro, false)?;
    let code = column(&headers, METRO_CODE);
    let area = column(&headers, AREA);
    let note = column(&headers, &["备注"]);
    let line = column(&headers, &["所属线路"]);
    let status = column(&headers, &["状态"]);
    let publication = column(&headers, &["可上下刊"]);
    let mut specs = BTreeMap::new();
    for (index, row) in table.rows.iter().enumerate().skip(start + 1) {
        let id = field(row, code);
        let Some(name) = cols.name(row, Medium::Metro, "")? else {
            continue;
        };
        let remark = field(row, note);
        let inactive = matches!(
            key(field(row, status)).as_str(),
            "关闭" | "停用" | "禁用" | "下线"
        );
        let denied = matches!(
            key(field(row, publication)).as_str(),
            "否" | "不可" | "不可上下刊"
        );
        let needs_approval = [
            "合同外点位",
            "提前申请",
            "需申请",
            "暂不可",
            "停止上刊",
            "不同意",
            "禁止",
        ]
        .iter()
        .any(|term| remark.contains(term));
        let spec = MetroSpec {
            station: cols.station_name(row, "").into(),
            name,
            area: parse_area(field(row, area)),
            grade: field(row, cols.grade).into(),
            format: field(row, cols.format).into(),
            evidence: MetroEvidence {
                source: table.source(index),
                remark: remark.into(),
                needs_confirmation: inactive || denied || needs_approval,
                line: metro_line(field(row, line)),
            },
        };
        if id.is_empty() || id.len() > 100 || specs.insert(key(id), spec).is_some() {
            return Err("地铁规格表存在缺失或重复的点位编号。");
        }
    }
    if specs.is_empty() {
        return Err("地铁规格表没有可识别的点位。");
    }
    Ok(specs)
}

pub(super) fn stock(
    table: &Table,
    medium: Medium,
    station_override: &str,
    specs: &BTreeMap<String, MetroSpec>,
    allow_empty_api: bool,
) -> Result<Vec<Inventory>, &'static str> {
    let allow_station_lookup = !station_override.is_empty() || !specs.is_empty();
    let (start, headers) = media_header(table, medium, true, allow_station_lookup)?;
    let cols = MediaColumns::new(&headers, medium, allow_station_lookup)?;
    let code = column(&headers, METRO_CODE);
    let free = column(&headers, FREE);
    let area = column(&headers, AREA);
    let mut result = Vec::new();
    for (index, row) in table.rows.iter().enumerate().skip(start + 1) {
        if row.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let id = field(row, code);
        let spec = specs.get(&key(id));
        let station_fallback = spec.map_or(station_override, |spec| spec.station.as_str());
        let Some(name) = cols.name(row, medium, station_fallback)? else {
            continue;
        };
        if let Some(spec) = spec
            && (key(&name) != key(&spec.name)
                || (!station_override.is_empty() && key(station_override) != key(&spec.station)))
        {
            return Err("地铁库存与规格表的同编号站点或出口不一致，请核对原表。");
        }
        let free = field(row, free)
            .parse::<u32>()
            .map_err(|_| "库存表的可售数量必须是非负整数。")?;
        let area = parse_area(field(row, area));
        let grade = field(row, cols.grade);
        let format = field(row, cols.format);
        if let Some(spec) = spec
            && ((!grade.is_empty() && !spec.grade.is_empty() && key(grade) != key(&spec.grade))
                || area.zip(spec.area).is_some_and(|(a, b)| a != b))
        {
            return Err("地铁库存与规格表的同编号等级或面积冲突，请核对原表。");
        }
        result.push(Inventory {
            medium,
            name,
            station: (medium == Medium::Metro)
                .then(|| cols.station_name(row, station_fallback).to_owned())
                .filter(|name| !name.is_empty()),
            station_attested: medium == Medium::Metro
                && cols.station.is_none()
                && spec.is_none()
                && !station_override.is_empty(),
            code: id.into(),
            metro_spec: spec.map(|spec| spec.evidence.clone()),
            free,
            area: spec.and_then(|spec| spec.area).or(area),
            operator: field(row, cols.operator).into(),
            grade: spec
                .filter(|spec| !spec.grade.is_empty())
                .map_or(grade, |spec| &spec.grade)
                .into(),
            format: spec
                .filter(|spec| !spec.format.is_empty())
                .map_or(format, |spec| &spec.format)
                .into(),
            source: table.source(index),
        });
    }
    let api_confirmed_empty = allow_empty_api
        && start == 0
        && table
            .rows
            .iter()
            .skip(1)
            .flatten()
            .all(|value| value.trim().is_empty());
    if result.is_empty() && !api_confirmed_empty {
        Err("库存表没有可识别的媒体记录。")
    } else {
        Ok(result)
    }
}

pub(super) fn coverage(
    table: &Table,
    medium: Medium,
    circle: &str,
    radius: u32,
) -> Result<Vec<Cover>, &'static str> {
    if let Some(rows) = cloud_coverage(table, Some(medium), None)? {
        return Ok(rows);
    }
    let (start, headers) = media_header(table, medium, false, false)?;
    let cols = MediaColumns::new(&headers, medium, false)?;
    let circle_col = column(&headers, &["目标圈", "目标地标", "地标", "门店"]);
    let radius_col = column(&headers, &["半径", "半径米", "半径(米)"]);
    let mut result = Vec::new();
    for (index, row) in table.rows.iter().enumerate().skip(start + 1) {
        let Some(name) = cols.name(row, medium, "")? else {
            continue;
        };
        let place = field(row, circle_col);
        let place = if place.is_empty() { circle } else { place };
        let value = field(row, radius_col);
        let distance = if value.is_empty() {
            radius
        } else {
            value
                .trim_end_matches("米")
                .trim()
                .parse::<u32>()
                .map_err(|_| "覆盖半径必须以米计。")?
        };
        if place.is_empty() || place.chars().count() > 70 || !(1..=5000).contains(&distance) {
            return Err("覆盖表须提供目标圈名称和 1–5000 米半径。");
        }
        result.push(Cover {
            center_point: None,
            media_point: None,
            medium,
            name,
            station: None,
            metro_line: None,
            circle: place.into(),
            radius: distance,
            distance: None,
            operator: field(row, cols.operator).into(),
            format: field(row, cols.format).into(),
            source: table.source(index),
        });
    }
    if result.is_empty() {
        Err("覆盖表没有可识别的媒体记录。")
    } else {
        Ok(result)
    }
}

pub(super) fn combined_coverage(
    table: &Table,
    center: &str,
    supplemental_metro_center: &str,
) -> Result<Vec<Cover>, &'static str> {
    let requested = center
        .split('|')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let mut verified = std::collections::BTreeSet::new();
    if requested.len() > 8 || requested.iter().any(|name| !verified.insert(key(name))) {
        return Err("主目标圈最多 8 个且不能重复。");
    }
    let mut rows = Vec::new();
    if requested.is_empty() {
        rows = cloud_coverage(table, None, None)?.ok_or("请上传含明细分析的新云官方覆盖表。")?;
        if rows
            .iter()
            .any(|row| key(&row.circle) != key(&rows[0].circle))
        {
            return Err("合并覆盖表有多个中心点，请在补充参数中指定本案中心点。");
        }
    }
    for name in requested {
        rows.extend(
            cloud_coverage(table, None, Some(name))?.ok_or("请上传含明细分析的新云官方覆盖表。")?,
        );
    }
    if !supplemental_metro_center.is_empty() {
        if rows
            .iter()
            .any(|row| key(&row.circle) == key(supplemental_metro_center))
        {
            return Err("地铁补充圈须与主目标圈使用不同中心点。");
        }
        let metro = cloud_coverage(table, Some(Medium::Metro), Some(supplemental_metro_center))?
            .ok_or("请上传含明细分析的新云官方覆盖表。")?;
        rows.extend(metro);
    }
    Ok(rows)
}

fn meters(value: &str) -> Option<u32> {
    if let Some(km) = value.trim().strip_suffix("公里") {
        let distance = km.trim().parse::<f64>().ok()? * 1_000.0;
        (0.0..=f64::from(u32::MAX))
            .contains(&distance)
            .then_some(distance.round() as u32)
    } else {
        value.trim().trim_end_matches("米").trim().parse().ok()
    }
}

fn cloud_coverage(
    table: &Table,
    wanted_medium: Option<Medium>,
    center: Option<&str>,
) -> Result<Option<Vec<Cover>>, &'static str> {
    let marker = table
        .rows
        .iter()
        .position(|row| row.first().is_some_and(|value| alias(value) == "明细分析"));
    let Some(header_index) = marker.map(|index| index + 1) else {
        return Ok(None);
    };
    let headers = table
        .rows
        .get(header_index)
        .ok_or("覆盖明细缺少表头。")?
        .iter()
        .map(|value| alias(value))
        .collect::<Vec<_>>();
    let place = column(&headers, &["中心点名称"]);
    let kind = column(&headers, &["媒体类型"]);
    let name = column(&headers, &["媒体名称"]);
    let radius = column(&headers, &["覆盖半径"]);
    let distance = column(&headers, &["最近距离"]);
    let station_col = column(&headers, &["最近站点"]);
    let center_lng = column(&headers, &["中心点经度"]);
    let center_lat = column(&headers, &["中心点纬度"]);
    let trace_lng = column(&headers, &["最近轨迹点经度"]);
    let trace_lat = column(&headers, &["最近轨迹点纬度"]);
    let station_lng = column(&headers, &["站点经度"]);
    let station_lat = column(&headers, &["站点纬度"]);
    if [place, kind, name, radius].iter().any(Option::is_none) {
        return Err("新云覆盖明细缺少中心点、媒体名称、类型或半径。");
    }
    let center = center.map(key);
    let mut result = Vec::new();
    for (index, row) in table.rows.iter().enumerate().skip(header_index + 1) {
        let medium = match field(row, kind) {
            "公交线路" | "公交车身" => Medium::Bus,
            "地铁出入口" | "地铁出口" => Medium::Metro,
            _ => continue,
        };
        let circle = field(row, place);
        if wanted_medium.is_some_and(|expected| expected != medium)
            || center
                .as_ref()
                .is_some_and(|expected| *expected != key(circle))
        {
            continue;
        }
        let title = field(row, name);
        let station_name = field(row, station_col);
        let station =
            (medium == Medium::Metro && !station_name.is_empty()).then(|| station_name.to_owned());
        let radius = meters(field(row, radius))
            .filter(|value| (1..=5000).contains(value))
            .ok_or("新云覆盖明细的半径须为 1–5000 米。")?;
        let raw_distance = field(row, distance);
        let distance = if raw_distance.is_empty() {
            None
        } else {
            Some(meters(raw_distance).ok_or("新云覆盖明细的最近距离须使用米或公里。")?)
        };
        if title.is_empty()
            || title.chars().count() > 128
            || circle.is_empty()
            || circle.chars().count() > 120
        {
            return Err("新云覆盖明细的媒体名称或中心点为空或过长。");
        }
        if distance.is_some_and(|distance| distance > radius) {
            return Err("新云覆盖明细的最近距离超出所列半径，请核对该行。");
        }
        result.push(Cover {
            center_point: coordinate(field(row, center_lng), field(row, center_lat)),
            media_point: match medium {
                Medium::Bus => coordinate(field(row, trace_lng), field(row, trace_lat)),
                Medium::Metro => coordinate(field(row, station_lng), field(row, station_lat)),
            },
            medium,
            name: station.as_ref().map_or_else(
                || title.to_owned(),
                |station| format!("{title} · {station}"),
            ),
            station,
            metro_line: if medium == Medium::Metro {
                metro_line(title)
            } else {
                None
            },
            operator: String::new(),
            format: String::new(),
            circle: circle.into(),
            radius,
            distance,
            source: table.source(index),
        });
    }
    if result.is_empty() {
        Err(if center.is_some() {
            "新云覆盖明细没有该中心点的可用媒体，请核对中心点原文。"
        } else {
            "新云覆盖明细未找到所选媒体类型。"
        })
    } else {
        Ok(Some(result))
    }
}

fn coordinate(lng: &str, lat: &str) -> Option<super::visuals::Coordinate> {
    fn number(value: &str) -> Option<f64> {
        value
            .trim()
            .trim_start_matches('=')
            .trim_matches('"')
            .parse()
            .ok()
    }
    super::visuals::Coordinate::shanghai(number(lng)?, number(lat)?)
}

pub(super) fn reach(table: &Table) -> Result<Vec<super::visuals::Reach>, &'static str> {
    let kinds = &["媒体类型"] as &[&str];
    let assets = &["资源编号或线路"];
    let months = &["统计月份"];
    let counts = &["每日触达人次"];
    let units = &["计量单位"];
    let references = &["来源记录"];
    let basis = &["估算基准"];
    let (start, headers) = table.header(&[kinds, assets, months, counts, units, references])?;
    let col = |aliases: &[&str]| column(&headers, aliases);
    let mut result = Vec::new();
    for (index, row) in table.rows.iter().enumerate().skip(start + 1) {
        if row.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let (medium, unit_verified) = match (field(row, col(kinds)), field(row, col(units))) {
            ("公交车身", "人次/辆/天") => (Medium::Bus, true),
            ("地铁出入口", "人次/点位/天") => (Medium::Metro, true),
            ("公交车身", "平台标签日均人次") => (Medium::Bus, false),
            ("地铁出入口", "平台标签日均人次") => (Medium::Metro, false),
            _ => return Err("触达口径需要填写单位日触达或新云平台标签日均人次。"),
        };
        let month = field(row, col(months))
            .strip_prefix("2023-")
            .filter(|month| month.len() == 2)
            .and_then(|month| month.parse::<u32>().ok())
            .filter(|month| (1..=12).contains(month))
            .ok_or("触达统计月份须为 2023-MM。")?;
        let daily = field(row, col(counts))
            .replace([',', '，'], "")
            .parse::<u32>()
            .ok()
            .filter(|daily| *daily > 0 && *daily <= 100_000_000)
            .ok_or("每日触达须是原系统提供的正整数人次。")?;
        let reference = field(row, col(references));
        let resource = field(row, col(assets));
        if !super::visuals::valid_reference(reference) || !super::visuals::valid_resource(resource)
        {
            return Err("触达记录需要有效的资源标识和新云查询依据。");
        }
        let baseline = match field(row, col(basis)) {
            "" => false,
            "新云2023-05基准" if month == 5 && unit_verified => true,
            _ => return Err("历史基准只接受 2023 年 5 月的新云单位日触达。"),
        };
        result.push(super::visuals::Reach {
            medium,
            resource: resource.into(),
            month,
            daily,
            reference: reference.into(),
            source: table.source(index),
            baseline,
            unit_verified,
        });
    }
    if result.is_empty() {
        return Err("触达表没有可用记录。");
    }
    Ok(result)
}

fn money(text: &str) -> Option<Cost> {
    static MONEY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^约?(\d+)(?:[-–—~至](\d+))?(万元?|千元?|元)?$").expect("valid price pattern")
    });
    let normalized = key(text).replace([',', '，'], "");
    let capture = MONEY.captures(&normalized)?;
    let multiplier = match capture.get(3).map(|m| m.as_str()) {
        Some("万" | "万元") => 10_000,
        Some("千" | "千元") => 1_000,
        _ => 1,
    };
    let low = capture[1].parse::<u64>().ok()?.checked_mul(multiplier)?;
    let high = capture.get(2).map_or(Some(low), |m| {
        m.as_str().parse::<u64>().ok()?.checked_mul(multiplier)
    })?;
    (low <= high && high <= 100_000_000).then_some(Cost { low, high })
}

pub(super) fn rates(table: &Table) -> Result<Vec<Rate>, &'static str> {
    let kinds = &["媒体类型"] as &[&str];
    let targets = &["发布线路/位置/级别/形式", "匹配对象", "点位或等级"];
    let prices = &["刊例价（区间化口径）", "刊例价", "发布费"];
    let units = &["媒体数量", "计价数量"];
    let periods = &["发布期", "计价周期"];
    let charges = &["制作费单价（区间化口径）", "制作费"];
    let (start, headers) = table.header(&[kinds, targets, prices, units, periods, charges])?;
    let kind_col = column(&headers, kinds);
    let target_col = column(&headers, targets);
    let price_col = column(&headers, prices);
    let unit_col = column(&headers, units);
    let period_col = column(&headers, periods);
    let charge_col = column(&headers, charges);
    let mut last_kind = "";
    let mut result = Vec::new();
    for (index, row) in table.rows.iter().enumerate().skip(start + 1) {
        let kind = field(row, kind_col);
        if !kind.is_empty() {
            last_kind = kind;
        }
        let medium = if last_kind.contains("地铁出入口") || last_kind == "地铁" {
            Medium::Metro
        } else if matches!(
            last_kind,
            "公交" | "公交车" | "公交车身" | "双层巴士" | "南京路铛铛车"
        ) {
            Medium::Bus
        } else {
            continue;
        };
        let target = field(row, target_col);
        let Some(fee) = money(field(row, price_col)) else {
            continue;
        };
        let unit = field(row, unit_col).parse::<u32>().unwrap_or(0);
        let charge = field(row, charge_col);
        let production = if let Some(cost) = money(charge) {
            Production::Fixed(cost)
        } else if let Some((amount, rest)) = charge.split_once("元/") {
            match (
                amount.parse::<u64>(),
                rest.starts_with('㎡') || rest.starts_with("平方米"),
            ) {
                (Ok(per), true) if per <= 100_000 => Production::ByArea(per),
                _ => Production::Unknown,
            }
        } else {
            Production::Unknown
        };
        if !target.is_empty() && fee.high > 0 {
            result.push(Rate {
                medium,
                target: target.into(),
                fee,
                production,
                unit,
                monthly: matches!(key(field(row, period_col)).as_str(), "1月" | "1个月" | "月"),
                source: table.source(index),
            });
        }
    }
    if result.is_empty() {
        Err("刊例表没有可识别的公交或地铁报价。")
    } else {
        Ok(result)
    }
}
